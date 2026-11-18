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

pub use index::{StateDir, StateDirError};
pub use launch::{MIN_BUILD, StateLock, fatal, serve_stdio, unregister_all_stdio};
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
