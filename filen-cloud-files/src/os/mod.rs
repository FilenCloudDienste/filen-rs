//! The seam between the core and the OS. The OS layer turns each Cloud Files callback into an
//! owned request plus a responder, and the core answers through the responder: each terminal call
//! consumes it, so a request is answered at most once, and an implementation's `Drop` fails a
//! request left unanswered, so a panic or a shutdown never leaves the OS waiting out its timeout.

// The Windows layer's rules, apart from the calls: built on Windows, and in tests on every host.
#[cfg(any(windows, test))]
pub(crate) mod cfapi;
mod local;
mod watch;
#[cfg(windows)]
pub(crate) mod win;

pub(crate) use local::ticks_to_unix_millis;
pub use local::{
	CheckedHash, Completed, DehydrateReason, DehydrateRequest, DehydrateResponder, DeleteRequest,
	DeleteResponder, FileSnapshot, FileStamp, IfPinned, LocalEntry, LocalError, LocalEvent,
	LocalFileId, LocalFs, LocalKind, PinAttr, PlaceholderState, RenameRequest, RenameResponder,
	RenameTarget, UnsupportedKind, Updated,
};
pub use watch::{LocalWatcher, WatchError, WatchSignal, WatchSink};

use crate::{failure::Failure, identity::IdentityBlob, placeholder::PlaceholderSpec, sealed};

/// The OS's handle on one data transfer (`CF_CALLBACK_INFO.TransferKey`; a counter in the fake).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TransferKey(i64);

impl TransferKey {
	/// The key the OS handed over.
	pub fn new(raw: i64) -> Self {
		Self(raw)
	}

	/// The key, as the OS takes it back.
	pub fn get(self) -> i64 {
		self.0
	}
}

/// A `FETCH_DATA` callback's numbers exactly as the OS sends them, in bytes: signed and
/// unvalidated (Windows was measured asking for `[-4096, +8192)` of a 10,000-byte file).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestedRange {
	file_size: i64,
	required_offset: i64,
	required_len: i64,
	optional: Option<OptionalRange>,
}

impl RequestedRange {
	/// The numbers of one `FETCH_DATA` callback: the placeholder's size, the range the OS needs
	/// and the range it would also take, if it offers one (see [`OptionalRange::new`]).
	pub fn new(
		file_size: i64,
		required_offset: i64,
		required_len: i64,
		optional: Option<OptionalRange>,
	) -> Self {
		Self {
			file_size,
			required_offset,
			required_len,
			optional,
		}
	}

	/// The placeholder's size in bytes, as the OS knows it.
	pub fn file_size(self) -> i64 {
		self.file_size
	}

	/// Where the range the OS needs starts.
	pub fn required_offset(self) -> i64 {
		self.required_offset
	}

	/// How many bytes the OS needs.
	pub fn required_len(self) -> i64 {
		self.required_len
	}

	/// The range the OS would also take, when it offers one.
	pub fn optional(self) -> Option<OptionalRange> {
		self.optional
	}
}

/// The range a `FETCH_DATA` callback offers beyond the required one, in bytes: its offset as the
/// OS sends it, signed and unvalidated, and a length of at least one byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OptionalRange {
	offset: i64,
	len: i64,
}

impl OptionalRange {
	/// The optional range of `len` bytes at `offset`, as `CF_CALLBACK_PARAMETERS.FetchData` gives
	/// them, or `None` when it holds no byte: Windows sends a length of 0 when it offers none, and
	/// a negative length offers nothing either.
	pub fn new(offset: i64, len: i64) -> Option<Self> {
		(len > 0).then_some(Self { offset, len })
	}

	/// Where the range starts.
	pub fn offset(self) -> i64 {
		self.offset
	}

	/// How many bytes it holds; at least one.
	pub fn length(self) -> i64 {
		self.len
	}
}

/// A `FETCH_DATA` callback: the OS wants bytes of a file placeholder.
#[derive(Debug)]
pub struct FetchDataRequest {
	identity: IdentityBlob,
	transfer: TransferKey,
	range: RequestedRange,
	own_process: bool,
}

impl FetchDataRequest {
	/// The request for `range` of the placeholder `identity`, under the OS's `transfer` key;
	/// `own_process` when the provider's own process raised it (its own hydrations and reads,
	/// measured to raise `FETCH_DATA` with its process id).
	pub fn new(
		identity: IdentityBlob,
		transfer: TransferKey,
		range: RequestedRange,
		own_process: bool,
	) -> Self {
		Self {
			identity,
			transfer,
			range,
			own_process,
		}
	}

	/// Whether the provider's own process raised the request.
	pub fn own_process(&self) -> bool {
		self.own_process
	}

	/// The placeholder's identity, unparsed.
	pub fn identity(&self) -> &IdentityBlob {
		&self.identity
	}

	/// The OS's key for this transfer, which a cancel names.
	pub fn transfer(&self) -> TransferKey {
		self.transfer
	}

	/// The bytes asked for.
	pub fn range(&self) -> RequestedRange {
		self.range
	}
}

/// A `FETCH_PLACEHOLDERS` callback: the OS wants a directory placeholder's children.
#[derive(Debug)]
pub struct PopulateRequest {
	identity: IdentityBlob,
	own_process: bool,
}

