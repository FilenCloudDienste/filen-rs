//! How far a whole walk has got: counters its threads bump as they go, and the throttled
//! [`SyncEvent::Scanning`] a pass reports them with while it waits on the walk.
//!
//! A pair's first pass reads every file it finds before it plans anything, which on a large tree
//! takes hours with nothing else to show for it. The counters are atomics bumped once per entry and
//! once per chunk a hash read takes from a file, so the walk's threads never wait on each other or
//! on the reporter; the reporter only reads them, on a timer.

use std::{
	io::Read,
	sync::atomic::{AtomicU64, Ordering},
	time::Duration,
};

use tokio::time::{Instant, MissedTickBehavior};

use crate::sync_engine::events::{LocalScanProgress, SyncEvent};

/// How often, at most, a pass reports how far its walk has got: one [`SyncEvent::Scanning`] per
/// interval per pair, and only when something moved since the last one.
pub(crate) const SCAN_PROGRESS_INTERVAL: Duration = Duration::from_millis(500);

/// The counters one walk bumps, shared between its own thread, its read workers and whoever reports
/// them (see [`report_scan_progress`]).
///
/// Every counter only ever grows. The "to hash" pair is bumped before a file's read starts and the
/// "hashed" pair as it goes and when it ends, each with `Release`, and a [`snapshot`](Self::snapshot)
/// loads the "hashed" pair first, with `Acquire`: so no snapshot shows more hashed than to hash.
#[derive(Debug, Default)]
pub(crate) struct WalkProgress {
	entries: AtomicU64,
	files_to_hash: AtomicU64,
	bytes_to_hash: AtomicU64,
	files_hashed: AtomicU64,
	bytes_hashed: AtomicU64,
}

impl WalkProgress {
	/// One more entry listed and admitted by the walk.
	pub(super) fn listed(&self) {
		self.entries.fetch_add(1, Ordering::Relaxed);
	}

	/// The counters as they stand.
	pub(crate) fn snapshot(&self) -> LocalScanProgress {
		// The "hashed" pair first: see the type's own doc.
		let files_hashed = self.files_hashed.load(Ordering::Acquire);
		let bytes_hashed = self.bytes_hashed.load(Ordering::Acquire);
		LocalScanProgress {
			entries: self.entries.load(Ordering::Relaxed),
			files_to_hash: self.files_to_hash.load(Ordering::Acquire),
			bytes_to_hash: self.bytes_to_hash.load(Ordering::Acquire),
			files_hashed,
			bytes_hashed,
		}
	}

	/// Start counting the hash read of a file the stat found `size` bytes long. The read is counted
	/// as ended when the returned guard drops, however it ended.
	pub(super) fn hash_read(&self, size: u64) -> HashRead<'_> {
		self.files_to_hash.fetch_add(1, Ordering::Release);
		add_saturating(&self.bytes_to_hash, size);
		HashRead {
			progress: self,
			size,
			counted: 0,
		}
	}
}

/// `counter += n`, stopping at `u64::MAX`: a file size is the filesystem's word, and a sparse file
/// can claim nearly all of `u64` on its own.
fn add_saturating(counter: &AtomicU64, n: u64) {
	// `fetch_update` only fails when the closure answers `None`, which this one never does.
	let _ = counter.fetch_update(Ordering::Release, Ordering::Relaxed, |now| {
		Some(now.saturating_add(n))
	});
}

/// One file's hash read, counted (see [`WalkProgress::hash_read`]). The bytes it reads count
/// towards [`LocalScanProgress::bytes_hashed`] up to the size the stat found, so a file that grows while
/// it is read never counts past it; when it drops, whatever of that size it did not count is
/// counted, so a read that failed or met a shorter file still adds up.
pub(super) struct HashRead<'p> {
	progress: &'p WalkProgress,
	size: u64,
	counted: u64,
}

impl HashRead<'_> {
	fn count(&mut self, read: usize) {
		let read = u64::try_from(read)
			.unwrap_or(u64::MAX)
			.min(self.size - self.counted);
		if read > 0 {
			self.counted += read;
			add_saturating(&self.progress.bytes_hashed, read);
		}
	}

	/// `inner`, counting what is read through it as this file's.
	pub(super) fn reader<R: Read>(&mut self, inner: R) -> impl Read {
		CountingReader { inner, read: self }
	}
}

impl Drop for HashRead<'_> {
	fn drop(&mut self) {
		add_saturating(&self.progress.bytes_hashed, self.size - self.counted);
		self.progress.files_hashed.fetch_add(1, Ordering::Release);
	}
}

/// [`HashRead::reader`]'s reader: `inner`'s bytes, untouched, and their count.
struct CountingReader<'r, 'p, R> {
	inner: R,
	read: &'r mut HashRead<'p>,
}

impl<R: Read> Read for CountingReader<'_, '_, R> {
	fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
		let read = self.inner.read(buf)?;
		self.read.count(read);
		Ok(read)
	}
}

