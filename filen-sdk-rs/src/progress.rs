//! Rate-limiting for single-file byte-progress callbacks.
//!
//! Single-file uploads ([`crate::fs::file::write`]) and downloads
//! ([`crate::io::client_impl`]) invoke their progress callback once per chunk/read with no
//! throttling, so a fast transfer fires the callback — and, across the UniFFI/WASM boundary, a
//! JS-thread store update — many times per second (roughly the transfer's MiB/s). Directory
//! transfers avoid this because they already aggregate behind a `tokio::time::interval`; the
//! single-file paths have no such point. [`ThrottledProgress`] is that point: it wraps the
//! callback and collapses the calls to at most one per [`CALLBACK_INTERVAL`], accumulating the
//! skipped byte *deltas* so the total reported stays exact.

use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
use std::time::Instant;
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
use wasmtimer::std::Instant;

use crate::{
	consts::CALLBACK_INTERVAL,
	util::{MaybeArc, MaybeSendCallback},
};

/// Wraps a per-event byte-*delta* progress callback and rate-limits its invocations.
///
/// [`report`](Self::report) is cheap and meant to be called for every transferred chunk: it
/// accumulates the bytes and only forwards them to the wrapped callback once at least
/// [`CALLBACK_INTERVAL`] has elapsed since the previously forwarded call. [`flush`](Self::flush)
/// delivers whatever is still pending and must be called once the transfer finishes, otherwise the
/// final (sub-interval) bytes are dropped.
///
/// Accumulation is exact — every byte passed to `report` is eventually forwarded — so throttling
/// never under-counts (the consumer adds the deltas). State lives in atomics so a single instance
/// can be shared, via [`MaybeArc`], across the concurrently-completing chunk futures of an upload.
pub(crate) struct ThrottledProgress<'a> {
	callback: MaybeSendCallback<'a, u64>,
	start: Instant,
	interval_millis: u64,
	last_emit_millis: AtomicU64,
	pending: AtomicU64,
}

impl<'a> ThrottledProgress<'a> {
	/// Wrap `callback` with the default [`CALLBACK_INTERVAL`], returning `None` when there is no
	/// callback so call sites keep their "no callback" fast path.
	pub(crate) fn new(callback: Option<MaybeSendCallback<'a, u64>>) -> Option<MaybeArc<Self>> {
		callback.map(|callback| Self::with_interval(callback, CALLBACK_INTERVAL.as_millis() as u64))
	}

	fn with_interval(callback: MaybeSendCallback<'a, u64>, interval_millis: u64) -> MaybeArc<Self> {
		MaybeArc::new(Self {
			callback,
			start: Instant::now(),
			interval_millis,
			last_emit_millis: AtomicU64::new(0),
			pending: AtomicU64::new(0),
		})
	}

	/// Record `delta` freshly-transferred bytes, forwarding the accumulated total to the wrapped
	/// callback only when the throttle interval has elapsed since the last forwarded call.
	pub(crate) fn report(&self, delta: u64) {
		if delta == 0 {
			return;
		}
		self.pending.fetch_add(delta, Ordering::Relaxed);
		let now = self.start.elapsed().as_millis() as u64;
		let last = self.last_emit_millis.load(Ordering::Relaxed);
		// Claim the time slot with a CAS so that, even if several chunk futures clear the interval
		// at the same instant, only one of them forwards; the rest leave their bytes pending and
		// they are swept up by the next forwarded call (or by `flush`).
		if now.saturating_sub(last) >= self.interval_millis
			&& self
				.last_emit_millis
				.compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
				.is_ok()
		{
			self.emit();
		}
	}

	/// Forward any bytes accumulated since the last emission. Call once the transfer completes.
	pub(crate) fn flush(&self) {
		self.emit();
	}

	fn emit(&self) {
		let pending = self.pending.swap(0, Ordering::Relaxed);
		if pending > 0 {
			(self.callback)(pending);
		}
	}
}

#[cfg(test)]
mod tests {
	use std::sync::{
		Arc,
		atomic::{AtomicU64, Ordering},
	};

	use super::ThrottledProgress;
	use crate::util::MaybeSendCallback;

	struct Sink {
		total: AtomicU64,
		calls: AtomicU64,
	}

	fn sink() -> (Arc<Sink>, MaybeSendCallback<'static, u64>) {
		let sink = Arc::new(Sink {
			total: AtomicU64::new(0),
			calls: AtomicU64::new(0),
		});
		let cb_sink = sink.clone();
		let callback: MaybeSendCallback<'static, u64> = Arc::new(move |n: u64| {
			cb_sink.total.fetch_add(n, Ordering::Relaxed);
			cb_sink.calls.fetch_add(1, Ordering::Relaxed);
		});
		(sink, callback)
	}

	/// A practically-infinite interval never forwards inline, so everything must surface via
	/// `flush` and the forwarded total must equal the sum of every reported delta.
	#[test]
	fn accumulates_until_flush_and_total_is_exact() {
		let (sink, callback) = sink();
		let progress = ThrottledProgress::with_interval(callback, u64::MAX);
		for _ in 0..100 {
			progress.report(7);
		}
		assert_eq!(
			sink.calls.load(Ordering::Relaxed),
			0,
			"throttle must not forward before the interval elapses"
		);
		progress.flush();
		assert_eq!(
			sink.total.load(Ordering::Relaxed),
			700,
			"no bytes may be dropped"
		);
		assert_eq!(
			sink.calls.load(Ordering::Relaxed),
			1,
			"flush forwards exactly once"
		);
	}

	/// A zero interval forwards every non-zero report immediately (the no-throttle baseline).
	#[test]
	fn zero_interval_forwards_every_report() {
		let (sink, callback) = sink();
		let progress = ThrottledProgress::with_interval(callback, 0);
		progress.report(3);
		progress.report(5);
		assert_eq!(sink.total.load(Ordering::Relaxed), 8);
		assert_eq!(sink.calls.load(Ordering::Relaxed), 2);
	}

	/// Zero-byte reports and a flush with nothing pending forward nothing.
	#[test]
	fn zero_deltas_and_empty_flush_are_noops() {
		let (sink, callback) = sink();
		let progress = ThrottledProgress::with_interval(callback, u64::MAX);
		progress.report(0);
		progress.flush();
		assert_eq!(sink.calls.load(Ordering::Relaxed), 0);
	}
}
