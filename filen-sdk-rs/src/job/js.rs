//! The records of a job's progress and report in the bindings that every job (copies,
//! archives) shares: its errors, and its times.

use std::{sync::Arc, time::Duration};

use filen_types::fs::Uuid;

use crate::Error;

/// An error in a job's progress or report: the SDK error itself, as the calls throw it. On
/// uniffi it is the error's `Arc`, as in the other uniffi records that carry one
/// (`UploadError`, `DownloadError`).
#[cfg(feature = "uniffi")]
pub type SdkError = Arc<Error>;

/// An error in a job's progress or report: the SDK error itself, the `FilenSdkError` class the
/// calls throw. Only the JS thread can make one, so the records holding it are built there, as
/// their callbacks are delivered and once the job has returned, never on the commander thread
/// the job runs on; that the type is not `Send` keeps it so.
#[cfg(not(feature = "uniffi"))]
#[derive(Debug, Clone)]
#[filen_macros::js_type(export, no_deser, no_default)]
pub struct SdkError(
	#[cfg_attr(
		feature = "wasm-full",
		serde(with = "serde_wasm_bindgen::preserve"),
		tsify(type = "FilenSdkError")
	)]
	wasm_bindgen::JsValue,
);

/// `error` for a job's record.
#[cfg(feature = "uniffi")]
pub(crate) fn sdk_error(error: Arc<Error>) -> SdkError {
	error
}

/// `error` for a job's record, made on the JS thread (see [`SdkError`]): the error itself when
/// nothing else holds it, else an error of the same kind wrapping the shared one, which reads as
/// it.
#[cfg(not(feature = "uniffi"))]
pub(crate) fn sdk_error(error: Arc<Error>) -> SdkError {
	SdkError(wasm_bindgen::JsValue::from(Error::unshared(error)))
}

/// A created item a job could not finish: it could not get its color, or could not be added to
/// one of the destination's public links or shares.
#[derive(Debug, Clone)]
#[filen_macros::js_type(export, no_deser, no_default)]
pub struct ItemError {
	/// The created item.
	pub dest_uuid: Uuid,
	/// Why it could not be finished.
	pub error: SdkError,
}

impl ItemError {
	pub(crate) fn new(dest_uuid: Uuid, error: Arc<Error>) -> Self {
		Self {
			dest_uuid,
			error: sdk_error(error),
		}
	}
}

// The names these records went by when only a copy had them, so TypeScript written against those
// keeps compiling.
#[cfg(feature = "wasm-full")]
#[wasm_bindgen::prelude::wasm_bindgen(typescript_custom_section)]
const TS_COPY_ALIASES: &str = r#"
export type CopyError = FilenSdkError;
export type CopyCounts = ItemCounts;
export type CopyItemError = ItemError;
export type CopyRenamedEntry = RenamedEntry;
"#;

// The same names for Kotlin, Swift and the React Native TypeScript. uniffi has no type alias,
// but a custom type over another becomes one there (`typealias CopyCounts = ItemCounts`, `export
// type CopyCounts = ItemCounts`). `CopyError` needs none: uniffi hands over the error itself.
#[cfg(feature = "uniffi")]
mod uniffi_copy_aliases {
	use super::ItemError;
	use crate::fs::drive_job::{counts::ItemCounts, plan::RenamedEntry};

	pub struct CopyCounts(ItemCounts);
	uniffi::custom_newtype!(CopyCounts, ItemCounts);

	pub struct CopyItemError(ItemError);
	uniffi::custom_newtype!(CopyItemError, ItemError);

	pub struct CopyRenamedEntry(RenamedEntry);
	uniffi::custom_newtype!(CopyRenamedEntry, RenamedEntry);
}

/// A duration in a job's progress, in the milliseconds the bindings report it in.
pub(crate) fn millis(duration: Duration) -> u64 {
	u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}
