//! The relay's side of one connection's output: a queue with a byte cap.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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
}

/// Handle used to queue frames for one connection.
#[derive(Debug, Clone)]
pub struct Outbox {
    tx: mpsc::UnboundedSender<Vec<u8>>,
    pub queued: Arc<AtomicUsize>,
    pub ctl: Arc<Ctl>,
}

impl Outbox {
    pub fn new() -> (Outbox, mpsc::UnboundedReceiver<Vec<u8>>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Outbox {
                tx,
                queued: Arc::new(AtomicUsize::new(0)),
                ctl: Arc::new(Ctl::default()),
            },
            rx,
        )
    }

    /// Queue a frame. Over `cap` queued bytes the connection is closed
    /// with `E too-slow`. Returns false when it was closed.
    pub fn send(&self, frame: Vec<u8>, cap: usize) -> bool {
        if self.ctl.killed.load(Ordering::Relaxed) {
            return false;
        }
        let n = frame.len();
        if self.queued.load(Ordering::Relaxed) + n > cap {
            self.kill(Some(crate::codec::encode(b'E', b"too-slow")));
            return false;
        }
        self.queued.fetch_add(n, Ordering::Relaxed);
        self.tx.send(frame).is_ok()
    }

    /// Stop the connection, writing `last` first when given. Frames still
    /// queued are dropped.
    pub fn kill(&self, last: Option<Vec<u8>>) {
        if self.ctl.killed.swap(true, Ordering::Relaxed) {
            return;
        }
        *self.ctl.last.lock().unwrap() = last;
        self.ctl.kill.notify_one();
    }
}