impl PopulateRequest {
	/// The request for the children of the directory placeholder `identity`; `own_process` when
	/// the provider's own process raised it (a rename or copy of its own into a directory not
	/// populated yet).
	pub fn new(identity: IdentityBlob, own_process: bool) -> Self {
		Self {
			identity,
			own_process,
		}
	}

	/// Whether the provider's own process raised the request.
	pub fn own_process(&self) -> bool {
		self.own_process
	}

	/// The directory placeholder's identity, unparsed.
	pub fn identity(&self) -> &IdentityBlob {
		&self.identity
	}
}

/// A `CANCEL_FETCH_DATA` callback: the OS no longer wants a range of the transfer `transfer`.
#[derive(Debug)]
pub struct CancelRequest {
	transfer: TransferKey,
	range: Option<CancelledRange>,
}

/// The range a cancel names: its numbers as the OS sends them, unvalidated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CancelledRange {
	offset: i64,
	length: i64,
}

impl CancelledRange {
	/// Where the cancelled range starts, in bytes.
	pub fn offset(self) -> i64 {
		self.offset
	}

	/// How many bytes are cancelled.
	pub fn length(self) -> i64 {
		self.length
	}
}

impl CancelRequest {
	/// The cancel of `length` bytes at `offset` under the `transfer` key.
	pub fn new(transfer: TransferKey, offset: i64, length: i64) -> Self {
		Self {
			transfer,
			range: Some(CancelledRange { offset, length }),
		}
	}

	/// A cancel under the `transfer` key whose range could not be read: its parameters were too
	/// short to hold one.
	pub fn unranged(transfer: TransferKey) -> Self {
		Self {
			transfer,
			range: None,
		}
	}

	/// The key of the transfer cancelled.
	pub fn transfer(&self) -> TransferKey {
		self.transfer
	}

	/// The range cancelled, when the OS's parameters held one.
	pub fn range(&self) -> Option<CancelledRange> {
		self.range
	}
}

/// Why the OS did not take what the core sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum OsError {
	/// The request is gone: cancelled, or answered by the OS already (a late `TRANSFER_DATA` was
	/// measured returning `0x8007018e`). Expected, not a failure.
	#[error("the OS cancelled the request")]
	Cancelled,
	/// The OS refused the operation with the HRESULT `code`.
	#[error("the OS refused the operation (code {code:#010x})")]
	Rejected { code: i32 },
}

/// What became of one placeholder sent in a population. Windows reports one result per entry, and
/// a refused entry does not fail the call, so each must be checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryOutcome {
	/// The placeholder exists.
	Created,
	/// A local item already holds its name: one the user made or moved in before the directory was
	/// populated, which the local side takes in. Not a refusal: nothing is skipped.
	Exists,
	/// The OS refused this placeholder.
	Rejected(OsError),
}

/// A piece of a file cfapi accepts in one transfer: its offset a multiple of 4096 bytes, its length
/// a multiple of 4096 unless it ends at the end of the file, not empty, and inside the range the
/// core planned for the request. Only the hydration planner builds one.
#[derive(Debug, Clone, Copy)]
pub struct AlignedChunk<'a> {
	offset: u64,
	bytes: &'a [u8],
}

impl<'a> AlignedChunk<'a> {
	/// The piece `bytes` at `offset`. Unchecked: only the planner's window, which checks every rule
	/// above, calls it.
	pub(crate) fn new(offset: u64, bytes: &'a [u8]) -> Self {
		Self { offset, bytes }
	}

	/// Where the piece starts in the file, in bytes.
	pub fn offset(&self) -> u64 {
		self.offset
	}

	/// The piece's bytes.
	pub fn bytes(&self) -> &'a [u8] {
		self.bytes
	}
}

/// Answers one `FETCH_PLACEHOLDERS` request. Dropping it unanswered fails the request with
/// [`Failure::Transient`].
pub trait PopulateResponder: sealed::Sealed + Send + 'static {
	/// Creates `entries` under the directory and turns on-demand population off for it, so the OS
	/// does not ask again. `Ok` holds one outcome per entry, in order; `Err` means the OS refused
	/// the whole call, and the request is answered as failed.
	fn populate(self, entries: &[PlaceholderSpec]) -> Result<Vec<EntryOutcome>, OsError>;

	/// Fails the request with `failure`.
	fn fail(self, failure: Failure);
}

/// Answers one `FETCH_DATA` request. Dropping it unanswered fails the request with
/// [`Failure::Transient`].
pub trait DataResponder: sealed::Sealed + Send + 'static {
	/// Hands the OS one piece of the file.
	fn transfer(&mut self, chunk: AlignedChunk<'_>) -> Result<(), OsError>;

	/// Ends the request: every byte the core planned for it was transferred. Nothing to tell the
	/// OS on Windows; the fake checks the request's range is covered.
	fn complete(self);

	/// Fails the range the OS needs with `failure`.
	fn fail(self, failure: Failure);

	/// Ends the request the OS cancelled: nothing more is sent.
	fn cancelled(self);
}

#[cfg(test)]
mod tests;
