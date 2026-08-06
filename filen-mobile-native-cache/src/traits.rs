#[uniffi::export(with_foreign)]
pub trait ProgressCallback: Send + Sync {
	fn set_total(&self, size: u64);
	fn on_progress(&self, bytes_processed: u64);
}

/// Fired when a live search's results change after the initial return — i.e. as the on-demand
/// resync converges. The provider re-queries (e.g. `notifyChange`) to surface the fuller set.
#[uniffi::export(with_foreign)]
pub trait SearchUpdateCallback: Send + Sync {
	fn on_update(&self);
}

/// Fired when working-set tracking has put a tracked file's fresh state into the cache (see
/// [`crate::working_set`]). The replica answers by asking for a diff — on iOS,
/// `signalEnumerator(for: .workingSet)`.
///
/// Bursts are not coalesced: one call per applied batch, which is one per socket drain.
#[uniffi::export(with_foreign)]
pub trait WorkingSetUpdateListener: Send + Sync {
	fn working_set_changed(&self);
}

impl<T> ProgressCallback for T
where
	T: Fn(u64) + Send + Sync,
{
	fn on_progress(&self, bytes_processed: u64) {
		self(bytes_processed);
	}

	fn set_total(&self, _size: u64) {}
}
