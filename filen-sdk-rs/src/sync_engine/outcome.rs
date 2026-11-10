//! The public shape of a pass: what one WOULD do ([`PlanOutcome`], from
//! [`SyncEngine::plan_pair`](super::SyncEngine::plan_pair)) and what one DID
//! ([`SyncReport`](super::SyncReport)).
//!
//! Both describe the same things with the same types, so a caller can render a dry run and the
//! pass that follows through one code path. The engine's internal action type
//! ([`SyncAction`](super::plan::SyncAction)) and its `NodeKind` stay private — they carry uuids and
//! reconciler bookkeeping that is not API — and are mapped onto the types here at the boundary.
//!
//! [`Display`](std::fmt::Display) renders every type as the one-liner a CLI would print, so a
//! caller that only wants text still gets it without matching the enums.

use std::fmt;

use filen_macros::js_type;

use super::{
	ConflictResolution, SyncMode,
	baseline::NodeKind,
	guard::GuardReason,
	ignore::IgnoredPath,
	plan::{RemoteNode, SyncAction},
	scan::LocalNode,
	side::NodesAt,
};
use crate::ErrorKind;

/// Whether a planned action's item is a directory or a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[js_type(export, no_deser, no_default, napi)]
pub enum PlannedNodeKind {
	Dir,
	File,
}

impl From<NodeKind> for PlannedNodeKind {
	fn from(kind: NodeKind) -> Self {
		match kind {
			NodeKind::Dir => Self::Dir,
			NodeKind::File => Self::File,
		}
	}
}

impl fmt::Display for PlannedNodeKind {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(match self {
			Self::Dir => "dir",
			Self::File => "file",
		})
	}
}

/// What a planned action would do.
#[derive(Debug, Clone, PartialEq, Eq)]
#[js_type(export, no_deser, no_default, tagged, napi)]
pub enum PlannedActionKind {
	/// Push a local file's content to the remote.
	UploadFile,
	/// Pull a remote file's content into the local tree.
	DownloadFile,
	CreateRemoteDir,
	CreateLocalDir,
	/// Re-parent/rename the remote item to `to` instead of re-uploading its content.
	MoveRemote {
		to: String,
	},
	/// Rename the local file to `to` instead of re-downloading it.
	MoveLocal {
		to: String,
	},
	/// Send the remote item to the Filen trash (recoverable).
	TrashRemote,
	/// Move the local item into the pair's `.filen-sync-trash/` quarantine dir, under the bucket of
	/// the UTC day it was moved (`.filen-sync-trash/YYYY-MM-DD/<path>`). A local deletion destroys
	/// nothing at once: the bytes stay recoverable inside the sync root for 30 days, after which
	/// the engine deletes that day's bucket permanently.
	DeleteLocal,
	/// Record a path that is already identical on both sides into the baseline. Transfers nothing;
	/// it exists so a later one-sided change at that path is classified correctly.
	AdoptBaseline,
}

impl PlannedActionKind {
	/// Whether the action deletes an item: what the mass-delete guard counts.
	pub(crate) fn is_deletion(&self) -> bool {
		match self {
			Self::TrashRemote | Self::DeleteLocal => true,
			Self::UploadFile
			| Self::DownloadFile
			| Self::CreateRemoteDir
			| Self::CreateLocalDir
			| Self::MoveRemote { .. }
			| Self::MoveLocal { .. }
			| Self::AdoptBaseline => false,
		}
	}
}

/// What a planned upload or download does at the path it writes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[js_type(export, no_deser, no_default, napi)]
pub enum PlannedTransfer {
	/// The side it writes to holds no file at the path: the transfer writes a new one.
	Create,
	/// The side it writes to holds a file at the path, which the transfer replaces.
	Replace,
}

/// One action a pass would apply (or a held one it would not), in apply order.
#[derive(Debug, Clone, PartialEq, Eq)]
#[js_type(export, no_deser, no_default, napi)]
pub struct PlannedAction {
	pub kind: PlannedActionKind,
	/// The item's path relative to both roots. For a move this is the SOURCE path; the destination
	/// is in [`PlannedActionKind::MoveRemote`] / [`PlannedActionKind::MoveLocal`].
	pub rel_path: String,
	pub node: PlannedNodeKind,
	/// The file's size in bytes where the pass knows it (a transfer, a move of a tracked file);
	/// `None` for directories and for anything whose size neither side reported.
	pub size: Option<u64>,
	/// For an upload or a download, whether it writes a new file at `rel_path` or replaces the file
	/// the side it writes to holds there, as the pass read that side; `None` for every other action.
	/// A directory at the path is no file to replace: a type flip removes it with a deletion of its
	/// own, planned ahead of the transfer, which then creates.
	pub transfer: Option<PlannedTransfer>,
}

impl fmt::Display for PlannedAction {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		let path = &self.rel_path;
		match &self.kind {
			PlannedActionKind::UploadFile => write!(f, "upload file {path:?}"),
			PlannedActionKind::DownloadFile => write!(f, "download file {path:?}"),
			PlannedActionKind::CreateRemoteDir => write!(f, "create remote dir {path:?}"),
			PlannedActionKind::CreateLocalDir => write!(f, "create local dir {path:?}"),
			PlannedActionKind::MoveRemote { to } => write!(f, "move remote {path:?} -> {to:?}"),
			PlannedActionKind::MoveLocal { to } => write!(f, "move local {path:?} -> {to:?}"),
			PlannedActionKind::TrashRemote => write!(f, "trash remote {} {path:?}", self.node),
			PlannedActionKind::DeleteLocal => {
				write!(f, "delete local {} {path:?} (to quarantine)", self.node)
			}
			PlannedActionKind::AdoptBaseline => write!(f, "adopt baseline {path:?}"),
		}
	}
}

