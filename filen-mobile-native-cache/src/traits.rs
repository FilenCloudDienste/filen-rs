#[uniffi::export(with_foreign)]
pub trait ProgressCallback: Send + Sync {
	fn init(&self, size: u64);
	fn on_progress(&self, bytes_processed: u64);
}

impl<T> ProgressCallback for T
where
	T: Fn(u64) + Send + Sync,
{
	fn on_progress(&self, bytes_processed: u64) {
		self(bytes_processed);
	}

	fn init(&self, _size: u64) {}
}
