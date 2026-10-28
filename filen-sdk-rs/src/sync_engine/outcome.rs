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

/// A path both sides changed since the last sync, held until
/// [`SyncEngine::resolve_conflict`](super::SyncEngine::resolve_conflict) picks a winner. The path
/// and its subtree are excluded from planning while it is held.
#[derive(Debug, Clone, PartialEq, Eq)]
#[js_type(export, no_deser, no_default, napi)]
pub struct PlannedConflict {
	pub rel_path: String,
	/// What the local side holds, or `None` when the path is GONE locally — a delete-vs-modify
	/// divergence rather than two competing edits.
	pub local: Option<PlannedNodeKind>,
	/// What the remote side holds, `None` when the remote deleted it.
	pub remote: Option<PlannedNodeKind>,
}

impl PlannedConflict {
	fn side(kind: Option<PlannedNodeKind>) -> String {
		kind.map_or_else(|| "absent".to_string(), |kind| kind.to_string())
	}
}

impl fmt::Display for PlannedConflict {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(
			f,
			"conflict {:?} (local {} vs remote {})",
			self.rel_path,
			Self::side(self.local),
			Self::side(self.remote)
		)
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
	/// The remote item's parent chain never reaches the sync root: its parent is missing from the
	/// listing, or the chain loops. Handled like [`RemoteUndecodable`](Self::RemoteUndecodable); for
	/// an item that was never synced, `rel_path` is its bare name, since it has no path.
	RemoteBrokenParent,
	/// A local symlink to a directory inside the sync root. The directory is synced under its real
	/// path, `target`; the link is left alone on both sides, so neither a copy synced at the link's
	/// path before nor anything the remote holds there is deleted, and nothing is written through
	/// the link.
	LocalAlias { target: String },
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

/// Why the engine refused to run a pass at all. Both cases make a 1:1 path mapping between the two
/// sides impossible, so NOTHING is applied until the caller resolves the collision by hand.
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
	/// was lost or timed out, or the server answered with a 5xx status, 408 or 429. The kind of
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
	/// the path's own, so it counts against the path's streak only spaced out (see the engine's
	/// `TRANSIENT_FAILURE_SPACING`).
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
	/// Every action the pass would apply, in apply order. Empty when `refused` is set.
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
	/// Paths that would be surfaced as two-way conflicts — held, never applied.
	pub conflicts: Vec<PlannedConflict>,
	/// Paths the engine will not act on at all, and why.
	pub unsyncable: Vec<UnsyncablePath>,
	/// The ignored paths the pass would report, as in [`SyncReport::ignored`](super::SyncReport::ignored).
	pub ignored: Vec<IgnoredPath>,
	/// Set when the pass would refuse to run; everything above is then empty.
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
		// A file's size is read at the source path, where each side still holds the bytes; a
		// directory has none.
		SyncAction::MoveRemote { to_path, kind, .. } => (
			PlannedActionKind::MoveRemote {
				to: to_path.clone(),
			},
			(*kind).into(),
			match kind {
				NodeKind::File => local_size(&rel_path),
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
				NodeKind::File => remote_size(&rel_path),
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

/// The public view of one conflicted path, with what each side held when it was surfaced.
pub(super) fn planned_conflict(
	rel_path: &str,
	local: &impl NodesAt<Node = LocalNode>,
	remote: &impl NodesAt<Node = RemoteNode>,
) -> PlannedConflict {
	PlannedConflict {
		rel_path: rel_path.to_owned(),
		local: local.at(rel_path).map(|node| node.kind.into()),
		remote: remote.at(rel_path).map(|node| node.kind.into()),
	}
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

	#[test]
	fn a_move_reports_the_source_path_and_names_its_destination() {
		let local = HashMap::from([local_file("old.txt", 7)]);
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
		let conflict = planned_conflict("gone.txt", &HashMap::new(), &remote);
		assert_eq!(conflict.local, None, "the local side is deleted");
		assert_eq!(conflict.remote, Some(PlannedNodeKind::File));
		assert_eq!(
			conflict.to_string(),
			r#"conflict "gone.txt" (local absent vs remote file)"#
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