/// Why a path is held as a conflict, and so which resolutions can clear it
/// ([`resolutions`](Self::resolutions)). Worked out from the pair's CURRENT mode and the kinds the
/// two sides hold at the path, so a conflict still held across a mode switch is reported under the
/// new mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[js_type(import, export, no_default, napi)]
pub enum ConflictReason {
	/// The two sides diverged from what they last agreed on: in two-way both changed (two edits, an
	/// edit against a deletion, two items of different kinds), or, in any mode, this engine's
	/// upload buried another client's edit, or a local edit sits over a remote version that does
	/// not match its hash. Every resolution clears it.
	Diverged,
	/// [`LocalBackup`](SyncMode::LocalBackup): the remote keeps a directory where the local side
	/// holds a file. A backup never deletes on its destination, whichever side made the kinds
	/// differ: the local item changed kind, another client replaced the remote one, or a first sync
	/// or a mode switch met a remote that already held the other kind. Keeping the remote version
	/// moves the local file to `.filen-sync-trash`, and keeping both renames it aside, where it is
	/// backed up; keeping the local version is refused.
	RemoteKeepsDir,
	/// [`LocalBackup`](SyncMode::LocalBackup): the remote keeps a file where the local side holds a
	/// directory, whichever side made the kinds differ (see [`RemoteKeepsDir`](Self::RemoteKeepsDir)).
	/// Only keeping both, which renames the local directory aside to be backed up under that name,
	/// clears it.
	RemoteKeepsFile,
	/// [`RemoteBackup`](SyncMode::RemoteBackup): the local backup keeps a directory where the remote
	/// holds a file, whichever side made the kinds differ: the remote item changed kind, the local
	/// one was replaced, or a first sync or a mode switch met a local tree that already held the
	/// other kind. Only keeping both, which renames the local directory aside and then pulls the
	/// remote file, clears it.
	LocalKeepsDir,
	/// [`RemoteBackup`](SyncMode::RemoteBackup): the local backup keeps a file where the remote holds
	/// a directory, whichever side made the kinds differ (see [`LocalKeepsDir`](Self::LocalKeepsDir)).
	/// Keeping the remote version moves the local file to `.filen-sync-trash`, and keeping both
	/// renames it aside; either way the remote directory is then pulled. Keeping the local version is
	/// refused.
	LocalKeepsFile,
}

impl ConflictReason {
	/// The reason a conflict is held for in `mode`, where the two sides hold `local` and `remote`.
	pub(super) fn of(
		mode: SyncMode,
		local: Option<PlannedNodeKind>,
		remote: Option<PlannedNodeKind>,
	) -> Self {
		let (Some(local), Some(remote)) = (local, remote) else {
			return Self::Diverged;
		};
		if local == remote {
			return Self::Diverged;
		}
		match mode {
			SyncMode::LocalBackup => match remote {
				PlannedNodeKind::Dir => Self::RemoteKeepsDir,
				PlannedNodeKind::File => Self::RemoteKeepsFile,
			},
			SyncMode::RemoteBackup => match local {
				PlannedNodeKind::Dir => Self::LocalKeepsDir,
				PlannedNodeKind::File => Self::LocalKeepsFile,
			},
			// These modes delete on their destination, so a resolution replaces the item.
			SyncMode::TwoWay | SyncMode::LocalToRemote | SyncMode::RemoteToLocal => Self::Diverged,
		}
	}

	/// The resolutions that can clear a conflict held for this reason, in the order
	/// [`KeepLocal`](ConflictResolution::KeepLocal), [`KeepRemote`](ConflictResolution::KeepRemote),
	/// [`KeepBoth`](ConflictResolution::KeepBoth).
	/// [`SyncEngine::resolve_conflict`](super::SyncEngine::resolve_conflict) refuses any other with
	/// [`ResolutionRefused`](super::ResolutionRefused).
	///
	/// A backup mode never deletes on its destination, so only a resolution that empties the local
	/// path clears its kind mismatch: one that leaves both items in place would have the next pass
	/// hold the same conflict again.
	pub fn resolutions(self) -> &'static [ConflictResolution] {
		match self {
			Self::Diverged => &[
				ConflictResolution::KeepLocal,
				ConflictResolution::KeepRemote,
				ConflictResolution::KeepBoth,
			],
			// The local item is a file: keeping the remote moves it to the bin, keeping both renames
			// it aside.
			Self::RemoteKeepsDir | Self::LocalKeepsFile => {
				&[ConflictResolution::KeepRemote, ConflictResolution::KeepBoth]
			}
			// The local item is a directory: only keeping both moves it off the path.
			Self::RemoteKeepsFile | Self::LocalKeepsDir => &[ConflictResolution::KeepBoth],
		}
	}
}

impl fmt::Display for ConflictReason {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(match self {
			Self::Diverged => "both sides changed since the last sync",
			Self::RemoteKeepsDir => {
				"the local side holds a file where the remote backup keeps a directory"
			}
			Self::RemoteKeepsFile => {
				"the local side holds a directory where the remote backup keeps a file"
			}
			Self::LocalKeepsDir => {
				"the remote side holds a file where the local backup keeps a directory"
			}
			Self::LocalKeepsFile => {
				"the remote side holds a directory where the local backup keeps a file"
			}
		})
	}
}

