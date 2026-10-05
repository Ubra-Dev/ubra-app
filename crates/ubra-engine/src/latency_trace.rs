//! Keystroke-to-screen hop timestamps for the latency harness.
//!
//! Compiled only with the `latency-trace` feature, so shipping builds carry no
//! trace state and no extra branch on the hot path. Records *which* hop fired
//! and *when*, never the bytes involved: typed input can be a secret.

use std::sync::Mutex;
use std::time::Instant;

/// One point on the local keystroke path, in the order a keystroke meets them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Hop {
    /// The attach channel decoded an input frame from the client.
    InputDecoded,
    /// The Session handed the bytes to its PTY (Holder acknowledged the write).
    InputWritten,
    /// The attach channel finished handling the input frame, including the
    /// status and PTY-fact bookkeeping that follows the write.
    InputHandled,
    /// The Session pump received PTY output from the Holder or the log.
    OutputReceived,
    /// The Session pump published the parsed grid to its attach pump.
    GridPublished,
    /// The attach pump queued a grid frame for the client socket.
    FrameEnqueued,
}

static EVENTS: Mutex<Vec<(Hop, Instant)>> = Mutex::new(Vec::new());

/// Records that `hop` happened now.
pub fn mark(hop: Hop) {
    let now = Instant::now();
    if let Ok(mut events) = EVENTS.lock() {
        // Bounded: a harness that forgets to drain must not grow forever.
        if events.len() < 1 << 20 {
            events.push((hop, now));
        }
    }
}

/// Takes every recorded hop, oldest first.
pub fn drain() -> Vec<(Hop, Instant)> {
    EVENTS
        .lock()
        .map(|mut events| std::mem::take(&mut *events))
        .unwrap_or_default()
}
