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

/// Fired on the SDK websocket thread when a remote drive socket event arrives (see
/// [`crate::socket`]). Keep the implementation FAST — just hand off to the File Provider's
/// `signalEnumerator`; never do blocking work here (it stalls all further socket events for this
/// connection). `changed_parent_uuids` are container uuids whose child listing may have changed;
/// `affects_trash` marks trash / restore / permanent-delete events. Receivers should also refresh
/// the working set unconditionally (it covers materialized / favorited / recent items).
#[uniffi::export(with_foreign)]
pub trait SocketNotificationCallback: Send + Sync {
	fn on_drive_change(&self, changed_parent_uuids: Vec<String>, affects_trash: bool);
	/// Fired on every socket (re)connect (AuthSuccess). The receiver should force a catch-up
	/// re-list (signal root + working set) for changes that landed while it was disconnected —
	/// the local sync anchor is a local-mutation counter and won't detect a missed remote change.
	fn on_reconnect(&self);
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