/// A path held as a conflict until
/// [`SyncEngine::resolve_conflict`](super::SyncEngine::resolve_conflict) picks a winner (or that a
/// dry run would hold): in two-way both sides changed and diverged; in a backup mode the two sides
/// hold items of different kinds, whichever side made them so. The path and its subtree are
/// excluded from planning while it is held; [`reason`](Self::reason) says which.
#[derive(Debug, Clone, PartialEq, Eq)]
#[js_type(export, no_deser, no_default, napi)]
pub struct PlannedConflict {
	pub rel_path: String,
	/// What the local side holds, or `None` when the path is GONE locally — a delete-vs-modify
	/// divergence rather than two competing edits.
	pub local: Option<PlannedNodeKind>,
	/// What the remote side holds, `None` when the remote deleted it.
	pub remote: Option<PlannedNodeKind>,
	/// Why the path is held, from the pair's mode and the two kinds above when it was reported;
	/// which resolutions clear it is [`ConflictReason::resolutions`].
	pub reason: ConflictReason,
}

impl PlannedConflict {
	/// The conflict held at `rel_path` in `mode`, where the two sides hold `local` and `remote`.
	pub(super) fn held(
		mode: SyncMode,
		rel_path: String,
		local: Option<PlannedNodeKind>,
		remote: Option<PlannedNodeKind>,
	) -> Self {
		Self {
			rel_path,
			local,
			remote,
			reason: ConflictReason::of(mode, local, remote),
		}
	}

	fn side(kind: Option<PlannedNodeKind>) -> String {
		kind.map_or_else(|| "absent".to_string(), |kind| kind.to_string())
	}
}

impl fmt::Display for PlannedConflict {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(
			f,
			"conflict {:?} (local {} vs remote {}",
			self.rel_path,
			Self::side(self.local),
			Self::side(self.remote)
		)?;
		match self.reason {
			ConflictReason::Diverged => f.write_str(")"),
			ConflictReason::RemoteKeepsDir
			| ConflictReason::RemoteKeepsFile
			| ConflictReason::LocalKeepsDir
			| ConflictReason::LocalKeepsFile => write!(f, "; {})", self.reason),
		}
	}
}

/// Why a path cannot be synced at all, so the engine stopped planning it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[js_type(export, no_deser, no_default, tagged, napi)]
pub enum UnsyncableReason {
	/// The local name is one the Filen backend rejects (a trailing dot or space, a reserved device
	/// name, a forbidden character, an over-long name). No upload or remote create at this path can
	/// ever succeed, so the path and its subtree are skipped rather than retried forever. `detail`
	/// is the SDK name validator's own message.
	///
	/// A name the remote already holds at that very path (a legacy client wrote it, and the local
	/// file system held it) is not one of these: the path syncs like any other. Only a content edit
	/// of such a file cannot go up, since the SDK never writes a name its validator rejects; that
	/// upload fails as a failure of its own path.
	InvalidName { detail: String },
	/// Every attempt to apply this path failed, `attempts` times in a row. The engine stopped
	/// planning it so one broken path cannot stall (or spam) every pass; the count is cleared by a
	/// success or by [`SyncEngine::retry_path`](super::SyncEngine::retry_path). The engine also
	/// tries the path once more on its own every
	/// [`PATH_FAILURE_RETRY_INTERVAL`](super::PATH_FAILURE_RETRY_INTERVAL), so a path that was only
	/// broken for a while (a file another process held locked) syncs again without a retry call.
	RepeatedFailure { attempts: u32, last_error: String },
	/// The remote item's metadata could not be decrypted or decoded (only a non-conforming client
	/// writes such an item), so nothing is known about it but that it exists. It is never pulled,
	/// and its absence from the remote view is NOT a deletion: a synced path whose item this became
	/// keeps its local copy, and nothing is pushed over it or under it, for as long as it stays so.
	/// For an item that was never synced, `rel_path` is the directory holding it (`""` for the sync
	/// root).
	RemoteUndecodable,
	/// The remote name cannot be used as a local path component: it is empty, `.` or `..`, or holds
	/// a path separator or NUL, so pulling it could write outside the sync root. Handled like
	/// [`RemoteUndecodable`](Self::RemoteUndecodable): a synced path whose item was renamed to such a
	/// name is left alone on both sides rather than read as deleted. `name` is the remote name.
	RemoteInvalidName { name: String },
	/// This device's file system cannot hold the remote name under its exact spelling: on Windows a
	/// trailing dot or space (which Win32 strips, so the item would land under another name), a
	/// character Windows forbids, or a device name such as `CON` or `NUL.txt`; anywhere, a name
	/// longer than the file system holds. Handled like [`RemoteInvalidName`](Self::RemoteInvalidName):
	/// it is never downloaded, and a synced path whose item was renamed to such a name keeps its local
	/// copy and is left alone on both sides, so the remote item is never moved or trashed because of
	/// it. Renaming it on the remote resolves it. `name` is the remote name, `detail` says why.
	RemoteNameNotStorable { name: String, detail: String },
	/// The remote item's parent chain never reaches the sync root: its parent is missing from the
	/// listing, or the chain loops. Handled like [`RemoteUndecodable`](Self::RemoteUndecodable); for
	/// an item that was never synced, `rel_path` is its bare name, since it has no path.
	RemoteBrokenParent,
	/// A local symlink to a directory inside the sync root. The directory is synced under its real
	/// path, `target`; the link is left alone on both sides, so neither a copy synced at the link's
	/// path before nor anything the remote holds there is deleted, and nothing is written through
	/// the link.
	LocalAlias { target: String },
	/// Another local item in the same directory has the same name once case and Unicode form are
	/// folded (`Report.txt` and `report.txt`, or the NFC and NFD spellings of one name), so the two
	/// cannot be told apart on the remote, whose names are unique case-insensitively. Every item of
	/// the group is reported and left alone on both sides, subtree and all: nothing is pushed or
	/// pulled at any of their paths, nothing there is deleted, and neither is a directory holding
	/// them. Renaming one of them resolves it, and the next pass syncs both. The NFC and NFD
	/// spellings of one name share one path, so such a pair is reported once, though both items are
	/// left alone.
	LocalNameCollision,
	/// Another remote item in the same directory has the same name once case is folded (only a
	/// non-conforming client can write such a pair, as the server's own name dedup is
	/// case-insensitive), so the two cannot be told apart on a local disk. Handled like
	/// [`LocalNameCollision`](Self::LocalNameCollision): every item of the group is left alone on
	/// both sides until one of them is renamed.
	RemoteNameCollision,
	/// The remote file's content does not hash to the BLAKE3 its metadata declares — its stored
	/// data or its metadata is not what the client that uploaded it wrote — so its download
	/// failed. Reported from the pass whose download found it on, for as long as the remote file
	/// at this path is that same version: the engine downloads nothing here and leaves the local
	/// copy (if any) exactly where it is. A new version of the remote file, or its deletion or move
	/// away, ends it and the path is planned as usual, at the first pass after the cache announces
	/// the change. A folder above the file going to the remote trash takes the file with it, as any
	/// deletion of it would: the skip ends, and the local copy goes to the bin with the folder.
	///
	/// [`SyncMode::LocalToRemote`](super::SyncMode::LocalToRemote) and
	/// [`SyncMode::LocalBackup`](super::SyncMode::LocalBackup) never download, so they do not skip
	/// the path: they plan it as usual, uploading over the remote file, and upload a local file that
	/// hashes to what the version declares too, where the two sides would otherwise read as
	/// agreeing. They do so on an account that keeps no old versions as well, which loses that
	/// version: those modes make the local side the one that counts.
	///
	/// In [`SyncMode::TwoWay`](super::SyncMode::TwoWay), a local file whose content changed since the
	/// last sync ends it too. While the account keeps old versions the upload replaces the damaged
	/// version, which stays in the file's history. With versioning off the path is held as a conflict
	/// for [`SyncEngine::resolve_conflict`](super::SyncEngine::resolve_conflict) instead: a mismatch
	/// may be wrong metadata over good bytes, so that version may be another device's real edit, and
	/// the upload would lose it for good. While the setting cannot be read neither happens: the path
	/// stays skipped and reported here, and the next pass asks again. A local copy no pass ever synced,
	/// a file deleted locally and a directory at the path are not such an edit and leave the skip in
	/// place.
	/// [`SyncEngine::retry_path`](super::SyncEngine::retry_path) ends it at once.
	///
	/// `last_error` is the last failure's own error: the download's, naming the version and both
	/// hashes, unless an upload over the version has failed since.
	RemoteContentHashMismatch { last_error: String },
}

