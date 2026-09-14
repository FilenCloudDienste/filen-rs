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

use std::{collections::HashMap, fmt};

use super::{
	baseline::NodeKind,
	guard::GuardReason,
	plan::{RemoteNode, SyncAction},
	scan::LocalNode,
};

/// Whether a planned action's item is a directory or a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
	/// Move the local item into the pair's `.filen-sync-trash/` quarantine dir. Local deletions are
	/// never destructive; the bytes stay recoverable inside the sync root.
	DeleteLocal,
	/// Record a path that is already identical on both sides into the baseline. Transfers nothing;
	/// it exists so a later one-sided change at that path is classified correctly.
	AdoptBaseline,
}

/// One action a pass would apply (or a held one it would not), in apply order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedAction {
	pub kind: PlannedActionKind,
	/// The item's path relative to both roots. For a move this is the SOURCE path; the destination
	/// is in [`PlannedActionKind::MoveRemote`] / [`PlannedActionKind::MoveLocal`].
	pub rel_path: String,
	pub node: PlannedNodeKind,
	/// The file's size in bytes where the pass knows it (a transfer, a move of a tracked file);
	/// `None` for directories and for anything whose size neither side reported.
	pub size: Option<u64>,
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
pub enum UnsyncableReason {
	/// The local name is one the Filen backend rejects (a trailing dot or space, a reserved device
	/// name, a forbidden character, an over-long name). No upload or remote create at this path can
	/// ever succeed, so the path and its subtree are skipped rather than retried forever. `detail`
	/// is the SDK name validator's own message.
	InvalidName { detail: String },
	/// Every attempt to apply this path failed, `attempts` times in a row. The engine stopped
	/// planning it so one broken path cannot stall (or spam) every pass; the count is cleared by a
	/// success or by [`SyncEngine::retry_path`](super::SyncEngine::retry_path).
	RepeatedFailure { attempts: u32, last_error: String },
}

impl fmt::Display for UnsyncableReason {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::InvalidName { detail } => {
				write!(f, "the remote would reject this name: {detail}")
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

/// What a pass WOULD do, from [`SyncEngine::plan_pair`](super::SyncEngine::plan_pair) — a dry run
/// that touches neither side and advances no baseline.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlanOutcome {
	/// Every action the pass would apply, in apply order. Empty when `refused` is set.
	pub actions: Vec<PlannedAction>,
	/// Deletions the mass-delete guard would hold back — plus the create half of a held type flip,
	/// which is only meaningful together with its delete. Not included in `actions`.
	pub held: Vec<PlannedAction>,
	/// Why the guard would hold; set exactly when `held` is non-empty.
	pub held_reason: Option<GuardReason>,
	/// The token identifying the held batch, for
	/// [`SyncEngine::approve_deletions`](super::SyncEngine::approve_deletions). Set alongside
	/// `held`.
	pub pass_token: Option<String>,
	/// Paths that would be surfaced as two-way conflicts — held, never applied.
	pub conflicts: Vec<PlannedConflict>,
	/// Paths the engine will not act on at all, and why.
	pub unsyncable: Vec<UnsyncablePath>,
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
		Ok(())
	}
}

/// The public view of one internal action, resolving the size from whichever side of the pass
/// knows it (the local scan for a push, the remote view for a pull).
pub(super) fn planned_action(
	action: &SyncAction,
	local: &HashMap<String, LocalNode>,
	remote: &HashMap<String, RemoteNode>,
) -> PlannedAction {
	let rel_path = action.endpoints().0.to_string();
	let local_size = |path: &str| local.get(path).map(|node| node.size);
	let remote_size = |path: &str| remote.get(path).map(|node| node.size);
	let (kind, node, size) = match action {
		SyncAction::UploadFile { rel_path } => (
			PlannedActionKind::UploadFile,
			PlannedNodeKind::File,
			local_size(rel_path),
		),
		SyncAction::DownloadFile { rel_path, .. } => (
			PlannedActionKind::DownloadFile,
			PlannedNodeKind::File,
			remote_size(rel_path),
		),
		SyncAction::CreateRemoteDir { .. } => (
			PlannedActionKind::CreateRemoteDir,
			PlannedNodeKind::Dir,
			None,
		),
		SyncAction::CreateLocalDir { .. } => (
			PlannedActionKind::CreateLocalDir,
			PlannedNodeKind::Dir,
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
		),
		SyncAction::TrashRemote { rel_path, kind, .. } => (
			PlannedActionKind::TrashRemote,
			(*kind).into(),
			remote_size(rel_path),
		),
		SyncAction::DeleteLocal { rel_path, kind } => (
			PlannedActionKind::DeleteLocal,
			(*kind).into(),
			local_size(rel_path),
		),
		// A conflict is not an executable action; it never reaches this mapping (the engine splits
		// it out into `conflicts` first). Rendering it as an adopt keeps the mapping total without
		// a panic on a path the type system cannot rule out.
		SyncAction::AdoptBaseline { rel_path } | SyncAction::Conflict { rel_path } => {
			let node = local
				.get(rel_path)
				.map(|n| n.kind)
				.or_else(|| remote.get(rel_path).map(|n| n.kind))
				.map_or(PlannedNodeKind::File, PlannedNodeKind::from);
			(PlannedActionKind::AdoptBaseline, node, None)
		}
	};
	PlannedAction {
		kind,
		rel_path,
		node,
		size: (node == PlannedNodeKind::File).then_some(size).flatten(),
	}
}

/// The public view of one conflicted path, with what each side held when it was surfaced.
pub(super) fn planned_conflict(
	rel_path: &str,
	local: &HashMap<String, LocalNode>,
	remote: &HashMap<String, RemoteNode>,
) -> PlannedConflict {
	PlannedConflict {
		rel_path: rel_path.to_string(),
		local: local.get(rel_path).map(|node| node.kind.into()),
		remote: remote.get(rel_path).map(|node| node.kind.into()),
	}
}

#[cfg(test)]
mod tests {
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
