//! The records of a job's progress and report in the bindings that every job (copies,
//! archives) shares: its errors, and its times.

use std::{sync::Arc, time::Duration};

use filen_types::fs::Uuid;

use crate::Error;

/// An error in a job's progress or report. On uniffi it is the SDK error itself, as in the
/// other uniffi records that carry one (`UploadError`, `DownloadError`).
#[cfg(feature = "uniffi")]
pub type JobError = Arc<Error>;

/// An error in a job's progress or report: the parts of the SDK error, not the error itself. A
/// tsify record cannot hold the wasm_bindgen `FilenSdkError` class, and the `JsValue` that
/// could carry one cannot be made on the commander thread, where jobs build their updates and
/// their reports.
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

/// A created item a job could not finish: it could not get its color, or could not be added to
/// one of the destination's public links or shares.
#[derive(Debug, Clone)]
#[filen_macros::js_type(export, no_deser, no_default)]
pub struct ItemError {
	pub dest_uuid: Uuid,
	pub error: JobError,
}

impl ItemError {
	pub(crate) fn new(dest_uuid: Uuid, error: Arc<Error>) -> Self {
		Self {
			dest_uuid,
			error: job_error(error),
		}
	}
}

// The names these records went by when only a copy had them, so TypeScript written against those
// keeps compiling.
#[cfg(feature = "wasm-full")]
#[wasm_bindgen::prelude::wasm_bindgen(typescript_custom_section)]
const TS_COPY_ALIASES: &str = r#"
export type CopyError = JobError;
export type CopyCounts = ItemCounts;
export type CopyItemError = ItemError;
export type CopyRenamedEntry = RenamedEntry;
"#;

/// A duration in a job's progress, in the milliseconds the bindings report it in.
pub(crate) fn millis(duration: Duration) -> u64 {
	u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}