impl fmt::Display for UnsyncableReason {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::LocalAlias { target } => {
				write!(
					f,
					"a symlink to {target:?}, which is synced under that path"
				)
			}
			Self::InvalidName { detail } => {
				write!(f, "the remote would reject this name: {detail}")
			}
			Self::RemoteUndecodable => {
				f.write_str("the remote item's metadata could not be decrypted or decoded")
			}
			Self::RemoteInvalidName { name } => {
				write!(f, "the remote name {name:?} is not a usable local name")
			}
			Self::RemoteNameNotStorable { name, detail } => {
				write!(
					f,
					"the remote name {name:?} cannot be stored on this device: {detail}"
				)
			}
			Self::RemoteBrokenParent => {
				f.write_str("the remote item's parent chain does not lead back to the sync root")
			}
			Self::RepeatedFailure {
				attempts,
				last_error,
			} => write!(
				f,
				"failed {attempts} time(s) in a row; last error: {last_error}"
			),
			Self::LocalNameCollision => f.write_str(
				"another local item's name is the same once case and Unicode form are folded",
			),
			Self::RemoteNameCollision => {
				f.write_str("another remote item's name is the same once case is folded")
			}
			Self::RemoteContentHashMismatch { last_error } => write!(
				f,
				"the remote file's content does not match its hash, so it is skipped until it \
				 changes: {last_error}"
			),
		}
	}
}

/// One path a pass reported as unsyncable, reported once per pass for as long as it stays so.
#[derive(Debug, Clone, PartialEq, Eq)]
#[js_type(export, no_deser, no_default, napi)]
pub struct UnsyncablePath {
	pub rel_path: String,
	pub reason: UnsyncableReason,
}

impl fmt::Display for UnsyncablePath {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "unsyncable {:?}: {}", self.rel_path, self.reason)
	}
}

/// Why the engine refused to run a pass at all.
///
/// No pass refuses any more: a name collision leaves only the colliding paths alone, each reported
/// as [`UnsyncableReason::LocalNameCollision`] or [`UnsyncableReason::RemoteNameCollision`], and
/// the rest of the pair syncs. The type stays part of the report, and a switch that adopts the
/// destination ([`Backlog::AdoptDestination`](super::Backlog::AdoptDestination)) still names one
/// of these when it will not adopt across a collision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[js_type(export, no_deser, no_default, napi)]
pub enum RefuseReason {
	/// Two remote items resolve to the same case-insensitive path.
	RemoteCollision,
	/// Two local items normalize to the same path.
	LocalCollision,
}

