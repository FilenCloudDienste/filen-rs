//! The seam for local changes: what the watcher and the platform's notifications report, how the
//! core reads and changes items in the sync root, and the delete and rename requests it answers.
//!
//! Every event is a path to look at, never a verdict: the core reads the item's state through
//! [`LocalFs::inspect`] and decides from that. The provider's own writes raise events too (a
//! hydration, a convert), and the state they leave, a placeholder in sync, is what tells them
//! apart from a user's change.

use std::{path::PathBuf, sync::atomic::AtomicBool};

use filen_sdk_rs::thumbnail::ByteSource;

use crate::{
	failure::Failure,
	identity::{IdentityBlob, IdentityError, PlaceholderIdentity},
	os::OsError,
	placeholder::PlaceholderSpec,
	root_path::{RootPath, RootPathError},
	sealed,
};

/// What the watcher or a notification reports about the sync root: paths to look at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalEvent {
	/// An item appeared at the path.
	Created(RootPath),
	/// The item's data or metadata changed, or a handle on it was closed after a write.
	Modified(RootPath),
	/// The item is gone from the path. Never a delete by itself: a delete reaches the remote only
	/// through a [`DeleteRequest`].
	Removed(RootPath),
	/// An item moved from one path to another; each side is looked at on its own.
	Renamed { from: RootPath, to: RootPath },
	/// A placeholder's last handle was closed (`NOTIFY_FILE_CLOSE_COMPLETION`): its writer is done.
	Closed(RootPath),
	/// The watcher lost events, or the root itself changed: every path must be looked at again.
	Rescan,
}

/// When and how big a local file is: compared to tell whether a file is still being written.
/// Times are FILETIME ticks (100 ns since 1601), as Windows reports them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileStamp {
	/// The size in bytes.
	pub size: u64,
	/// When the file was created.
	pub created: i64,
	/// When its data was last written.
	pub modified: i64,
	/// When its data or metadata last changed.
	pub changed: i64,
}

/// FILETIME ticks per millisecond.
const TICKS_PER_MILLI: i64 = 10_000;
/// Milliseconds from the FILETIME epoch (1601) to the Unix epoch.
const UNIX_EPOCH_FILETIME_MILLIS: i64 = 11_644_473_600_000;

/// The Unix time in milliseconds of `ticks` FILETIME ticks; `None` for a time before 1970.
pub(crate) fn ticks_to_unix_millis(ticks: i64) -> Option<i64> {
	(ticks / TICKS_PER_MILLI)
		.checked_sub(UNIX_EPOCH_FILETIME_MILLIS)
		.filter(|millis| *millis >= 0)
}

/// A local item's identity on its volume (`FILE_ID_INFO`: the volume serial number and the 128-bit
/// file id), which survives a rename: how the core follows a file it is uploading.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LocalFileId(u128);

impl LocalFileId {
	/// The id as the OS layer derives it.
	pub const fn new(raw: u128) -> Self {
		Self(raw)
	}

	/// The id.
	pub fn get(self) -> u128 {
		self.0
	}
}

/// Whether a local item is a file, with its stamp, or a directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalKind {
	/// A file.
	File(FileStamp),
	/// A directory, created at `created` (FILETIME ticks).
	Dir { created: i64 },
}

/// What the user asked for an item's data, by its own attributes: Explorer's "Always keep on this
/// device" sets `FILE_ATTRIBUTE_PINNED`, its "Free up space" `FILE_ATTRIBUTE_UNPINNED`. Measured
/// (Windows 11, in the VM): the platform acts on neither, so the provider does. An item
/// with neither of its own follows the nearest directory above it that has one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinAttr {
	/// Neither: the item follows its directory.
	Unspecified,
	/// Kept on this device: hydrated whole and never dehydrated by the provider.
	Pinned,
	/// Its space freed: dehydrated unless it holds local changes.
	Unpinned,
}

/// What [`LocalFs::dehydrate_clean`] does with a file pinned to this device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IfPinned {
	/// Refuses it with [`LocalError::Pinned`], as the platform does: the user's pin stands.
	Refuse,
	/// Unpins it for the dehydration and pins it again after: only for content found damaged,
	/// which must not stay on disk.
	Override,
}

