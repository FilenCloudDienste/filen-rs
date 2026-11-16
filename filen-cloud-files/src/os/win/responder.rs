//! The responders the Windows layer hands the core with each request, answering it through cfapi.
//! Each sends exactly one terminal answer, and a responder dropped unanswered fails its request
//! as transient as it drops, so a panic or a shutdown never leaves the OS waiting out its 60 s
//! timeout. Every cfapi call, failure answers included, goes through `run_blocking`.

use std::{
	fs::{File, OpenOptions},
	os::windows::fs::OpenOptionsExt,
	path::PathBuf,
};

use tracing::{debug, trace, warn};
use windows_sys::Win32::{
	Foundation::STATUS_SUCCESS,
	Storage::FileSystem::{
		FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
	},
};

use super::{
	LOG_TARGET,
	ffi::{self, Keys},
	run_blocking,
};
use crate::{
	failure::Failure,
	identity::IdentityBlob,
	os::{
		AlignedChunk, DataResponder, DehydrateResponder, DeleteResponder, EntryOutcome, OsError,
		PopulateResponder, RenameResponder,
		cfapi::{
			E_INVALIDARG, Hresult, STATUS_CLOUD_FILE_ACCESS_DENIED, covers_required,
			entry_outcomes, failure_range, failure_status, os_error, transfer_error,
			transfer_refusal_unconfirmed,
		},
	},
	placeholder::PlaceholderSpec,
	sealed,
};

/// `status` in hex, for the logs.
fn hex(status: i32) -> String {
	format!("{:#010x}", status.cast_unsigned())
}

/// Answers one `FETCH_DATA` request.
pub(crate) struct WinData {
	keys: Keys,
	/// The range the OS required, as it sent it: offset and length in bytes; `None` when the
	/// callback's parameters held none.
	required: Option<(i64, i64)>,
	/// The placeholder's size in bytes, as the OS knows it.
	file_size: i64,
	/// The file's full path, which the provider's own handle holds open; `None` when the callback
	/// carried none.
	path: Option<PathBuf>,
	/// The provider's own handle on the file, opened before the first transfer and held until the
	/// request's terminal answer (see [`WinData::hold_open`]).
	held: Option<File>,
	/// The bytes the OS took, `[start, end)`: from the first transfer's start to the last one's
	/// end, as the core transfers a request's pieces in order and without gaps.
	transferred: Option<(i64, i64)>,
	/// Whether the OS refused a transfer in a way that leaves it unsure the request is gone (see
	/// [`transfer_refusal_unconfirmed`]): the request is then failed rather than ended in silence.
	refused_unconfirmed: bool,
	/// Whether a terminal answer was sent.
	answered: bool,
}

impl WinData {
	/// The responder for a request under `keys` that required `required` (offset, length) of a
	/// `file_size`-byte file at `path`.
	pub(super) fn new(
		keys: Keys,
		required: Option<(i64, i64)>,
		file_size: i64,
		path: Option<PathBuf>,
	) -> Self {
		Self {
			keys,
			required,
			file_size,
			path,
			held: None,
			transferred: None,
			refused_unconfirmed: false,
			answered: false,
		}
	}

	/// Opens the provider's own handle on the file, for reading its attributes only and refusing no
	/// other handle, unless it holds one already. Measured (Windows 11, in the VM): while
	/// the reader that raised the request closes its handle, the platform often refuses the transfer
	/// in flight with `ERROR_NOT_A_CLOUD_FILE` and writes none of its range, leaving the file partly
	/// hydrated (6 of 10 on x64, 8 of 10 on ARM64); with such a handle held by the provider from
	/// before the first transfer to the request's end, 0 of 40 on each. The open raises no callback.
	/// Why it prevents the refusal is not known. A failed open is logged and the transfer goes ahead.
	fn hold_open(&mut self) {
		if self.held.is_some() {
			return;
		}
		let Some(path) = self.path.as_deref() else {
			return;
		};
		match OpenOptions::new()
			.access_mode(FILE_READ_ATTRIBUTES)
			.share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
			.open(path)
		{
			Ok(file) => self.held = Some(file),
			Err(error) => debug!(
				target: LOG_TARGET,
				transfer = self.keys.transfer(),
				%error,
				"could not hold the file open for its transfer"
			),
		}
	}

