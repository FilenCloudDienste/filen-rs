//! The rclone daemon's UniFFI exports the shared module does not carry.

use super::{JsRcloneDaemon, RcloneDaemonOptions, rclone_refusal};
use crate::{Error, auth::JsClient, error::FilenSdkError, rclone::RcloneRefusal};

#[uniffi::export]
impl JsClient {
	/// Start an `rclone rcd` for this account, with rclone's remotes in its memory and its rc
	/// behind a random password: no secret in its argv, environment or files.
	pub async fn start_rclone_daemon(
		&self,
		options: RcloneDaemonOptions,
	) -> Result<JsRcloneDaemon, Error> {
		JsRcloneDaemon::start(self.inner(), options).await
	}
}

// Named by the struct, not the `Error` alias: UniFFI registers these methods under the name it is
// given, and the object JS knows is `FilenSdkError`.
#[uniffi::export]
impl FilenSdkError {
	/// The rclone refusal this error is, for a UI to act on without reading the message: a missing
	/// binary or FUSE, a mount point or port in use, a serve without credentials, a cache with
	/// uploads pending, and the rest of `RcloneRefusal`. `None` for every other error.
	pub fn rclone_refusal(&self) -> Option<RcloneRefusal> {
		rclone_refusal(self)
	}
}
