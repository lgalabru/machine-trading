//! Stopping the runtime's sockets with nothing left behind: what lets the
//! validator reload the plugin (`agave-validator plugin reload`) instead of
//! restarting.
//!
//! On a reload the host unloads the old plugin's library and loads the new
//! one in the same process. Anything of the old one still running then (a
//! thread blocked reading a sidecar's kept connection, a second strategy
//! manager) runs code that may be unmapped, or steps strategies twice. So
//! every thread the sockets start is spawned here, every connection is
//! kept here, and [`Connections::stop`] shuts the connections (a blocked
//! read returns) and joins the threads before the plugin's `on_unload`
//! returns.

use std::{
    net::Shutdown,
    os::unix::net::UnixStream,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

/// How long [`Connections::stop`] waits for the threads: a connection's
/// thread ends as soon as its stream is shut, so a second is generous, and
/// a reload stalls the host's notifications while it waits.
pub const STOP_WITHIN: Duration = Duration::from_secs(5);

/// How often [`Connections::stop`] looks whether a thread has ended: a
/// shut connection's thread ends within a few, and a reload waits on it.
const STOP_LOOKS_EVERY: Duration = Duration::from_millis(5);

/// Threads and connections at most: the control and event sockets' bounds
/// (16 each) and their listeners, with room.
const KEPT_MAX: usize = 256;

/// The sockets' threads and open connections.
#[derive(Debug, Default)]
pub struct Connections {
    stopped: AtomicBool,
    /// Each open connection, under the number its [`Kept`] removes it by.
    streams: Mutex<Vec<(u64, UnixStream)>>,
    /// The number the next kept connection gets.
    next: AtomicU64,
    threads: Mutex<Vec<JoinHandle<()>>>,
}

/// A connection [`Connections::keep`] holds: dropped when the connection's
/// thread is done with it, which shuts the connection and forgets it. So a
/// connection the runtime ends (a slow reader, a command past its bound)
/// is closed for its peer too, and a runtime that serves many connections
/// in its life holds only the open ones.
#[derive(Debug)]
#[must_use = "dropping it at once shuts the connection"]
pub struct Kept {
    connections: Arc<Connections>,
    /// `None` for a connection kept after the stop: shut already.
    id: Option<u64>,
}

impl Drop for Kept {
    fn drop(&mut self) {
        let Some(id) = self.id else {
            return;
        };
        let mut streams = lock(&self.connections.streams);
        // Not found: the stop drained it, and shut it, first.
        if let Some(at) = streams.iter().position(|(kept, _)| *kept == id) {
            let (_, stream) = streams.swap_remove(at);
            shut(&stream);
        }
    }
}

impl Connections {
    /// Whether [`Connections::stop`] was called: a listener woken then
    /// accepts nothing more.
    pub fn stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }

    /// Keep `stream` to shut it on stop, until the [`Kept`] is dropped. One
    /// kept after the stop is shut at once. Refused past [`KEPT_MAX`] open
    /// connections, which the sockets' own bounds keep far from.
    pub fn keep(self: &Arc<Self>, stream: &UnixStream) -> std::io::Result<Kept> {
        let kept = stream.try_clone()?;
        if self.stopped() {
            shut(&kept);
            return Ok(Kept {
                connections: self.clone(),
                id: None,
            });
        }
        let mut streams = lock(&self.streams);
        if streams.len() >= KEPT_MAX {
            return Err(std::io::Error::other(format!(
                "{KEPT_MAX} connections are open already"
            )));
        }
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        streams.push((id, kept));
        assert!(streams.len() <= KEPT_MAX, "kept connections are bounded");
        Ok(Kept {
            connections: self.clone(),
            id: Some(id),
        })
    }

    /// Spawn a named thread that [`Connections::stop`] joins.
    pub fn spawn(&self, name: &str, work: impl FnOnce() + Send + 'static) -> std::io::Result<()> {
        let handle = std::thread::Builder::new().name(name.into()).spawn(work)?;
        let mut threads = lock(&self.threads);
        threads.retain(|t| !t.is_finished());
        threads.push(handle);
        assert!(
            threads.len() <= KEPT_MAX * 2,
            "threads are bounded by the sockets' bounds"
        );
        Ok(())
    }

    /// Shut every connection and join every thread, waiting at most
    /// [`STOP_WITHIN`]. How many threads were still running when it gave up
    /// (0 is a clean stop).
    pub fn stop(&self) -> usize {
        self.stopped.store(true, Ordering::Release);
        for (_, stream) in lock(&self.streams).drain(..) {
            shut(&stream);
        }
        let deadline = Instant::now() + STOP_WITHIN;
        let threads: Vec<JoinHandle<()>> = lock(&self.threads).drain(..).collect();
        let mut left = 0;
        for thread in threads {
            while !thread.is_finished() && Instant::now() < deadline {
                std::thread::sleep(STOP_LOOKS_EVERY);
            }
            if thread.is_finished() {
                // A thread that panicked has ended all the same.
                let _ = thread.join();
            } else {
                left += 1;
            }
        }
        left
    }
}