/// A placeholder's sync state, as `CfGetPlaceholderInfo` and its attributes report it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlaceholderState {
	in_sync: bool,
	partial: bool,
	on_disk: u64,
	modified_bytes: u64,
	pin: PinAttr,
}

impl PlaceholderState {
	/// The state of a placeholder `in_sync` or not, `partial` or not, holding `on_disk` bytes of
	/// data locally of which `modified_bytes` were written since it was last marked in sync, with
	/// the pin `pin` of its own.
	pub fn new(
		in_sync: bool,
		partial: bool,
		on_disk: u64,
		modified_bytes: u64,
		pin: PinAttr,
	) -> Self {
		Self {
			in_sync,
			partial,
			on_disk,
			modified_bytes,
			pin,
		}
	}

	/// The pin the item has of its own, not inherited.
	pub fn pin(self) -> PinAttr {
		self.pin
	}

	/// Whether the placeholder is partial: a file not all on disk, or a directory the platform has
	/// not populated yet, which asks the provider for its children the next time it is listed.
	pub fn partial(self) -> bool {
		self.partial
	}

	/// Whether the placeholder is marked in sync. A data write clears it, except an append through a
	/// handle opened for appending only, which was measured to leave it set.
	pub fn in_sync(self) -> bool {
		self.in_sync
	}

	/// How many bytes of the file's data are on disk (`OnDiskDataSize`).
	pub fn on_disk(self) -> u64 {
		self.on_disk
	}

	/// How many bytes were written since the placeholder was last marked in sync
	/// (`ModifiedDataSize`); measured set by every write, the append-only one included.
	pub fn modified_bytes(self) -> u64 {
		self.modified_bytes
	}
}

/// A file placeholder as it was at one moment: compared before and after a hash, so a change made
/// while the hash ran is told from damaged content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileSnapshot {
	/// The file's id.
	pub id: LocalFileId,
	/// Its size and times.
	pub stamp: FileStamp,
	/// Its placeholder state.
	pub state: PlaceholderState,
}

/// What [`LocalFs::update_from_remote`] did beside the update.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Updated {
	/// The file was pinned: it was unpinned for the update and is pinned again, dehydrated.
	pub was_pinned: bool,
}

/// A hash of a file placeholder's bytes on disk, with the file as it was just before and just
/// after the hash read them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckedHash {
	/// The file before the hash.
	pub before: FileSnapshot,
	/// The BLAKE3 hash of its bytes.
	pub hash: [u8; 32],
	/// The file after the hash.
	pub after: FileSnapshot,
}

/// Why the core skips a local item it cannot sync safely.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnsupportedKind {
	/// A symbolic link: never followed.
	Symlink,
	/// A junction or mount point: never followed.
	Junction,
	/// A reparse point of another driver.
	ForeignReparsePoint,
}

/// What is at a path in the sync root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalEntry {
	/// Nothing.
	Missing,
	/// An ordinary file or directory: one the provider has not taken in yet.
	Plain { kind: LocalKind, id: LocalFileId },
	/// A placeholder, with what it stands for when its identity decodes.
	Placeholder {
		kind: LocalKind,
		id: LocalFileId,
		identity: Result<PlaceholderIdentity, IdentityError>,
		state: PlaceholderState,
	},
	/// An item the core skips.
	Unsupported(UnsupportedKind),
}

/// Why a local operation failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum LocalError {
	/// The path names nothing, or no longer the item expected.
	#[error("the item is gone")]
	NotFound,
	/// Another handle stops the operation: a writer has the file open, or the platform has the
	/// placeholder in use (`ERROR_CLOUD_FILE_IN_USE`). A later try may pass.
	#[error("the item is in use")]
	Busy,
	/// The target name is taken, or a directory to remove is not empty.
	#[error("the target exists")]
	Exists,
	/// The placeholder holds data the drive may not have: it is not in sync, or bytes were written
	/// to it since it was marked so. Only the local side may act on it.
	#[error("the placeholder was changed locally")]
	Modified,
	/// The user pinned the file to this device, and the platform refuses to dehydrate it
	/// (`ERROR_CLOUD_FILE_PINNED`).
	#[error("the file is pinned to this device")]
	Pinned,
	/// The OS refused the operation.
	#[error(transparent)]
	Os(#[from] OsError),
}

