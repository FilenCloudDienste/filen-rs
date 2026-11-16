//! Hydration: answering a `FETCH_DATA` request with the bytes of a file placeholder.

mod plan;

use std::{future::ready, sync::Arc};

use filen_mobile_native_cache::{
	CacheError,
	abort::FfiAbortSignal,
	ffi::FfiId,
	range::{RangeIntegrity, RangeRead, RangeReadError, RangeSink},
};
use filen_types::fs::Uuid;

use self::plan::{ChunkError, PlanError, TransferWindow, plan};
use crate::{
	drive::Drive,
	failure::{Answer, Failure, cache_answer},
	identity::{IdentityError, PlaceholderIdentity, stable_id},
	log_safe::LoggedError,
	os::{DataResponder, FetchDataRequest, OsError},
};

/// The sink a hydration reads into: it checks the read is of the placeholder's version and size
/// before any byte, then hands each piece to the OS.
struct TransferSink<'a, R: DataResponder> {
	responder: &'a mut R,
	window: TransferWindow,
	/// The content version the placeholder holds.
	version: Uuid,
	/// Bytes the OS took.
	transferred: u64,
}

/// Why the sink refused a read or a piece of it.
#[derive(Debug, thiserror::Error)]
pub(crate) enum SinkError {
	/// The cache holds another version of the file than the placeholder.
	#[error("the cache holds version {current}, the placeholder {placeholder}")]
	VersionChanged { placeholder: Uuid, current: Uuid },
	/// The cache gives the file another size than the placeholder.
	#[error("the cache says {current} bytes, the placeholder {placeholder}")]
	SizeChanged { placeholder: u64, current: u64 },
	/// A piece cfapi would refuse. An invariant: an aligned read yields aligned pieces.
	#[error(transparent)]
	Chunk(#[from] ChunkError),
	/// The OS did not take a piece.
	#[error(transparent)]
	Os(#[from] OsError),
}

impl<R: DataResponder> TransferSink<'_, R> {
	/// Hands `bytes` at `offset` to the OS.
	fn deliver(&mut self, offset: u64, bytes: &[u8]) -> Result<(), SinkError> {
		let chunk = self.window.chunk(offset, bytes)?;
		self.responder.transfer(chunk)?;
		// Bounded by the window, which is inside a file of at most `i64::MAX` bytes.
		self.transferred = self
			.transferred
			.saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
		Ok(())
	}
}

impl<R: DataResponder> RangeSink for TransferSink<'_, R> {
	type Error = SinkError;

	/// Refuses a read of another version or size than the placeholder's, before any byte. With
	/// both equal, the read's range is the window: the window lies inside the placeholder's size,
	/// so clamping it to the same size leaves it whole.
	fn on_start(&mut self, read: &RangeRead) -> Result<(), SinkError> {
		if read.uuid != self.version {
			return Err(SinkError::VersionChanged {
				placeholder: self.version,
				current: read.uuid,
			});
		}
		let placeholder = self.window.file_size().get();
		if read.file_size != placeholder {
			return Err(SinkError::SizeChanged {
				placeholder,
				current: read.file_size,
			});
		}
		Ok(())
	}

	fn on_bytes(
		&mut self,
		offset: u64,
		bytes: &[u8],
	) -> impl Future<Output = Result<(), SinkError>> + Send {
		// The OS takes each piece synchronously.
		ready(self.deliver(offset, bytes))
	}
}

/// What a successful hydration checked its bytes against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Verification {
	/// The window was the whole file, and the cache's read checked its declared hash.
	HashChecked,
	/// Nothing beyond decryption: a file read across several requests is checked once it is all
	/// on disk, by the local engine's hydration job.
	Unchecked,
}

/// What a hydration transferred.
#[derive(Debug)]
pub(crate) struct HydrateReport {
	/// The read: the version, the file's size and the bytes transferred.
	pub(crate) read: RangeRead,
	/// What the bytes were checked against.
	pub(crate) verification: Verification,
}

/// A failed hydration, with how much of it the OS took.
#[derive(Debug)]
pub(crate) struct HydrateFailed {
	/// Bytes the OS took before the failure.
	pub(crate) transferred: u64,
	pub(crate) error: HydrateError,
}

