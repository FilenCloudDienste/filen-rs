//! The sync engine's UniFFI exports.

use super::types::{SyncRefusal, sync_refusal};
use crate::{error::FilenSdkError, sync_engine::DEFAULT_IGNORE_PATTERNS};

// Named by the struct, not the `Error` alias: UniFFI registers these methods under the name it is
// given, and the object JS knows is `FilenSdkError`.
#[uniffi::export]
impl FilenSdkError {
	/// The sync refusal this error is, for a UI to act on without reading the message: a baseline
	/// another sync engine has open, or a pair whose roots overlap an existing one's. `None` for
	/// every other error.
	pub fn sync_refusal(&self) -> Option<SyncRefusal> {
		sync_refusal(self)
	}
}

/// The ignore patterns every sync pair applies below its own, for an ignore editor to show as
/// built in.
#[uniffi::export]
pub fn default_sync_ignore_patterns() -> Vec<String> {
	DEFAULT_IGNORE_PATTERNS
		.iter()
		.map(|pattern| (*pattern).to_owned())
		.collect()
}