/// How the core reads and changes items in the sync root. Each call is synchronous and short apart
/// from [`LocalFs::hash`] and [`LocalFs::hydrate`]; the core runs them off the async workers.
/// Sealed, as the core relies on each call doing exactly what it says.
pub trait LocalFs: sealed::Sealed + Send + Sync + 'static {
	/// A file opened for reading that no other handle may write while it is open (shared for
	/// reading and deleting only): what the core hashes a file through and converts it under, so
	/// no write slips between the two.
	type Settled: Send + 'static;

	/// A directory opened for writing its attributes, shared with every other handle: what the core
	/// converts a directory through. A rename of the directory goes through while it is open, and
	/// the handle follows it.
	type Dir: Send + 'static;

	/// A file opened so that no other handle may open it for writing while this one is open, which
	/// reads and changes nothing of it: what an unregister holds on each file the OS's unregister
	/// removes, from its last look at the file until the OS let the root go ([`LocalFs::guard_file`]).
	/// It does not shut out a mapped view of the file, and once, with writes stuck in the platform,
	/// a guard got past a write handle held open (see [`LocalFs::guard_file`]).
	type Guard: Send + 'static;

	/// What is at `path`, read without opening the item's data, so a placeholder is not hydrated.
	fn inspect(&self, path: &RootPath) -> Result<LocalEntry, LocalError>;

	/// The children of the directory `path`, each as a path or why it cannot be one. An on-demand
	/// directory not yet populated lists no child and raises no population.
	fn list_dir(&self, path: &RootPath)
	-> Result<Vec<Result<RootPath, RootPathError>>, LocalError>;

	/// Opens the file `path` as a [`LocalFs::Settled`]: [`LocalError::Busy`] while any other
	/// handle has it open for writing. A placeholder not all on disk is fetched whole by the open
	/// itself (measured on Windows), so this is for a file whose bytes the caller reads or holds.
	fn open_settled(&self, path: &RootPath) -> Result<Self::Settled, LocalError>;

	/// The id and stamp of the file `file` holds.
	fn settled_stamp(&self, file: &Self::Settled) -> Result<(LocalFileId, FileStamp), LocalError>;

	/// The BLAKE3 hash of the file's bytes, read through `file`. Reading a placeholder's data the
	/// file does not hold asks the provider for it.
	fn hash(&self, file: &mut Self::Settled) -> Result<[u8; 32], LocalError>;

	/// Where the drive reads the file `path` from to upload it.
	fn upload_path(&self, path: &RootPath) -> PathBuf;

	/// Makes the ordinary file `file` the placeholder `identity`, marked in sync when `in_sync`.
	fn convert_file(
		&self,
		file: &Self::Settled,
		identity: PlaceholderIdentity,
		in_sync: bool,
	) -> Result<(), LocalError>;

	/// Makes the placeholder `file`, all its data on disk, an ordinary file again, its data left as
	/// it is: takes back a convert that a rename of the file, then still ordinary and announced by
	/// no callback, overtook.
	fn revert_settled(&self, file: &Self::Settled) -> Result<(), LocalError>;

	/// Where the file `file` is now, read through the handle, which follows the file through a
	/// rename: `Ok(None)` once it left the sync root.
	fn settled_path(&self, file: &Self::Settled) -> Result<Option<RootPath>, LocalError>;

	/// Gives the placeholder `file` the identity `identity`, marked in sync when `in_sync` and not
	/// in sync otherwise.
	fn update_file(
		&self,
		file: &Self::Settled,
		identity: PlaceholderIdentity,
		in_sync: bool,
	) -> Result<(), LocalError>;

	/// Marks the file placeholder at `path` in sync again, if it is still the item `expect` holding
	/// `identity`, not all on disk and with no byte written since it was last in sync: only its name
	/// or attributes changed, as a write hydrates the whole file first. Its size and times stay as
	/// the platform holds them, and its data is neither read nor fetched. Checked through the handle
	/// it is marked through, before the mark and after it: [`LocalError::Modified`] once it is all
	/// on disk or holds a written byte, left not in sync when it came all on disk during the mark,
	/// as a write the mark hid would have; [`LocalError::NotFound`] when another item or identity is
	/// there.
	fn mark_partial_in_sync(
		&self,
		path: &RootPath,
		expect: LocalFileId,
		identity: PlaceholderIdentity,
	) -> Result<(), LocalError>;

	/// Gives the placeholder at `path` the identity `identity` and marks it not in sync, whatever
	/// handles others hold on it, if it is still the item `expect`.
	fn retag_unsynced(
		&self,
		path: &RootPath,
		expect: LocalFileId,
		identity: PlaceholderIdentity,
	) -> Result<(), LocalError>;

	/// Opens the directory at `path`, if still the item `expect`, as a [`LocalFs::Dir`].
	fn open_dir(&self, path: &RootPath, expect: LocalFileId) -> Result<Self::Dir, LocalError>;

	/// Makes the ordinary directory `dir` the placeholder `identity`, in sync. Unless `populated`,
	/// the platform asks the provider for its children the next time it is listed.
	fn convert_dir(
		&self,
		dir: &Self::Dir,
		identity: PlaceholderIdentity,
		populated: bool,
	) -> Result<(), LocalError>;

	/// Makes the directory placeholder `dir` an ordinary directory again, what it holds left as it
	/// is: takes back a convert that a rename of the directory, then still ordinary and announced by
	/// no callback, overtook.
	fn revert_dir(&self, dir: &Self::Dir) -> Result<(), LocalError>;

	/// Where the directory `dir` is now, read through the handle as [`LocalFs::settled_path`] reads
	/// a file's.
	fn dir_path(&self, dir: &Self::Dir) -> Result<Option<RootPath>, LocalError>;

	/// Gives the directory placeholder `dir` the identity `identity` and marks it in sync, as
	/// [`LocalFs::update_dir`] does through a handle of its own.
	fn update_open_dir(
		&self,
		dir: &Self::Dir,
		identity: PlaceholderIdentity,
	) -> Result<(), LocalError>;

	/// Marks the directory placeholder at `path`, if still the item `expect`, in sync.
	fn set_dir_in_sync(&self, path: &RootPath, expect: LocalFileId) -> Result<(), LocalError>;

	/// Marks the directory placeholder at `path`, if still the item `expect`, populated, its
	/// identity `identity` kept: the platform no longer asks the provider for its children, as after
	/// it populated the directory itself. What the directory holds and its in-sync state stay as
	/// they are. For a directory whose children the provider placed itself.
	fn set_dir_populated(
		&self,
		path: &RootPath,
		expect: LocalFileId,
		identity: PlaceholderIdentity,
	) -> Result<(), LocalError>;

	/// Fetches every byte of the file placeholder at `path` the file does not hold yet, if it is
	/// still the item `expect`.
	fn hydrate(&self, path: &RootPath, expect: LocalFileId) -> Result<(), LocalError>;

	/// The BLAKE3 hash of the bytes on disk of the file placeholder at `path`, if it is still the
	/// item `expect`, read through a handle that refuses no other, so a user's writer is never
	/// held up, with the file as it was just before and after. Only for a file all on disk, as
	/// reading it then asks the provider for nothing: one that is not when the hash starts (a
	/// dehydration got there first) is [`LocalError::NotFound`]. Ends with [`LocalError::Busy`]
	/// soon after `stop` is set.
	fn hash_checked(
		&self,
		path: &RootPath,
		expect: LocalFileId,
		stop: &AtomicBool,
	) -> Result<CheckedHash, LocalError>;

	/// Removes the bytes on disk of the file placeholder at `path`, if it is still the clean
	/// placeholder `expect`, checked in the same call: refused with [`LocalError::Modified`] when
	/// it is not clean and [`LocalError::Busy`] while another process holds the file open. Never
	/// removes bytes the drive may not have, and never fetches the bytes a file partly on disk
	/// lacks. A pinned file is refused with [`LocalError::Pinned`], or with [`IfPinned::Override`]
	/// unpinned for the call and pinned again after it.
	fn dehydrate_clean(
		&self,
		path: &RootPath,
		expect: PlaceholderIdentity,
		if_pinned: IfPinned,
	) -> Result<(), LocalError>;

	/// Creates the placeholder `spec` in the directory `parent`, not yet hydrated and in sync.
	fn create_placeholder(
		&self,
		parent: &RootPath,
		spec: &PlaceholderSpec,
	) -> Result<(), LocalError>;

	/// Renames `from` to `to`, refusing with [`LocalError::Exists`] when `to` is taken. The
	/// provider's own rename raises no rename notification.
	fn rename_no_replace(&self, from: &RootPath, to: &RootPath) -> Result<(), LocalError>;

	/// Makes the file placeholder at `path`, if it is still the clean placeholder `expect`, the
	/// placeholder `to` stands for: its identity, size and times, with no data on disk, in sync.
	/// Clean is in sync with no byte written since; anything else is refused with
	/// [`LocalError::Modified`] and left as it is, checked in the same call that changes it. Its
	/// data is neither read nor fetched: one with no byte on disk only gets its new identity. Refused
	/// with [`LocalError::Busy`] while another handle has the file open for writing, as a writer
	/// that opened it before would write into the new version unseen, or refuses its delete, and
	/// while any other handle or mapped view holds a file with bytes on disk, as dehydrating it
	/// under a reader would have the reader's later reads return the new version's bytes after the
	/// old one's. A pinned file, which the platform refuses to dehydrate, is unpinned for the
	/// update and pinned again after it, a refused update included; the result says it was, so the
	/// core hydrates the new version.
	fn update_from_remote(
		&self,
		path: &RootPath,
		expect: PlaceholderIdentity,
		to: &PlaceholderSpec,
	) -> Result<Updated, LocalError>;

	/// Gives the placeholder at `path`, if it is still the item `expect` and its own pin is still
	/// `from`, the pin `to` of its own, with no other change: its data and its in-sync state stay
	/// as they are. Says whether it did: a pin the user changed meanwhile stays. The pin is read
	/// and set through one handle, a few system calls apart, as the platform has no call that
	/// compares and sets it at once.
	fn swap_pin(
		&self,
		path: &RootPath,
		expect: LocalFileId,
		from: PinAttr,
		to: PinAttr,
	) -> Result<bool, LocalError>;

	/// Removes the clean placeholder `expect` at `path`: a file in sync with no byte written since,
	/// or an empty directory in sync ([`LocalError::Exists`] when it holds anything). Refused with
	/// [`LocalError::Modified`] when it is not clean, and with [`LocalError::Busy`] while another
	/// handle refuses its delete. Raises no delete notification.
	fn remove_clean(&self, path: &RootPath, expect: PlaceholderIdentity) -> Result<(), LocalError>;

	/// Renames the clean placeholder `expect` from `from` to `to`, refusing with
	/// [`LocalError::Exists`] when `to` is taken, [`LocalError::Modified`] when it is not clean and
	/// [`LocalError::Busy`] while another handle refuses its rename. It stays in sync. Raises no
	/// rename notification.
	fn rename_clean(
		&self,
		from: &RootPath,
		to: &RootPath,
		expect: PlaceholderIdentity,
	) -> Result<(), LocalError>;

	/// Makes the placeholder at `path`, if it is still the item `expect`, an ordinary file or
	/// directory, its data left as it is: unregistering the root then cannot touch it.
	fn revert_to_plain(&self, path: &RootPath, expect: LocalFileId) -> Result<(), LocalError>;

	/// The bytes of the file at `path`, an ordinary file or a placeholder, for reading at any offset,
	/// if it is still the item `expect` and all its data is on disk: [`LocalError::NotFound`] when
	/// the file is another item or not all on disk. Opened through a handle that refuses no other
	/// and checked through it, so a user's writer is never held up and the read asks the provider
	/// for nothing. A dehydration that lands after the check is not stopped: a read past it then
	/// asks the provider for the bytes, as any reader would.
	fn open_on_disk(
		&self,
		path: &RootPath,
		expect: LocalFileId,
	) -> Result<Box<dyn ByteSource>, LocalError>;

	/// Copies the bytes of the file at `path`, if it is still the item `expect` and all its data is
	/// on disk, into a new ordinary file at `to`: [`LocalError::Exists`] when anything is at `to`,
	/// [`LocalError::NotFound`] when the file is another item or not all on disk. Never asks the
	/// provider for data. A copy cut short is removed.
	fn copy_to_plain(
		&self,
		path: &RootPath,
		expect: LocalFileId,
		to: &RootPath,
	) -> Result<(), LocalError>;

	/// Opens the file at `path`, if it is still the item `expect`, as a [`LocalFs::Guard`]: until
	/// the guard drops, every other open of the file for writing fails with a sharing violation, a
	/// placeholder's data is neither read nor fetched, and the file can still be renamed and
	/// deleted. [`LocalError::Busy`] while another handle has the file open for writing, which no
	/// guard can shut out, or open without sharing deleting, which would leave the file to the OS's
	/// unregister as a placeholder no process can read; a reader that shares deleting does not stop
	/// it. [`LocalError::NotFound`] when another item or a directory is at `path`, or
	/// [`LocalError::Busy`] for a directory another handle's share mode refuses the open. Each open
	/// is one system call the platform may hold up, with no bound (one was measured at about 60 s
	/// with writes stuck in the platform).
	///
	/// Measured on Windows 11 ARM64 (in the VM, about two million attempts by writers that open,
	/// write and close in a loop, then single writes): no write landed while the guard was open,
	/// through the root's disconnect and the OS's unregister, an append to a file with no byte on
	/// disk included; a write handle opened before refused the guard; a file placeholder not all on
	/// disk that the guard holds stays, delete pending, until the guard drops, and one all on disk
	/// is made ordinary by the OS's unregister with the guard open. A mapped view of the file is not
	/// shut out. Not explained: once, with writes stuck in the platform, a guard opened for reading
	/// in share checks got past a write handle held open, whose writes were all failing (not run
	/// against the guard's `DELETE` access used now).
	fn guard_file(&self, path: &RootPath, expect: LocalFileId) -> Result<Self::Guard, LocalError>;

	/// The file `guard` holds, as it is now, read through it: its id, its size and times, and its
	/// placeholder state, which the platform reports through the guard as it does connected even
	/// once the root is disconnected (measured on Windows), apart from whether it is partial: that
	/// is read from its attributes, which a disconnected root disguises. [`LocalError::NotFound`]
	/// once it is no placeholder.
	fn guarded_snapshot(&self, guard: &Self::Guard) -> Result<FileSnapshot, LocalError>;

	/// Gives the directory placeholder at `path`, if still the item `expect`, the identity
	/// `identity`, and marks it in sync.
	fn update_dir(
		&self,
		path: &RootPath,
		expect: LocalFileId,
		identity: PlaceholderIdentity,
	) -> Result<(), LocalError>;

	/// Tells the windows showing the item at `path` that it changed, so they ask for its thumbnail
	/// again: what the thumbnail service does once it finished one it answered "not yet" for. Only
	/// a hint: nothing is read or changed, and a failure goes unnoticed.
	fn notify_item_updated(&self, path: &RootPath);
}

