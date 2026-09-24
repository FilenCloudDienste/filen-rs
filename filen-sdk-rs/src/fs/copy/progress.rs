//! Progress plumbing for long-running jobs: batching events into throttled updates and
//! estimating throughput and time left.
//!
//! Everything here takes the current time as an argument (time since the job started), so it
//! is deterministic under test; the job reads its clock once per call.

use std::{collections::VecDeque, time::Duration};

use crate::consts::CALLBACK_INTERVAL;

/// Queued events beyond which a batch is delivered without waiting for the interval, so a burst
/// of tiny items cannot build an unbounded payload.
pub(crate) const MAX_EVENTS_PER_UPDATE: usize = 1000;

/// Collects events between updates. An update is due once [`CALLBACK_INTERVAL`] has passed
/// since the previous one, immediately for urgent changes (phase transitions, pause/cancel,
/// created top-level items), or when [`MAX_EVENTS_PER_UPDATE`] events are queued.
#[derive(Debug)]
pub(crate) struct EventBatcher<E> {
	events: Vec<E>,
	interval: Duration,
	max_events: usize,
	last_update: Option<Duration>,
	urgent: bool,
}

impl<E> Default for EventBatcher<E> {
	fn default() -> Self {
		Self::new(CALLBACK_INTERVAL, MAX_EVENTS_PER_UPDATE)
	}
}

impl<E> EventBatcher<E> {
	pub(crate) fn new(interval: Duration, max_events: usize) -> Self {
		Self {
			events: Vec::new(),
			interval,
			max_events: max_events.max(1),
			last_update: None,
			urgent: false,
		}
	}

	pub(crate) fn push(&mut self, event: E) {
		self.events.push(event);
	}

	/// Makes the next [`is_due`](Self::is_due) true regardless of the interval.
	pub(crate) fn mark_urgent(&mut self) {
		self.urgent = true;
	}

	/// `changed` says whether anything (counters included) moved since the last update; an
	/// update carrying no change is never due on the interval alone.
	pub(crate) fn is_due(&self, now: Duration, changed: bool) -> bool {
		if self.urgent || self.events.len() >= self.max_events {
			return true;
		}
		if !changed && self.events.is_empty() {
			return false;
		}
		self.last_update
			.is_none_or(|last| now.saturating_sub(last) >= self.interval)
	}

	/// Starts a new batch, returning the events of the one just delivered.
	pub(crate) fn take(&mut self, now: Duration) -> Vec<E> {
		self.last_update = Some(now);
		self.urgent = false;
		std::mem::take(&mut self.events)
	}
}

/// Accumulates the time a job spends running, leaving out the time it is paused.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct ActiveClock {
	accumulated: Duration,
	running_since: Option<Duration>,
}

impl ActiveClock {
	pub(crate) fn resume(&mut self, now: Duration) {
		if self.running_since.is_none() {
			self.running_since = Some(now);
		}
	}

	pub(crate) fn pause(&mut self, now: Duration) {
		if let Some(since) = self.running_since.take() {
			self.accumulated += now.saturating_sub(since);
		}
	}

	pub(crate) fn active(&self, now: Duration) -> Duration {
		self.accumulated
			+ self
				.running_since
				.map_or(Duration::ZERO, |since| now.saturating_sub(since))
	}
}

/// Per-file weight in "work units" on top of the file's bytes: small files are dominated by
/// their per-file requests, so counting bytes alone makes the estimate far too optimistic for
/// trees of many small files.
pub(crate) const PER_FILE_WORK_UNITS: u64 = 512 * 1024;

/// Work units for `files` files totalling `bytes` bytes.
pub(crate) fn work_units(files: u64, bytes: u64) -> u64 {
	bytes.saturating_add(files.saturating_mul(PER_FILE_WORK_UNITS))
}

/// The active time the rate is measured over.
const RATE_WINDOW: Duration = Duration::from_secs(10);

/// Progress at a point in active time (see [`ActiveClock`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Sample {
	active: Duration,
	bytes_done: u64,
	units_done: u64,
}

/// Throughput over a sliding window of active time, and the time left at that rate.
#[derive(Debug)]
pub(crate) struct RateEstimator {
	window: Duration,
	/// Oldest first.
	samples: VecDeque<Sample>,
}

impl Default for RateEstimator {
	fn default() -> Self {
		Self::new(RATE_WINDOW)
	}
}

impl RateEstimator {
	pub(crate) fn new(window: Duration) -> Self {
		Self {
			window,
			samples: VecDeque::new(),
		}
	}

	/// Records progress at `active` (see [`ActiveClock`]). Samples must not go back in time.
	pub(crate) fn record(&mut self, active: Duration, bytes_done: u64, units_done: u64) {
		let sample = Sample {
			active,
			bytes_done,
			units_done,
		};
		if self.samples.back() == Some(&sample) {
			return;
		}
		self.samples.push_back(sample);
		// keep one sample at or before the window start so the window stays fully covered
		while self.samples.len() > 2
			&& self
				.samples
				.get(1)
				.is_some_and(|s| active.saturating_sub(s.active) >= self.window)
		{
			self.samples.pop_front();
		}
	}

	/// The progress made across the window: its newest sample less its oldest. `None` while the
	/// samples span no active time.
	fn span(&self) -> Option<Sample> {
		let first = self.samples.front()?;
		let last = self.samples.back()?;
		let active = last
			.active
			.checked_sub(first.active)
			.filter(|e| !e.is_zero())?;
		Some(Sample {
			active,
			bytes_done: last.bytes_done.saturating_sub(first.bytes_done),
			units_done: last.units_done.saturating_sub(first.units_done),
		})
	}