	/// Fails the request with `failure` and logs the answer as `answer`. A request whose range was
	/// not read is failed over the whole file, as a failure of no byte would leave it unanswered.
	fn send_failure(&mut self, failure: Failure, answer: &'static str) {
		let hr = self.try_failure(failure, answer);
		if !hr.is_ok() {
			warn!(
				target: LOG_TARGET,
				transfer = self.keys.transfer(),
				?failure,
				%hr,
				"cfapi refused a failure answer"
			);
		}
	}

	/// Fails the request with `failure` as [`WinData::send_failure`] does, but logs a refusal at
	/// debug only: what cfapi answered.
	fn try_failure(&mut self, failure: Failure, answer: &'static str) -> Hresult {
		self.answered = true;
		let status = failure_status(failure);
		let (offset, len) = failure_range(self.required, self.file_size);
		let keys = self.keys;
		let hr = run_blocking(|| ffi::fail_data(&keys, status, offset, len));
		debug!(
			target: LOG_TARGET,
			callback = "fetch_data",
			transfer = self.keys.transfer(),
			answer,
			status = hex(status),
			offset,
			len,
			hr = %hr,
			"cf answer"
		);
		hr
	}
}

impl sealed::Sealed for WinData {}

impl DataResponder for WinData {
	/// Hands the OS `chunk` in one `TRANSFER_DATA`: the core hands over at most one cache chunk
	/// (1 MiB) at a time, measured to block for about 4 to 9 ms. A refusal as cancelled, aborted or
	/// `ERROR_NOT_A_CLOUD_FILE` means the request is gone, measured after the reader closed or
	/// cancelled, and ends the transfer as [`OsError::Cancelled`].
	fn transfer(&mut self, chunk: AlignedChunk<'_>) -> Result<(), OsError> {
		let bytes = chunk.bytes();
		let (Ok(at), Ok(len)) = (i64::try_from(chunk.offset()), i64::try_from(bytes.len())) else {
			return Err(OsError::Rejected { code: E_INVALIDARG });
		};
		let Some(end) = at.checked_add(len) else {
			return Err(OsError::Rejected { code: E_INVALIDARG });
		};
		self.hold_open();
		let keys = self.keys;
		let hr = run_blocking(|| ffi::transfer_data(&keys, at, bytes));
		if !hr.is_ok() {
			debug!(
				target: LOG_TARGET,
				callback = "fetch_data",
				transfer = keys.transfer(),
				answer = "transferred",
				offset = at,
				len,
				hr = %hr,
				"cf answer refused"
			);
			self.refused_unconfirmed = transfer_refusal_unconfirmed(hr);
			return Err(transfer_error(hr));
		}
		trace!(
			target: LOG_TARGET,
			callback = "fetch_data",
			transfer = keys.transfer(),
			answer = "transferred",
			offset = at,
			len,
			"cf answer"
		);
		self.transferred = Some(match self.transferred {
			Some((start, _)) => (start, end),
			None => (at, end),
		});
		Ok(())
	}

	/// Nothing to send: cfapi needs no completion once the bytes are transferred. A completion
	/// whose transfers do not cover the required range is a core bug, logged at warn, as the
	/// fake OS flags it in the tests.
	fn complete(mut self) {
		self.answered = true;
		let covered = self
			.required
			.is_none_or(|required| covers_required(self.transferred, required, self.file_size));
		debug!(
			target: LOG_TARGET,
			callback = "fetch_data",
			transfer = self.keys.transfer(),
			answer = "complete",
			covered,
			"cf answer"
		);
		if !covered {
			warn!(
				target: LOG_TARGET,
				transfer = self.keys.transfer(),
				required = ?self.required,
				transferred = ?self.transferred,
				"completed a request whose required range was not transferred"
			);
		}
	}

	fn fail(mut self, failure: Failure) {
		self.send_failure(failure, "failed");
	}