/// A `NOTIFY_DELETE` callback: a placeholder is about to be deleted, and the core allows it or
/// vetoes it.
#[derive(Debug)]
pub struct DeleteRequest {
	identity: IdentityBlob,
	path: RootPath,
	is_directory: bool,
	undelete: bool,
}

impl DeleteRequest {
	/// The request to delete the placeholder `identity` at `path`; `undelete` when the platform
	/// takes back a delete it announced (the recycle bin does so before moving the item out).
	pub fn new(identity: IdentityBlob, path: RootPath, is_directory: bool, undelete: bool) -> Self {
		Self {
			identity,
			path,
			is_directory,
			undelete,
		}
	}

	/// The placeholder's identity, unparsed.
	pub fn identity(&self) -> &IdentityBlob {
		&self.identity
	}

	/// Where the placeholder is.
	pub fn path(&self) -> &RootPath {
		&self.path
	}

	/// Whether it is a directory.
	pub fn is_directory(&self) -> bool {
		self.is_directory
	}

	/// Whether the platform takes back a delete rather than announcing one.
	pub fn undelete(&self) -> bool {
		self.undelete
	}
}

/// Why the platform asks to dehydrate a placeholder (`CF_CALLBACK_DEHYDRATION_REASON`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DehydrateReason {
	/// No reason given.
	None,
	/// A user or an application asked for it, as `CfDehydratePlaceholder` does.
	UserManual,
	/// The volume runs low on space.
	SystemLowSpace,
	/// The file was not used for a while.
	SystemInactivity,
	/// An upgrade of Windows.
	SystemOsUpgrade,
	/// A value this build does not know, as sent.
	Other(i32),
}

