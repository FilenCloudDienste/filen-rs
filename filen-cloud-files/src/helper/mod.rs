//! The helper the desktop app spawns: it serves one user's sync root at a time, driven over its
//! stdin and stdout by the protocol in [`protocol`].

mod events;
mod framing;
mod identify;
mod index;
mod launch;
mod logging;
mod platform;
mod protocol;
mod supervisor;
#[cfg(test)]
mod tests;
mod unregister;
mod validate;
mod watch_keeper;

use tokio::sync::watch;

pub(crate) use identify::{HOST, below_root};
pub use index::{StateDir, StateDirError};
pub use launch::{
	MIN_BUILD, StateLock, THUMBNAIL_SERVER_ARG, THUMBNAIL_SERVER_IDLE, fatal,
	serve_refusing_thumbnails, serve_stdio, unregister_all_stdio,
};
pub use logging::install_stderr_logging;
pub use platform::{
	BrandingScope, CacheOpener, DriveOpener, ExistingRoot, Platform, PlatformError, PlatformFailed,
	PlatformOp, RootRefusal,
};
pub use supervisor::{ExitReason, HelperConfig, run_helper};
pub use unregister::{Cleared, UnregisterAll, unregister_all};
#[cfg(windows)]
pub(crate) use unregister::{clear_plain_root, path_taken, set_plain_root_aside};

pub use protocol::{
	ActivityEntry, ActivityKind, ErrorCode, FatalCode, Hello, IdentifiedItem, InMessage, ItemPath,
	KeptEarlier, KeptId, KeptItem, KeptItemReason, MAX_LINE_BYTES, MAX_LIST_ENTRIES,
	MAX_MESSAGE_CHARS, OutMessage, PROTOCOL_VERSION, ParseError, PhaseName, Registration,
	ReplyError, ReplyValue, Request, RequestId, RootDir, RootDirError, RootListing, SkippedEntry,
	SkippedPlace, StateReport, StatusEvent, UnregisterAllEntry, UnregisterReport, UserId,
	parse_line,
};

/// Whether the parent asked the helper to stop: set once it sent `shutdown` or closed its input,
/// and never unset. Readable on the blocking pool too; clones share it.
#[derive(Debug, Clone)]
pub(crate) struct Stop(watch::Receiver<bool>);

impl Stop {
	/// The stop `stopping` reports, set by the input's reader.
	pub(crate) fn new(stopping: watch::Receiver<bool>) -> Self {
		Self(stopping)
	}

	/// A stop never asked for, as the uninstaller's unregister has.
	pub(crate) fn never() -> Self {
		let (_, never) = watch::channel(false);
		Self(never)
	}

	/// Whether a stop was asked for.
	pub(crate) fn requested(&self) -> bool {
		*self.0.borrow()
	}

	/// Waits until a stop is asked for: forever for one that never is.
	pub(crate) async fn asked(&self) {
		let mut stopping = self.0.clone();
		if stopping.wait_for(|stopping| *stopping).await.is_err() {
			// Nothing can set it any more.
			std::future::pending::<()>().await;
		}
	}
}

/// Runs `f` on the blocking pool: every platform, key store and session store call blocks.
pub(crate) async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
	match tokio::task::spawn_blocking(f).await {
		Ok(value) => value,
		Err(error) => match error.try_into_panic() {
			Ok(panic) => std::panic::resume_unwind(panic),
			Err(_) => panic!("the runtime shut down under the helper (should be impossible)"),
		},
	}
}
