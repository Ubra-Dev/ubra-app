//! Bytes on their way from the PTY pump to the output log.
//!
//! A macOS PTY hands over one kilobyte per read, so a queue of chunks carried
//! one kilobyte per entry: an allocation to hand it over, a thread wakeup to
//! receive it, and a `write(2)` to append it. At streaming rates that is tens
//! of thousands of syscalls a second per session, and a bound counted in
//! chunks was only a quarter of a megabyte of slack.
//!
//! This is one byte buffer instead. The pump appends to it, and the writer
//! swaps it for an empty one and appends the lot in one write. The writer
//! lingers briefly for a batch to build rather than waking per kilobyte. The
//! log is not on the display path (subscribers get their bytes directly) and
//! is synced only every couple of seconds, so a few milliseconds here change
//! no promise. The linger exists only while bytes are pending; an idle feed
//! parks the writer with no deadline.

use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

/// A batch this large is written at once instead of lingering for more.
pub(super) const LOG_BATCH_BYTES: usize = 64 << 10;

/// The longest a pending byte waits for its batch to fill.
pub(super) const LOG_LINGER: Duration = Duration::from_millis(4);

/// How many bytes may be waiting before the pump has to wait for the writer.
/// A filesystem stall (a truncation rewrite, most of all) is absorbed up to
/// here without stalling the PTY; beyond it the writer applies backpressure
/// rather than letting memory grow without bound.
pub(super) const LOG_PENDING_LIMIT: usize = 4 << 20;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Writer {
    /// Waiting with no deadline for the first pending byte.
    Parked,
    /// Waiting with a deadline for a batch to fill.
    Lingering,
    /// Writing, or about to look at the buffer again.
    Busy,
}

struct State {
    pending: Vec<u8>,
    writer: Writer,
    /// When the oldest pending byte arrived.
    since: Option<Instant>,
    closed: bool,
}

pub(super) struct LogFeed {
    state: Mutex<State>,
    changed: Condvar,
    batch: usize,
    linger: Duration,
    limit: usize,
    /// Every time the writer woke, so a test can prove it is not woken per
    /// chunk.
    #[cfg(test)]
    pub(super) writer_wakeups: std::sync::atomic::AtomicUsize,
}

impl LogFeed {
    pub(super) fn new() -> Self {
        Self::with_limits(LOG_BATCH_BYTES, LOG_LINGER, LOG_PENDING_LIMIT)
    }