/// A `NOTIFY_DEHYDRATE` callback: another process asks to remove a file placeholder's bytes from
/// the disk, and the core allows it or vetoes it. Measured (Windows 11, in the VM): once
/// allowed, the request waits until the provider dehydrates the file itself.
#[derive(Debug)]
pub struct DehydrateRequest {
	identity: IdentityBlob,
	path: RootPath,
	background: bool,
	reason: DehydrateReason,
}

impl DehydrateRequest {
	/// The request to dehydrate the placeholder `identity` at `path` for `reason`; `background`
	/// when the platform asks on its own rather than for a waiting caller.
	pub fn new(
		identity: IdentityBlob,
		path: RootPath,
		background: bool,
		reason: DehydrateReason,
	) -> Self {
		Self {
			identity,
			path,
			background,
			reason,
		}
	}

	/// The placeholder's identity, unparsed.
	pub fn identity(&self) -> &IdentityBlob {
		&self.identity
	}

	/// Where the placeholder is.
	pub fn path(&self) -> &RootPath {
		&self.path
	}

	/// Whether the platform asks on its own (`CF_CALLBACK_DEHYDRATE_FLAG_BACKGROUND`).
	pub fn background(&self) -> bool {
		self.background
	}

	/// Why it asks.
	pub fn reason(&self) -> DehydrateReason {
		self.reason
	}
}