impl fmt::Display for RefuseReason {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(match self {
			Self::RemoteCollision => "two remote items resolve to the same path (case-insensitive)",
			Self::LocalCollision => "two local items normalize to the same path",
		})
	}
}

/// Why a pass stopped starting some of its work part-way: a side that ran out of room, or a remote
/// it could not reach or was refused by. No action that runs into one can succeed until it clears,
/// so the pass holds those actions back, and the failure counts against no path's failure streak
/// (see [`SyncReport::halted`](super::SyncReport::halted)).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[js_type(export, no_deser, no_default, napi)]
pub enum HaltReason {
	/// The local disk has no room for another write (or a disk quota on it is used up).
	LocalStorageFull,
	/// The account's storage is used up: the server refused a write with `max_storage_reached`.
	RemoteStorageFull,
	/// The remote could not be reached or turned the request away for now: the connection failed,
	/// was lost or timed out, or the server answered with a 5xx status, 408 or 429 — while that
	/// server was not shown answering anything else. A server that answers other requests while
	/// it turns one away halts nothing: that is a failure of the request's own path. The kind of
	/// error is [`ErrorKind::Reqwest`], so a host can show the pair as offline.
	RemoteUnavailable,
	/// The server no longer accepts the client's session: an API key it does not know, or a 401.
	/// The kind of error is [`ErrorKind::Unauthenticated`], so a host can ask the user to sign in
	/// again.
	Unauthenticated,
}

impl HaltReason {
	/// The actions this condition holds back: `"uploads"` for a full account, `"downloads"` for a
	/// full local disk, and `"remote actions"` (transfers, remote directory creates, moves and
	/// trashes) for a remote out of reach or refusing the session.
	pub fn held_transfers(self) -> &'static str {
		match self {
			Self::LocalStorageFull => "downloads",
			Self::RemoteStorageFull => "uploads",
			Self::RemoteUnavailable | Self::Unauthenticated => "remote actions",
		}
	}

	/// The kind of error this condition is: the server's refusal of a write to a full account, the
	/// IO error of a write to a full disk, the transport's error, or the server's refusal of the
	/// session.
	pub(super) fn error_kind(self) -> ErrorKind {
		match self {
			Self::LocalStorageFull => ErrorKind::IO,
			Self::RemoteStorageFull => ErrorKind::MaxStorageReached,
			Self::RemoteUnavailable => ErrorKind::Reqwest,
			Self::Unauthenticated => ErrorKind::Unauthenticated,
		}
	}

	/// Whether this condition is the remote being out of reach for now, rather than a side out of
	/// room: such a failure says nothing about its path, yet the same answer on every pass may be
	/// the path's own when nothing else showed the server answering, so it counts against the
	/// path's streak only spaced out (see the engine's `TRANSIENT_FAILURE_SPACING`).
	pub(super) fn is_remote_outage(self) -> bool {
		match self {
			Self::LocalStorageFull | Self::RemoteStorageFull => false,
			Self::RemoteUnavailable | Self::Unauthenticated => true,
		}
	}

	/// The line a pass's [`errors`](super::SyncReport::errors) carries once it halted on this.
	pub(super) fn held_line(self) -> String {
		format!(
			"{self}: no further {} were started this pass",
			self.held_transfers()
		)
	}
}

impl fmt::Display for HaltReason {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(match self {
			Self::LocalStorageFull => "the local disk is full",
			Self::RemoteStorageFull => "the account's storage is full",
			Self::RemoteUnavailable => "the remote could not be reached or is unavailable",
			Self::Unauthenticated => "the server no longer accepts this session",
		})
	}
}

/// What a pass WOULD do, from [`SyncEngine::plan_pair`](super::SyncEngine::plan_pair) — a dry run
/// that touches neither side and advances no baseline.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlanOutcome {
	/// Every action the pass would apply, in apply order.
	pub actions: Vec<PlannedAction>,
	/// Deletions the mass-delete guard would hold back — plus the create half of a held type flip,
	/// which is only meaningful together with its delete. Not included in `actions`. One per item:
	/// the guard counts what a pass deletes, so a held directory lists every deletion under it, all
	/// of which `actions` would leave to the directory's own.
	pub held: Vec<PlannedAction>,
	/// Why the guard would hold; set exactly when `held` is non-empty.
	pub held_reason: Option<GuardReason>,
	/// The token identifying the held batch and the kind of reason it is held for, for
	/// [`SyncEngine::approve_deletions`](super::SyncEngine::approve_deletions). Set alongside
	/// `held`.
	pub pass_token: Option<String>,
	/// Paths that would be surfaced as conflicts — held, never applied: in two-way, both sides
	/// changed; in a backup mode, the two sides hold items of different kinds. Each with its
	/// [`reason`](PlannedConflict::reason).
	pub conflicts: Vec<PlannedConflict>,
	/// Paths the engine will not act on at all, and why.
	pub unsyncable: Vec<UnsyncablePath>,
	/// The ignored paths the pass would report, as in [`SyncReport::ignored`](super::SyncReport::ignored).
	pub ignored: Vec<IgnoredPath>,
	/// Set when the pass would refuse to run; everything above is then empty. No dry run sets it
	/// any more (see [`RefuseReason`]).
	pub refused: Option<RefuseReason>,
}

