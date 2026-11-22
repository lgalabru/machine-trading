//! The runtime's event socket (`docs/sidecar.md`): every connection gets
//! a hello, then each [`Event`] as the runtime emits it, one JSON line
//! each ([`mtx_core::events`]).
//!
//! Emitting costs nothing when nobody listens, and never waits when
//! someone does: an event is serialized once into a broadcast queue, and
//! each connection's thread writes what it reads from it. A connection that
//! falls [`EVENTS_QUEUED_MAX`] behind loses the oldest and is told how many.

use std::{
    io::Write,
    os::unix::net::{UnixListener, UnixStream},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use mtx_core::{
    control::{Command, Reply},
    events::{Event, Line, PROTOCOL},
};
use tokio::sync::{broadcast, mpsc, oneshot};

/// Events a connection may fall behind by before it loses the oldest: a
/// few seconds of every strategy's steps and fills, and of the market's
/// trades.
pub const EVENTS_QUEUED_MAX: usize = 8192;

/// Connections at once: a sidecar, its UI's server and a person watching.
pub const SUBSCRIBERS_MAX: usize = 16;

/// A reader that takes longer than this to accept a write is dropped: it
/// would lose events anyway, and holds a thread.
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);

/// Where the runtime's events go: cloned to everything that emits.
#[derive(Debug, Clone)]
pub struct Events {
    sender: broadcast::Sender<Arc<str>>,
}

impl Default for Events {
    fn default() -> Self {
        let (sender, _) = broadcast::channel(EVENTS_QUEUED_MAX);
        Self { sender }
    }
}

impl Events {
    /// Hand `event` to every connection. Nothing to do, not even the
    /// serialization, when there is none.
    pub fn emit(&self, event: Event) {
        if self.sender.receiver_count() == 0 {
            return;
        }
        let mut text = match serde_json::to_string(&Line {
            at_us: now_us(),
            event,
        }) {
            Ok(text) => text,
            Err(error) => {
                tracing::error!(%error, "an event couldn't be written");
                return;
            }
        };
        text.push('\n');
        // No receiver left since the check: nobody to tell.
        let _ = self.sender.send(Arc::from(text));
    }

    fn subscribe(&self) -> broadcast::Receiver<Arc<str>> {
        self.sender.subscribe()
    }

    /// The events' lines, in this process: a test, or a consumer that
    /// lives in the runtime. Each line is a [`Line`], newline included.
    pub fn receiver(&self) -> broadcast::Receiver<Arc<str>> {
        self.subscribe()
    }
}

/// A command, and where the strategies' answer goes: how a connection
/// gets the status its hello carries.
pub type Request = (Command, oneshot::Sender<Reply>);

/// Serve `events` on `socket`, on a thread of its own, until `commands`
/// closes (the strategies stopped).
pub fn listen(
    socket: &Path,
    events: Events,
    commands: mpsc::Sender<Request>,
    connections: Arc<super::stopping::Connections>,
) -> Result<(), String> {
    match std::fs::remove_file(socket) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("{}: {e}", socket.display())),
    }
    // Blocking: the strategies' thread wakes it with a connection when they
    // stop, and it sees `commands` closed and ends.
    let listener = UnixListener::bind(socket).map_err(|e| format!("{}: {e}", socket.display()))?;
    let open = Arc::new(AtomicUsize::new(0));
    let kept = connections.clone();
    connections
        .spawn("mtx-live-events", move || {
            let connections = kept;
            loop {
                let accepted = listener.accept();
                if commands.is_closed() || connections.stopped() {
                    return;
                }
                match accepted {
                    Ok((stream, _)) => {
                        if open.load(Ordering::Acquire) >= SUBSCRIBERS_MAX {
                            tracing::warn!(
                                max = SUBSCRIBERS_MAX,
                                "an event connection refused: too many"
                            );
                            continue;
                        }
                        if let Err(error) = connections.keep(&stream) {
                            tracing::warn!(%error, "an event connection can't be kept");
                            continue;
                        }
                        open.fetch_add(1, Ordering::AcqRel);
                        // Subscribed here, so the writer holds no sender:
                        // once the runtime's are dropped, its queue closes.
                        let received = events.subscribe();
                        let (commands, open) = (commands.clone(), open.clone());
                        let spawned = connections.spawn("mtx-live-event-writer", move || {
                            if let Err(error) = serve(stream, received, &commands) {
                                tracing::info!(%error, "an event connection ended");
                            }
                            open.fetch_sub(1, Ordering::AcqRel);
                        });
                        if let Err(error) = spawned {
                            tracing::warn!(%error, "an event connection's thread didn't start");
                        }
                    }
                    Err(error) => tracing::warn!(%error, "an event connection failed"),
                }
            }
        })
        .map_err(|e| format!("the events thread: {e}"))?;
    Ok(())
}