/// Where a renamed placeholder goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenameTarget {
	/// Another path in the sync root.
	InRoot(RootPath),
	/// Out of the sync root: another folder, another volume or the recycle bin.
	OutOfRoot,
}

/// A `NOTIFY_RENAME` callback: a placeholder is about to be renamed or moved, and the core allows
/// it or vetoes it.
#[derive(Debug)]
pub struct RenameRequest {
	identity: IdentityBlob,
	from: RootPath,
	target: RenameTarget,
	is_directory: bool,
}

impl RenameRequest {
	/// The request to move the placeholder `identity` from `from` to `target`.
	pub fn new(
		identity: IdentityBlob,
		from: RootPath,
		target: RenameTarget,
		is_directory: bool,
	) -> Self {
		Self {
			identity,
			from,
			target,
			is_directory,
		}
	}

	/// The placeholder's identity, unparsed.
	pub fn identity(&self) -> &IdentityBlob {
		&self.identity
	}

	/// Where the placeholder is.
	pub fn from(&self) -> &RootPath {
		&self.from
	}

	/// Where it goes.
	pub fn target(&self) -> &RenameTarget {
		&self.target
	}

	/// Whether it is a directory.
	pub fn is_directory(&self) -> bool {
		self.is_directory
	}
}

/// Answers one [`DeleteRequest`]. Dropping it unanswered vetoes the delete with
/// [`Failure::Transient`].
pub trait DeleteResponder: sealed::Sealed + Send + 'static {
	/// Lets the delete go ahead.
	fn allow(self);

	/// Stops the delete; the application sees `failure`'s status.
	fn veto(self, failure: Failure);
}

