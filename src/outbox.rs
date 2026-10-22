//! The relay's side of one connection's output: a queue with a byte cap.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, Notify};

/// Shared control of one connection's writer.
#[derive(Debug, Default)]
pub struct Ctl {
    /// Wakes the writer to stop.
    pub kill: Notify,
    /// Set once the writer has been told to stop.
    pub killed: AtomicBool,
    /// The last frame to write before closing, if any.
    pub last: Mutex<Option<Vec<u8>>>,
    /// Woken by the writer when the socket is closed.
    pub closed: Notify,
    /// Why the connection was stopped, for the log.
    pub cause: Mutex<Option<String>>,
}

impl Ctl {
    /// The cause given to the first `kill`, if any.
    pub fn cause(&self) -> Option<String> {
        self.cause.lock().unwrap().clone()
    }
}

/// Counters for the frames queued to one client.
#[derive(Debug, Default)]
pub struct OutStats {
    pub frames: AtomicU64,
    pub bytes: AtomicU64,
    /// Most bytes ever queued at once.
    pub high: AtomicUsize,
}

/// What `send` did.
#[derive(Debug, PartialEq, Eq)]
pub enum Sent {
    Ok,
    /// The connection was already stopping or closed.
    Closed,
    /// The queue went over its cap and the connection was stopped. The
    /// text says how many bytes it held.
    TooSlow(String),
}

/// Handle used to queue frames for one connection.
#[derive(Debug, Clone)]
pub struct Outbox {
    tx: mpsc::UnboundedSender<Vec<u8>>,
    pub queued: Arc<AtomicUsize>,
    pub ctl: Arc<Ctl>,
    pub stats: Arc<OutStats>,
}

impl Outbox {
    pub fn new() -> (Outbox, mpsc::UnboundedReceiver<Vec<u8>>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Outbox {
                tx,
                queued: Arc::new(AtomicUsize::new(0)),
                ctl: Arc::new(Ctl::default()),
                stats: Arc::new(OutStats::default()),
            },
            rx,
        )
    }

    /// Queue a frame. Over `cap` queued bytes the connection is closed
    /// with `E too-slow`.
    pub fn send(&self, frame: Vec<u8>, cap: usize) -> Sent {
        if self.ctl.killed.load(Ordering::Relaxed) {
            return Sent::Closed;
        }
        let n = frame.len();
        let total = self.queued.load(Ordering::Relaxed) + n;
        if total > cap {
            let detail = format!("{total} bytes queued (limit {cap})");
            let text = format!("too-slow {detail}");
            self.kill(
                Some(crate::codec::encode(b'E', text.as_bytes())),
                "too-slow",
            );
            return Sent::TooSlow(detail);
        }
        self.queued.fetch_add(n, Ordering::Relaxed);
        self.stats.high.fetch_max(total, Ordering::Relaxed);
        self.stats.frames.fetch_add(1, Ordering::Relaxed);
        self.stats.bytes.fetch_add(n as u64, Ordering::Relaxed);
        if self.tx.send(frame).is_ok() {
            Sent::Ok
        } else {
            Sent::Closed
        }
    }

    /// Stop the connection, writing `last` first when given. Frames still
    /// queued are dropped. `cause` is for the log; the first call wins.
    pub fn kill(&self, last: Option<Vec<u8>>, cause: &str) {
        if self.ctl.killed.swap(true, Ordering::Relaxed) {
            return;
        }
        *self.ctl.cause.lock().unwrap() = Some(cause.to_string());
        *self.ctl.last.lock().unwrap() = last;
        self.ctl.kill.notify_one();
    }
}
