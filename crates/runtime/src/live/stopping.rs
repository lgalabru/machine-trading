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
        Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

/// How long [`Connections::stop`] waits for the threads: a connection's
/// thread ends as soon as its stream is shut, so a second is generous, and
/// a reload stalls the host's notifications while it waits.
pub const STOP_WITHIN: Duration = Duration::from_secs(5);

/// Threads and connections at most: the control and event sockets' bounds
/// (16 each) and their listeners, with room.
const KEPT_MAX: usize = 256;

/// The sockets' threads and open connections.
#[derive(Debug, Default)]
pub struct Connections {
    stopped: AtomicBool,
    streams: Mutex<Vec<UnixStream>>,
    threads: Mutex<Vec<JoinHandle<()>>>,
}

impl Connections {
    /// Whether [`Connections::stop`] was called: a listener woken then
    /// accepts nothing more.
    pub fn stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }

    /// Keep `stream` to shut it on stop. One kept after the stop is shut
    /// at once.
    pub fn keep(&self, stream: &UnixStream) -> std::io::Result<()> {
        let kept = stream.try_clone()?;
        if self.stopped() {
            shut(&kept);
            return Ok(());
        }
        let mut streams = lock(&self.streams);
        // Connections closed by their peers are dropped from the list
        // lazily: a shut on a closed socket is harmless.
        if streams.len() >= KEPT_MAX {
            streams.retain(|s| s.peer_addr().is_ok());
        }
        streams.push(kept);
        Ok(())
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
        for stream in lock(&self.streams).drain(..) {
            shut(&stream);
        }
        let deadline = Instant::now() + STOP_WITHIN;
        let threads: Vec<JoinHandle<()>> = lock(&self.threads).drain(..).collect();
        let mut left = 0;
        for thread in threads {
            while !thread.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
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
        connections.keep(&server).unwrap();
        connections
            .spawn("reader", move || {
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
        let connections = Connections::default();
        let (release, wait) = std::sync::mpsc::channel::<()>();
        connections
            .spawn("stuck", move || {
                let _ = wait.recv_timeout(STOP_WITHIN * 2);
            })
            .unwrap();
        assert_eq!(connections.stop(), 1);
        drop(release);
        connections.keep(&a).unwrap();
        let mut line = String::new();
        assert_eq!(BufReader::new(b).read_line(&mut line).unwrap(), 0);
    }
}