    pub(super) fn with_limits(batch: usize, linger: Duration, limit: usize) -> Self {
        Self {
            state: Mutex::new(State {
                pending: Vec::new(),
                writer: Writer::Busy,
                since: None,
                closed: false,
            }),
            changed: Condvar::new(),
            batch,
            linger,
            limit: limit.max(batch),
            #[cfg(test)]
            writer_wakeups: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// Queues bytes for the log, waiting for room if the writer has fallen a
    /// full [`LOG_PENDING_LIMIT`] behind. Returns false once the feed is
    /// closed, so the caller appends inline instead of losing the bytes.
    pub(super) fn push(&self, bytes: &[u8]) -> bool {
        if bytes.is_empty() {
            return true;
        }
        let mut state = self.state.lock().expect("log feed");
        while !state.closed && state.pending.len() >= self.limit {
            state = self.changed.wait(state).expect("log feed");
        }
        if state.closed {
            return false;
        }
        let was_empty = state.pending.is_empty();
        if was_empty {
            state.since = Some(Instant::now());
        }
        state.pending.extend_from_slice(bytes);
        // Wake the writer only when that changes what it would do: the first
        // byte for a parked writer, or a full batch for a lingering one.
        // Anything else it collects when its linger ends.
        let wake = match state.writer {
            Writer::Parked => was_empty,
            Writer::Lingering => state.pending.len() >= self.batch,
            Writer::Busy => false,
        };
        drop(state);
        if wake {
            self.changed.notify_all();
        }
        true
    }

    /// Swaps everything pending into `batch` (which must be empty), waiting
    /// for bytes to arrive and, briefly, for a batch to build. Returns false
    /// once the feed is closed and drained.
    pub(super) fn take(&self, batch: &mut Vec<u8>) -> bool {
        debug_assert!(batch.is_empty());
        let mut state = self.state.lock().expect("log feed");
        loop {
            if state.pending.is_empty() {
                if state.closed {
                    state.writer = Writer::Busy;
                    return false;
                }
                state.writer = Writer::Parked;
                state = self.changed.wait(state).expect("log feed");
                #[cfg(test)]
                self.count_wakeup();
                continue;
            }
            let lingered = state.since.map_or(Duration::MAX, |since| since.elapsed());
            if state.closed || state.pending.len() >= self.batch || lingered >= self.linger {
                break;
            }
            state.writer = Writer::Lingering;
            let remaining = self.linger - lingered;
            state = self
                .changed
                .wait_timeout(state, remaining)
                .expect("log feed")
                .0;
            #[cfg(test)]
            self.count_wakeup();
        }
        state.writer = Writer::Busy;
        state.since = None;
        std::mem::swap(&mut state.pending, batch);
        drop(state);
        // A pump waiting for room can continue.
        self.changed.notify_all();
        true
    }

    /// Ends the feed. The writer still drains what is pending.
    pub(super) fn close(&self) {
        self.state.lock().expect("log feed").closed = true;
        self.changed.notify_all();
    }

    #[cfg(test)]
    fn count_wakeup(&self) {
        self.writer_wakeups
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Held by the writer thread: however it stops, a panic included, the feed
/// closes, so the pump appends inline instead of waiting forever for room a
/// dead writer will never make. (The channel this replaced failed its sends
/// once the receiver was gone; this keeps that property.)
pub(super) struct WriterExit<'a>(pub(super) &'a LogFeed);

impl Drop for WriterExit<'_> {
    fn drop(&mut self) {
        self.0.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;

    fn drain(feed: &LogFeed) -> Vec<Vec<u8>> {
        let mut batches = Vec::new();
        let mut batch = Vec::new();
        while feed.take(&mut batch) {
            batches.push(std::mem::take(&mut batch));
        }
        batches
    }

    #[test]
    fn kilobyte_chunks_reach_the_log_in_few_large_writes() {
        let feed = Arc::new(LogFeed::with_limits(
            64 << 10,
            Duration::from_millis(50),
            1 << 20,
        ));
        let writer = {
            let feed = Arc::clone(&feed);
            std::thread::spawn(move || drain(&feed))
        };
        let mut expected = Vec::new();
        for index in 0..1024_u32 {
            let chunk = [(index % 251) as u8; 1024];
            expected.extend_from_slice(&chunk);
            assert!(feed.push(&chunk));
        }
        feed.close();
        let batches = writer.join().expect("writer");
        assert_eq!(batches.concat(), expected, "every byte, in order");
        // A megabyte in kilobyte chunks: one write per chunk was the old
        // shape. Batches of 64 KiB need 16; allow slack for scheduling.
        assert!(
            batches.len() <= 64,
            "{} writes for 1024 chunks",
            batches.len()
        );
        assert!(
            feed.writer_wakeups.load(Ordering::SeqCst) <= 128,
            "writer woke {} times",
            feed.writer_wakeups.load(Ordering::SeqCst)
        );
    }

    #[test]
    fn a_lone_chunk_is_written_within_the_linger() {
        let feed = Arc::new(LogFeed::with_limits(
            64 << 10,
            Duration::from_millis(20),
            1 << 20,
        ));
        let writer = {
            let feed = Arc::clone(&feed);
            std::thread::spawn(move || {
                let mut batch = Vec::new();
                assert!(feed.take(&mut batch));
                batch
            })
        };
        std::thread::sleep(Duration::from_millis(30));
        let pushed = Instant::now();
        assert!(feed.push(b"prompt$ "));
        assert_eq!(writer.join().expect("writer"), b"prompt$ ");
        assert!(
            pushed.elapsed() < Duration::from_secs(1),
            "a small write must not wait for a batch that never fills"
        );
    }

    #[test]
    fn an_idle_writer_stays_parked() {
        let feed = Arc::new(LogFeed::new());
        let writer = {
            let feed = Arc::clone(&feed);
            std::thread::spawn(move || drain(&feed))
        };
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(feed.writer_wakeups.load(Ordering::SeqCst), 0);
        feed.close();
        assert!(writer.join().expect("writer").is_empty());
    }

    #[test]
    fn a_full_feed_holds_the_pump_until_the_writer_drains() {
        let feed = Arc::new(LogFeed::with_limits(4, Duration::from_millis(1), 8));
        assert!(feed.push(&[1; 8]));
        let pusher = {
            let feed = Arc::clone(&feed);
            std::thread::spawn(move || feed.push(&[2; 4]))
        };
        std::thread::sleep(Duration::from_millis(50));
        assert!(!pusher.is_finished(), "the pump waits for room");
        let mut batch = Vec::new();
        assert!(feed.take(&mut batch));
        assert_eq!(batch, [1; 8]);
        assert!(pusher.join().expect("pusher"));
        feed.close();
        batch.clear();
        assert!(
            feed.take(&mut batch),
            "closing still drains what is pending"
        );
        assert_eq!(batch, [2; 4]);
        batch.clear();
        assert!(!feed.take(&mut batch));
        assert!(!feed.push(b"late"), "a closed feed refuses bytes");
    }

    #[test]
    fn a_writer_that_dies_releases_the_pump_instead_of_stalling_it() {
        let feed = Arc::new(LogFeed::with_limits(4, Duration::ZERO, 8));
        assert!(feed.push(b"first"));
        let writer = {
            let feed = Arc::clone(&feed);
            std::thread::spawn(move || {
                let _exit = WriterExit(&feed);
                let mut batch = Vec::new();
                assert!(feed.take(&mut batch));
                panic!("the disk write failed hard");
            })
        };
        assert!(writer.join().is_err());
        let (done, finished) = std::sync::mpsc::channel();
        {
            let feed = Arc::clone(&feed);
            std::thread::spawn(move || {
                // Far past the bound: with nobody draining, a pump that kept
                // waiting for room would never return.
                let accepted = (0..64).all(|_| feed.push(b"output"));
                let _ = done.send(accepted);
            });
        }
        assert_eq!(
            finished.recv_timeout(Duration::from_secs(5)),
            Ok(false),
            "the pump must learn the writer is gone and append inline"
        );
    }

    /// A small xorshift, so a failing round can be replayed from its seed.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        /// Sometimes nothing, sometimes a yield, sometimes a short sleep:
        /// enough to push either thread across every wait in `LogFeed`.
        fn pause(&mut self) {
            match self.next() % 16 {
                0 => std::thread::sleep(Duration::from_micros(self.next() % 300)),
                1..=3 => std::thread::yield_now(),
                _ => {}
            }
        }
    }

    /// One pump and one writer at thresholds small enough that every round
    /// crosses the park, the linger, the batch wake and the backpressure wait
    /// many times, with random delays on both sides. Any lost wakeup shows as
    /// a round that never finishes; any reordering or loss as a byte mismatch.
    fn stress(first: u64, rounds: u64) {
        for round in first..first + rounds {
            let seed = 0x9E37_79B9_7F4A_7C15 ^ round.wrapping_mul(0x2545_F491_4F6C_DD1D);
            let mut rng = Rng(seed | 1);
            let batch = (rng.next() % 64 + 1) as usize;
            let limit = batch + (rng.next() % 256) as usize;
            let linger = Duration::from_micros(rng.next() % 400);
            let feed = Arc::new(LogFeed::with_limits(batch, linger, limit));
            let writer = {
                let feed = Arc::clone(&feed);
                let mut rng = Rng(rng.next() | 1);
                std::thread::spawn(move || {
                    let mut written = Vec::new();
                    let mut batch = Vec::new();
                    while feed.take(&mut batch) {
                        // The disk write, of whatever length.
                        rng.pause();
                        written.extend_from_slice(&batch);
                        batch.clear();
                    }
                    written
                })
            };
            let mut expected = Vec::new();
            for index in 0..(rng.next() % 600 + 50) {
                let length = (rng.next() % 3 + 1) as usize * (rng.next() % 40 + 1) as usize;
                let chunk: Vec<u8> = (0..length).map(|at| (index as usize ^ at) as u8).collect();
                expected.extend_from_slice(&chunk);
                assert!(feed.push(&chunk));
                rng.pause();
            }
            feed.close();
            let (done, finished) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let _ = done.send(writer.join());
            });
            let written = finished
                .recv_timeout(Duration::from_secs(10))
                .unwrap_or_else(|_| panic!("round {round} (seed {seed:#x}) hung"))
                .expect("writer");
            assert!(
                written == expected,
                "round {round} (seed {seed:#x}) lost or reordered bytes"
            );
        }
    }

    #[test]
    fn the_pump_and_writer_never_lose_a_wakeup_or_a_byte() {
        stress(0, 100);
    }

    /// The same, for as many rounds as `UBRA_STRESS_ROUNDS` asks (default
    /// 20,000) from round `UBRA_STRESS_FIRST`:
    /// `cargo test --release -p ubra-engine --lib log_feed -- --ignored`.
    #[test]
    #[ignore = "long; run on demand"]
    fn the_pump_and_writer_never_lose_a_wakeup_or_a_byte_at_length() {
        let setting = |name: &str, default: u64| {
            std::env::var(name)
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(default)
        };
        stress(
            setting("UBRA_STRESS_FIRST", 0),
            setting("UBRA_STRESS_ROUNDS", 20_000),
        );
    }
}