/// Answers one [`RenameRequest`]. Dropping it unanswered vetoes the rename with
/// [`Failure::Transient`].
pub trait RenameResponder: sealed::Sealed + Send + 'static {
	/// Lets the rename go ahead.
	fn allow(self);

	/// Stops the rename; the application sees `failure`'s status.
	fn veto(self, failure: Failure);
}

/// Answers one [`DehydrateRequest`]. Dropping it unanswered vetoes the dehydration.
pub trait DehydrateResponder: sealed::Sealed + Send + 'static {
	/// Lets the dehydration go ahead: the core then dehydrates the file itself, which is what ends
	/// the caller's wait.
	fn allow(self);

	/// Stops the dehydration; the file keeps its bytes.
	fn veto(self);
}

/// A local operation the platform reports finished.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Completed {
	/// The placeholder at the path was deleted (`NOTIFY_FILE_CLOSE_COMPLETION` with its deleted
	/// flag: neither `NOTIFY_DELETE` nor `NOTIFY_DELETE_COMPLETION` proves a delete, as the recycle
	/// bin takes its delete back).
	Deleted(RootPath),
	/// The placeholder moved from `from` to `to` (`NOTIFY_RENAME_COMPLETION`, which comes only for
	/// a target inside the root).
	Renamed { from: RootPath, to: RootPath },
}