/// One connection: its hello, then every event until either side stops.
fn serve(
    stream: UnixStream,
    mut received: broadcast::Receiver<Arc<str>>,
    commands: &mpsc::Sender<Request>,
) -> Result<(), String> {
    stream.set_nonblocking(false).map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(Some(WRITE_TIMEOUT))
        .map_err(|e| e.to_string())?;
    // Subscribed (by the listener) before the status is read: an event
    // between the two is seen twice, never missed.
    let (reply, replied) = oneshot::channel();
    commands
        .blocking_send((Command::Status, reply))
        .map_err(|_| "the runtime's strategies stopped".to_string())?;
    let hello = match replied.blocking_recv() {
        Ok(Reply::Ok { feed, strategies }) => Event::Hello {
            protocol: PROTOCOL,
            feed,
            strategies,
        },
        Ok(Reply::Error { error }) => return Err(error.to_string()),
        Err(_) => return Err("the runtime dropped the status".into()),
    };
    let mut text = serde_json::to_string(&Line {
        at_us: now_us(),
        event: hello,
    })
    .map_err(|e| e.to_string())?;
    text.push('\n');
    let mut writer = &stream;
    writer
        .write_all(text.as_bytes())
        .map_err(|e| e.to_string())?;
    loop {
        match received.blocking_recv() {
            Ok(line) => writer
                .write_all(line.as_bytes())
                .map_err(|e| e.to_string())?,
            Err(broadcast::error::RecvError::Lagged(dropped)) => {
                let mut text = serde_json::to_string(&Line {
                    at_us: now_us(),
                    event: Event::Lagged { dropped },
                })
                .map_err(|e| e.to_string())?;
                text.push('\n');
                writer
                    .write_all(text.as_bytes())
                    .map_err(|e| e.to_string())?;
            }
            Err(broadcast::error::RecvError::Closed) => return Ok(()),
        }
    }
}

fn now_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_micros()).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
    use mtx_core::control::FeedStatus;

    use super::*;

    /// A runtime's strategies, as far as the event socket asks: they answer
    /// `status` with nothing running.
    fn strategies() -> mpsc::Sender<Request> {
        let (commands, mut requests) = mpsc::channel::<Request>(8);
        std::thread::spawn(move || {
            while let Some((_, reply)) = requests.blocking_recv() {
                let _ = reply.send(Reply::Ok {
                    feed: FeedStatus {
                        slot: 9,
                        behind: 0,
                        dropped: 0,
                    },
                    strategies: Vec::new(),
                });
            }
        });
        commands
    }

    fn socket(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("mtx-events-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("events.sock")
    }

    fn connect(socket: &Path) -> impl Iterator<Item = std::io::Result<Line>> {
        for _ in 0..100 {
            if let Ok(lines) = mtx_core::events::subscribe(socket) {
                return lines;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("the event socket never listened");
    }

    #[test]
    fn a_connection_gets_a_hello_then_the_events() {
        let socket = socket("hello");
        let events = Events::default();
        // Nobody listens: emitting is free and nothing queues.
        events.emit(Event::Slot {
            slot: 1,
            behind: 0,
            dropped: 0,
        });
        listen(&socket, events.clone(), strategies(), Arc::default()).unwrap();
        let mut lines = connect(&socket);
        let hello = lines.next().unwrap().unwrap();
        assert!(matches!(
            hello.event,
            Event::Hello {
                protocol: PROTOCOL,
                feed: FeedStatus { slot: 9, .. },
                ..
            }
        ));
        events.emit(Event::Slot {
            slot: 10,
            behind: 1,
            dropped: 0,
        });
        let next = lines.next().unwrap().unwrap();
        assert_eq!(
            next.event,
            Event::Slot {
                slot: 10,
                behind: 1,
                dropped: 0
            }
        );
    }

    #[test]
    fn a_reader_that_falls_behind_is_told_and_never_holds_the_runtime() {
        let socket = socket("lagged");
        let events = Events::default();
        listen(&socket, events.clone(), strategies(), Arc::default()).unwrap();
        let mut lines = connect(&socket);
        assert!(matches!(
            lines.next().unwrap().unwrap().event,
            Event::Hello { .. }
        ));
        // Far more than the queue holds, emitted while the reader sleeps:
        // each emit returns at once.
        let started = std::time::Instant::now();
        let burst = EVENTS_QUEUED_MAX as u64 * 4;
        for slot in 0..burst {
            events.emit(Event::Slot {
                slot,
                behind: 0,
                dropped: 0,
            });
        }
        assert!(started.elapsed() < Duration::from_secs(5));
        let mut lagged = 0;
        let mut last = 0;
        for line in lines.by_ref() {
            match line.unwrap().event {
                Event::Lagged { dropped } => lagged += dropped,
                Event::Slot { slot, .. } => last = slot,
                other => panic!("{other:?}"),
            }
            if last == burst - 1 {
                break;
            }
        }
        assert!(lagged > 0, "the reader was told it fell behind");
        assert_eq!(last, burst - 1, "and reads on from where it can");
    }
}
