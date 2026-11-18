//! `ChangeDebouncer`: groups close events into one batch.
//!
//! Every accepted path goes into a de-duplicated pending set. Each new event
//! re-arms the stability window; the batch is emitted only once the window
//! elapsed without any new event. A hard cap (`max_wait`) guarantees that a
//! continuous stream of writes cannot postpone the rebuild forever.
//!
//! The debouncer keeps collecting while a build is running: nothing is lost,
//! and the coordinator decides what to do with the batch. Time is passed in
//! explicitly so the logic is deterministic and unit-testable.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// A stabilized group of candidate changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DebouncedBatch {
    /// De-duplicated candidate paths.
    pub paths: BTreeSet<PathBuf>,
    /// The OS reported dropped events: the whole scope must be re-checked.
    pub rescan: bool,
    /// Number of accepted path events folded into this batch.
    pub events: usize,
    /// How long the set had been quiet when it was emitted.
    pub stable_for: Duration,
}

#[derive(Debug)]
pub struct ChangeDebouncer {
    window: Duration,
    max_wait: Duration,
    paths: BTreeSet<PathBuf>,
    rescan: bool,
    events: usize,
    first_at: Option<Instant>,
    last_at: Option<Instant>,
}

impl ChangeDebouncer {
    pub fn new(window: Duration) -> Self {
        Self {
            window,
            // Never hold a batch for more than ~10 windows (min 2 s).
            max_wait: (window * 10).max(Duration::from_secs(2)),
            paths: BTreeSet::new(),
            rescan: false,
            events: 0,
            first_at: None,
            last_at: None,
        }
    }

    pub fn window(&self) -> Duration {
        self.window
    }

    /// Record an accepted candidate path.
    pub fn push(&mut self, path: PathBuf, now: Instant) {
        self.paths.insert(path);
        self.touch(now);
    }

    /// Record that the OS dropped events.
    pub fn push_rescan(&mut self, now: Instant) {
        self.rescan = true;
        self.touch(now);
    }

    fn touch(&mut self, now: Instant) {
        self.events += 1;
        self.first_at.get_or_insert(now);
        self.last_at = Some(now);
    }

    pub fn is_pending(&self) -> bool {
        self.last_at.is_some()
    }

    /// When the pending batch becomes due, if any.
    pub fn deadline(&self) -> Option<Instant> {
        let last = self.last_at?;
        let first = self.first_at?;
        Some((last + self.window).min(first + self.max_wait))
    }

    /// Emit the batch if its deadline has passed.
    pub fn poll(&mut self, now: Instant) -> Option<DebouncedBatch> {
        if now < self.deadline()? {
            return None;
        }
        let stable_for = now.saturating_duration_since(self.last_at?);
        let batch = DebouncedBatch {
            paths: std::mem::take(&mut self.paths),
            rescan: std::mem::take(&mut self.rescan),
            events: std::mem::take(&mut self.events),
            stable_for,
        };
        self.first_at = None;
        self.last_at = None;
        Some(batch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: Duration = Duration::from_millis(400);

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn nothing_pending_means_nothing_emitted() {
        let mut d = ChangeDebouncer::new(W);
        assert!(!d.is_pending());
        assert_eq!(d.deadline(), None);
        assert_eq!(d.poll(Instant::now()), None);
    }

    #[test]
    fn duplicate_events_produce_one_batch_with_one_path() {
        let t0 = Instant::now();
        let mut d = ChangeDebouncer::new(W);
        for i in 0..5 {
            d.push("src/main.rs".into(), t0 + ms(i * 10));
        }
        assert_eq!(d.poll(t0 + ms(100)), None);
        let batch = d.poll(t0 + ms(40) + W).unwrap();
        assert_eq!(batch.paths.len(), 1);
        assert_eq!(batch.events, 5);
        assert!(!d.is_pending());
        assert_eq!(d.poll(t0 + ms(5000)), None);
    }

    #[test]
    fn each_event_rearms_the_window() {
        let t0 = Instant::now();
        let mut d = ChangeDebouncer::new(W);
        d.push("src/a.rs".into(), t0);
        d.push("src/b.rs".into(), t0 + ms(300));
        // 400 ms after the first event, but only 100 ms after the second.
        assert_eq!(d.poll(t0 + ms(400)), None);
        let batch = d.poll(t0 + ms(700)).unwrap();
        assert_eq!(batch.paths.len(), 2);
        assert_eq!(batch.stable_for, ms(400));
    }

    #[test]
    fn continuous_stream_is_capped() {
        let t0 = Instant::now();
        let mut d = ChangeDebouncer::new(W);
        let mut now = t0;
        let mut emitted = None;
        for _ in 0..200 {
            d.push("src/a.rs".into(), now);
            now += ms(100);
            if let Some(b) = d.poll(now) {
                emitted = Some(now - t0);
                assert_eq!(b.paths.len(), 1);
                break;
            }
        }
        let after = emitted.expect("a batch must eventually be emitted");
        assert!(after <= ms(4100), "emitted after {after:?}");
    }

    #[test]
    fn events_spaced_beyond_the_window_make_separate_batches() {
        let t0 = Instant::now();
        let mut d = ChangeDebouncer::new(W);
        d.push("src/a.rs".into(), t0);
        assert!(d.poll(t0 + W).is_some());
        d.push("src/a.rs".into(), t0 + ms(1000));
        assert!(d.poll(t0 + ms(1000) + W).is_some());
    }

    #[test]
    fn rescan_flag_travels_with_the_batch() {
        let t0 = Instant::now();
        let mut d = ChangeDebouncer::new(W);
        d.push_rescan(t0);
        let batch = d.poll(t0 + W).unwrap();
        assert!(batch.rescan);
        assert!(batch.paths.is_empty());
    }
}