fn shut(stream: &UnixStream) {
    // Already closed by its peer: nothing to shut.
    let _ = stream.shutdown(Shutdown::Both);
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use std::{
        io::{BufRead, BufReader},
        os::unix::net::UnixListener,
        sync::Arc,
    };

    use super::*;

    /// A thread blocked reading a kept connection ends on stop, and stop
    /// reports a clean stop.
    #[test]
    fn a_blocked_reader_ends_on_stop() {
        let dir = std::env::temp_dir().join(format!("mtx-stopping-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.sock");
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let _client = UnixStream::connect(&path).unwrap();
        let (server, _) = listener.accept().unwrap();
        let connections = Arc::new(Connections::default());
        let kept = connections.keep(&server).unwrap();
        connections
            .spawn("reader", move || {
                let _kept = kept;
                let mut line = String::new();
                let read = BufReader::new(server).read_line(&mut line);
                assert!(matches!(read, Ok(0)), "{read:?}");
            })
            .unwrap();
        let started = Instant::now();
        assert_eq!(connections.stop(), 0);
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(connections.stopped());
    }

    /// A connection kept after the stop is shut at once, and a thread that
    /// never ends is reported, not waited on forever.
    #[test]
    fn late_connections_are_shut_and_stuck_threads_counted() {
        let (a, b) = UnixStream::pair().unwrap();
        let connections = Arc::new(Connections::default());
        let (release, wait) = std::sync::mpsc::channel::<()>();
        connections
            .spawn("stuck", move || {
                let _ = wait.recv_timeout(STOP_WITHIN * 2);
            })
            .unwrap();
        assert_eq!(connections.stop(), 1);
        drop(release);
        let _kept = connections.keep(&a).unwrap();
        let mut line = String::new();
        assert_eq!(BufReader::new(b).read_line(&mut line).unwrap(), 0);
    }

    /// A connection whose thread is done with it is shut for its peer and
    /// forgotten, though the runtime runs on: its kept copy neither holds
    /// it open nor counts against the bound.
    #[test]
    fn a_connection_its_thread_is_done_with_is_shut_and_forgotten() {
        let connections = Arc::new(Connections::default());
        for _ in 0..KEPT_MAX + 1 {
            let (ours, theirs) = UnixStream::pair().unwrap();
            let kept = connections.keep(&ours).unwrap();
            // The thread ends its side, as a writer to a slow reader does.
            drop(ours);
            drop(kept);
            let mut line = String::new();
            assert_eq!(BufReader::new(theirs).read_line(&mut line).unwrap(), 0);
        }
        assert!(lock(&connections.streams).is_empty());
    }

    /// Past the bound, a connection is refused rather than kept.
    #[test]
    fn open_connections_past_the_bound_are_refused() {
        let connections = Arc::new(Connections::default());
        let open: Vec<_> = (0..KEPT_MAX)
            .map(|_| {
                let (ours, theirs) = UnixStream::pair().unwrap();
                (connections.keep(&ours).unwrap(), ours, theirs)
            })
            .collect();
        let (ours, _theirs) = UnixStream::pair().unwrap();
        assert!(connections.keep(&ours).is_err());
        drop(open);
        assert!(connections.keep(&ours).is_ok());
    }
}
