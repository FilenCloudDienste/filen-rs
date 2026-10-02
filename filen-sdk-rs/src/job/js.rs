//! The error record of a job's progress and report in the bindings, for the jobs that build
//! those where they run (the archive jobs); a copy's carry the SDK error itself.

use std::sync::Arc;

use crate::Error;

/// An error in a job's progress or report. On uniffi it is the SDK error itself, as in the
/// other uniffi records that carry one (`UploadError`, `DownloadError`).
#[cfg(feature = "uniffi")]
pub type JobError = Arc<Error>;

/// An error in a job's progress or report: the parts of the SDK error, not the error itself. A
/// tsify record cannot hold the wasm_bindgen `FilenSdkError` class, and the `JsValue` that
/// could carry one cannot be made on the commander thread, where these jobs build their updates
/// and their reports.
#[cfg(not(feature = "uniffi"))]
#[filen_macros::js_type(export, no_deser)]
pub struct JobError {
	pub kind: crate::ErrorKind,
	pub message: String,
	/// The server's message, for errors the server returned.
	pub server_message: Option<String>,
	pub server_code: Option<String>,
	/// The wrapped error's message, without the `Error of kind ...` of `message`.
	pub inner_message: Option<String>,
}

#[cfg(feature = "uniffi")]
pub(crate) fn job_error(error: Arc<Error>) -> JobError {
	error
}

// Takes the Arc, though it only reads the error, to share its signature with the uniffi twin.
#[cfg(not(feature = "uniffi"))]
pub(crate) fn job_error(error: Arc<Error>) -> JobError {
	JobError {
		kind: error.kind(),
		message: error.message(),
		server_message: error.server_message(),
		server_code: error.server_code(),
		inner_message: error.inner_message(),
	}
}