	/// Nothing to send: the OS cancelled the request. One that ended on a refused transfer that
	/// leaves its end unsure is failed as [`Failure::Transient`] instead, an answer cfapi refuses
	/// when the request is gone.
	fn cancelled(mut self) {
		if self.refused_unconfirmed {
			self.try_failure(Failure::Transient, "failed after a refused transfer");
			return;
		}
		self.answered = true;
		debug!(
			target: LOG_TARGET,
			callback = "fetch_data",
			transfer = self.keys.transfer(),
			answer = "cancelled",
			"cf answer"
		);
	}
}

impl Drop for WinData {
	fn drop(&mut self) {
		if !self.answered {
			self.send_failure(Failure::Transient, "abandoned");
		}
	}
}

/// Answers one `FETCH_PLACEHOLDERS` request.
pub(crate) struct WinPopulate {
	keys: Keys,
	/// Whether a terminal answer was sent.
	answered: bool,
}

impl WinPopulate {
	/// The responder for a request under `keys`.
	pub(super) fn new(keys: Keys) -> Self {
		Self {
			keys,
			answered: false,
		}
	}

	/// Fails the request with `failure` and logs the answer as `answer`.
	fn send_failure(&mut self, failure: Failure, answer: &'static str) {
		self.answered = true;
		let status = failure_status(failure);
		let keys = self.keys;
		let hr = run_blocking(|| ffi::fail_placeholders(&keys, status));
		debug!(
			target: LOG_TARGET,
			callback = "fetch_placeholders",
			answer,
			status = hex(status),
			hr = %hr,
			"cf answer"
		);
		if !hr.is_ok() {
			warn!(target: LOG_TARGET, ?failure, %hr, "cfapi refused a failure answer");
		}
	}
}

impl sealed::Sealed for WinPopulate {}

impl PopulateResponder for WinPopulate {
	/// Sends `entries` in one call. A call refused as a whole leaves the request unanswered
	/// (measured: an over-long identity failed it with `E_INVALIDARG` and the listing waited
	/// 182 s), so it is failed as transient before the refusal is returned.
	fn populate(mut self, entries: &[PlaceholderSpec]) -> Result<Vec<EntryOutcome>, OsError> {
		let sent = run_blocking(|| ffi::transfer_placeholders(&self.keys, entries));
		if !sent.hr.is_ok() {
			debug!(
				target: LOG_TARGET,
				callback = "fetch_placeholders",
				answer = "populated",
				hr = %sent.hr,
				"cf answer refused"
			);
			self.send_failure(Failure::Transient, "failed");
			return Err(os_error(sent.hr));
		}
		self.answered = true;
		let outcomes = entry_outcomes(&sent.results, sent.processed);
		let created = outcomes
			.iter()
			.filter(|outcome| **outcome == EntryOutcome::Created)
			.count();
		debug!(
			target: LOG_TARGET,
			callback = "fetch_placeholders",
			answer = "populated",
			created,
			rejected = outcomes.len() - created,
			"cf answer"
		);
		Ok(outcomes)
	}

	/// Fails the request with `failure`, leaving on-demand population on, so the OS asks again on
	/// the next listing. Measured: the listing shows the directory empty meanwhile.
	fn fail(mut self, failure: Failure) {
		self.send_failure(failure, "failed");
	}
}

impl Drop for WinPopulate {
	fn drop(&mut self) {
		if !self.answered {
			self.send_failure(Failure::Transient, "abandoned");
		}
	}
}

/// Which notification a [`WinAck`] answers.
#[derive(Debug, Clone, PartialEq, Eq)]
enum AckKind {
	Delete,
	Rename,
	/// A dehydration of the placeholder of this identity, which the acknowledgement carries; none
	/// when the request carried none the core could read.
	Dehydrate(Option<IdentityBlob>),
}

impl AckKind {
	fn callback(&self) -> &'static str {
		match self {
			Self::Delete => "notify_delete",
			Self::Rename => "notify_rename",
			Self::Dehydrate(_) => "notify_dehydrate",
		}
	}
}