impl fmt::Display for PlanOutcome {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		if let Some(reason) = self.refused {
			return write!(f, "refused: {reason}");
		}
		if self.actions.is_empty()
			&& self.held.is_empty()
			&& self.conflicts.is_empty()
			&& self.unsyncable.is_empty()
			&& self.ignored.is_empty()
		{
			return f.write_str("nothing to do");
		}
		let mut first = true;
		let mut line = |f: &mut fmt::Formatter<'_>, text: &dyn fmt::Display| {
			let result = if first {
				fmt::Display::fmt(text, f)
			} else {
				write!(f, "\n{text}")
			};
			first = false;
			result
		};
		for action in &self.actions {
			line(f, action)?;
		}
		if let Some(reason) = &self.held_reason {
			line(f, &format_args!("held: {reason}"))?;
		}
		for action in &self.held {
			line(f, &format_args!("  (held) {action}"))?;
		}
		for conflict in &self.conflicts {
			line(f, conflict)?;
		}
		for path in &self.unsyncable {
			line(f, path)?;
		}
		for path in &self.ignored {
			line(f, path)?;
		}
		Ok(())
	}
}

/// The public view of one internal action, resolving the size from whichever side of the pass
/// knows it (the local scan for a push, the remote view for a pull).
pub(super) fn planned_action(
	action: &SyncAction,
	local: &impl NodesAt<Node = LocalNode>,
	remote: &impl NodesAt<Node = RemoteNode>,
) -> PlannedAction {
	let rel_path = action.endpoints().0.to_string();
	let local_size = |path: &str| local.at(path).map(|node| node.size);
	let remote_size = |path: &str| remote.at(path).map(|node| node.size);
	let transfer = |holds_file: bool| {
		Some(if holds_file {
			PlannedTransfer::Replace
		} else {
			PlannedTransfer::Create
		})
	};
	let is_file = |kind: NodeKind| kind == NodeKind::File;
	let (kind, node, size, transfer) = match action {
		SyncAction::UploadFile { rel_path } => (
			PlannedActionKind::UploadFile,
			PlannedNodeKind::File,
			local_size(rel_path),
			transfer(remote.at(rel_path).is_some_and(|node| is_file(node.kind))),
		),
		SyncAction::DownloadFile { rel_path, .. } => (
			PlannedActionKind::DownloadFile,
			PlannedNodeKind::File,
			remote_size(rel_path),
			transfer(local.at(rel_path).is_some_and(|node| is_file(node.kind))),
		),
		SyncAction::CreateRemoteDir { .. } => (
			PlannedActionKind::CreateRemoteDir,
			PlannedNodeKind::Dir,
			None,
			None,
		),
		SyncAction::CreateLocalDir { .. } => (
			PlannedActionKind::CreateLocalDir,
			PlannedNodeKind::Dir,
			None,
			None,
		),
		// A file's size is read at the destination, where the side that moved holds the node; a
		// directory has none.
		SyncAction::MoveRemote { to_path, kind, .. } => (
			PlannedActionKind::MoveRemote {
				to: to_path.clone(),
			},
			(*kind).into(),
			match kind {
				NodeKind::File => local_size(to_path),
				NodeKind::Dir => None,
			},
			None,
		),
		SyncAction::MoveLocal { to_path, kind, .. } => (
			PlannedActionKind::MoveLocal {
				to: to_path.clone(),
			},
			(*kind).into(),
			match kind {
				NodeKind::File => remote_size(to_path),
				NodeKind::Dir => None,
			},
			None,
		),
		SyncAction::TrashRemote { rel_path, kind, .. } => (
			PlannedActionKind::TrashRemote,
			(*kind).into(),
			remote_size(rel_path),
			None,
		),
		SyncAction::DeleteLocal { rel_path, kind } => (
			PlannedActionKind::DeleteLocal,
			(*kind).into(),
			local_size(rel_path),
			None,
		),
		// A conflict is not an executable action; it never reaches this mapping (the engine splits
		// it out into `conflicts` first). Rendering it as an adopt keeps the mapping total without
		// a panic on a path the type system cannot rule out.
		SyncAction::AdoptBaseline { rel_path } | SyncAction::Conflict { rel_path } => {
			let node = local
				.at(rel_path)
				.map(|n| n.kind)
				.or_else(|| remote.at(rel_path).map(|n| n.kind))
				.map_or(PlannedNodeKind::File, PlannedNodeKind::from);
			(PlannedActionKind::AdoptBaseline, node, None, None)
		}
	};
	PlannedAction {
		kind,
		rel_path,
		node,
		size: (node == PlannedNodeKind::File).then_some(size).flatten(),
		transfer,
	}
}

/// The public view of one conflicted path in a pair synced in `mode`, with what each side held when
/// it was surfaced.
pub(super) fn planned_conflict(
	mode: SyncMode,
	rel_path: &str,
	local: &impl NodesAt<Node = LocalNode>,
	remote: &impl NodesAt<Node = RemoteNode>,
) -> PlannedConflict {
	PlannedConflict::held(
		mode,
		rel_path.to_owned(),
		local.at(rel_path).map(|node| node.kind.into()),
		remote.at(rel_path).map(|node| node.kind.into()),
	)
}

#[cfg(test)]
mod tests {
	use std::collections::HashMap;

	use uuid::Uuid;

	use super::*;

	fn local_file(rel: &str, size: u64) -> (String, LocalNode) {
		(
			rel.to_string(),
			LocalNode {
				rel_path: rel.to_string(),
				kind: NodeKind::File,
				size,
				mtime_millis: 0,
				mtime_nanos: 0,
				content_hash: None,
			},
		)
	}

