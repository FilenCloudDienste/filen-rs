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

impl<T> ProgressCallback for T
where
	T: Fn(u64) + Send + Sync,
{
	fn on_progress(&self, bytes_processed: u64) {
		self(bytes_processed);
	}

	fn set_total(&self, _size: u64) {}
}