/// Drive `work` to its end, reporting `progress` to `observer` while it runs: a
/// [`SyncEvent::Scanning`] each [`SCAN_PROGRESS_INTERVAL`] in which the counters moved, and, if
/// any was reported, one with the counters as they stand when `work` ends, unless the last one
/// already said exactly that. A walk that ends within the first interval, and counters that never
/// leave zero, report nothing.
///
/// `observer` is called from this task only, between polls of `work`, as every other event of a
/// pass is.
pub(crate) async fn report_scan_progress<F: Future>(
	work: F,
	progress: &WalkProgress,
	observer: &mut (dyn FnMut(SyncEvent) + Send),
) -> F::Output {
	let mut reported: Option<LocalScanProgress> = None;
	let mut report = |reported: &mut Option<LocalScanProgress>| {
		let now = progress.snapshot();
		if now != LocalScanProgress::default() && reported.as_ref() != Some(&now) {
			observer(SyncEvent::Scanning { progress: now });
			*reported = Some(now);
		}
	};
	let mut ticks = tokio::time::interval_at(
		Instant::now() + SCAN_PROGRESS_INTERVAL,
		SCAN_PROGRESS_INTERVAL,
	);
	// A tick held up behind a slow observer is not made up for with a burst.
	ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
	let mut work = std::pin::pin!(work);
	let out = loop {
		tokio::select! {
			// The work first: a tick landing with its end is the final report's to make.
			biased;
			out = &mut work => break out,
			_ = ticks.tick() => report(&mut reported),
		}
	};
	if reported.is_some() {
		report(&mut reported);
	}
	out
}

#[cfg(test)]
mod tests {
	use super::*;

	/// What a walk of `files` files of `size` bytes each looks like to the counters, one file per
	/// `step`.
	async fn walk(progress: &WalkProgress, files: u64, size: u64, step: Duration) {
		for _ in 0..files {
			tokio::time::sleep(step).await;
			progress.listed();
			let mut read = progress.hash_read(size);
			std::io::copy(
				&mut read.reader(std::io::repeat(7).take(size)),
				&mut std::io::sink(),
			)
			.expect("reading from memory");
		}
	}

	fn scanning(entries: u64, size: u64) -> SyncEvent {
		SyncEvent::Scanning {
			progress: LocalScanProgress {
				entries,
				files_to_hash: entries,
				bytes_to_hash: entries * size,
				files_hashed: entries,
				bytes_hashed: entries * size,
			},
		}
	}

	#[tokio::test(start_paused = true)]
	async fn a_long_walk_reports_once_per_interval_and_once_at_its_end() {
		let progress = WalkProgress::default();
		let mut events = Vec::new();
		// Twelve files, one every 100 ms: the walk ends at 1.2 s, between the ticks at 1 s and 1.5 s.
		report_scan_progress(
			walk(&progress, 12, 10, Duration::from_millis(100)),
			&progress,
			&mut |event| events.push(event),
		)
		.await;

		assert_eq!(
			events,
			vec![scanning(5, 10), scanning(10, 10), scanning(12, 10)]
		);
	}

	#[tokio::test(start_paused = true)]
	async fn a_walk_shorter_than_one_interval_reports_nothing() {
		let progress = WalkProgress::default();
		let mut events = Vec::new();
		report_scan_progress(
			walk(&progress, 4, 10, Duration::from_millis(100)),
			&progress,
			&mut |event| events.push(event),
		)
		.await;

		assert_eq!(events, Vec::new());
	}

	#[tokio::test(start_paused = true)]
	async fn counters_that_stopped_moving_are_not_reported_again() {
		let progress = WalkProgress::default();
		let mut events = Vec::new();
		// The walk is done at 600 ms; the work goes on to 3 s without it, as a pass reading the
		// remote after its walk ended does.
		report_scan_progress(
			async {
				walk(&progress, 6, 1, Duration::from_millis(100)).await;
				tokio::time::sleep(Duration::from_millis(2400)).await;
			},
			&progress,
			&mut |event| events.push(event),
		)
		.await;

		assert_eq!(events, vec![scanning(5, 1), scanning(6, 1)]);
	}

	#[tokio::test(start_paused = true)]
	async fn a_walk_with_nothing_to_count_reports_nothing() {
		let progress = WalkProgress::default();
		let mut events = Vec::new();
		report_scan_progress(
			tokio::time::sleep(Duration::from_secs(3)),
			&progress,
			&mut |event| events.push(event),
		)
		.await;

		assert_eq!(events, Vec::new());
	}

	#[test]
	fn a_file_that_grew_while_read_counts_only_the_size_its_stat_found() {
		let progress = WalkProgress::default();
		{
			let mut read = progress.hash_read(10);
			std::io::copy(
				&mut read.reader(std::io::repeat(1).take(25)),
				&mut std::io::sink(),
			)
			.expect("reading from memory");
			assert_eq!(progress.snapshot().bytes_hashed, 10);
			assert_eq!(progress.snapshot().files_hashed, 0);
		}
		assert_eq!(
			progress.snapshot(),
			LocalScanProgress {
				entries: 0,
				files_to_hash: 1,
				bytes_to_hash: 10,
				files_hashed: 1,
				bytes_hashed: 10,
			}
		);
	}

	#[test]
	fn a_read_that_ended_early_still_counts_its_file_whole() {
		let progress = WalkProgress::default();
		{
			let mut read = progress.hash_read(100);
			std::io::copy(
				&mut read.reader(std::io::repeat(1).take(30)),
				&mut std::io::sink(),
			)
			.expect("reading from memory");
			assert_eq!(progress.snapshot().bytes_hashed, 30);
		}
		assert_eq!(progress.snapshot().bytes_hashed, 100);
		assert_eq!(progress.snapshot().files_hashed, 1);
	}

	#[test]
	fn byte_counts_stop_at_u64_max() {
		let progress = WalkProgress::default();
		drop(progress.hash_read(u64::MAX));
		drop(progress.hash_read(u64::MAX));
		let snapshot = progress.snapshot();
		assert_eq!(snapshot.bytes_to_hash, u64::MAX);
		assert_eq!(snapshot.bytes_hashed, u64::MAX);
		assert_eq!(snapshot.files_hashed, 2);
	}
}