/// Answers one `NOTIFY_DELETE`, `NOTIFY_RENAME` or `NOTIFY_DEHYDRATE`: its acknowledgement with
/// success to let the operation go ahead, with a failure status to veto it. The application sees the status's
/// error (measured: 388 for `STATUS_CLOUD_FILE_NETWORK_UNAVAILABLE`).
struct WinAck {
	keys: Keys,
	kind: AckKind,
	/// Whether an answer was sent.
	answered: bool,
}

impl WinAck {
	fn new(keys: Keys, kind: AckKind) -> Self {
		Self {
			keys,
			kind,
			answered: false,
		}
	}

	/// Sends the answer `status`, logged as `answer`.
	fn send(&mut self, status: i32, answer: &'static str) {
		self.answered = true;
		let (keys, kind) = (self.keys, &self.kind);
		let hr = run_blocking(|| match kind {
			AckKind::Delete => ffi::ack_delete(&keys, status),
			AckKind::Rename => ffi::ack_rename(&keys, status),
			AckKind::Dehydrate(identity) => ffi::ack_dehydrate(&keys, status, identity.as_ref()),
		});
		debug!(
			target: LOG_TARGET,
			callback = self.kind.callback(),
			answer,
			status = hex(status),
			hr = %hr,
			"cf answer"
		);
		if !hr.is_ok() {
			warn!(
				target: LOG_TARGET,
				callback = self.kind.callback(),
				answer,
				%hr,
				"cfapi refused an acknowledgement"
			);
		}
	}

	fn allow(&mut self) {
		self.send(STATUS_SUCCESS, "allowed");
	}

	fn veto(&mut self, failure: Failure) {
		self.send(failure_status(failure), "vetoed");
	}
}

impl Drop for WinAck {
	fn drop(&mut self) {
		if !self.answered {
			self.send(failure_status(Failure::Transient), "abandoned");
		}
	}
}

/// Answers one `NOTIFY_DELETE`. Dropped unanswered, it vetoes the delete as transient.
pub(crate) struct WinDelete(WinAck);

impl WinDelete {
	/// The responder for a request under `keys`.
	pub(super) fn new(keys: Keys) -> Self {
		Self(WinAck::new(keys, AckKind::Delete))
	}
}

impl sealed::Sealed for WinDelete {}

impl DeleteResponder for WinDelete {
	fn allow(mut self) {
		self.0.allow();
	}

	fn veto(mut self, failure: Failure) {
		self.0.veto(failure);
	}
}

/// Answers one `NOTIFY_RENAME`. Dropped unanswered, it vetoes the rename as transient.
pub(crate) struct WinRename(WinAck);

impl WinRename {
	/// The responder for a request under `keys`.
	pub(super) fn new(keys: Keys) -> Self {
		Self(WinAck::new(keys, AckKind::Rename))
	}
}

impl sealed::Sealed for WinRename {}

impl RenameResponder for WinRename {
	fn allow(mut self) {
		self.0.allow();
	}

	fn veto(mut self, failure: Failure) {
		self.0.veto(failure);
	}
}

/// Answers one `NOTIFY_DEHYDRATE`. A veto is `STATUS_CLOUD_FILE_ACCESS_DENIED`, measured
/// (Windows 11, in the VM) to fail the caller at once with `ERROR_CLOUD_FILE_ACCESS_DENIED` and
/// leave the file's bytes and pin state alone. Dropped unanswered, it vetoes the dehydration.
pub(crate) struct WinDehydrate(WinAck);

impl WinDehydrate {
	/// The responder for a request under `keys` to dehydrate the placeholder `identity`, or one
	/// whose identity could not be read (`None`), which is only ever vetoed.
	pub(super) fn new(keys: Keys, identity: Option<IdentityBlob>) -> Self {
		Self(WinAck::new(keys, AckKind::Dehydrate(identity)))
	}
}

impl sealed::Sealed for WinDehydrate {}

impl DehydrateResponder for WinDehydrate {
	fn allow(mut self) {
		self.0.allow();
	}

	fn veto(mut self) {
		self.0.send(STATUS_CLOUD_FILE_ACCESS_DENIED, "vetoed");
	}
}