	/// Bytes per second over the window, or `None` until two samples span some active time.
	pub(crate) fn bytes_per_second(&self) -> Option<u64> {
		let span = self.span()?;
		Some((span.bytes_done as f64 / span.active.as_secs_f64()) as u64)
	}

	/// Time left to process `remaining_units` at the windowed rate, or `None` while there is no
	/// rate to extrapolate from.
	pub(crate) fn eta(&self, remaining_units: u64) -> Option<Duration> {
		if remaining_units == 0 {
			return Some(Duration::ZERO);
		}
		let span = self.span()?;
		if span.units_done == 0 {
			return None;
		}
		let seconds = remaining_units as f64 * span.active.as_secs_f64() / span.units_done as f64;
		Duration::try_from_secs_f64(seconds).ok()
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	const MS: Duration = Duration::from_millis(1);

	#[test]
	fn first_update_is_due_as_soon_as_something_changed() {
		let batcher = EventBatcher::<u32>::default();
		assert!(!batcher.is_due(Duration::ZERO, false));
		assert!(batcher.is_due(Duration::ZERO, true));
	}

	#[test]
	fn updates_are_throttled_to_the_interval() {
		let mut batcher = EventBatcher::new(200 * MS, 1000);
		batcher.push(1);
		assert_eq!(batcher.take(Duration::ZERO), vec![1]);
		batcher.push(2);
		batcher.push(3);
		assert!(!batcher.is_due(199 * MS, true));
		assert!(batcher.is_due(200 * MS, true));
		assert_eq!(
			batcher.take(200 * MS),
			vec![2, 3],
			"events keep their order"
		);
		assert!(
			!batcher.is_due(1000 * MS, false),
			"nothing changed, nothing to send"
		);
	}

	#[test]
	fn urgent_changes_and_full_batches_skip_the_interval() {
		let mut batcher = EventBatcher::new(200 * MS, 3);
		batcher.take(Duration::ZERO);
		batcher.mark_urgent();
		assert!(batcher.is_due(MS, false));
		batcher.take(MS);
		assert!(
			!batcher.is_due(2 * MS, false),
			"urgency is reset by an update"
		);
		for event in 0..3 {
			batcher.push(event);
		}
		assert!(batcher.is_due(3 * MS, false));
	}

	#[test]
	fn a_burst_of_ten_thousand_events_becomes_few_updates() {
		let mut batcher = EventBatcher::default();
		let mut updates = Vec::new();
		batcher.take(Duration::ZERO);
		for event in 0..10_000u32 {
			batcher.push(event);
			// all within one interval
			let now = Duration::from_micros(u64::from(event) * 10);
			if batcher.is_due(now, true) {
				updates.push(batcher.take(now));
			}
		}
		assert_eq!(updates.len(), 10, "one update per full batch");
		let delivered = updates.concat();
		assert_eq!(delivered, (0..10_000).collect::<Vec<_>>());
	}

	#[test]
	fn active_clock_leaves_out_paused_time() {
		let mut clock = ActiveClock::default();
		clock.resume(Duration::from_secs(0));
		clock.pause(Duration::from_secs(10));
		assert_eq!(
			clock.active(Duration::from_secs(100)),
			Duration::from_secs(10)
		);
		clock.resume(Duration::from_secs(100));
		clock.resume(Duration::from_secs(101)); // resuming twice changes nothing
		assert_eq!(
			clock.active(Duration::from_secs(105)),
			Duration::from_secs(15)
		);
		clock.pause(Duration::from_secs(110));
		clock.pause(Duration::from_secs(120));
		assert_eq!(
			clock.active(Duration::from_secs(200)),
			Duration::from_secs(20)
		);
	}

	#[test]
	fn rate_and_eta_follow_the_window() {
		let mut rate = RateEstimator::new(Duration::from_secs(10));
		assert_eq!(rate.bytes_per_second(), None);
		assert_eq!(rate.eta(100), None);
		rate.record(Duration::ZERO, 0, 0);
		rate.record(Duration::from_secs(2), 2_000, 4_000);
		assert_eq!(rate.bytes_per_second(), Some(1_000));
		assert_eq!(rate.eta(6_000), Some(Duration::from_secs(3)));
		assert_eq!(rate.eta(0), Some(Duration::ZERO));

		// old samples fall out of the window: a slower recent rate takes over
		rate.record(Duration::from_secs(20), 12_000, 14_000);
		rate.record(Duration::from_secs(30), 13_000, 15_000);
		assert_eq!(rate.bytes_per_second(), Some(100));
	}

	#[test]
	fn no_progress_means_no_eta() {
		let mut rate = RateEstimator::default();
		rate.record(Duration::ZERO, 0, 0);
		rate.record(Duration::from_secs(5), 0, 0);
		assert_eq!(rate.eta(10), None);
		assert_eq!(rate.bytes_per_second(), Some(0));
	}

	#[test]
	fn work_units_weigh_files_and_bytes() {
		assert_eq!(work_units(0, 10), 10);
		assert_eq!(work_units(2, 10), 10 + 2 * PER_FILE_WORK_UNITS);
		assert_eq!(work_units(u64::MAX, u64::MAX), u64::MAX);
	}
}