	fn remote_file(rel: &str, size: u64) -> (String, RemoteNode) {
		(
			rel.to_string(),
			RemoteNode {
				rel_path: rel.to_string(),
				kind: NodeKind::File,
				remote_uuid: Uuid::nil(),
				// These render-only fixtures never reach the lineage rules; an unrecorded
				// lineage is the value those rules read as no evidence either way.
				stable_uuid: None,
				content_hash: None,
				size,
				modified_millis: 0,
			},
		)
	}

	#[test]
	fn an_upload_carries_the_local_size_a_download_the_remote_one() {
		let local = HashMap::from([local_file("a.txt", 11)]);
		let remote = HashMap::from([remote_file("b.txt", 22)]);

		let upload = planned_action(
			&SyncAction::UploadFile {
				rel_path: "a.txt".to_string(),
			},
			&local,
			&remote,
		);
		assert_eq!(upload.kind, PlannedActionKind::UploadFile);
		assert_eq!(upload.size, Some(11));
		assert_eq!(upload.node, PlannedNodeKind::File);

		let download = planned_action(
			&SyncAction::DownloadFile {
				rel_path: "b.txt".to_string(),
				remote_uuid: Uuid::nil(),
			},
			&local,
			&remote,
		);
		assert_eq!(download.size, Some(22), "the pull side knows the size");
	}

	fn upload(rel: &str) -> SyncAction {
		SyncAction::UploadFile {
			rel_path: rel.to_string(),
		}
	}

	fn download(rel: &str) -> SyncAction {
		SyncAction::DownloadFile {
			rel_path: rel.to_string(),
			remote_uuid: Uuid::nil(),
		}
	}

	fn local_dir(rel: &str) -> (String, LocalNode) {
		let (path, node) = local_file(rel, 0);
		(
			path,
			LocalNode {
				kind: NodeKind::Dir,
				..node
			},
		)
	}

	fn remote_dir(rel: &str) -> (String, RemoteNode) {
		let (path, node) = remote_file(rel, 0);
		(
			path,
			RemoteNode {
				kind: NodeKind::Dir,
				..node
			},
		)
	}

	#[test]
	fn an_upload_over_a_remote_file_replaces_it_and_one_to_a_free_path_creates() {
		let local = HashMap::from([local_file("new.txt", 1), local_file("edited.txt", 2)]);
		let remote = HashMap::from([remote_file("edited.txt", 3)]);
		assert_eq!(
			planned_action(&upload("new.txt"), &local, &remote).transfer,
			Some(PlannedTransfer::Create)
		);
		assert_eq!(
			planned_action(&upload("edited.txt"), &local, &remote).transfer,
			Some(PlannedTransfer::Replace)
		);
	}

	#[test]
	fn a_download_over_a_local_file_replaces_it_and_one_to_a_free_path_creates() {
		let local = HashMap::from([local_file("edited.txt", 2)]);
		let remote = HashMap::from([remote_file("new.txt", 1), remote_file("edited.txt", 3)]);
		assert_eq!(
			planned_action(&download("new.txt"), &local, &remote).transfer,
			Some(PlannedTransfer::Create)
		);
		assert_eq!(
			planned_action(&download("edited.txt"), &local, &remote).transfer,
			Some(PlannedTransfer::Replace)
		);
	}

	/// A directory at the path is no file to replace: a type flip trashes or quarantines it with a
	/// deletion of its own, and the transfer then writes a new file.
	#[test]
	fn a_transfer_onto_a_directory_creates() {
		let local = HashMap::from([local_file("flip", 1), local_dir("other")]);
		let remote = HashMap::from([remote_dir("flip"), remote_file("other", 2)]);
		assert_eq!(
			planned_action(&upload("flip"), &local, &remote).transfer,
			Some(PlannedTransfer::Create)
		);
		assert_eq!(
			planned_action(&download("other"), &local, &remote).transfer,
			Some(PlannedTransfer::Create)
		);
	}

	#[test]
	fn an_action_that_transfers_nothing_says_neither() {
		let local = HashMap::from([local_file("a.txt", 1)]);
		let remote = HashMap::from([remote_file("a.txt", 1)]);
		for action in [
			SyncAction::TrashRemote {
				rel_path: "a.txt".to_string(),
				kind: NodeKind::File,
				remote_uuid: Uuid::nil(),
			},
			SyncAction::DeleteLocal {
				rel_path: "a.txt".to_string(),
				kind: NodeKind::File,
			},
			SyncAction::AdoptBaseline {
				rel_path: "a.txt".to_string(),
			},
		] {
			assert_eq!(
				planned_action(&action, &local, &remote).transfer,
				None,
				"{action:?}"
			);
		}
	}

	/// The side that moved holds the node only at the destination, so the size is read there.
	#[test]
	fn a_move_reports_the_source_path_and_names_its_destination() {
		let local = HashMap::from([local_file("new/here.txt", 7)]);
		let action = planned_action(
			&SyncAction::MoveRemote {
				from_path: "old.txt".to_string(),
				to_path: "new/here.txt".to_string(),
				kind: NodeKind::File,
				remote_uuid: Uuid::nil(),
			},
			&local,
			&HashMap::new(),
		);
		assert_eq!(action.rel_path, "old.txt");
		assert_eq!(
			action.kind,
			PlannedActionKind::MoveRemote {
				to: "new/here.txt".to_string()
			}
		);
		assert_eq!(action.size, Some(7));
		assert_eq!(
			action.to_string(),
			r#"move remote "old.txt" -> "new/here.txt""#
		);

		let remote = HashMap::from([remote_file("moved/there.txt", 9)]);
		let action = planned_action(
			&SyncAction::MoveLocal {
				from_path: "was.txt".to_string(),
				to_path: "moved/there.txt".to_string(),
				kind: NodeKind::File,
			},
			&HashMap::new(),
			&remote,
		);
		assert_eq!(action.rel_path, "was.txt");
		assert_eq!(
			action.kind,
			PlannedActionKind::MoveLocal {
				to: "moved/there.txt".to_string()
			}
		);
		assert_eq!(action.size, Some(9));
	}