/// Why a hydration failed.
#[derive(Debug, thiserror::Error)]
pub(crate) enum HydrateError {
	/// The request's identity is not one this build writes.
	#[error(transparent)]
	Identity(#[from] IdentityError),
	/// The request names no file placeholder.
	#[error("not a file placeholder")]
	NotAFile,
	/// The request's range has no window.
	#[error(transparent)]
	Plan(#[from] PlanError),
	/// The read failed.
	#[error(transparent)]
	Read(#[from] RangeReadError<SinkError>),
}

impl HydrateError {
	/// How the request ends. Each arm is a recorded default.
	pub(crate) fn answer(&self) -> Answer {
		match self {
			Self::Identity(_) | Self::NotAFile | Self::Plan(_) => {
				Answer::Fail(Failure::InvalidRequest)
			}
			Self::Read(error) => match error {
				// The cache's file is shorter than the placeholder, or of another version or size:
				// the placeholder is out of date until the remote change is applied to it.
				RangeReadError::PastEnd { .. }
				| RangeReadError::Sink(
					SinkError::VersionChanged { .. } | SinkError::SizeChanged { .. },
				) => Answer::Fail(Failure::StalePlaceholder),
				// The server holds fewer bytes than the file's size. No dehydrate: the bytes
				// delivered are authentic, only the rest is missing.
				RangeReadError::Truncated { .. } => Answer::Fail(Failure::ContentDamaged),
				RangeReadError::Sink(SinkError::Chunk(_)) => Answer::Fail(Failure::Internal),
				RangeReadError::Sink(SinkError::Os(OsError::Cancelled)) => Answer::Cancelled,
				RangeReadError::Sink(SinkError::Os(OsError::Rejected { .. })) => {
					Answer::Fail(Failure::Transient)
				}
				RangeReadError::Cache(error) => cache_answer(error),
			},
		}
	}
}

impl LoggedError for HydrateError {
	fn cache_error(&self) -> Option<&CacheError> {
		match self {
			Self::Read(RangeReadError::Cache(error)) => Some(error),
			Self::Identity(_)
			| Self::NotAFile
			| Self::Plan(_)
			| Self::Read(
				RangeReadError::PastEnd { .. }
				| RangeReadError::Truncated { .. }
				| RangeReadError::Sink(_),
			) => None,
		}
	}
}

/// Answers `request` through `responder` with the bytes of its file placeholder, read from
/// `drive` and cancelled through `abort`.
///
/// The placeholder's identity names the content version it holds, and a read of any other version
/// is refused as a stale placeholder rather than mixing two versions in one file, until the
/// remote change is applied to the placeholder. The file is resolved from the cache alone, with no
/// server round trip.
pub(crate) async fn hydrate<D: Drive, R: DataResponder>(
	drive: &D,
	request: FetchDataRequest,
	mut responder: R,
	abort: &Arc<FfiAbortSignal>,
) -> Result<HydrateReport, HydrateFailed> {
	let (id, version, window) = match prepare(&request) {
		Ok(prepared) => prepared,
		Err(error) => return Err(finish_failed(responder, error, 0)),
	};
	if abort.is_aborted() {
		// Cancelled before its read: the cache is not asked for anything.
		let aborted = CacheError::Aborted("the request was cancelled before its read".into());
		let error = HydrateError::Read(RangeReadError::Cache(aborted));
		return Err(finish_failed(responder, error, 0));
	}
	let mut sink = TransferSink {
		responder: &mut responder,
		window,
		version,
		transferred: 0,
	};
	let result = drive
		.read_range(&id, window.byte_range(), &mut sink, Some(abort))
		.await;
	let transferred = sink.transferred;
	match result {
		Ok(read) => {
			let verification = if read.integrity == RangeIntegrity::HashChecked {
				Verification::HashChecked
			} else {
				Verification::Unchecked
			};
			responder.complete();
			Ok(HydrateReport { read, verification })
		}
		Err(error) => Err(finish_failed(responder, error.into(), transferred)),
	}
}

/// The id the cache resolves the request's file by, its version and the request's window.
fn prepare(request: &FetchDataRequest) -> Result<(FfiId, Uuid, TransferWindow), HydrateError> {
	let identity = PlaceholderIdentity::decode(request.identity())?;
	let PlaceholderIdentity::File { stable, version } = identity else {
		return Err(HydrateError::NotAFile);
	};
	Ok((stable_id(stable), version, plan(request.range())?))
}

/// Ends the request for `error`, through exactly one of the responder's terminal calls.
fn finish_failed<R: DataResponder>(
	responder: R,
	error: HydrateError,
	transferred: u64,
) -> HydrateFailed {
	match error.answer() {
		Answer::Cancelled => responder.cancelled(),
		Answer::Fail(failure) => responder.fail(failure),
		Answer::Damaged => responder.fail(Failure::ContentDamaged),
	}
	HydrateFailed { transferred, error }
}

#[cfg(test)]
mod tests;