	#[test]
	fn a_directory_action_reports_no_size() {
		let action = planned_action(
			&SyncAction::TrashRemote {
				rel_path: "sub".to_string(),
				kind: NodeKind::Dir,
				remote_uuid: Uuid::nil(),
			},
			&HashMap::new(),
			&HashMap::new(),
		);
		assert_eq!(action.node, PlannedNodeKind::Dir);
		assert_eq!(action.size, None);
		assert_eq!(action.to_string(), r#"trash remote dir "sub""#);
	}

	#[test]
	fn a_conflict_records_an_absent_side_rather_than_guessing_a_kind() {
		let remote = HashMap::from([remote_file("gone.txt", 3)]);
		let conflict = planned_conflict(SyncMode::TwoWay, "gone.txt", &HashMap::new(), &remote);
		assert_eq!(conflict.local, None, "the local side is deleted");
		assert_eq!(conflict.remote, Some(PlannedNodeKind::File));
		assert_eq!(conflict.reason, ConflictReason::Diverged);
		assert_eq!(
			conflict.to_string(),
			r#"conflict "gone.txt" (local absent vs remote file)"#
		);
	}

	/// The reason in `mode` for each pair of sides, in the order (local file, remote dir), (local
	/// dir, remote file), (both files), (local file, remote absent).
	fn reasons_in(mode: SyncMode) -> [ConflictReason; 4] {
		let (file, dir) = (Some(PlannedNodeKind::File), Some(PlannedNodeKind::Dir));
		[
			ConflictReason::of(mode, file, dir),
			ConflictReason::of(mode, dir, file),
			ConflictReason::of(mode, file, file),
			ConflictReason::of(mode, file, None),
		]
	}

	#[test]
	fn local_backup_names_what_the_remote_keeps_where_the_kinds_differ() {
		assert_eq!(
			reasons_in(SyncMode::LocalBackup),
			[
				ConflictReason::RemoteKeepsDir,
				ConflictReason::RemoteKeepsFile,
				ConflictReason::Diverged,
				ConflictReason::Diverged,
			]
		);
	}

	#[test]
	fn remote_backup_names_what_the_local_side_keeps_where_the_kinds_differ() {
		assert_eq!(
			reasons_in(SyncMode::RemoteBackup),
			[
				ConflictReason::LocalKeepsFile,
				ConflictReason::LocalKeepsDir,
				ConflictReason::Diverged,
				ConflictReason::Diverged,
			]
		);
	}

	#[test]
	fn the_modes_that_delete_on_their_destination_hold_every_conflict_as_diverged() {
		for mode in [
			SyncMode::TwoWay,
			SyncMode::LocalToRemote,
			SyncMode::RemoteToLocal,
		] {
			assert_eq!(reasons_in(mode), [ConflictReason::Diverged; 4], "{mode:?}");
		}
	}

	#[test]
	fn every_resolution_clears_a_divergence() {
		assert_eq!(
			ConflictReason::Diverged.resolutions(),
			[
				ConflictResolution::KeepLocal,
				ConflictResolution::KeepRemote,
				ConflictResolution::KeepBoth,
			]
		);
	}

	#[test]
	fn a_local_file_against_a_remote_directory_clears_by_moving_the_file_off_the_path() {
		for reason in [
			ConflictReason::RemoteKeepsDir,
			ConflictReason::LocalKeepsFile,
		] {
			assert_eq!(
				reason.resolutions(),
				[ConflictResolution::KeepRemote, ConflictResolution::KeepBoth],
				"{reason:?}"
			);
		}
	}

	#[test]
	fn a_local_directory_against_a_remote_file_clears_only_by_keeping_both() {
		for reason in [
			ConflictReason::RemoteKeepsFile,
			ConflictReason::LocalKeepsDir,
		] {
			assert_eq!(
				reason.resolutions(),
				[ConflictResolution::KeepBoth],
				"{reason:?}"
			);
		}
	}

	#[test]
	fn a_kind_mismatch_conflict_renders_its_reason() {
		let local = HashMap::from([local_file("x", 3)]);
		let remote = HashMap::from([(
			"x".to_string(),
			RemoteNode {
				kind: NodeKind::Dir,
				..remote_file("x", 0).1
			},
		)]);
		let conflict = planned_conflict(SyncMode::LocalBackup, "x", &local, &remote);
		assert_eq!(conflict.reason, ConflictReason::RemoteKeepsDir);
		assert_eq!(
			conflict.to_string(),
			r#"conflict "x" (local file vs remote dir; the local side holds a file where the remote backup keeps a directory)"#
		);
	}

	#[test]
	fn a_refusal_renders_as_the_whole_outcome() {
		let outcome = PlanOutcome {
			refused: Some(RefuseReason::LocalCollision),
			..PlanOutcome::default()
		};
		assert!(outcome.to_string().starts_with("refused: "));
		assert_eq!(PlanOutcome::default().to_string(), "nothing to do");
	}
}
