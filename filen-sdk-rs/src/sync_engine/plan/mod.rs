//! The pure planning layer: build the remote-side view from a cache snapshot, then reconcile the
//! three inputs (baseline, local scan, remote view) into an ordered list of [`SyncAction`]s.
//!
//! `reconcile` is deliberately side-effect-free and synchronous so the whole decision matrix is
//! unit-testable without a filesystem or network. One-way modes converge the destination onto the
//! source (`make dst match src`, gated by whether deletions propagate); two-way uses the baseline
//! to tell which side changed and surfaces a genuine both-sides-changed divergence as a conflict.

use std::{
	borrow::Cow,
	collections::{BTreeMap, BTreeSet, HashMap, HashSet, btree_map},
	ffi::OsStr,
	hash::Hash,
};

use filen_types::{crypto::Blake3Hash, fs::StableUuid};
use itertools::{EitherOrBoth, Itertools};
use unicode_normalization::UnicodeNormalization;
use uuid::Uuid;

use super::{
	baseline::{BaselineEntry, BaselineState, NodeKind, SyncedPaths},
	changes::FullPassReason,
	events::SyncEvent,
	ignore::{IgnoreDecision, IgnoreLevel, IgnoreRules},
	outcome::{UnsyncablePath, UnsyncableReason},
	rows::Baseline,
	scan::{LocalNode, QUARANTINE_DIR, collision_hash, collision_key, is_legacy_trash},
	side::{Nodes, NodesAt, Side, SideRef},
};
use crate::cache::{RemoteItem, UndecodableItem};

/// Guards against a malformed (cyclic) remote parent chain when resolving a path.
pub(super) const MAX_REMOTE_DEPTH: usize = 256;

/// One item in the remote view, keyed (like [`LocalNode`]) by its NFC-normalized `/`-joined path
/// relative to the sync root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RemoteNode {
	pub(crate) rel_path: String,
	pub(crate) kind: NodeKind,
	pub(crate) remote_uuid: Uuid,
	/// The server-minted whole-life id of a FILE: `remote_uuid` is re-minted by every content edit
	/// and version restore, this is not. `None` for a directory, which has no such id — a
	/// directory's own uuid already survives its renames.
	pub(crate) stable_uuid: Option<StableUuid>,
	/// BLAKE3 of the content (files); `None` for dirs and for older files the server stored
	/// without a hash.
	pub(crate) content_hash: Option<Blake3Hash>,
	pub(crate) size: u64,
	pub(crate) modified_millis: i64,
}

/// One action the apply layer should execute. Pull actions move remote -> local; push actions move
/// local -> remote; a conflict is left untouched for the caller to resolve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SyncAction {
	CreateLocalDir {
		rel_path: String,
	},
	DownloadFile {
		rel_path: String,
		remote_uuid: Uuid,
	},
	DeleteLocal {
		rel_path: String,
		kind: NodeKind,
	},
	CreateRemoteDir {
		rel_path: String,
	},
	UploadFile {
		rel_path: String,
	},
	TrashRemote {
		rel_path: String,
		kind: NodeKind,
		remote_uuid: Uuid,
	},
	Conflict {
		rel_path: String,
	},
	/// A path already identical on both sides (or already gone from both) but lacking a correct
	/// baseline row -> record the converged state into the baseline with NO network/FS transfer, so
	/// a later one-sided change at this path is classified correctly (a delete reads as a delete, an
	/// edit as an edit) rather than misread against an empty baseline. Emitted in every mode: a
	/// one-way pair opened on an already-synced tree transfers nothing, so this is the only thing
	/// that seeds its baseline — and until it has one, the guard treats every pass as a first sync.
	AdoptBaseline {
		rel_path: String,
	},
	/// An item moved/renamed on the local side -> re-parent + rename the remote item (uuid kept)
	/// instead of re-uploading its content or re-creating it.
	///
	/// A DIRECTORY move carries its whole subtree: it is planned by [`fold_dir_moves`], which
	/// re-keys the pass's inputs so every child is read at its new path and is a no-op unless it
	/// changed on its own. It runs before every other action of the pass.
	MoveRemote {
		from_path: String,
		to_path: String,
		kind: NodeKind,
		remote_uuid: Uuid,
	},
	/// An item moved/renamed on the remote side -> rename the local item instead of re-downloading
	/// it (a file) or quarantining and re-creating its subtree (a directory, see
	/// [`fold_dir_moves`]).
	MoveLocal {
		from_path: String,
		to_path: String,
		kind: NodeKind,
	},
}

impl SyncAction {
	/// The path that determines apply ORDER (the destination for a move). Parents sort before
	/// children, so a create of the destination's parent dir precedes a move into it.
	pub(super) fn rel_path(&self) -> &str {
		match self {
			Self::CreateLocalDir { rel_path }
			| Self::DownloadFile { rel_path, .. }
			| Self::DeleteLocal { rel_path, .. }
			| Self::CreateRemoteDir { rel_path }
			| Self::UploadFile { rel_path }
			| Self::TrashRemote { rel_path, .. }
			| Self::Conflict { rel_path }
			| Self::AdoptBaseline { rel_path } => rel_path,
			Self::MoveRemote { to_path, .. } | Self::MoveLocal { to_path, .. } => to_path,
		}
	}

	/// Both paths an action touches: `(from, to)` for a move or rename, the one path twice
	/// otherwise.
	pub(super) fn endpoints(&self) -> (&str, &str) {
		match self {
			Self::MoveRemote {
				from_path, to_path, ..
			}
			| Self::MoveLocal {
				from_path, to_path, ..
			} => (from_path, to_path),
			other => (other.rel_path(), other.rel_path()),
		}
	}

	pub(super) fn is_delete(&self) -> bool {
		matches!(self, Self::DeleteLocal { .. } | Self::TrashRemote { .. })
	}

	/// Whether this action MATERIALIZES an item at its path (a directory create or a content
	/// transfer). A delete whose path matches one of these in the SAME pass is a "replace" (a type
	/// flip — old item removed, new-kind item created at the same path), and the delete must run
	/// BEFORE the create or the server rejects it (the old same-name item still exists). See
	/// [`replaced_paths`], which pairs them, [`order_actions`] and the apply layer.
	pub(super) fn is_create(&self) -> bool {
		matches!(
			self,
			Self::CreateLocalDir { .. }
				| Self::CreateRemoteDir { .. }
				| Self::UploadFile { .. }
				| Self::DownloadFile { .. }
		)
	}

	/// A short human-readable description of the action, for the `tracing` lines a pass logs.
	pub(super) fn describe(&self) -> String {
		match self {
			Self::CreateLocalDir { rel_path } => format!("create local dir {rel_path:?}"),
			Self::DownloadFile { rel_path, .. } => format!("download file {rel_path:?}"),
			Self::DeleteLocal { rel_path, kind } => {
				format!("delete local {kind:?} {rel_path:?} (to quarantine)")
			}
			Self::CreateRemoteDir { rel_path } => format!("create remote dir {rel_path:?}"),
			Self::UploadFile { rel_path } => format!("upload file {rel_path:?}"),
			Self::TrashRemote { rel_path, kind, .. } => {
				format!("trash remote {kind:?} {rel_path:?}")
			}
			Self::Conflict { rel_path } => format!("conflict {rel_path:?}"),
			Self::AdoptBaseline { rel_path } => format!("adopt baseline {rel_path:?}"),
			Self::MoveRemote {
				from_path,
				to_path,
				kind,
				..
			} => format!("move remote {kind:?} {from_path:?} -> {to_path:?}"),
			Self::MoveLocal {
				from_path,
				to_path,
				kind,
			} => format!("move local {kind:?} {from_path:?} -> {to_path:?}"),
		}
	}

	/// The "in progress" [`SyncEvent`] for this action, emitted by the apply layer just before it
	/// executes the action.
	pub(super) fn to_event(&self) -> SyncEvent {
		match self {
			Self::UploadFile { rel_path } => SyncEvent::Uploading {
				rel_path: rel_path.clone(),
			},
			Self::DownloadFile { rel_path, .. } => SyncEvent::Downloading {
				rel_path: rel_path.clone(),
			},
			Self::CreateRemoteDir { rel_path } => SyncEvent::CreatingRemoteDir {
				rel_path: rel_path.clone(),
			},
			Self::CreateLocalDir { rel_path } => SyncEvent::CreatingLocalDir {
				rel_path: rel_path.clone(),
			},
			Self::TrashRemote { rel_path, .. } => SyncEvent::TrashingRemote {
				rel_path: rel_path.clone(),
			},
			Self::DeleteLocal { rel_path, .. } => SyncEvent::DeletingLocal {
				rel_path: rel_path.clone(),
			},
			Self::MoveRemote {
				from_path, to_path, ..
			} => SyncEvent::MovingRemote {
				from: from_path.clone(),
				to: to_path.clone(),
			},
			Self::MoveLocal {
				from_path, to_path, ..
			} => SyncEvent::MovingLocal {
				from: from_path.clone(),
				to: to_path.clone(),
			},
			Self::Conflict { rel_path } => SyncEvent::Conflict {
				rel_path: rel_path.clone(),
			},
			Self::AdoptBaseline { rel_path } => SyncEvent::AdoptedBaseline {
				rel_path: rel_path.clone(),
			},
		}
	}
}

/// Whether a remote item name is safe to use as a single local path component. Rejects empty,
/// `.`/`..`, and names containing a path separator or NUL — a remote name is untrusted input (a
/// non-conforming client or, in future, a shared-folder peer could set one), and any of these
/// would let a pull escape the sync root (path traversal) or corrupt the `/`-joined key.
pub(super) fn is_safe_name(name: &str) -> bool {
	!name.is_empty()
		&& name != "."
		&& name != ".."
		&& !name.contains('/')
		&& !name.contains('\\')
		&& !name.contains('\0')
}

/// Why [`resolve_parent`] could not place a directory under the sync root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Unresolved {
	/// An ancestor — the directory with this uuid — is itself left out of the view (an unsafe name,
	/// an undecodable directory, or a broken chain further up). The item is recorded under that
	/// ancestor's path and reason.
	UnderSkipped(Uuid),
	/// The immediate parent is nowhere in the snapshot, or the chain runs past
	/// [`MAX_REMOTE_DEPTH`] (a malformed cycle).
	BrokenParent,
}

/// Resolve the `/`-joined, NFC-normalized path of the directory `parent` by walking the dir index
/// up to `root` (`""` for the root itself). Fails when the chain passes a directory the view leaves
/// out — one whose name is not a safe path component, so a traversal name like `..` never enters
/// the plan, or one listed in `undecodable_dirs` — or never reaches `root`.
fn resolve_parent(
	parent: Uuid,
	root: Uuid,
	dir_index: &HashMap<Uuid, (String, Uuid)>,
	undecodable_dirs: &HashSet<Uuid>,
) -> Result<String, Unresolved> {
	let mut parts: Vec<&str> = Vec::new();
	// The directory the walk stepped up from, whose own parent `current` is.
	let mut below: Option<Uuid> = None;
	let mut current = parent;
	while current != root {
		if parts.len() >= MAX_REMOTE_DEPTH {
			return Err(Unresolved::BrokenParent);
		}
		if undecodable_dirs.contains(&current) {
			return Err(Unresolved::UnderSkipped(current));
		}
		let Some((name, grandparent)) = dir_index.get(&current) else {
			return Err(match below {
				// That directory's parent is missing: it is the one recorded as broken.
				Some(orphan) => Unresolved::UnderSkipped(orphan),
				None => Unresolved::BrokenParent,
			});
		};
		if !is_safe_name(name) {
			return Err(Unresolved::UnderSkipped(current));
		}
		parts.push(name);
		below = Some(current);
		current = *grandparent;
	}
	parts.reverse();
	Ok(parts.join("/"))
}

pub(super) fn join_path(parent_path: &str, name: &str) -> String {
	if parent_path.is_empty() {
		name.to_string()
	} else {
		format!("{parent_path}/{name}")
	}
}

/// Whether `rel_path` is a path the engine never syncs: the local quarantine dir or inside it, or a
/// path with filen-sync's trash folder as any component (see [`is_legacy_trash`]). The local scan
/// never lists either, so the remote view leaves out a remote item that happens to carry the name
/// too, with everything under it: it is never mistaken for (or synced into) the quarantine area,
/// never downloaded, and never trashed by a deletion propagated to it. A deletion of a directory
/// above it still trashes it with that directory, which is what the remote does with any content.
pub(super) fn is_reserved_path(rel_path: &str) -> bool {
	rel_path == QUARANTINE_DIR
		|| rel_path.starts_with(&format!("{QUARANTINE_DIR}/"))
		|| rel_path
			.split('/')
			.any(|component| is_legacy_trash(OsStr::new(component)))
}

/// The remote view of a sync root's subtree, plus whether it is safe to reconcile against.
#[derive(Debug)]
pub(crate) struct RemoteView {
	pub(crate) nodes: Side<RemoteNode>,
	/// `true` if two distinct remote items resolved to the same case-insensitive path but NOT the
	/// same byte-identical one. The engine refuses to reconcile such a pair (a 1:1 local mapping is
	/// impossible) until the user cleans it up.
	pub(crate) has_collisions: bool,
	/// Paths two byte-identically named remote items resolved to. The server never allows that —
	/// its dedup is on the lowercased name hash — so it can only be a cache mid-transition: a
	/// re-upload whose successor has been applied while the predecessor's trash has not. Only that
	/// one path is held back; the pass runs.
	pub(crate) held_paths: BTreeSet<String>,
	/// Remote items that exist but are NOT in `nodes`, and why. Their absence from `nodes` is no
	/// evidence of a deletion (see [`unknown_remote_paths`]).
	pub(crate) skipped: Vec<SkippedRemote>,
	/// The top-most items the ignore rules hide, with the deciding rule. Neither they nor anything
	/// under them is in `nodes` or `skipped`, or checked for collisions.
	pub(crate) ignored: BTreeMap<String, IgnoreDecision>,
	/// How many items only the BUILT-IN defaults hid with no baseline row at or under them — left
	/// out of `ignored` for the reason [`LocalScan::ignored_default_untracked`](super::scan::LocalScan::ignored_default_untracked)
	/// gives. Counted per item rather than per root, since it is logged and never reported.
	///
	/// A LOG LINE, not a fact about the tree, and the only field here that a change-scoped pass
	/// under-reports on purpose: [`filter_changed`](RemoteView::filter_changed) asks the rules about
	/// the decided paths alone, so it counts hits among those and not the whole view. Anything that
	/// ever needs the real number has to ask the rules about every node, which is the scan that
	/// narrowing removed.
	pub(crate) ignored_default_untracked: usize,
}

/// The rules a view is filtered with, and the baseline that says which of their hits are roots.
///
/// The two travel together because neither answers alone: an ignored path is only a ROOT the pass
/// records and untracks when a row sits at or under it, unless a level above the built-in defaults
/// hid it (see [`Baseline::tracked`]). A view built with no filter has no ignored roots and needs
/// no baseline.
#[derive(Clone, Copy)]
pub(crate) struct ViewFilter<'a> {
	pub(crate) rules: &'a IgnoreRules,
	pub(crate) baseline: &'a Baseline,
}

/// A remote item the snapshot holds that the view could not place at a path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SkippedRemote {
	pub(crate) remote_uuid: Uuid,
	/// The file's whole-life id; `None` for a directory.
	pub(crate) stable_uuid: Option<StableUuid>,
	/// Where the item would sit: its parent's path joined with its name. For an undecodable item,
	/// whose name is unknown, the parent's path (`""` for the sync root); for a broken parent chain,
	/// which has no path at all, the bare name. An item under a skipped directory carries that
	/// directory's path and reason.
	pub(crate) rel_path: String,
	/// Whether [`rel_path`](Self::rel_path) names a DIRECTORY — a fact about the path this record
	/// reports, not about the item that owns the record, and the two differ. An undecodable item is
	/// recorded at the directory that holds it, and an item under a skipped directory takes that
	/// directory's path, so both are directory paths whatever the item itself is; only an item
	/// recorded at its own would-be path carries its own kind.
	///
	/// [`RemoteView::filter`] asks the ignore rules about this path, and a rule written for
	/// directories only (`build/`) answers differently depending on which it is asked about.
	pub(crate) path_is_dir: bool,
	pub(crate) reason: UnsyncableReason,
}

/// How many directories [`RemoteView::hide`] keeps a decision for at once.
const HIDE_MEMO: usize = 4096;

/// Builds a [`RemoteView`] out of a cache subtree snapshot's items AS THEY ARRIVE, so the whole
/// subtree never exists as a `Vec` beside the view built from it. Names are NFC-normalized so
/// they key 1:1 against the local scan.
///
/// A whole pass no longer builds its view in memory: it stages it (see `stage::remote`), and this is
/// what that staged view is held to, node for node and record for record, in the tests and the
/// probe. It is also what records the items the staged walk cannot place
/// ([`replay`](Self::replay)), so the records are this builder's own.
///
/// An item that cannot be placed — an unsafe name, a broken parent chain, or metadata the cache
/// could not decode (the undecodable records, which may hold other roots' too: only those whose
/// parent resolves under `root` are this view's) — is left out of `nodes` and recorded in
/// [`RemoteView::skipped`]. A descendant of a skipped directory is left out with it and recorded
/// under that directory's path and reason: its own uuid is still what a synced item moved beneath
/// the directory is found by (see [`unknown_remote_paths`]).
///
/// This is the view a pass reads the remote AS IT IS from: what the snapshot confirms, which
/// remote rule files there are, which of this engine's own writes the cache has caught up to, and
/// whether the remote came back emptied are facts about the remote whatever the rules hide. Those
/// reads come first of necessity — the rules are not known until the local scan has read the
/// `.filenignore` files on disk — and then [`RemoteView::filter`] hides what they hide IN PLACE,
/// leaving the one map reconcile, the folds and apply act on.
///
/// The rows may arrive in any order (the read's statement promises none). An item whose ancestry
/// has not arrived yet is set aside and placed by [`finish`](Self::finish), when the index is
/// complete and every answer is final — so the view does not depend on the order, and a read that
/// happens to hand over parents first sets nothing aside at all.
pub(crate) struct ViewBuilder {
	root: Uuid,
	/// uuid -> (NFC name, parent) for every directory seen so far, which is what resolves a path.
	dir_index: HashMap<Uuid, (String, Uuid)>,
	/// Held until [`finish`](Self::finish): resolving an undecodable record's path needs the
	/// COMPLETE index, since the record is placed at the directory that holds it.
	undecodable: Vec<UndecodableItem>,
	/// The undecodable DIRECTORIES, which placement needs from the first item on: nothing under
	/// one can be placed, and the read hands them over before any item.
	undecodable_dirs: HashSet<Uuid>,
	/// A plain map, not a [`Side`]: a builder is what a WHOLE read produces, so there is no
	/// baseline to derive from and every placement has to know what it displaced.
	nodes: HashMap<String, RemoteNode>,
	held_paths: BTreeSet<String>,
	skipped: Skipped,
	/// Items whose ancestry the index could not answer for when they arrived (see the type doc).
	deferred: Vec<(bool, RemoteItem)>,
}

impl ViewBuilder {
	/// A builder for `root`'s subtree, its map sized for `items` nodes: one sized for the whole tree
	/// never grows, so it never holds its old table and its new one at once.
	///
	/// What a pass built its view with before it staged it (see `stage::remote`): the tests and the
	/// probe build one beside the staged view, which has to come out the same.
	#[cfg(any(test, feature = "bench-internals"))]
	pub(crate) fn with_capacity(root: Uuid, items: usize) -> Self {
		Self {
			root,
			dir_index: HashMap::new(),
			undecodable: Vec::new(),
			undecodable_dirs: HashSet::new(),
			nodes: HashMap::with_capacity(items),
			held_paths: BTreeSet::new(),
			skipped: Skipped::default(),
			deferred: Vec::new(),
		}
	}

	/// A builder that answers for the rows a staged walk could not place, `items`, and for the
	/// `undecodable` records (see `stage::remote`), with `dir_index` holding every directory their
	/// chains climb. Their answers depend on nothing else, so [`finish`](Self::finish) records each
	/// exactly as it does with the whole snapshot's index.
	pub(super) fn replay(
		root: Uuid,
		dir_index: HashMap<Uuid, (String, Uuid)>,
		undecodable: Vec<UndecodableItem>,
		items: Vec<(bool, RemoteItem)>,
	) -> Self {
		let mut builder = Self {
			root,
			dir_index,
			undecodable: Vec::new(),
			undecodable_dirs: HashSet::new(),
			nodes: HashMap::new(),
			held_paths: BTreeSet::new(),
			skipped: Skipped::default(),
			deferred: items,
		};
		builder.take_undecodable(undecodable);
		builder
	}

	/// [`finish`](Self::finish) for a [`replay`](Self::replay), whose items resolve under no chain
	/// of placeable directories: the walk placed every item that does. So what comes of it is
	/// records, and never a node or a held path.
	pub(super) fn finish_unplaced(self) -> Vec<SkippedRemote> {
		let view = self.finish();
		assert!(
			view.held_paths.is_empty()
				&& matches!(&view.nodes, Side::Whole(nodes) if nodes.is_empty()),
			"an item the staged walk left resolved to a path (should be impossible)"
		);
		view.skipped
	}

	/// The records the cache could not decode, taken before the first item (see
	/// [`SnapshotSink`](crate::cache::SnapshotSink)).
	fn take_undecodable(&mut self, items: Vec<UndecodableItem>) {
		self.undecodable_dirs = items
			.iter()
			.filter(|item| item.stable_uuid.is_none())
			.map(|item| item.uuid)
			.collect();
		self.undecodable = items;
	}

	/// Index `item` if it is a directory, then place it at its path or record why it has none.
	///
	/// `defer` is set while the read is still running, where an unresolved chain means one of two
	/// things the arriving item cannot tell apart: an ancestor the read has not reached, or an
	/// ancestor that is genuinely missing, unsafely named or undecodable. So it is set aside, and
	/// [`finish`](Self::finish) asks again with the index complete, where the answer is final. An
	/// item the index CAN answer for is answered once: a later row adds entries to the index, and
	/// adding entries cannot change a walk that already found every step it needed.
	fn place(&mut self, is_dir: bool, item: &RemoteItem, defer: bool) {
		let name = item.name.nfc().collect::<String>();
		if is_dir {
			self.dir_index
				.insert(item.uuid, (name.clone(), item.parent));
		}
		// A whole-life id is a file's; a directory's own uuid already survives its renames. Taken
		// from the KIND rather than from the field, which a directory is merely expected to leave
		// empty.
		let stable_uuid = if is_dir { None } else { item.stable_uuid };
		let skip = match resolve_parent(
			item.parent,
			self.root,
			&self.dir_index,
			&self.undecodable_dirs,
		) {
			Ok(parent_path) if is_safe_name(&name) => {
				self.insert(join_path(&parent_path, &name), is_dir, item, stable_uuid);
				return;
			}
			Ok(parent_path) => SkippedRemote {
				remote_uuid: item.uuid,
				stable_uuid,
				rel_path: join_path(&parent_path, &name),
				path_is_dir: is_dir,
				reason: UnsyncableReason::RemoteInvalidName { name },
			},
			Err(_) if defer => {
				self.deferred.push((is_dir, item.clone()));
				return;
			}
			Err(Unresolved::UnderSkipped(ancestor)) => {
				self.skipped.under(item.uuid, stable_uuid, ancestor);
				return;
			}
			Err(Unresolved::BrokenParent) => SkippedRemote {
				remote_uuid: item.uuid,
				stable_uuid,
				rel_path: name,
				path_is_dir: is_dir,
				reason: UnsyncableReason::RemoteBrokenParent,
			},
		};
		self.skipped.record(skip);
	}

	/// The node at `rel_path`, unless the path is the engine's own quarantine directory or a
	/// second item has already claimed it.
	fn insert(
		&mut self,
		rel_path: String,
		is_dir: bool,
		item: &RemoteItem,
		stable_uuid: Option<StableUuid>,
	) {
		// The engine's reserved paths come before every rule, and are never reported as ignored.
		if is_reserved_path(&rel_path) {
			return;
		}
		// Byte-identical names under one parent cannot exist on the server, so a second item at
		// this exact path can only be the cache showing both halves of a re-upload at once: the
		// successor has been applied and the predecessor's trash has not. Withhold that one path
		// for this pass rather than refusing the whole one; the next snapshot has one of them.
		if self.held_paths.contains(&rel_path) {
			return;
		}
		let node = RemoteNode {
			rel_path: rel_path.clone(),
			kind: if is_dir {
				NodeKind::Dir
			} else {
				NodeKind::File
			},
			remote_uuid: item.uuid,
			stable_uuid,
			content_hash: if is_dir { None } else { item.hash },
			size: if is_dir { 0 } else { item.size },
			modified_millis: item.modified_millis,
		};
		if self.nodes.insert(rel_path.clone(), node).is_some() {
			tracing::debug!(
				"remote view: holding {rel_path:?} — the cache is mid-transition, listing two items under that exact name"
			);
			self.nodes.remove(&rel_path);
			self.held_paths.insert(rel_path);
		}
	}

	/// The view, once everything that needed the WHOLE subtree has been answered: the undecodable
	/// records, whose path is the directory that holds them, the items that arrived before their
	/// ancestry, and the descendants of a skipped directory, which take that directory's record.
	pub(crate) fn finish(mut self) -> RemoteView {
		for item in std::mem::take(&mut self.undecodable) {
			match resolve_parent(
				item.parent,
				self.root,
				&self.dir_index,
				&self.undecodable_dirs,
			) {
				Ok(rel_path) => self.skipped.record_first(SkippedRemote {
					remote_uuid: item.uuid,
					stable_uuid: item.stable_uuid,
					rel_path,
					// The holding directory's path: an undecodable item's own name is what the
					// cache could not read.
					path_is_dir: true,
					reason: UnsyncableReason::RemoteUndecodable,
				}),
				Err(Unresolved::UnderSkipped(ancestor)) => {
					self.skipped.under(item.uuid, item.stable_uuid, ancestor);
				}
				// Not under this root at all: another root's record.
				Err(Unresolved::BrokenParent) => {}
			}
		}
		for (is_dir, item) in std::mem::take(&mut self.deferred) {
			self.place(is_dir, &item, false);
		}
		RemoteView {
			nodes: self.nodes.into(),
			has_collisions: false,
			held_paths: self.held_paths,
			skipped: self.skipped.finish(),
			ignored: BTreeMap::new(),
			ignored_default_untracked: 0,
		}
	}
}

impl crate::cache::SnapshotSink for ViewBuilder {
	fn undecodable(&mut self, items: Vec<UndecodableItem>) {
		self.take_undecodable(items);
	}

	fn item(&mut self, is_dir: bool, item: RemoteItem) {
		self.place(is_dir, &item, true);
	}
}

/// [`ViewBuilder`] over a snapshot that is already materialized: the shape the tests and the
/// probe read, and the one a pass read before it learned to place rows as they arrive. A pass
/// itself no longer has the two slices to hand — it never builds them — so this is gated with
/// the read that does.
#[cfg(any(test, feature = "bench-internals"))]
pub(crate) fn place_remote_items(
	root: Uuid,
	dirs: &[RemoteItem],
	files: &[RemoteItem],
	undecodable: &[UndecodableItem],
) -> RemoteView {
	let mut builder = ViewBuilder::with_capacity(root, dirs.len() + files.len());
	builder.take_undecodable(undecodable.to_vec());
	// Set aside and answered at `finish` exactly as a streamed row is: these slices are in no
	// more of an order than the read's rows are, and a directory that follows its own children
	// must not read as one with no parent.
	for dir in dirs {
		builder.place(true, dir, true);
	}
	for file in files {
		builder.place(false, file, true);
	}
	builder.finish()
}

impl RemoteView {
	/// Hide what `filter`'s rules hide, then resolve the name collisions among what is left — the
	/// two steps that turn a placed view ([`place_remote_items`]) into the set a pass reconciles,
	/// folds and applies against.
	///
	/// The rules come off first, so ignored case-twins never refuse a pass. With no filter nothing
	/// is hidden and the view stays the remote as it is, collisions resolved.
	///
	/// The two halves are `pub(super)` so the probe can time them apart — they are the largest
	/// per-node cost a change-scoped pass has, and one figure for both cannot say which of them a
	/// narrowing moved. THIS is the order; a caller that runs them itself mirrors it.
	pub(crate) fn filter(&mut self, filter: Option<ViewFilter<'_>>) {
		// ONE tree for both halves. The ignore half reads the side through `filter.baseline` and
		// the collision half walks the rows of the tree it is handed; taking them from two places
		// would let a caller derive the ignore decisions from one tree and the collision keys from
		// another, and the view would be one neither tree describes. `filter_changed` has always
		// read both off the filter, and this now does too.
		match filter {
			Some(filter) => {
				self.hide(filter, PassPaths::Whole);
				self.resolve_collisions(filter.baseline);
			}
			// No rules, so nothing to hide and no carried side to read: the collision check has
			// only the placed nodes to fold, and an empty tree is the whole of what it needs.
			None => self.resolve_collisions(&Baseline::default()),
		}
	}

	/// [`filter`](Self::filter) for a view a change-scoped pass DERIVED, which asks the rules only
	/// about the paths that pass decided.
	///
	/// The narrowing rests on the rules standing still. Every level of them forces a whole read
	/// when it changes — a `.filenignore` on either side, the user patterns, the pair's mode, all
	/// [`FullPassReason::RulesChanged`](super::changes::FullPassReason::RulesChanged) — so a
	/// scoped pass runs under exactly the rules its predecessor ran under, and a node it CARRIED
	/// from a baseline row is a node those same rules already let through. What is left to ask
	/// about is what moved: `decided` is the exact set of keys this pass's producers put somewhere
	/// other than the node its row carries, which is the same set the reconcile is driven from.
	///
	/// The rows that are the exception — rows left at hidden paths by an `untrack_ignored` whose
	/// subtree delete failed — stay in the view here where the whole form would drop them. That is
	/// the safe direction, and deliberately so: their LOCAL half is carried from the same row, so
	/// the pair reads as converged and the narrowed reconcile, which visits only `decided`, plans
	/// nothing at them. Dropping the remote half alone is what would read as a local deletion.
	///
	/// [`ignored`](Self::ignored) therefore comes back holding only the roots among `decided`. A
	/// root an earlier pass found goes on being reported and untracked without this pass re-deriving
	/// it, because `PairFacts::merge_remote_view` EXTENDS the carried ignored roots with this set
	/// rather than replacing them. Its PRUNE is the half that needs the care, and the reason that
	/// merge takes a baseline: it drops the whole subtree under each touched path, so a carried root
	/// strictly under one would be dropped on the strength of a view that never looked there. It
	/// re-supplies such a root where the baseline still names a row at or under it, and drops one
	/// with no rows left under it exactly as it did before this narrowing.
	pub(crate) fn filter_changed(&mut self, filter: ViewFilter<'_>, decided: &BTreeSet<String>) {
		let kept = self.hide(filter, PassPaths::Changed(decided));
		self.resolve_collisions_changed(filter.baseline, kept);
	}

	/// The ignore half of [`filter`](Self::filter): an item the rules hide at or above its path
	/// leaves `nodes` — everything under it with it, since nothing under an ignored directory can
	/// be re-included — and its top-most ignored path is recorded in
	/// [`ignored`](Self::ignored). An item the view could not place under an ignored path leaves
	/// [`skipped`](Self::skipped) too: it is out of sync, not unsyncable.
	///
	/// A [held](Self::held_paths) path stays held whatever the rules say. Both halves are
	/// byte-identical, so a rule hides them together, and a path the rules hide is blocked from
	/// every action anyway — holding it costs the pass nothing and it is untracked, with its rule
	/// reported, by the pass that finds the cache no longer mid-transition there.
	///
	/// `paths` says which nodes to ASK the rules about: every one of them
	/// ([`PassPaths::Whole`], what a pass that read the remote whole owes), or only the keys a
	/// change-scoped pass decided (see [`filter_changed`](Self::filter_changed)). A whole STAGED
	/// view is asked in path order, and only the first node of each subtree the rules hide is
	/// asked: the rest goes with it unasked, each node counted as an untracked default hit where
	/// that first one's hit is one.
	///
	/// A change-scoped pass gets back the decided keys the view still holds, in the set's order:
	/// what [`resolve_collisions_changed`](Self::resolve_collisions_changed) folds. Asking the
	/// rules about a key needs its node, and on a carried side that is a read of its row, which
	/// already says whether the view holds the key — the collision check asking again read every
	/// decided row twice. A whole pass has no decided set, and gets nothing back.
	pub(super) fn hide<'d>(
		&mut self,
		filter: ViewFilter<'_>,
		paths: PassPaths<'d>,
	) -> Vec<&'d String> {
		let mut memo = HashMap::new();
		let mut ignored = BTreeMap::new();
		let mut untracked = 0usize;
		// The root of what the rules hide at or above `rel_path`, if they hide it, recording it as
		// the pass reports and untracks one (see [`Baseline::tracked`]); and whether the item counts
		// on its own, as an untracked default hit does.
		let mut hidden = |rel_path: &str, is_dir: bool| {
			// The memo caches a decision per directory asked about, and a staged view is walked in
			// path order, where a directory's subtree is asked about together: a bound costs it
			// nothing, and keeps the memo from growing with the tree's directories.
			if memo.len() >= HIDE_MEMO {
				memo.clear();
			}
			let (ignored_root, decision) =
				filter.rules.ignored_root(rel_path, is_dir, &mut memo)?;
			// A root the rules hid ABOVE this item is a directory; the item's own hit is a root of
			// whatever kind the item is.
			let root_is_dir = ignored_root != rel_path || is_dir;
			// Hidden either way — but only a tracked default hit is a ROOT (see `Baseline::tracked`).
			let each = decision.level == IgnoreLevel::Default
				&& !filter.baseline.tracked(ignored_root, root_is_dir);
			if each {
				untracked += 1;
			} else if !ignored.contains_key(ignored_root) {
				ignored.insert(ignored_root.to_owned(), decision);
			}
			Some((ignored_root.to_owned(), each))
		};
		let kept = match paths {
			PassPaths::Whole => {
				match &mut self.nodes {
					// In path order, so a directory the rules hide is met before anything under it,
					// which goes with it unasked and is counted as it would have been asked about.
					Side::Staged(staged) => {
						let also = staged.drop_hidden(|rel_path, node| {
							hidden(rel_path, node.kind == NodeKind::Dir)
						});
						untracked += also;
					}
					nodes => nodes.retain(filter.baseline, |rel_path, node| {
						hidden(rel_path, node.kind == NodeKind::Dir).is_none()
					}),
				}
				Vec::new()
			}
			PassPaths::Changed(decided) => {
				let mut kept = Vec::new();
				let mut roots = Vec::new();
				for rel_path in decided {
					let Some(is_dir) = self
						.nodes
						.of(filter.baseline)
						.at(rel_path)
						.map(|node| node.kind == NodeKind::Dir)
					else {
						continue;
					};
					if hidden(rel_path, is_dir).is_some() {
						roots.push(rel_path);
					} else {
						kept.push(rel_path);
					}
				}
				for root in &roots {
					// Everything under a hidden directory goes with it, exactly as the whole form
					// drops it: nothing under an ignored directory can be re-included.
					//
					// That costs a subtree walk of the side per hit, where the whole form pays one
					// walk for the pass. Only a decided path that the rules hide pays it, which is
					// a remote create or move INTO an already-ignored directory and nothing else —
					// a rule that newly hides a directory forces a whole read instead. If that ever
					// stops being rare, the subtree is also named key-by-key in `decided` (every
					// re-key records both ends), so the walk can be dropped for a scan of the set.
					for under in self.nodes.subtree_paths(filter.baseline, root) {
						self.nodes.remove(filter.baseline, &under);
					}
					self.nodes.remove(filter.baseline, root);
				}
				// A kept key under a hidden directory left with it. Asked again only when the rules
				// hid something, which is the rare case the walk above already pays for.
				if !roots.is_empty() {
					kept.retain(|rel_path| self.nodes.of(filter.baseline).holds(rel_path));
				}
				kept
			}
		};
		self.ignored = ignored;
		self.ignored_default_untracked = untracked;
		// An unplaceable item's own record leaves with whatever hides it: the holding directory for
		// an undecodable one, whose name is unknown, and the would-be path for a name the remote
		// spells unsafely. A broken parent chain has no path to ask about. ASKED, not recorded —
		// the root of what hides such an item is the directory the rules hid, which is a node and
		// recorded as one; the item itself was never a root of its own.
		//
		// Asked about the kind the record's PATH is
		// ([`path_is_dir`](SkippedRemote::path_is_dir)), not the kind the item is: a file recorded
		// under a skipped directory carries that directory's path, and a rule written for
		// directories only (`build/`) hides the path it names or nothing at all. Asked as a file,
		// such a rule missed, the record outlived the directory it describes, and the pass reported
		// the very path the rule was written to silence.
		let mut is_hidden = |rel_path: &str, is_dir: bool| {
			filter
				.rules
				.ignored_root(rel_path, is_dir, &mut memo)
				.is_some()
		};
		self.skipped.retain(|skip| match &skip.reason {
			UnsyncableReason::RemoteUndecodable | UnsyncableReason::RemoteInvalidName { .. } => {
				!is_hidden(&skip.rel_path, skip.path_is_dir)
			}
			UnsyncableReason::InvalidName { .. }
			| UnsyncableReason::RepeatedFailure { .. }
			| UnsyncableReason::RemoteBrokenParent
			| UnsyncableReason::LocalAlias { .. } => true,
		});
		kept
	}

	/// The collision half of [`filter`](Self::filter): two remote items whose paths fold together
	/// case-insensitively have no 1:1 local mapping, so the loser leaves the view and the pass is
	/// refused ([`has_collisions`](Self::has_collisions)).
	pub(super) fn resolve_collisions(&mut self, baseline: &Baseline) {
		// A staged view groups its folded index instead: the first of each group in path order
		// stays, where this keeps whichever the map's order met first. The pass is refused either
		// way, and neither keeps a node per item to find the groups.
		if let Side::Staged(staged) = &mut self.nodes {
			self.has_collisions |= staged.drop_fold_twins(&self.held_paths);
			return;
		}
		// A digest of every collision key taken so far. The keys themselves are not kept: a hit is
		// rare and is resolved against the paths already placed, which is what tells a real
		// case-twin from two keys that merely share a digest — a pass is never refused over that.
		let mut claimed: HashSet<u128> =
			HashSet::with_capacity(self.nodes.of(baseline).len() + self.held_paths.len());
		// A held path took its key when the view held it, so a case-variant of one still collides.
		claimed.extend(
			self.held_paths
				.iter()
				.map(|path| collision_hash(&collision_key(path))),
		);
		let mut clashes: Vec<String> = Vec::new();
		for rel_path in self.nodes.of(baseline).paths() {
			if !claimed.insert(collision_hash(&collision_key(&rel_path))) {
				clashes.push(rel_path.into_owned());
			}
		}
		for rel_path in clashes {
			let key = collision_key(&rel_path);
			// Which item already folded that way — asked on the error path only. A held path
			// counts: both halves of such a name were taken back out of `nodes`.
			let folds_like = |taken: &str| taken != rel_path && collision_key(taken) == key;
			let folds_onto = self
				.nodes
				.of(baseline)
				.paths()
				.any(|taken| folds_like(&taken))
				|| self.held_paths.iter().any(|taken| folds_like(taken));
			if folds_onto {
				tracing::debug!(
					"remote view: {rel_path:?} folds onto another remote item's name, so no 1:1 local mapping exists"
				);
				self.has_collisions = true;
				self.nodes.remove(baseline, &rel_path);
			}
		}
	}

	/// [`resolve_collisions`](Self::resolve_collisions) for a view a change-scoped pass DERIVED,
	/// which folds only the keys that pass decided.
	///
	/// The pass's CARRIED keys are not folded against each other, and that is not a gap. A pass
	/// that finds a collision REFUSES, and a refusal forces the next pass to read both sides whole
	/// (`next_pass_scope`'s `FullPassReason::PreviousRefusal`). So every pass since the last whole
	/// read folded exactly the keys it could itself have introduced — its decided set, which is
	/// what this does — and by induction no two carried keys fold together undetected. The whole
	/// read that starts the chain folded every one of them.
	///
	/// Each decided key is folded against three things, which together are every other key the view
	/// can hold: the other decided keys, the held paths (a held path took its key when the view
	/// held it), and the carried keys — asked of the BASELINE, whose rows are what a carried node
	/// is, so the question costs the path's own depth instead of a scan
	/// ([`Baseline::folded_row_paths`]).
	///
	/// A row the baseline names is only a collision where the view still HOLDS it: the delta may
	/// have detached that node, and refusing a pass over a row nothing is reconciling against
	/// would stall the pair for as long as the row lasted. Real keys are compared rather than
	/// digests of them, which the whole form uses to keep one `u128` per node instead of a second
	/// copy of every path — the decided set is a handful of paths, so there is nothing to save.
	///
	/// `kept` is the decided keys the view holds, in path order: what [`hide`](Self::hide) hands
	/// back, having read each one's node for the rules already. A decided key the view does not
	/// hold claims no name, and the order is the order the kept ones claim theirs in.
	/// `pub(super)` for the same reason [`hide`](Self::hide) is: the probe times the two halves of
	/// the scoped filter apart.
	pub(super) fn resolve_collisions_changed<'d>(
		&mut self,
		baseline: &Baseline,
		kept: impl IntoIterator<Item = &'d String>,
	) {
		let mut claimed: HashMap<String, String> = HashMap::new();
		let mut clashes: Vec<String> = Vec::new();
		for rel_path in kept {
			let key = collision_key(rel_path);
			let folds_like = |taken: &str| taken != rel_path && collision_key(taken) == key;
			let onto = claimed
				.get(&key)
				.filter(|taken| folds_like(taken))
				.cloned()
				.or_else(|| {
					self.held_paths
						.iter()
						.find(|held| folds_like(held))
						.cloned()
				})
				.or_else(|| {
					baseline
						.folded_row_paths(rel_path)
						.into_iter()
						.find(|row| folds_like(row) && self.nodes.of(baseline).holds(row))
				});
			if let Some(onto) = onto {
				tracing::debug!(
					"remote view: {rel_path:?} folds onto {onto:?}, so no 1:1 local mapping exists"
				);
				clashes.push(rel_path.clone());
			} else {
				claimed.insert(key, rel_path.clone());
			}
		}
		for rel_path in clashes {
			self.has_collisions = true;
			self.nodes.remove(baseline, &rel_path);
		}
	}
}

/// [`place_remote_items`] + [`RemoteView::filter`] in one call, for a caller that already has the
/// rules to hand. A pass builds the two halves apart, so the reads it owes the unfiltered view can
/// happen in between.
///
/// Every test that builds a view this way builds its staged twin too — the same items copied into
/// a stage and placed there, as a whole pass reads them, then filtered alike — and holds it to the
/// one returned (see `stage::assert_views_alike`).
#[cfg(test)]
pub(crate) fn build_remote_view(
	root: Uuid,
	dirs: &[RemoteItem],
	files: &[RemoteItem],
	undecodable: &[UndecodableItem],
	filter: Option<ViewFilter<'_>>,
) -> RemoteView {
	let mut view = place_remote_items(root, dirs, files, undecodable);
	view.filter(filter);
	let items = dirs
		.iter()
		.map(|dir| (true, dir.clone()))
		.chain(files.iter().map(|file| (false, file.clone())));
	let (_dir, mut staged) = super::stage::staged_view(root, items, undecodable);
	staged.filter(filter);
	super::stage::assert_views_alike("the staged twin", &view, &staged);
	view
}

/// The skipped items of a view being built: the ones recorded with a reason of their own, and the
/// ones under a skipped directory, which take that directory's record once every item is placed.
#[derive(Default)]
struct Skipped {
	/// The records that belong at the HEAD of the answer: the undecodable ones, which a
	/// materialized build resolved before it placed anything and a streamed build can only
	/// resolve once every directory has arrived. Keeping them apart is what makes the two builds
	/// answer identically, in the same order, however the rows came in.
	first: Vec<SkippedRemote>,
	recorded: Vec<SkippedRemote>,
	/// Every item under a skipped directory: its uuid, whole-life id, and the nearest skipped
	/// ancestor `resolve_parent` found.
	under: Vec<(Uuid, Option<StableUuid>, Uuid)>,
}

impl Skipped {
	/// Record an item with its own reason — unless it sits at a reserved path (the quarantine dir,
	/// filen-sync's trash), which the view leaves out without a word (and its descendants with it).
	fn record(&mut self, skip: SkippedRemote) {
		if is_reserved_path(&skip.rel_path) {
			return;
		}
		self.recorded.push(skip);
	}

	/// [`record`](Self::record) for a record that belongs at the head of the answer (see
	/// [`first`](Self::first)).
	fn record_first(&mut self, skip: SkippedRemote) {
		if is_reserved_path(&skip.rel_path) {
			return;
		}
		self.first.push(skip);
	}

	fn under(&mut self, remote_uuid: Uuid, stable_uuid: Option<StableUuid>, ancestor: Uuid) {
		self.under.push((remote_uuid, stable_uuid, ancestor));
	}

	/// Resolve every item under a skipped directory to the record of the directory that has one: the
	/// nearest skipped ancestor may itself sit under another, so the walk climbs, one skipped
	/// directory per step. An item whose chain ends in nothing recorded (a reserved path) stays out
	/// unrecorded.
	fn finish(mut self) -> Vec<SkippedRemote> {
		let mut recorded = std::mem::take(&mut self.first);
		recorded.append(&mut self.recorded);
		// uuid -> its index in `recorded`, built once the two halves are in their final order.
		// The records this loop appends are not in it, exactly as they were not before: an item
		// under a skipped directory is not itself a directory anything sits under.
		let by_uuid: HashMap<Uuid, usize> = recorded
			.iter()
			.enumerate()
			.map(|(index, record)| (record.remote_uuid, index))
			.collect();
		let parent_of: HashMap<Uuid, Uuid> = self
			.under
			.iter()
			.map(|&(uuid, _, ancestor)| (uuid, ancestor))
			.collect();
		for &(remote_uuid, stable_uuid, ancestor) in &self.under {
			let mut at = ancestor;
			for _ in 0..=MAX_REMOTE_DEPTH {
				if let Some(&index) = by_uuid.get(&at) {
					let record = &recorded[index];
					let (rel_path, reason) = (record.rel_path.clone(), record.reason.clone());
					recorded.push(SkippedRemote {
						remote_uuid,
						stable_uuid,
						rel_path,
						// `under` is only reached through `Unresolved::UnderSkipped`, whose ancestor
						// is always a DIRECTORY — an unsafe-named one, an undecodable one, or one
						// whose own parent is missing — and a directory's record is either its own
						// would-be path or the directory that holds it. So the path this record
						// takes names a directory whatever the item under it is, and the rules have
						// to be asked about it as one.
						path_is_dir: true,
						reason,
					});
					break;
				}
				match parent_of.get(&at) {
					Some(&up) => at = up,
					None => break,
				}
			}
		}
		recorded
	}
}

/// The remote ids [`unknown_remote_paths`] will ask about, for the caller to resolve against the
/// store first (see [`SyncedPaths`]).
///
/// The ONE place that enumeration lives. A caller that resolved a smaller set would hand
/// `unknown_remote_paths` an id it cannot answer for, and an unanswerable id reads as "never
/// synced" — which drops the path's protection and lets the pass plan a local delete over a remote
/// item that is still there.
pub(crate) fn skipped_ids(skipped: &[SkippedRemote]) -> (Vec<Uuid>, Vec<StableUuid>) {
	(
		skipped.iter().map(|item| item.remote_uuid).collect(),
		skipped.iter().filter_map(|item| item.stable_uuid).collect(),
	)
}

/// Split the view's skipped items into the SYNCED paths they make unknown and the reports for items
/// never synced.
///
/// A baseline row whose remote item — by uuid, or for a file by its whole-life id, which survives
/// the new version a foreign same-name upload makes — is among the skipped was not deleted: the
/// item is still there, only the view cannot say where it is or what it holds. Planning such a path
/// from its absence would quarantine the local copy (or re-upload it over the item), and one such
/// path is far below what the mass-delete guard holds. So it is returned keyed by its baseline path
/// with its reason, for the pass to leave that path and its subtree alone and report it. A skipped
/// item no baseline row names is only reported, under its would-be path.
pub(crate) fn unknown_remote_paths(
	synced: &SyncedPaths,
	skipped: &[SkippedRemote],
) -> (BTreeMap<String, UnsyncableReason>, Vec<UnsyncablePath>) {
	// Nothing was skipped, so nothing is looked up at all.
	if skipped.is_empty() {
		return (BTreeMap::new(), Vec::new());
	}
	let mut unknown = BTreeMap::new();
	let mut never_synced: Vec<UnsyncablePath> = Vec::new();
	for item in skipped {
		// The baseline's own uuid and lineage indexes, rather than two maps built over every row
		// of the pair to answer one question per skipped item.
		let synced_at = synced.path_by_uuid(item.remote_uuid).or_else(|| {
			item.stable_uuid
				.and_then(|lineage| synced.path_by_lineage(lineage))
		});
		if let Some(path) = synced_at {
			unknown.insert(path, item.reason.clone());
			continue;
		}
		let report = UnsyncablePath {
			rel_path: item.rel_path.clone(),
			reason: item.reason.clone(),
		};
		// Two undecodable items in one directory, or the items under one skipped directory, report
		// the same line; once is enough.
		if !never_synced.contains(&report) {
			never_synced.push(report);
		}
	}
	// A synced directory's subtree is already left alone with it: its synced descendants add no
	// line of their own.
	let unknown_dirs: Vec<String> = unknown.keys().cloned().collect();
	unknown.retain(|path, _| !unknown_dirs.iter().any(|dir| is_under(path, dir)));
	(unknown, never_synced)
}

/// How one side compares to the baseline at a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SideState {
	/// Neither the side nor the baseline has the path.
	Absent,
	/// Present and matching the baseline.
	Unchanged,
	/// Present with no baseline (newly appeared).
	Created,
	/// Present but diverged from the baseline.
	Modified,
	/// Gone, but the baseline had it.
	Deleted,
}

impl SideState {
	fn changed(self) -> bool {
		matches!(self, Self::Created | Self::Modified | Self::Deleted)
	}
}

fn classify_local(node: Option<&LocalNode>, base: Option<&BaselineEntry>) -> SideState {
	match (node, base) {
		(None, None) => SideState::Absent,
		(Some(_), None) => SideState::Created,
		(None, Some(_)) => SideState::Deleted,
		(Some(node), Some(base)) => {
			if node.kind != base.kind {
				SideState::Modified
			} else {
				match node.kind {
					NodeKind::Dir => SideState::Unchanged,
					NodeKind::File => {
						if node.content_hash.is_some() && node.content_hash == base.content_hash {
							SideState::Unchanged
						} else {
							SideState::Modified
						}
					}
				}
			}
		}
	}
}

/// Whether the remote item at a path is the SAME file the baseline row recorded there — the same
/// server-minted lineage, whatever version id it currently wears. `false` whenever either side has
/// no lineage id: an unknown lineage is not evidence of sameness.
pub(super) fn same_file_lineage(base: &BaselineEntry, node: &RemoteNode) -> bool {
	matches!(
		(base.remote_stable_uuid, node.stable_uuid),
		(Some(recorded), Some(current)) if recorded == current
	)
}

fn classify_remote(node: Option<&RemoteNode>, base: Option<&BaselineEntry>) -> SideState {
	match (node, base) {
		(None, None) => SideState::Absent,
		(Some(_), None) => SideState::Created,
		(None, Some(_)) => SideState::Deleted,
		(Some(node), Some(base)) => {
			if node.kind != base.kind {
				SideState::Modified
			} else {
				match node.kind {
					NodeKind::File => classify_remote_file(node, base),
					NodeKind::Dir => SideState::Unchanged,
				}
			}
		}
	}
}

/// How a remote FILE compares to its baseline row. Three cases, told apart by the two ids the
/// server gives a file:
///
/// - the SAME version uuid: nothing has happened to it;
/// - a new version uuid of the SAME lineage: an EDIT — but only where the content actually moved. A
///   re-upload of identical bytes (another client pushing what we already have, a restore of the
///   version we are already on) re-mints the uuid without changing anything, and reading that as a
///   remote edit turns a one-sided LOCAL edit into a conflict that has no second side;
/// - a DIFFERENT lineage, or one neither side can name: a REPLACEMENT — another file has taken the
///   path over. It reconciles exactly like an edit, since the destination has to converge on
///   whatever holds the path either way. Where the distinction is load-bearing is
///   [`PendingWrites::settle`](super::engine::PendingWrites): another client's edit of the file we
///   just pushed is a two-way conflict, its replacement of it is not.
fn classify_remote_file(node: &RemoteNode, base: &BaselineEntry) -> SideState {
	if base.remote_uuid == Some(node.remote_uuid) {
		return SideState::Unchanged;
	}
	let identical_content = matches!(
		(base.content_hash, node.content_hash),
		(Some(recorded), Some(current)) if recorded == current
	);
	if same_file_lineage(base, node) && identical_content {
		SideState::Unchanged
	} else {
		SideState::Modified
	}
}

/// Whether the local file matches the remote file by content. Prefers a direct hash comparison;
/// when the remote has no stored hash, falls back to the baseline (same local hash + same remote
/// version) so an old hashless remote file is not re-transferred every pass.
fn remote_matches_local(
	local: &LocalNode,
	remote: &RemoteNode,
	base: Option<&BaselineEntry>,
) -> bool {
	if local.kind != remote.kind {
		return false;
	}
	match local.kind {
		NodeKind::Dir => true,
		NodeKind::File => match (local.content_hash, remote.content_hash) {
			(Some(l), Some(r)) => l == r,
			// Remote hash missing: trust the baseline if it records this exact local content
			// already synced to the remote's current version.
			_ => base.is_some_and(|b| {
				b.content_hash == local.content_hash && b.remote_uuid == Some(remote.remote_uuid)
			}),
		},
	}
}

/// Do the local and remote nodes represent the same content right now (the two-way convergence
/// check)? Both-absent counts as agreement (both deleted).
fn nodes_agree(local: Option<&LocalNode>, remote: Option<&RemoteNode>) -> bool {
	match (local, remote) {
		(None, None) => true,
		(Some(l), Some(r)) => {
			l.kind == r.kind
				&& match l.kind {
					NodeKind::Dir => true,
					NodeKind::File => l.content_hash.is_some() && l.content_hash == r.content_hash,
				}
		}
		_ => false,
	}
}

/// Whether the baseline already records exactly what both sides currently hold at a path, so there
/// is nothing to adopt. `(None, None)` agrees only when there is no row at all.
fn baseline_is_current(
	local: Option<&LocalNode>,
	remote: Option<&RemoteNode>,
	base: Option<&BaselineEntry>,
) -> bool {
	match (local, remote, base) {
		(None, None, base) => base.is_none(),
		(Some(local), Some(remote), Some(base)) => {
			base.kind == local.kind
				&& base.remote_uuid == Some(remote.remote_uuid)
				&& match local.kind {
					NodeKind::Dir => true,
					NodeKind::File => base.content_hash == local.content_hash,
				}
		}
		_ => false,
	}
}

/// Record an already-converged path into the baseline when the baseline does not (yet) say so.
///
/// The one-way paths transfer nothing when the two sides already agree, so without this a pair
/// opened on an already-synced tree would never seed a baseline at all: every later pass would
/// still see an empty baseline, read as a first sync, and have all its deletions held by the guard.
/// It is also what retires a stale row for a path both sides have since dropped.
fn adopt_if_baseline_stale(
	rel_path: &str,
	local: Option<&LocalNode>,
	remote: Option<&RemoteNode>,
	base: Option<&BaselineEntry>,
	actions: &mut Vec<SyncAction>,
) {
	if baseline_is_current(local, remote, base) {
		return;
	}
	let action = SyncAction::AdoptBaseline {
		rel_path: rel_path.to_string(),
	};
	tracing::trace!(
		"plan: {} — the two sides already agree; recording the baseline",
		action.describe()
	);
	actions.push(action);
}

/// Make the remote match the local at one path (push). `delete_ok` gates deletions (off for backup
/// modes, which never delete on their destination).
fn push_to_remote(
	rel_path: &str,
	local: Option<&LocalNode>,
	remote: Option<&RemoteNode>,
	base: Option<&BaselineEntry>,
	delete_ok: bool,
	actions: &mut Vec<SyncAction>,
) {
	match (local, remote) {
		(Some(local), None) => {
			let action = create_remote(rel_path, local.kind);
			tracing::trace!(
				"plan: {} — new locally, absent on remote",
				action.describe()
			);
			actions.push(action);
		}
		(Some(local), Some(remote)) => {
			if !remote_matches_local(local, remote, base) {
				if local.kind != remote.kind {
					// Type flip: trash the stale remote node, then create the new kind.
					let trash = SyncAction::TrashRemote {
						rel_path: rel_path.to_string(),
						kind: remote.kind,
						remote_uuid: remote.remote_uuid,
					};
					tracing::trace!("plan: {} — local/remote kind differs", trash.describe());
					actions.push(trash);
				}
				let action = create_remote(rel_path, local.kind);
				tracing::trace!(
					"plan: {} — local content differs from remote",
					action.describe()
				);
				actions.push(action);
			} else {
				adopt_if_baseline_stale(rel_path, Some(local), Some(remote), base, actions);
			}
		}
		(None, Some(remote)) => {
			if delete_ok {
				let trash = SyncAction::TrashRemote {
					rel_path: rel_path.to_string(),
					kind: remote.kind,
					remote_uuid: remote.remote_uuid,
				};
				tracing::trace!("plan: {} — gone locally (deletion)", trash.describe());
				actions.push(trash);
			}
		}
		(None, None) => adopt_if_baseline_stale(rel_path, None, None, base, actions),
	}
}

fn create_remote(rel_path: &str, kind: NodeKind) -> SyncAction {
	match kind {
		// Files: a same-name upload versions the existing remote file, so create and update are
		// the same action.
		NodeKind::File => SyncAction::UploadFile {
			rel_path: rel_path.to_string(),
		},
		NodeKind::Dir => SyncAction::CreateRemoteDir {
			rel_path: rel_path.to_string(),
		},
	}
}

/// Make the local match the remote at one path (pull). `delete_ok` gates deletions.
fn pull_to_local(
	rel_path: &str,
	local: Option<&LocalNode>,
	remote: Option<&RemoteNode>,
	base: Option<&BaselineEntry>,
	delete_ok: bool,
	actions: &mut Vec<SyncAction>,
) {
	match (remote, local) {
		(Some(remote), None) => {
			let action = create_local(rel_path, remote);
			tracing::trace!(
				"plan: {} — new on remote, absent locally",
				action.describe()
			);
			actions.push(action);
		}
		(Some(remote), Some(local)) => {
			if !remote_matches_local(local, remote, base) {
				if local.kind != remote.kind {
					let del = SyncAction::DeleteLocal {
						rel_path: rel_path.to_string(),
						kind: local.kind,
					};
					tracing::trace!("plan: {} — local/remote kind differs", del.describe());
					actions.push(del);
				}
				let action = create_local(rel_path, remote);
				tracing::trace!(
					"plan: {} — remote content differs from local",
					action.describe()
				);
				actions.push(action);
			} else {
				adopt_if_baseline_stale(rel_path, Some(local), Some(remote), base, actions);
			}
		}
		(None, Some(local)) => {
			if delete_ok {
				let del = SyncAction::DeleteLocal {
					rel_path: rel_path.to_string(),
					kind: local.kind,
				};
				tracing::trace!("plan: {} — gone on remote (deletion)", del.describe());
				actions.push(del);
			}
		}
		(None, None) => adopt_if_baseline_stale(rel_path, None, None, base, actions),
	}
}

fn create_local(rel_path: &str, remote: &RemoteNode) -> SyncAction {
	match remote.kind {
		NodeKind::File => SyncAction::DownloadFile {
			rel_path: rel_path.to_string(),
			remote_uuid: remote.remote_uuid,
		},
		NodeKind::Dir => SyncAction::CreateLocalDir {
			rel_path: rel_path.to_string(),
		},
	}
}

/// `row`, confirmed: its agreed-content marker advanced to the content it records. What a
/// confirmation IS, whatever the evidence — and the row as the caller persists it.
pub(super) fn confirm_agreed(row: BaselineEntry) -> BaselineEntry {
	BaselineEntry {
		agreed_hash: row.content_hash,
		..row
	}
}

/// Rule (c): whether this pass's RAW cache snapshot confirms `entry`, a row awaiting confirmation —
/// `at` being what the snapshot holds at the row's path.
///
/// The confirmation is one specific observation: the snapshot lists the row's own version uuid at
/// the row's path. That is the remote saying it holds exactly what the row records, which is what
/// makes the row's content agreed by both sides. A push cannot claim this for itself — the upload
/// only proves the server took the bytes — so this is the step that retires the gap a push leaves.
///
/// It MUST be given the raw snapshot, never the view
/// [`PendingWrites::fold_into`](super::engine::PendingWrites::fold_into) has corrected: the fold
/// synthesises our own just-written version at the path from the very row being confirmed, so a
/// folded view would confirm every push against itself and the marker would mean nothing.
pub(super) fn snapshot_confirms(at: Option<&RemoteNode>, entry: &BaselineEntry) -> bool {
	let confirmed = at.map(|node| node.remote_uuid) == entry.remote_uuid;
	if confirmed {
		tracing::trace!(
			"plan: the remote confirms {:?} — both sides hold what the baseline records",
			entry.rel_path
		);
	}
	confirmed
}

/// Whether `entry`, a row awaiting confirmation, carries a version uuid in `vouched` — the pushes
/// some other evidence than a snapshot has vouched for (a tenure as the remote head, or the server's
/// own version chain).
///
/// Split from [`snapshot_confirms`] because the evidence comes from outside the plan (the cache's
/// announcement timestamps, a version listing) while the row rule is the same; keeping the rule
/// here keeps the pass's confirmation policy in one place and unit-testable.
pub(super) fn push_vouched(vouched: &HashSet<Uuid>, entry: &BaselineEntry) -> bool {
	let stood = entry
		.remote_uuid
		.is_some_and(|uuid| vouched.contains(&uuid));
	if stood {
		tracing::trace!(
			"plan: {:?} — this engine's push stood as the remote head long enough to count as agreed",
			entry.rel_path
		);
	}
	stood
}

/// What the server's version chain says about a push of ours: how long it stood before anything
/// landed on top of it.
///
/// `versions` is the lineage as [`Client::list_file_versions`](crate::auth::Client::list_file_versions)
/// returns it and `superseded_by` is the version the pass's snapshot found at the row's path.
/// `Some(true)` when ours stood for at least `tenure`.
///
/// The tenure is measured against the EARLIEST version that is not ours and is not older than ours
/// — never against `superseded_by` itself, which is only the version the path carries now. A client
/// that edited beside us and then edited again minutes later leaves both on the chain, and dating
/// ours against the later one would read a concurrent edit as consent and pull over our bytes.
/// `superseded_by` is required to be on the chain (a chain that does not carry it is not answering
/// about this path) and is a candidate like any other.
///
/// Deliberately not read from the chain's ORDER: the server stamps versions to the SECOND, so the
/// two versions of one race carry the same stamp and their relative order in the listing is
/// arbitrary. A tie therefore measures as no tenure at all — the safe direction, and the right
/// answer for a race. The version our upload replaced ties with ours the same way if the two landed
/// in one second, which costs a confirmation the row could have had; a conflict surfaced is the
/// direction to be wrong in.
///
/// `None` when the chain does not carry ours or `superseded_by` — a versioning-disabled account
/// keeps only the head — and when nothing on it is newer than ours at all, which a lineage somebody
/// has restored an old version into can look like. No evidence either way.
///
/// This is the evidence of last resort — it needs no observation of ours, so it answers for a row a
/// restarted engine knows nothing about.
pub(super) fn version_chain_verdict(
	versions: &[(Uuid, chrono::DateTime<chrono::Utc>)],
	ours: Uuid,
	superseded_by: Uuid,
	tenure: std::time::Duration,
) -> Option<bool> {
	let at = |wanted: Uuid| {
		versions
			.iter()
			.find(|(uuid, _)| *uuid == wanted)
			.map(|(_, at)| *at)
	};
	let ours_at = at(ours)?;
	// The version the snapshot shows has to be on the chain, or it is not this file's chain.
	at(superseded_by)?;
	let first_after = versions
		.iter()
		.filter(|(uuid, at)| *uuid != ours && *at >= ours_at)
		.map(|(_, at)| *at)
		.min()?;
	let stood_for = first_after.signed_duration_since(ours_at);
	Some(stood_for.to_std().is_ok_and(|stood| stood >= tenure))
}

/// The version our own upload buried, if it buried one.
///
/// `versions` is the lineage as the server lists it — with the versions this engine minted itself
/// already filtered out, bar `replaced` — `ours` the version the upload minted and `replaced` the
/// version the pass's snapshot said the path held. A version that is neither, and that is later
/// than `replaced`, landed between the pass reading the remote and our upload: another client's
/// edit that a same-name upload versioned rather than refused — our bytes are on top of it now, and
/// its author has no way of knowing. The earliest such version is the one our upload buried.
///
/// Not read from the chain's ORDER, which cannot answer it: the server records a version's time to
/// the second, so two versions of one second sort arbitrarily, and our own previous upload is
/// routinely listed above the version it was later replaced by. The comparison is strict for the
/// same reason — inside one second the chain cannot say which of two versions came first, and
/// guessing there costs a conflict on a path nobody but us touched. What that gives up is an
/// interleave that landed in the very second of the version it displaced; the client that made it
/// still surfaces its own side of the divergence. The caller strips our own recent uploads first,
/// which is what the stamps cannot do at all.
///
/// `None` when nothing on the chain is later, and when the chain does not carry `replaced` at
/// all — a versioning-disabled account keeps only the head, which is no evidence either way.
///
/// A version that landed on top of OURS between the upload and this listing is later than
/// `replaced` too, and is reported the same way. It is a concurrent edit whichever of the two is
/// the remote head, and the resolution reads that from the server.
pub(super) fn interleaved_version(
	versions: &[(Uuid, chrono::DateTime<chrono::Utc>)],
	ours: Uuid,
	replaced: Uuid,
) -> Option<Uuid> {
	let replaced_at = versions
		.iter()
		.find(|(uuid, _)| *uuid == replaced)
		.map(|(_, at)| *at)?;
	versions
		.iter()
		.filter(|(uuid, at)| *uuid != ours && *uuid != replaced && *at > replaced_at)
		.min_by_key(|(_, at)| *at)
		.map(|(uuid, _)| *uuid)
}

/// The baseline rows a [`Backlog::AdoptDestination`](super::mode::Backlog::AdoptDestination) mode
/// switch writes: one per path the NEW mode's source side has nothing at while the destination
/// holds a copy — the pair's standing backlog, plus whatever else the destination has accumulated.
///
/// Each row records what the destination holds RIGHT NOW (from the same snapshot and scan a pass
/// reads), marked [`BaselineState::Adopted`]: the copy is intended, not a deletion waiting to be
/// propagated. What that means per mode is [`reconcile`]'s business, not this function's.
///
/// Two sources, both destination-only:
/// - a TRACKED path (a `Synced` row) the source has lost: the standing backlog a backup mode
///   accumulated.
/// - an UNTRACKED path — no baseline row at all — that only the destination holds: a file another
///   client created straight on the destination. It has never been the pair's to delete, and
///   without a row a one-way mirror trashes it on the very next pass, so the switch adopts it too.
///   Only in the ONE-WAY modes: two-way has no destination to spare, an untracked copy already
///   flows back to the other side under the ordinary rules, and a row here would only take the path
///   out of move detection (see [`detect_moves`]).
///
/// A held conflict is excluded from both: it is the caller's to
/// [`resolve_conflict`](super::SyncEngine::resolve_conflict), and overwriting the row with an
/// adoption would drop the divergence it is holding.
pub(crate) fn adopt_destination_rows(
	mode: super::SyncMode,
	baseline: &Baseline,
	local: &impl Nodes<Node = LocalNode>,
	remote: &impl Nodes<Node = RemoteNode>,
) -> Vec<BaselineEntry> {
	let mut rows = Vec::new();
	for base in baseline.iter() {
		if base.state != BaselineState::Synced {
			continue;
		}
		let rel_path = &base.rel_path;
		let (local_at, remote_at) = (local.at(rel_path), remote.at(rel_path));
		let (local_node, remote_node) = (local_at.as_deref(), remote_at.as_deref());
		let row = match mode {
			super::SyncMode::LocalToRemote | super::SyncMode::LocalBackup => remote_node
				.filter(|_| local_node.is_none())
				.map(|node| adopted_from_remote(rel_path, node)),
			super::SyncMode::RemoteToLocal | super::SyncMode::RemoteBackup => local_node
				.filter(|_| remote_node.is_none())
				.map(|node| adopted_from_local(rel_path, node)),
			// Either side may be the one that kept the item; a path both sides still hold is not a
			// standing deletion at all.
			super::SyncMode::TwoWay => match (local_node, remote_node) {
				(None, Some(node)) => Some(adopted_from_remote(rel_path, node)),
				(Some(node), None) => Some(adopted_from_local(rel_path, node)),
				_ => None,
			},
		};
		if let Some(row) = row {
			tracing::trace!(
				"reconfigure: adopting the destination's copy of {rel_path:?} — the source no longer has it"
			);
			rows.push(row);
		}
	}
	let untracked = |rel_path: &str| !baseline.contains_key(rel_path);
	match mode {
		super::SyncMode::LocalToRemote | super::SyncMode::LocalBackup => {
			for (rel_path, node) in remote.iter().filter(|(path, _)| untracked(path)) {
				if local.holds(&rel_path) {
					continue;
				}
				tracing::trace!(
					"reconfigure: adopting the destination-only item {rel_path:?} — the pair never tracked it"
				);
				rows.push(adopted_from_remote(&rel_path, &node));
			}
		}
		super::SyncMode::RemoteToLocal | super::SyncMode::RemoteBackup => {
			for (rel_path, node) in local.iter().filter(|(path, _)| untracked(path)) {
				if remote.holds(&rel_path) {
					continue;
				}
				tracing::trace!(
					"reconfigure: adopting the destination-only item {rel_path:?} — the pair never tracked it"
				);
				rows.push(adopted_from_local(&rel_path, &node));
			}
		}
		super::SyncMode::TwoWay => {}
	}
	rows
}

/// An [`Adopted`](BaselineState::Adopted) row anchored to what the REMOTE holds at a path.
fn adopted_from_remote(rel_path: &str, node: &RemoteNode) -> BaselineEntry {
	let is_file = node.kind == NodeKind::File;
	BaselineEntry {
		rel_path: rel_path.to_string(),
		kind: node.kind,
		remote_uuid: Some(node.remote_uuid),
		content_hash: is_file.then_some(node.content_hash).flatten(),
		size: is_file.then_some(node.size),
		local_mtime: None,
		remote_modified: is_file.then_some(node.modified_millis),
		state: BaselineState::Adopted,
		local_kind: None,
		remote_kind: None,
		remote_hash: None,
		remote_size: None,
		remote_stable_uuid: node.stable_uuid,
		// Nothing is agreed: only one side holds this path.
		agreed_hash: None,
	}
}

/// An [`Adopted`](BaselineState::Adopted) row anchored to what the LOCAL tree holds at a path.
fn adopted_from_local(rel_path: &str, node: &LocalNode) -> BaselineEntry {
	let is_file = node.kind == NodeKind::File;
	BaselineEntry {
		rel_path: rel_path.to_string(),
		kind: node.kind,
		remote_uuid: None,
		content_hash: is_file.then_some(node.content_hash).flatten(),
		size: is_file.then_some(node.size),
		local_mtime: Some(node.mtime_millis),
		remote_modified: None,
		state: BaselineState::Adopted,
		local_kind: None,
		remote_kind: None,
		remote_hash: None,
		remote_size: None,
		remote_stable_uuid: None,
		agreed_hash: None,
	}
}

/// Whether the remote change at a path is another client's edit made CONCURRENTLY with a push of
/// ours that no snapshot ever confirmed — the one remote change a two-way pass must NOT just pull.
///
/// The row holds our own content and the version uuid our push minted; `agreed_hash` holds the last
/// content both sides were known to hold. Equal, and the push was confirmed (a pass saw our version
/// as the remote head), so a foreign version stacked on top of it is an edit made AFTER ours and
/// pulling it is right — that is the ordinary sequential case. Different, and our push was never
/// observed to land while a foreign version of the SAME file now carries content that is neither
/// ours nor the agreed one: both sides moved off the agreed content, which is a conflict.
///
/// Deliberately narrow — a file on both sides, the same server-minted lineage (a DIFFERENT lineage
/// is a replacement and converges the ordinary way), a different version uuid, and hashes on both
/// sides that actually differ.
///
/// `agreed_hash: None` — a file this side created and pushed, nothing having confirmed it since —
/// is an UNCONFIRMED push like any other, not an absence of evidence: the two clients that create
/// the same name concurrently both hold such a row, and treating the marker's absence as consent
/// resolves that silently in whichever client's favour happened to look second. Only a snapshot
/// listing our own version at the path (`agreed_hash == content_hash`) makes a foreign version on
/// top of it an edit made AFTER ours.
///
/// A snapshot listing our version is not the only thing that can confirm it: a push that stood as
/// the remote head for [`CONFIRM_TENURE`](super::engine::CONFIRM_TENURE) counts too, dated either
/// from the cache's own announcements or from the server's version chain (see
/// `SyncEngine::confirm_pushes`). That is what keeps a pair that was paused for an hour — no pass,
/// so nothing a snapshot could confirm — from reading every foreign edit made since as a conflict.
/// What stays a conflict is the case the rule is for: a foreign version that landed on ours INSIDE
/// that window, which is what a genuinely concurrent edit looks like.
///
/// TWO-WAY ONLY. A one-way mode has an authoritative side and no divergence to surface:
/// [`SyncMode::LocalToRemote`](super::SyncMode::LocalToRemote) re-pushes the local copy and
/// [`SyncMode::RemoteToLocal`](super::SyncMode::RemoteToLocal) pulls the remote one, exactly as
/// before.
fn is_unconfirmed_concurrent_edit(
	local: Option<&LocalNode>,
	remote: Option<&RemoteNode>,
	base: Option<&BaselineEntry>,
) -> bool {
	let (Some(local), Some(remote), Some(base)) = (local, remote, base) else {
		return false;
	};
	local.kind == NodeKind::File
		&& remote.kind == NodeKind::File
		&& base.kind == NodeKind::File
		// Our push is still unconfirmed: the row's content is not what both sides last agreed on.
		// `None` is not agreement — nothing has ever confirmed this row's content.
		&& base.content_hash != base.agreed_hash
		// A foreign VERSION of the same file — not our own, and not a different file taking over.
		&& base.remote_uuid != Some(remote.remote_uuid)
		&& same_file_lineage(base, remote)
		// ... carrying content that is genuinely not ours. An identical re-upload is no divergence,
		// and a remote with no stored hash is no evidence of one.
		&& matches!(
			(base.content_hash, remote.content_hash),
			(Some(ours), Some(theirs)) if ours != theirs
		)
}

fn reconcile_two_way(
	rel_path: &str,
	local: Option<&LocalNode>,
	remote: Option<&RemoteNode>,
	base: Option<&BaselineEntry>,
	actions: &mut Vec<SyncAction>,
) {
	let local_side = classify_local(local, base);
	let remote_side = classify_remote(remote, base);

	match (local_side.changed(), remote_side.changed()) {
		// Already in sync (relative to the baseline).
		(false, false) => {}
		// Only one side moved — propagate it (deletes always propagate in two-way).
		(true, false) => push_to_remote(rel_path, local, remote, base, true, actions),
		// The remote moved and the local side did not — a pull, UNLESS the row's own content is a
		// push we never saw land and the remote now carries somebody else's edit of the same file:
		// then both sides moved off the last agreed content and pulling would bury ours.
		(false, true) => {
			if is_unconfirmed_concurrent_edit(local, remote, base) {
				let action = SyncAction::Conflict {
					rel_path: rel_path.to_string(),
				};
				tracing::debug!(
					"plan: {} — a foreign version of the same file landed on a push this engine never saw confirmed",
					action.describe()
				);
				actions.push(action);
			} else {
				pull_to_local(rel_path, local, remote, base, true, actions);
			}
		}
		// Both moved: either they converged on the same content (adopt, no transfer) or they
		// genuinely diverged (a conflict the caller resolves; modify-vs-delete is refined later).
		(true, true) => {
			if !nodes_agree(local, remote) {
				let action = SyncAction::Conflict {
					rel_path: rel_path.to_string(),
				};
				tracing::debug!(
					"plan: {} — both sides changed and diverged (local {local_side:?}, remote {remote_side:?})",
					action.describe()
				);
				actions.push(action);
			} else {
				// Both sides changed but hold identical content — there is nothing to transfer, but
				// the baseline is missing or stale here (otherwise this would be the (false,false)
				// branch). Record the converged state so a LATER one-sided change at this path is
				// classified correctly instead of being misread against an absent baseline.
				let action = SyncAction::AdoptBaseline {
					rel_path: rel_path.to_string(),
				};
				tracing::trace!(
					"plan: {} — both sides converged on identical content; recording the baseline",
					action.describe()
				);
				actions.push(action);
			}
		}
	}
}

/// Hand `visit` every baseline row that could be the SOURCE of a move this pass detects.
///
/// On a change-scoped pass that is the rows at the paths it decides, and no others. A row it does
/// not decide still carries BOTH its nodes, so the file sits at its recorded path on both sides,
/// and the only endpoint such a row could match is its own path — which [`detect_moves`] refuses
/// (`to != from`). The one shape that would break the equivalence is two `Synced` rows recording
/// one remote uuid or one lineage id, and no write path produces one: a move commits the delete of
/// its source and the insert of its destination in a single transaction, and the seeding stage
/// (see [`seed`](super::seed)) writes no row for a remote item a row already records. A debug build
/// asserts it over the decided rows where the directory-move fold reads them (`FoldDirs::read`),
/// whose pruning of what a proven move carried rests on it as well (see [`fold_dir_moves`]).
fn visit_move_sources(
	paths: PassPaths<'_>,
	baseline: &Baseline,
	mut visit: impl FnMut(&BaselineEntry),
) {
	match paths {
		PassPaths::Whole => baseline.visit_rows(visit),
		PassPaths::Changed(changed) => {
			let mut rows = baseline.cursor();
			for path in changed {
				if let Some(row) = rows.get(path) {
					visit(&row);
				}
			}
		}
	}
}

/// Hand `visit` every node of `map` a move this pass detects could END at, by the same argument as
/// [`visit_move_sources`]: a node the pass did not re-read is the one its own row records, so it
/// names no path but its own. A node with no row at all was necessarily observed, so it is in the
/// set by construction (see [`PassPaths::Changed`]).
fn visit_move_targets<'m, N: Nodes>(
	paths: PassPaths<'m>,
	map: &'m N,
	mut visit: impl FnMut(Cow<'m, str>, Cow<'m, N::Node>),
) {
	match paths {
		PassPaths::Whole => {
			for (path, node) in map.iter() {
				visit(path, node);
			}
		}
		PassPaths::Changed(changed) => {
			for path in changed {
				if let Some(node) = map.at(path) {
					visit(Cow::Borrowed(path.as_str()), node);
				}
			}
		}
	}
}

/// [`visit_move_targets`] over both sides, for a pass that detects moves in both directions.
///
/// A whole pass walks each map. A change-scoped one asks both sides about each of its paths in
/// turn: a carried side answers from the path's row, and walking the paths once per side read the
/// rows there twice — after a directory rename, every row under the new name. Asked together,
/// whichever question about a path reads its row first, the rest are answered out of the page it
/// read.
fn visit_move_targets_of_both<'m, L: Nodes, R: Nodes>(
	paths: PassPaths<'m>,
	local: &'m L,
	remote: &'m R,
	mut visit_local: impl FnMut(Cow<'m, str>, Cow<'m, L::Node>),
	mut visit_remote: impl FnMut(Cow<'m, str>, Cow<'m, R::Node>),
) {
	let PassPaths::Changed(changed) = paths else {
		visit_move_targets(paths, local, visit_local);
		visit_move_targets(paths, remote, visit_remote);
		return;
	};
	for path in changed {
		if let Some(node) = local.at(path) {
			visit_local(Cow::Borrowed(path.as_str()), node);
		}
		if let Some(node) = remote.at(path) {
			visit_remote(Cow::Borrowed(path.as_str()), node);
		}
	}
}

/// [`visit_move_targets`] over the local side where `local_too` asks for it and over the remote one
/// where `remote_too` does: both in one walk where both are asked (see
/// [`visit_move_targets_of_both`]).
fn visit_move_targets_of<'m, L: Nodes, R: Nodes>(
	(local_too, remote_too): (bool, bool),
	paths: PassPaths<'m>,
	local: &'m L,
	remote: &'m R,
	visit_local: impl FnMut(Cow<'m, str>, Cow<'m, L::Node>),
	visit_remote: impl FnMut(Cow<'m, str>, Cow<'m, R::Node>),
) {
	match (local_too, remote_too) {
		(true, true) => visit_move_targets_of_both(paths, local, remote, visit_local, visit_remote),
		(true, false) => visit_move_targets(paths, local, visit_local),
		(false, true) => visit_move_targets(paths, remote, visit_remote),
		(false, false) => {}
	}
}

/// Where the targets carrying one key of [`detect_moves`]' join sit, indexed from whichever side of
/// the join has fewer candidates.
///
/// Indexing every target only pays where the sources outnumber the targets. A first sync has a
/// target for every new file and no row to move from, and a converged pass one for every remote
/// file and no row whose file left its path: there an index of the targets holds the tree for
/// nothing. So the sources ask for their keys first, each getting an
/// empty slot, and a target fills a slot only where one was asked for. Where the sources ask for
/// more keys than the targets carry — a mass deletion beside a handful of new files — the index
/// drops the slots and takes every target instead, the smaller side once more.
///
/// Either way a key a source looks up holds the same targets, met in the same order.
struct MoveTargets<K, V> {
	slots: HashMap<K, V>,
	/// How many keys the targets carry between them.
	targets: usize,
	/// Whether `slots` is keyed by every target rather than by the keys the sources asked for.
	by_targets: bool,
}

impl<K: Copy + Eq + Hash, V: Default> MoveTargets<K, V> {
	/// An index for a join whose targets carry `targets` keys between them.
	fn new(targets: usize) -> Self {
		Self {
			slots: HashMap::new(),
			targets,
			by_targets: false,
		}
	}

	/// A source will look `key` up. Where no target carries a key at all, no source gets a slot.
	fn want(&mut self, key: K) {
		if self.by_targets || self.targets == 0 || self.slots.contains_key(&key) {
			return;
		}
		if self.slots.len() == self.targets {
			self.slots = HashMap::new();
			self.by_targets = true;
			return;
		}
		self.slots.insert(key, V::default());
	}

	/// Whether the targets have to be walked to fill this index.
	fn wants_targets(&self) -> bool {
		self.by_targets || !self.slots.is_empty()
	}

	/// The slot a target carrying `key` goes into, where this index keeps it.
	fn slot(&mut self, key: K) -> Option<&mut V> {
		if self.by_targets {
			Some(self.slots.entry(key).or_default())
		} else {
			self.slots.get_mut(&key)
		}
	}

	/// What the targets carrying `key` left in its slot.
	fn get(&self, key: &K) -> Option<&V> {
		self.slots.get(key)
	}

	/// Whether any slot holds what `met` looks for: whether this index can match anything at all.
	fn any(&self, met: impl FnMut(&V) -> bool) -> bool {
		self.slots.values().any(met)
	}
}

/// Detect file moves/renames so they apply as a single metadata op instead of a re-transfer, and
/// return the set of paths they consume (excluded from the per-path reconcile). Files only.
///
/// - Remote moves are matched by UUID (a server-side move keeps the file's uuid): a baseline file
///   whose uuid now sits at a different remote path, still unchanged locally, becomes a `MoveLocal`.
///   Failing that, by the file's server-minted LINEAGE id, which survives the content edit that
///   re-mints the uuid — so a move and an edit landing inside one window stay one item rather than
///   splitting into a local deletion and an unrelated download. Such a move is paired with a
///   download of the new version onto the renamed copy.
///
///   "Still unchanged locally" is a REQUIREMENT of both matches, not a description of the usual
///   case. A local edit at the move's source is a second change to the same item, and renaming the
///   local copy would carry that edit to the destination for the pull to write over — silently,
///   since a same-size edit satisfies the scanner's `(size, mtime)` fast path afterwards and is
///   never looked at again. Such a path falls through to the per-path reconcile, which resolves it
///   by mode: TwoWay surfaces the source as a conflict and downloads the moved version under its
///   new name; RemoteToLocal lets the remote win, quarantining the local edit and downloading at
///   the new name; RemoteBackup never deletes on its destination, so the edited file stays where it
///   is and the moved version arrives beside it. The push-only modes never reach this block at all:
///   `LocalToRemote` re-pushes the edit at the source path and trashes the remote's moved copy,
///   `LocalBackup` re-pushes and leaves it.
///
///   A lineage match whose new version is a foreign EDIT over a push nothing confirmed
///   ([`is_unconfirmed_concurrent_edit`]) is not carried across either, for the same reason at one
///   remove: the rename would put our unconfirmed copy under the download. In two-way it surfaces
///   the source as a conflict right here — the reconcile could not, since with nothing left at the
///   source path it reads a plain remote-side deletion — and the moved version downloads at its new
///   name as an untracked remote item.
/// - Local moves are matched by content hash: a baseline file gone locally whose content reappears
///   at a new local path (uniquely — ambiguous content is left to delete+create), with the remote
///   still holding the original, becomes a `MoveRemote`.
///
/// Only a `Synced` row is a move endpoint. A path held in conflict must not be one: consuming it
/// here would skip the per-path conflict hold, rewrite its baseline as `Synced` against one side's
/// content and lose the divergence for good. Nor is a row
/// [`Adopted`](BaselineState::Adopted) from a destination at a mode switch — it records one side's
/// standing copy, not a synced pair, so matching it by uuid or by content would move an item the
/// source never had. Both fall through to the reconcile loop, which knows what to do with them.
fn detect_moves<'m>(
	mode: super::SyncMode,
	baseline: &Baseline,
	local: &'m impl Nodes<Node = LocalNode>,
	remote: &'m impl Nodes<Node = RemoteNode>,
	paths: PassPaths<'m>,
	actions: &mut Vec<SyncAction>,
	consumed: &mut HashSet<String>,
) {
	let created = |path: &str, node: &LocalNode| {
		push_target(node, || remote.holds(path), || baseline.contains_key(path))
	};
	// How many keys each index's targets carry, which decides the side each is keyed from, and
	// whether a half has anything to join at all. Counted over the same walk that fills the indexes
	// below, keeping nothing: each half's targets only in a mode that moves things that way, and a
	// two-way pass counts both in one walk.
	let (mut created_files, mut remote_files, mut remote_lineages) = (0, 0, 0);
	visit_move_targets_of(
		(mode.pushes(), mode.pulls()),
		paths,
		local,
		remote,
		|path, node| created_files += usize::from(created(&path, &node).is_some()),
		|_, node| {
			if node.kind == NodeKind::File {
				remote_files += 1;
				remote_lineages += usize::from(node.stable_uuid.is_some());
			}
		},
	);
	// Content hash -> the new local paths carrying it: what the push half matches the rows against.
	let mut created_by_hash: MoveTargets<[u8; 32], Vec<Cow<'m, str>>> =
		MoveTargets::new(created_files);
	// Where each remote file sits now, by uuid and by lineage: what the pull half matches the rows
	// against. The second index is there because a file's uuid is re-minted by every content edit,
	// so a move that CARRIED an edit inside one window is invisible to the first — the uuid the
	// baseline recorded no longer names anything live. The server-minted lineage id is not
	// re-minted, so it still finds the file at its new path and the pass carries the item across
	// instead of quarantining the old path and downloading the new one from scratch.
	let mut remote_path_of_uuid: MoveTargets<Uuid, Option<Cow<'m, str>>> =
		MoveTargets::new(remote_files);
	let mut remote_path_of_lineage: MoveTargets<StableUuid, Option<Cow<'m, str>>> =
		MoveTargets::new(remote_lineages);

	let pull_uuid = |base: &BaselineEntry| pull_source(base, || remote.at(&base.rel_path));
	let push_hash = |base: &BaselineEntry| {
		push_source(
			base,
			|| local.holds(&base.rel_path),
			|| remote.at(&base.rel_path),
		)
	};
	// The sources ask for their keys first, so a pass with none — a first sync, a converged tree —
	// indexes no target at all.
	if remote_files > 0 || created_files > 0 {
		visit_move_sources(paths, baseline, |base| {
			if remote_files > 0
				&& let Some(uuid) = pull_uuid(base)
			{
				remote_path_of_uuid.want(uuid);
				if let Some(lineage) = base.remote_stable_uuid {
					remote_path_of_lineage.want(lineage);
				}
			}
			if created_files > 0
				&& let Some(hash) = push_hash(base)
			{
				created_by_hash.want(hash);
			}
		});
	}
	let pull_targets =
		remote_path_of_uuid.wants_targets() || remote_path_of_lineage.wants_targets();
	if !pull_targets && !created_by_hash.wants_targets() {
		return;
	}
	visit_move_targets_of(
		(created_by_hash.wants_targets(), pull_targets),
		paths,
		local,
		remote,
		|path, node| {
			if let Some(hash) = created(&path, &node)
				&& let Some(at) = created_by_hash.slot(hash)
			{
				at.push(path);
			}
		},
		|path, node| {
			if node.kind != NodeKind::File {
				return;
			}
			if let Some(lineage) = node.stable_uuid
				&& let Some(at) = remote_path_of_lineage.slot(lineage)
			{
				*at = Some(path.clone());
			}
			if let Some(at) = remote_path_of_uuid.slot(node.remote_uuid) {
				*at = Some(path);
			}
		},
	);
	let pulls =
		remote_path_of_uuid.any(Option::is_some) || remote_path_of_lineage.any(Option::is_some);
	let pushes = created_by_hash.any(|at| !at.is_empty());
	if !pulls && !pushes {
		return;
	}

	// The rows a LOCAL move can start from: those whose content reappears at a new local path.
	// Collected on the pull half's walk of the rows, so the two halves share one walk — in the
	// order a walk of their own would hand them out, and judged against `consumed` only once the
	// pull half is done with it, exactly as that walk would judge them. Only a row with somewhere to
	// move to is kept: a mass deletion keeps none.
	let local_source = |base: &BaselineEntry| {
		push_hash(base)
			.and_then(|hash| created_by_hash.get(&hash))
			.is_some_and(|at| !at.is_empty())
	};
	let sides = MoveSides {
		mode,
		local,
		remote,
		baseline,
	};
	let mut local_sources: Vec<BaselineEntry> = Vec::new();
	visit_move_sources(paths, baseline, |base| {
		if pushes && local_source(base) {
			local_sources.push(base.clone());
		}
		if !pulls {
			return;
		}
		let Some(uuid) = pull_uuid(base) else {
			return;
		};
		// The file the row records by its uuid, else by its lineage, which a move that carried an
		// edit keeps (see `pull_move`).
		let target = || match remote_path_of_uuid.get(&uuid).and_then(Option::as_ref) {
			Some(to) => Some((Cow::Borrowed(to.as_ref()), false)),
			None => base
				.remote_stable_uuid
				.and_then(|lineage| {
					remote_path_of_lineage
						.get(&lineage)
						.and_then(Option::as_ref)
				})
				.map(|to| (Cow::Borrowed(to.as_ref()), true)),
		};
		if let Some(took) = pull_move(
			&sides,
			|path| consumed.contains(path),
			base,
			target,
			actions,
		) {
			took.consume(&base.rel_path, consumed);
		}
	});

	for base in &local_sources {
		let candidates = |hash: &[u8; 32]| {
			created_by_hash
				.get(hash)
				.map(|at| at.iter().map(Cow::as_ref))
		};
		if let Some(took) = push_move(
			&sides,
			|path| consumed.contains(path),
			base,
			candidates,
			actions,
		) {
			took.consume(&base.rel_path, consumed);
		}
	}
}

/// Whether `base` is a row the PULL half of the move join can move from, and the uuid it is matched
/// on: a synced file whose uuid the remote no longer holds at the row's own path, which `remote_at`
/// reads, and only for such a row. One still there would find its own path by either key — a uuid
/// names one remote file, the view placing each item once and the pending-write fold keeping it so
/// — and a move onto `from` is none.
pub(super) fn pull_source<'n>(
	base: &BaselineEntry,
	remote_at: impl FnOnce() -> Option<Cow<'n, RemoteNode>>,
) -> Option<Uuid> {
	if base.kind != NodeKind::File || base.state != BaselineState::Synced {
		return None;
	}
	let uuid = base.remote_uuid?;
	let stayed =
		remote_at().is_some_and(|node| node.kind == NodeKind::File && node.remote_uuid == uuid);
	(!stayed).then_some(uuid)
}

/// Whether `base` is a row the PUSH half of the move join can move from, and the content it is
/// matched on: a synced file the local side no longer holds (`local_holds`), whose original the
/// remote still holds at the row's path (`remote_at`). Its hash is keyed by the raw bytes, since
/// `Blake3Hash` is not `std::hash::Hash`.
pub(super) fn push_source<'n>(
	base: &BaselineEntry,
	local_holds: impl FnOnce() -> bool,
	remote_at: impl FnOnce() -> Option<Cow<'n, RemoteNode>>,
) -> Option<[u8; 32]> {
	if base.kind != NodeKind::File || base.state != BaselineState::Synced || local_holds() {
		return None;
	}
	let (hash, uuid) = (base.content_hash?, base.remote_uuid?);
	(remote_at().map(|node| node.remote_uuid) == Some(uuid)).then(|| *hash.as_ref())
}

/// Whether the local `node` is one a move of the PUSH half can end at, and the content it is matched
/// on: a file the remote does not hold (`remote_holds`) at a path no row tracks (`tracked`). The
/// remote is asked first: on a whole pass it is a hash lookup and the baseline is a read of the
/// store, and almost every local file is on the remote too.
pub(super) fn push_target(
	node: &LocalNode,
	remote_holds: impl FnOnce() -> bool,
	tracked: impl FnOnce() -> bool,
) -> Option<[u8; 32]> {
	if node.kind != NodeKind::File || remote_holds() || tracked() {
		return None;
	}
	node.content_hash.map(|hash| *hash.as_ref())
}

/// What a move of the join is decided against beyond its source row: the pass's mode, its two
/// sides and its rows, asked about the paths a source could move to.
pub(super) struct MoveSides<'s, L, R> {
	pub(super) mode: super::SyncMode,
	pub(super) local: &'s L,
	pub(super) remote: &'s R,
	pub(super) baseline: &'s Baseline,
}

/// What a source of the move join took: its own path alone, where it surfaced a conflict, or its
/// path and the one it moved to.
pub(super) enum Took {
	Source,
	Move { to: String },
}

impl Took {
	/// Record in `consumed` the paths taken from `from`.
	fn consume(self, from: &str, consumed: &mut HashSet<String>) {
		consumed.insert(from.to_owned());
		if let Self::Move { to } = self {
			consumed.insert(to);
		}
	}
}

/// The PULL half of the move join for one source row, `base`: a remote move becomes a local one —
/// one the resident [`detect_moves`] and a whole pass's staged reconcile (see `stage::reconcile`)
/// both decide here. `consumed` says whether an earlier move of the join, or a path the pass
/// withholds, has taken a path. `target` is where the remote holds the row's file now: by the uuid
/// the row records, else by its lineage, which a move that carried an edit keeps (`true`).
///
/// What the source took, if anything: a conflict surfaced at it, or the move.
pub(super) fn pull_move<'t, L, R>(
	sides: &MoveSides<'_, L, R>,
	consumed: impl Fn(&str) -> bool,
	base: &BaselineEntry,
	target: impl FnOnce() -> Option<(Cow<'t, str>, bool)>,
	actions: &mut Vec<SyncAction>,
) -> Option<Took>
where
	L: NodesAt<Node = LocalNode>,
	R: NodesAt<Node = RemoteNode>,
{
	let from = &base.rel_path;
	if consumed(from) {
		return None;
	}
	// Only where the local side still matches the baseline — for EITHER match. With a local
	// edit at `from` this is a genuine both-sides divergence, and consuming it as a move
	// would rename the local copy and then write over it, laundering a conflict into a
	// silent overwrite. The mode decides what it becomes instead; see `detect_moves`.
	//
	// A row with NO local content on record is not such an edit: it is a row waiting for a
	// push (a `KeepLocal` resolution anchors the baseline to the remote and clears the
	// local half on purpose). Comparing a hash against `None` reports "changed" for every
	// one of them, which would turn a remote rename landing before that push into a second
	// conflict at the source plus a download at the new name. No local evidence is not
	// evidence of a local change: carry the move across and let the pending push follow at
	// the new name. The waiver is for the RENAME only — a move that also carried an EDIT is
	// refused below, where the kept copy would end up under the download.
	let local_at_from = sides.local.at(from);
	if base.content_hash.is_some()
		&& classify_local(local_at_from.as_deref(), Some(base)) != SideState::Unchanged
	{
		return None;
	}
	// `carries_edit`: the lineage moved AND changed version, so the local copy this move
	// renames is the pre-edit one and still has to be refreshed.
	let (to, carries_edit) = target()?;
	let to: &str = &to;
	// The edit a move carried is measured by the same rule as one that stayed put: over a
	// push no snapshot ever confirmed it is a divergence, not an item to carry across.
	// Consuming it would rename our unconfirmed copy to the destination for the paired
	// download to write over — the loss the rule exists to stop, one rename away — and
	// leaving the pair to the reconcile is no better, since the source would then read as a
	// remote-side deletion. So the source is surfaced here and its path consumed; the moved
	// version is untracked at its new name and downloads there through the ordinary
	// reconcile, which is the same outcome the uuid branch produces for a two-sided change.
	//
	// A row with no local content on record is measured the same way, and has to be spelled
	// out because [`is_unconfirmed_concurrent_edit`] has no hash of ours to compare: being
	// excused from the local-change guard above is not agreement. The copy a `KeepLocal`
	// resolution kept is diverged from what the remote holds by construction, so a remote
	// change that renamed AND edited the file is the same divergence one rename away.
	let remote_at_to = sides.remote.at(to);
	if carries_edit
		&& sides.mode.pushes()
		&& (base.content_hash.is_none()
			|| is_unconfirmed_concurrent_edit(
				local_at_from.as_deref(),
				remote_at_to.as_deref(),
				Some(base),
			)) {
		let action = SyncAction::Conflict {
			rel_path: from.clone(),
		};
		tracing::debug!(
			"plan: {} — the moved version is a foreign edit over a push this engine never saw confirmed",
			action.describe()
		);
		actions.push(action);
		return Some(Took::Source);
	}
	if to == from
		|| sides.baseline.contains_key(to)
		|| local_at_from.is_none()
		|| sides.local.holds(to)
		|| consumed(to)
	{
		return None;
	}
	let action = SyncAction::MoveLocal {
		from_path: from.clone(),
		to_path: to.to_string(),
		kind: NodeKind::File,
	};
	tracing::debug!(
		"plan: {} — remote item moved (matched by {}); renaming locally instead of re-downloading",
		action.describe(),
		if carries_edit { "lineage" } else { "uuid" }
	);
	actions.push(action);
	if carries_edit {
		// The move brought a new version with it: pull it onto the renamed copy in this
		// same pass. Without this the pass would leave the pre-edit bytes at the new
		// path with a baseline row that has to describe them as stale, and the next
		// pass would have to re-pull anyway.
		let download = SyncAction::DownloadFile {
			rel_path: to.to_string(),
			// The lineage index named this path, so the node is there.
			remote_uuid: remote_at_to
				.as_deref()
				.expect("the moved item's new path is in the remote side")
				.remote_uuid,
		};
		tracing::debug!(
			"plan: {} — the move carried a content edit",
			download.describe()
		);
		actions.push(download);
	}
	Some(Took::Move { to: to.to_owned() })
}

/// The PUSH half of the move join for one source row, `base`: a local move becomes a remote one, by
/// content — decided here for [`detect_moves`] and a whole pass's staged reconcile alike (see
/// [`pull_move`]). `candidates` are the new local files carrying a content hash, in path order.
///
/// Only an UNAMBIGUOUS match is a move: a content hash that more than one new file carries, none
/// of them taken, falls back to delete + create.
pub(super) fn push_move<L, R, I>(
	sides: &MoveSides<'_, L, R>,
	consumed: impl Fn(&str) -> bool,
	base: &BaselineEntry,
	candidates: impl FnOnce(&[u8; 32]) -> Option<I>,
	actions: &mut Vec<SyncAction>,
) -> Option<Took>
where
	R: NodesAt<Node = RemoteNode>,
	I: IntoIterator,
	I::Item: AsRef<str>,
{
	let from = &base.rel_path;
	if consumed(from) {
		return None;
	}
	let (Some(hash), Some(uuid)) = (base.content_hash, base.remote_uuid) else {
		return None;
	};
	let mut fresh = candidates(hash.as_ref())?
		.into_iter()
		.filter(|to| !consumed(to.as_ref()) && !sides.remote.holds(to.as_ref()));
	let to = fresh.next()?;
	if fresh.next().is_some() {
		return None;
	}
	let action = SyncAction::MoveRemote {
		from_path: from.clone(),
		to_path: to.as_ref().to_owned(),
		kind: NodeKind::File,
		remote_uuid: uuid,
	};
	tracing::debug!(
		"plan: {} — local file moved (matched by content hash); re-parenting/renaming on the remote instead of re-uploading",
		action.describe()
	);
	actions.push(action);
	Some(Took::Move {
		to: to.as_ref().to_owned(),
	})
}

/// Whether `key` is one of `held` or lives under one. A path the view cannot resolve makes
/// everything the snapshot would otherwise resolve beneath it unresolvable too.
pub(super) fn is_under_held_path(key: &str, held: &BTreeSet<String>) -> bool {
	at_or_under_root(held, key)
}

/// Whether `path` is a STRICT descendant of `prefix` (`prefix/...`).
pub(super) fn is_under(path: &str, prefix: &str) -> bool {
	path.len() > prefix.len()
		&& path.as_bytes()[prefix.len()] == b'/'
		&& path.as_bytes()[..prefix.len()] == *prefix.as_bytes()
}

/// Whether `path` is one of `roots` or lives under one, found by walking the path's OWN ancestors
/// and asking the sorted set for each: O(depth · log n), where asking [`is_under`] per root is a
/// scan of every root for every path.
///
/// The walk is what makes it an ancestor test. Seeking to the nearest key at or before `path`
/// answers something else: with `a` and `a/b` recorded, the key before `a/c` is `a/b`, which is not
/// an ancestor of it, while the ancestor `a` lies further back still.
///
/// An ancestor walk never yields `""`, so a root of `""` covers only the pair root itself here. The
/// one caller for which it means "everything is under it" asks for that separately
/// (`engine::Blocking`).
pub(super) fn at_or_under_root(roots: &BTreeSet<String>, path: &str) -> bool {
	roots.contains(path) || ancestors(path).any(|at| roots.contains(at))
}

/// Every proper ancestor of `path`, shallowest first: `a/b/c` yields `a`, then `a/b`. Never the pair
/// root `""`, and nothing for a top-level path.
pub(super) fn ancestors(path: &str) -> impl Iterator<Item = &str> {
	// Each cut is at a `/`, which is one byte and so a character boundary: `get` never refuses one.
	path.match_indices('/').filter_map(|(at, _)| path.get(..at))
}

/// The key range of everything strictly under `dir`: `dir/` up to `dir0`. `/` is 0x2F and `0` is
/// 0x30 and paths compare bytewise, so the half-open range is exactly that subtree, and a sorted map
/// or set answers "is anything under here?" by seeking to it instead of testing every key. `""`
/// holds nothing by this rule, as `is_under(path, "")` holds for nothing.
pub(super) fn subtree_bounds(dir: &str) -> std::ops::Range<String> {
	format!("{dir}/")..format!("{dir}0")
}

/// The entries of `map` strictly under `dir`, in key order (see [`subtree_bounds`]).
pub(super) fn under_dir<'m, V>(
	map: &'m BTreeMap<String, V>,
	dir: &str,
) -> btree_map::Range<'m, String, V> {
	map.range(subtree_bounds(dir))
}

/// Whether a directory move with `end` as one of its ends touches a path the cache is holding: at
/// or under it, or above it.
fn touches_held(end: &str, held: &BTreeSet<String>) -> bool {
	is_under_held_path(end, held) || held.range(subtree_bounds(end)).next().is_some()
}

/// Where `path` lands when the directory at `from` moves to `to`; `None` when it is neither `from`
/// nor under it.
pub(super) fn moved_path(path: &str, from: &str, to: &str) -> Option<String> {
	let rest = path.strip_prefix(from)?;
	(rest.is_empty() || rest.starts_with('/')).then(|| format!("{to}{rest}"))
}

/// Drop every action that falls under a path this pass reports as a conflict — freshly surfaced or
/// still held from an earlier pass. The conflicting path itself cannot be materialized until the
/// caller resolves it, so an action on its subtree would fail (e.g. an upload into a remote dir
/// that cannot be created while the conflicting file holds the name). The conflicts themselves are
/// kept.
fn suppress_conflicted_subtrees(actions: &mut Vec<SyncAction>) {
	let conflicted: Vec<String> = actions
		.iter()
		.filter_map(|a| match a {
			SyncAction::Conflict { rel_path } => Some(rel_path.clone()),
			_ => None,
		})
		.collect();
	if conflicted.is_empty() {
		return;
	}
	actions.retain(|action| !under_conflict(action, &conflicted));
}

/// Whether [`suppress_conflicted_subtrees`] drops `action` for one of `conflicted`, the paths the
/// plan surfaces as conflicts: an end of it sits under one. A conflict itself never does.
pub(super) fn under_conflict(action: &SyncAction, conflicted: &[String]) -> bool {
	if matches!(action, SyncAction::Conflict { .. }) {
		return false;
	}
	let (from, to) = action.endpoints();
	let held = conflicted
		.iter()
		.any(|c| is_under(from, c) || is_under(to, c));
	if held {
		tracing::debug!(
			"plan: dropping {} — it sits under a path held in conflict this pass",
			action.describe()
		);
	}
	held
}

/// Whether [`finish_plan`] drops `action` as a deletion of a remote item this engine already sent to
/// the trash (`trashed`), which the cache has not caught up with: the path is deferred.
pub(super) fn already_trashed(action: &SyncAction, trashed: &HashSet<Uuid>) -> bool {
	let SyncAction::TrashRemote {
		rel_path,
		remote_uuid,
		..
	} = action
	else {
		return false;
	};
	if !trashed.contains(remote_uuid) {
		return false;
	}
	tracing::debug!(
		"reconcile: skipping the remote deletion of {rel_path:?} — this engine already trashed it and the cache has not caught up"
	);
	true
}

/// Carry every directory move the pass can identify across as ONE move of the directory, and re-key
/// the three inputs so the rest of the pass reads that subtree where it ends up. Returns the moves
/// in the order they have to run: each one is found in inputs the earlier ones already re-keyed, so
/// a directory's move precedes any move inside it.
///
/// Planned per path, a directory move is a create at the new path, a move per file and a trash of
/// the old directory: a round trip per child instead of one, and a new directory uuid, which loses
/// everything keyed by it (a public link is). A move of the directory keeps its uuid, and with the
/// inputs re-keyed every child is a no-op unless it changed on its own — an edit inside a moved
/// directory is planned at its new path and applied after the move.
///
/// Two shapes are found, a case-only rename first:
///
/// - A directory the two sides spell differently only by case. Planned any other way it destroys
///   the directory: the per-path reconcile sees a create at the new spelling and a delete at the
///   old one, the server's name dedup is case-insensitive, so the create is handed back the very
///   directory the delete then trashes, subtree and all; the pull side quarantines the whole local
///   subtree on a case-insensitive filesystem the same way. Only a directory under the SAME parent
///   path on both sides is a candidate; a case change deeper down is found on a later iteration,
///   once its parent has been re-keyed. Which side is renamed:
///   - a one-way mode renames its destination to the source's spelling;
///   - two-way renames the REMOTE when the baseline recorded the remote's spelling for that same
///     directory and has no row at the local one — the local side is what changed — and the LOCAL
///     side otherwise: the remote renamed it, or nothing says which side moved and the spelling
///     every other device of the account already sees is the one to keep.
///
///   A path held in conflict at either spelling, or one touching a path the cache is holding, is
///   left to the ordinary reconcile.
/// - A directory moved to a different path on one side; see [`next_dir_move`].
///
/// The engine re-keys the paths it blocks the same way (see `Prepared::fold_dir_moves`), so a
/// block follows its item into the moved directory.
///
/// Only the side that moved is re-keyed. The other one keeps what it holds where it holds it,
/// which a whole map does by itself and a carried side has to be told: the rows it derives its
/// nodes from move with the fold (see `Side::stay_put`).
///
/// `decided` is the set a change-scoped pass decides (see [`PassPaths::Changed`]); `None` is a
/// whole pass. It narrows what the two detectors ENUMERATE, exactly as it narrows what
/// [`reconcile`] decides, and for the same reason: a move both of whose endpoints the pass carried
/// from their own baseline rows is not a move. Both endpoints of a real one are always in the set —
///
/// - the side that moved observed an absence at the source and a node at the destination, and
///   [`PassPaths::Changed`]'s contract puts every such path in the set;
/// - a case-only rename is that same pair of observations under one folded name;
/// - the twin a pushed move is refused for is a second directory GONE locally, which is an
///   observed absence too.
///
/// — so scoping loses no move the whole read finds, and a move it does leave out is one the
/// narrowed reconcile would not have decided either. Each iteration re-keys the set in place with
/// the move it just folded, so a move nested in that subtree is named by where the outer one put
/// it, and reads the set's rows once for both detectors (see [`FoldDirs`]). The set the caller gets
/// back is keyed like the maps and the rows, which is what the reconcile after it is handed; a
/// caller that re-keyed it again would move a chained move's paths twice (`a -> b`, then
/// `c -> a`).
///
/// A push matched on the new directory's FULL signature proves more than that it is the move:
/// every path under the destination is one of the rows', with the row's kind, hash and size
/// ([`Matched::Signature`]). At such a path the local node is the row's content, and — where this
/// pass recorded nothing on the remote side (`Side::untouched`) — the remote node is the row's
/// own item, so the reconcile would decide nothing there: no transfer, since both sides agree
/// with the row; no end of a file move, since a node carried off its row names no path but its
/// own; and no nested move for the next iteration, since both sides hold it. So a scoped fold
/// takes those paths back out of the set as it re-keys it, and neither the next iteration nor the
/// reconcile reads their rows again. Whatever the remote side DID record under the destination
/// stays in the set — an edit or re-mint announced inside the directory, a deletion, a move out,
/// one of this engine's own writes folded in (`PendingWrites::fold_into`) — and so does the
/// destination itself. Nothing leaves the set after a case-only rename, a pull, a push matched
/// only once a nested move was set aside, on a whole pass, or where the remote side is whole.
///
/// The pruning rests on two things it does not read. No row under the source may be uncarryable,
/// or its path would read as an absence once it left the set: the engine holds every such path,
/// which refuses the move, and any other caller is caught by one read of the uncarryable rows'
/// index, which keeps the set whole. And no two decided `Synced` rows may record one remote uuid
/// or one lineage, which the move detection already assumes (see [`visit_move_sources`]).
///
/// On a whole pass's staged sides nothing here costs the tree: the detectors walk each side's
/// directories off its stage's index of them, the destination check (`occupied`) is two seeks of
/// the folded index, the re-key a statement per slice of the moved subtree, and a pushed move is
/// matched by digests of the candidates' subtrees, read in one ordered walk per side (see
/// [`next_dir_move`]). What the fold holds is per directory that MOVED, or that one side no longer
/// holds where its row records it: the move sources the walk of the rows and both sides' directories
/// side by side leaves (see [`FoldDirs::visit_sources`]), the remote's directories of those sources'
/// uuids, and a digest per candidate. A converged pair's fold holds the pages its walks read. On a
/// whole map the destination check and the re-key walk the map, which has no order to bisect: what
/// the tests compare the stage against.
///
/// A CARRIED side the move does not carry records a node or a tombstone at every row it would
/// otherwise follow (`Side::stay_put`), so a move over more rows than a change-scoped pass expands
/// one change into is not folded at all: the fold hands back the reason the pass reads both sides
/// whole instead, before it re-keys anything for that move. A whole pass's sides derive nothing
/// from the rows, so it never gets one.
pub(crate) fn fold_dir_moves(
	mode: super::SyncMode,
	baseline: &mut Baseline,
	local: &mut Side<LocalNode>,
	remote: &mut Side<RemoteNode>,
	held: &BTreeSet<String>,
	mut decided: Option<&mut BTreeSet<String>>,
) -> Result<Vec<SyncAction>, FullPassReason> {
	let mut renames = Vec::new();
	loop {
		// An immutable reborrow, scoped so it is dead before `move_subtree` below wants the
		// mutable one back. A carried side reads its nodes off these very rows, which is why the
		// re-key and the row move that follows it cannot be reordered (see `Side::rekey_subtree`).
		// The set is borrowed the same way, so the re-key below can take it back.
		let action = {
			let scope = decided
				.as_deref()
				.map_or(PassPaths::Whole, PassPaths::Changed);
			let bl: &Baseline = baseline;
			let (local_ref, remote_ref) = (local.of(bl), remote.of(bl));
			let dirs = FoldDirs::read(scope, bl, &local_ref, &remote_ref);
			next_case_only_dir_rename(mode, bl, &local_ref, &remote_ref, held, &dirs)
				.map(|action| (action, Matched::Identity))
				.or_else(|| next_dir_move(mode, bl, &local_ref, &remote_ref, held, &dirs))
		};
		let Some((action, matched)) = action else {
			break;
		};
		let (from, to) = action.endpoints();
		// Whether what this move carried may leave the set (see above), asked while `from` still
		// names its rows.
		let prune = matched == (Matched::Signature { full: true })
			&& decided.is_some()
			&& !baseline
				.uncarryable_paths()
				.any(|path| is_under(&path, from));
		// The side the move carries is re-keyed, and the other one told to stay where it is: both
		// read the rows about to move (see `Side::stay_put`). A push matched by its signature asks
		// for no walk of them, because the match already proved what staying put would record — no
		// local node at the source, and one at every path a row lands on. Staying put goes first:
		// it is the half that can refuse, and then neither side has moved. The reason names the
		// side the change came from, which is the one staying put.
		let stayed = {
			let bl: &Baseline = baseline;
			if matches!(action, SyncAction::MoveRemote { .. }) {
				let stayed = match matched {
					Matched::Identity => local
						.stay_put(bl, from, to)
						.ok_or(FullPassReason::LocalOverflow)?,
					Matched::Signature { .. } => Vec::new(),
				};
				remote.rekey_subtree(bl, from, to, |node, path| node.rel_path = path.to_string());
				stayed
			} else {
				let stayed = remote
					.stay_put(bl, from, to)
					.ok_or(FullPassReason::RemoteOverflow)?;
				local.rekey_subtree(bl, from, to, |node, path| node.rel_path = path.to_string());
				stayed
			}
		};
		// Written only once a move is actually being folded: the rows the pass reads are shared
		// with the store, and a pass that folds no directory move must not copy them.
		baseline.move_subtree(from, to);
		// The next iteration's scope, and the reconcile's: keyed like the maps and the rows this
		// move just re-keyed. Without it a move nested under this one would be looked for at a path
		// nothing holds, and the reconcile would decide paths no input is keyed by any more. What
		// the other side had to record to stay put is decided too, keyed where it now stands, and
		// what a proven move carried unchanged leaves (see above).
		if let Some(set) = decided.as_deref_mut() {
			*set = std::mem::take(set)
				.into_iter()
				.map(|path| moved_path(&path, from, to).unwrap_or(path))
				.filter(|path| !(prune && is_under(path, to) && remote.untouched(path)))
				.chain(stayed)
				.collect();
		}
		tracing::debug!(
			"plan: {} — the two sides spell the directory differently only by case",
			action.describe()
		);
		renames.push(action);
	}
	Ok(renames)
}

/// How a directory move was matched, which is what [`fold_dir_moves`] may take as known about the
/// subtree it carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Matched {
	/// By name (a case-only rename) or by the server's uuid for the directory (a pull): nothing
	/// about what either side holds under it.
	Identity,
	/// A push, by the signature of the local subtree ([`next_dir_move`]): nothing local is left at
	/// or under the source, and every row under it has a local node where it lands. `full` when it
	/// was the new directory's WHOLE signature that matched, rather than what it holds once a
	/// nested move is set aside: then every local node under the destination is a row's as well,
	/// with the row's kind, hash and size.
	Signature { full: bool },
}

/// What one iteration of [`fold_dir_moves`] enumerates for its two detectors: the directories each
/// side holds, and the directory rows a move can start from.
///
/// A whole pass enumerates each side's directories — off its stage's index of them, which reads
/// none of its files — and the directory rows, and a detector walks what it asks about only once
/// the one before it has left something to find. A change-scoped pass
/// enumerates its own paths for all of them, and asked one detector at a time it read the rows at
/// those paths once per walk: after a directory rename, the remote's directories twice and the move
/// sources once, three reads of the renamed rows an iteration. So a scoped iteration reads them in
/// ONE cursor walk, asks both sides about each path while the page that answers for it is the one
/// the cursor just read, and keeps only what the detectors ask about: the directories, a few among
/// the paths.
enum FoldDirs<'m> {
	/// A whole pass: each detector walks the sides' directories and the directory rows itself.
	Whole,
	/// A change-scoped pass: its own paths, read once.
	Changed(ChangedDirs<'m>),
}

/// The directories at a change-scoped pass's own paths, each list in path order — the order the
/// detectors walked those paths in — and already narrowed by the question its detector asks of
/// each, asked in the walk while that path's row was in hand.
#[derive(Default)]
struct ChangedDirs<'m> {
	/// The rows a directory move can start from, with their remote uuids.
	sources: Vec<(&'m str, Uuid)>,
	/// The local directories the remote holds nothing at: a case-only rename's local end.
	local_only: Vec<&'m str>,
	/// The local directories no row records: where a pushed move can end.
	local_new: Vec<&'m str>,
	/// The remote directories, with their uuids: where a pulled move can end.
	remote: Vec<(&'m str, Uuid)>,
	/// The remote directories the local side holds nothing at: a case-only rename's remote end.
	remote_only: Vec<(&'m str, Uuid)>,
}

impl<'m> FoldDirs<'m> {
	/// What one iteration enumerates at `paths`: nothing read up front for a whole pass, and one
	/// walk of its own paths for a scoped one.
	fn read(
		paths: PassPaths<'m>,
		baseline: &Baseline,
		local: &impl NodesAt<Node = LocalNode>,
		remote: &impl NodesAt<Node = RemoteNode>,
	) -> Self {
		let PassPaths::Changed(changed) = paths else {
			return Self::Whole;
		};
		let mut dirs = ChangedDirs::default();
		let mut rows = baseline.cursor();
		// The remote uuid and the whole-life id of every decided `Synced` row, with the path that
		// records it. The move detection trusts that no two such rows record one of them (see
		// [`visit_move_sources`]); this walk has every decided row in hand, so it is where a second
		// claimant shows, at no read of its own.
		#[cfg(debug_assertions)]
		let (mut uuids, mut lineages) = (HashMap::new(), HashMap::new());
		for path in changed {
			// First: every question below about `path` is answered out of the page this keeps.
			let row = rows.get(path);
			#[cfg(debug_assertions)]
			if let Some(row) = row
				.as_ref()
				.filter(|row| row.state == BaselineState::Synced)
			{
				let by_uuid = row.remote_uuid.and_then(|uuid| uuids.insert(uuid, path));
				let by_lineage = row
					.remote_stable_uuid
					.and_then(|lineage| lineages.insert(lineage, path));
				if let Some(first) = by_uuid.or(by_lineage) {
					debug_assert!(
						false,
						"{first:?} and {path:?} are both synced rows recording one remote item"
					);
				}
			}
			if let Some(uuid) = row.as_ref().and_then(dir_move_source) {
				dirs.sources.push((path, uuid));
			}
			if local
				.at(path)
				.is_some_and(|node| node.kind == NodeKind::Dir)
			{
				if !remote.holds(path) {
					dirs.local_only.push(path);
				}
				if row.is_none() {
					dirs.local_new.push(path);
				}
			}
			if let Some(node) = remote.at(path)
				&& node.kind == NodeKind::Dir
			{
				dirs.remote.push((path, node.remote_uuid));
				if !local.holds(path) {
					dirs.remote_only.push((path, node.remote_uuid));
				}
			}
		}
		Self::Changed(dirs)
	}

	/// Hand `visit` every row a directory move in `mode` can start from (see [`MoveSource`]): one
	/// with a local directory at its path only where `mode` pulls, which is the only move it can
	/// start, and any other only where `mode` pushes.
	///
	/// A whole pass reads them off the index that holds nothing but directory rows — a few per cent
	/// of the rows, where a walk of every row would read all of them once per move the fold carries
	/// — with both sides' directories beside them, all three in path order, rather than asking a side
	/// about each row. Where the local side holds the row's directory and the remote holds a
	/// directory of the row's uuid at its path, the directory is where the row records it on both
	/// sides, and the row is not handed over: the fold takes a whole pass's remote view to hold a
	/// directory's uuid at one path, as the cache keys its items by uuid, so that path is where a
	/// pull would look for it and find no move. What is handed over is then what a side changed: a
	/// converged pair hands over nothing, where every directory row was held for a pull to look up
	/// and find unmoved. A mode that does not pull reads none of the remote's directories here.
	fn visit_sources(
		&self,
		mode: super::SyncMode,
		baseline: &Baseline,
		local: &SideRef<'_, LocalNode>,
		remote: &SideRef<'_, RemoteNode>,
		mut visit: impl FnMut(MoveSource),
	) {
		let wanted = |local_dir| match local_dir {
			true => mode.pulls(),
			false => mode.pushes(),
		};
		match self {
			Self::Whole => {
				let mut local_dirs = local.dirs().peekable();
				let mut remote_dirs = mode.pulls().then(|| remote.dirs().peekable());
				baseline.visit_dir_rows(|row| {
					let Some(uuid) = dir_move_source(row) else {
						return;
					};
					let path = row.rel_path.as_str();
					while local_dirs.next_if(|(at, _)| at.as_ref() < path).is_some() {}
					let local_dir = local_dirs.peek().is_some_and(|(at, _)| at == path);
					if !wanted(local_dir) {
						return;
					}
					if local_dir && let Some(remote_dirs) = &mut remote_dirs {
						while remote_dirs.next_if(|(at, _)| at.as_ref() < path).is_some() {}
						if remote_dirs
							.peek()
							.is_some_and(|(at, node)| at == path && node.remote_uuid == uuid)
						{
							return;
						}
					}
					visit(MoveSource {
						// A copy: the row is lent for this visit only.
						path: row.rel_path.clone(),
						uuid,
						local_dir,
					});
				});
			}
			Self::Changed(dirs) => {
				for &(path, uuid) in &dirs.sources {
					let local_dir = local
						.at(path)
						.is_some_and(|node| node.kind == NodeKind::Dir);
					if wanted(local_dir) {
						visit(MoveSource {
							path: path.to_string(),
							uuid,
							local_dir,
						});
					}
				}
			}
		}
	}

	/// Hand `visit` every local directory the remote holds nothing at under that exact spelling.
	fn visit_local_only(
		&self,
		local: &'m SideRef<'m, LocalNode>,
		remote: &SideRef<'_, RemoteNode>,
		mut visit: impl FnMut(Cow<'m, str>),
	) {
		match self {
			Self::Whole => {
				// The two sides' directories side by side, in path order: one the remote holds a
				// directory at is held there, and only the others are asked about.
				for pair in local
					.dirs()
					.merge_join_by(remote.dirs(), |(left, _), (right, _)| left.cmp(right))
				{
					if let EitherOrBoth::Left((path, _)) = pair
						&& !remote.holds(&path)
					{
						visit(path);
					}
				}
			}
			Self::Changed(dirs) => {
				for &path in &dirs.local_only {
					visit(Cow::Borrowed(path));
				}
			}
		}
	}

	/// Hand `visit` every local directory no row records.
	fn visit_local_new(
		&self,
		local: &'m SideRef<'m, LocalNode>,
		baseline: &Baseline,
		mut visit: impl FnMut(Cow<'m, str>),
	) {
		match self {
			Self::Whole => {
				for (path, _) in local.dirs() {
					if !baseline.contains_key(&path) {
						visit(path);
					}
				}
			}
			Self::Changed(dirs) => {
				for &path in &dirs.local_new {
					visit(Cow::Borrowed(path));
				}
			}
		}
	}

	/// Hand `visit` every remote directory, with its uuid.
	fn visit_remote(
		&self,
		remote: &'m SideRef<'m, RemoteNode>,
		mut visit: impl FnMut(Cow<'m, str>, Uuid),
	) {
		match self {
			Self::Whole => {
				for (path, node) in remote.dirs() {
					visit(path, node.remote_uuid);
				}
			}
			Self::Changed(dirs) => {
				for &(path, uuid) in &dirs.remote {
					visit(Cow::Borrowed(path), uuid);
				}
			}
		}
	}

	/// Hand `visit` every remote directory the local side holds nothing at under that exact
	/// spelling, with its uuid.
	fn visit_remote_only(
		&self,
		remote: &'m SideRef<'m, RemoteNode>,
		local: &SideRef<'_, LocalNode>,
		mut visit: impl FnMut(Cow<'m, str>, Uuid),
	) {
		match self {
			Self::Whole => {
				// As [`visit_local_only`](Self::visit_local_only) walks them.
				for pair in remote
					.dirs()
					.merge_join_by(local.dirs(), |(left, _), (right, _)| left.cmp(right))
				{
					if let EitherOrBoth::Left((path, node)) = pair
						&& !local.holds(&path)
					{
						visit(path, node.remote_uuid);
					}
				}
			}
			Self::Changed(dirs) => {
				for &(path, uuid) in &dirs.remote_only {
					visit(Cow::Borrowed(path), uuid);
				}
			}
		}
	}
}

/// A row a directory move can start from: a synced directory row, with its remote uuid.
struct MoveSource {
	path: String,
	uuid: Uuid,
	/// Whether the local side holds a directory at its path.
	local_dir: bool,
}

/// The remote uuid of `row` where a directory move can start from it: a synced directory row's.
fn dir_move_source(row: &BaselineEntry) -> Option<Uuid> {
	(row.kind == NodeKind::Dir && row.state == BaselineState::Synced)
		.then_some(row.remote_uuid)
		.flatten()
}

/// The shallowest case-only directory rename left in the inputs (see [`fold_dir_moves`]).
fn next_case_only_dir_rename<'m>(
	mode: super::SyncMode,
	baseline: &Baseline,
	local: &'m SideRef<'m, LocalNode>,
	remote: &'m SideRef<'m, RemoteNode>,
	held: &BTreeSet<String>,
	dirs: &FoldDirs<'m>,
) -> Option<SyncAction> {
	// Collision key -> the local directory no remote item holds under that exact spelling. The scan
	// refuses two local entries with one key, so the map loses nothing.
	let mut local_only: HashMap<String, Cow<'m, str>> = HashMap::new();
	dirs.visit_local_only(local, remote, |path| {
		local_only.insert(collision_key(&path), path);
	});
	if local_only.is_empty() {
		return None;
	}
	let mut candidates: Vec<(Cow<'m, str>, Cow<'m, str>, Uuid)> = Vec::new();
	dirs.visit_remote_only(remote, local, |remote_path, remote_uuid| {
		let Some(local_path) = local_only.get(&collision_key(&remote_path)) else {
			return;
		};
		if parent_path(local_path) == parent_path(&remote_path) {
			candidates.push((local_path.clone(), remote_path, remote_uuid));
		}
	});
	candidates.sort_unstable_by(|(a, ..), (b, ..)| {
		(a.matches('/').count(), a.as_ref()).cmp(&(b.matches('/').count(), b.as_ref()))
	});
	candidates
		.into_iter()
		.find_map(|(local_path, remote_path, remote_uuid)| {
			let at_local = baseline.get(&local_path);
			let at_remote = baseline.get(&remote_path);
			if at_local
				.iter()
				.chain(at_remote.iter())
				.any(|row| row.state.is_conflict())
				|| [local_path.as_ref(), remote_path.as_ref()]
					.into_iter()
					.any(|end| touches_held(end, held))
			{
				return None;
			}
			let rename_remote = match mode {
				super::SyncMode::LocalToRemote | super::SyncMode::LocalBackup => true,
				super::SyncMode::RemoteToLocal | super::SyncMode::RemoteBackup => false,
				super::SyncMode::TwoWay => {
					at_local.is_none()
						&& at_remote.is_some_and(|row| {
							row.kind == NodeKind::Dir
								&& row.state == BaselineState::Synced
								&& row.remote_uuid == Some(remote_uuid)
						})
				}
			};
			Some(if rename_remote {
				SyncAction::MoveRemote {
					from_path: remote_path.to_string(),
					to_path: local_path.to_string(),
					kind: NodeKind::Dir,
					remote_uuid,
				}
			} else {
				SyncAction::MoveLocal {
					from_path: local_path.to_string(),
					to_path: remote_path.to_string(),
					kind: NodeKind::Dir,
				}
			})
		})
}

/// The shallowest directory move to a DIFFERENT path left in the inputs (see [`fold_dir_moves`]).
/// Two ways to find one, each tied to the side that moved:
///
/// - Pulled (a mode that pulls): a `Synced` baseline directory whose remote uuid the view now shows
///   at another path, while the local directory still sits at the recorded path. The uuid is the
///   server's own identity for the directory, so the match is certain. It becomes a `MoveLocal`.
/// - Pushed (a mode that pushes): a `Synced` baseline directory gone locally, still on the remote
///   under the recorded uuid at the recorded path, whose subtree reappears under exactly one new
///   local directory (its [`Signature`]: the same relative paths and kinds, and every file with the
///   same hash and size). The subtree has to hold at least one file — a subtree of empty
///   directories says nothing about identity — and a second match on either end is ambiguous. It
///   becomes a `MoveRemote`. A directory nested in the new one that is itself such a move does not
///   count against the match (see [`new_local_dir_signatures`]). A move that also changed something
///   inside does not match; it falls back to the per-path plan, which is correct and only slower.
///
/// Either is left to the per-path plan when any of these holds:
/// - a baseline row at or under the source is not `Synced` (a held conflict, an adopted row);
/// - the destination is taken, or has something under it, in the baseline or on the side being
///   moved — compared by collision key, which is how the server dedups names;
/// - one end lies under the other, or under a path the cache is holding (`held`);
/// - a directory above the destination is missing on the side being moved and the pass will not
///   create it there (see [`parents_ready`]). One the other side has new is created just before the
///   move, and one another move of this fold is headed to waits for that move;
/// - on the remote, the path a directory passes through between its re-parent and its rename is
///   taken.
fn next_dir_move<'m>(
	mode: super::SyncMode,
	baseline: &Baseline,
	local: &'m SideRef<'m, LocalNode>,
	remote: &'m SideRef<'m, RemoteNode>,
	held: &BTreeSet<String>,
	dirs: &FoldDirs<'m>,
) -> Option<(SyncAction, Matched)> {
	// Only a synced directory can be the source of a directory move, and a whole pass hands over
	// only the ones a side no longer holds where their row records them — so a path is copied only
	// for a row that is actually a candidate.
	let mut sources: Vec<MoveSource> = Vec::new();
	dirs.visit_sources(mode, baseline, local, remote, |source| sources.push(source));
	if sources.is_empty() {
		return None;
	}
	sources.sort_unstable_by(|a, b| {
		let (a, b) = (a.path.as_str(), b.path.as_str());
		(a.matches('/').count(), a).cmp(&(b.matches('/').count(), b))
	});
	// Where the remote holds each directory a pull is looked for, and only those: the last of each
	// uuid's directories in path order, as a walk keeping every one would leave it.
	let remote_dir_at: HashMap<Uuid, Cow<'m, str>> = {
		let pulled: HashSet<Uuid> = sources
			.iter()
			.filter(|source| source.local_dir)
			.map(|source| source.uuid)
			.collect();
		let mut at = HashMap::new();
		if !pulled.is_empty() {
			dirs.visit_remote(remote, |path, uuid| {
				if pulled.contains(&uuid) {
					at.insert(uuid, path);
				}
			});
		}
		at
	};
	let mut search = DirMoveSearch {
		mode,
		baseline,
		local,
		remote,
		held,
		dirs,
		sources: &sources,
		remote_dir_at,
		gone: None,
		new_local_dirs: None,
	};

	// Every move that holds up but for the parents of its destination. A missing parent another of
	// them is moving into place is not one the pass creates: that move goes first, or this one would
	// fill its destination and refuse it.
	let candidates: Vec<(SyncAction, Matched)> = sources
		.iter()
		.filter_map(|source| dir_move_from(&mut search, source))
		.collect();
	let pending: HashSet<String> = candidates
		.iter()
		.map(|(action, _)| collision_key(action.rel_path()))
		.collect();
	candidates.into_iter().find(|(action, _)| {
		let to = action.rel_path();
		match action {
			SyncAction::MoveRemote { .. } => {
				parents_ready(remote, baseline, &pending, to, |n| n.kind == NodeKind::Dir)
			}
			_ => parents_ready(local, baseline, &pending, to, |n| n.kind == NodeKind::Dir),
		}
	})
}

/// One [`next_dir_move`] search: the pass's inputs and the indexes it builds once for every source.
struct DirMoveSearch<'a> {
	mode: super::SyncMode,
	baseline: &'a Baseline,
	local: &'a SideRef<'a, LocalNode>,
	remote: &'a SideRef<'a, RemoteNode>,
	held: &'a BTreeSet<String>,
	dirs: &'a FoldDirs<'a>,
	sources: &'a [MoveSource],
	remote_dir_at: HashMap<Uuid, Cow<'a, str>>,
	/// The sources gone locally with their signatures, and the new local directories with theirs,
	/// built only once a pushed move is possible — and each read once for every source it answers.
	gone: Option<Gone<'a>>,
	new_local_dirs: Option<NewLocalDirs<'a>>,
}

/// The directory move of the baseline directory at `source`, checked against everything
/// [`next_dir_move`] requires except the parents of its destination.
fn dir_move_from(
	search: &mut DirMoveSearch<'_>,
	source: &MoveSource,
) -> Option<(SyncAction, Matched)> {
	let (from, uuid) = (source.path.as_str(), source.uuid);
	let DirMoveSearch {
		mode,
		baseline,
		local,
		remote,
		held,
		dirs,
		sources,
		ref remote_dir_at,
		ref mut gone,
		ref mut new_local_dirs,
	} = *search;
	let (action, matched) = if source.local_dir {
		let to = remote_dir_at.get(&uuid)?.as_ref();
		// The steady-state answer, before the scan that would reach it the slow way: the directory
		// is where the baseline recorded it. `occupied(local, from)` is true whenever it is — the
		// branch only runs with a local DIRECTORY at `from` — so this refuses exactly what it
		// refused before, without folding the case of every local key to find that out.
		if to == from {
			return None;
		}
		let action = (!local.occupied(to)).then(|| SyncAction::MoveLocal {
			from_path: from.to_string(),
			to_path: to.to_string(),
			kind: NodeKind::Dir,
		})?;
		(action, Matched::Identity)
	} else if mode.pushes()
		&& remote
			.at(from)
			.is_some_and(|n| n.kind == NodeKind::Dir && n.remote_uuid == uuid)
		&& !local.occupied(from)
	{
		// Nothing local is at `from` under any spelling, so it holds nothing there either: it is one
		// of the sources gone locally, and in `gone` exactly when the baseline vouches for it. The
		// first source to get here reads every such source's signature, in one walk of the rows.
		let gone = gone.get_or_insert_with(|| gone_signatures(baseline, local, sources));
		let signature = *gone.by_path.get(from)?;
		let new_dirs = new_local_dirs
			.get_or_insert_with(|| new_local_dir_signatures(baseline, local, gone, dirs));
		let mut matches = new_dirs.matching(baseline, local, from, signature);
		let (dir, full) = matches.next()?;
		if matches.next().is_some() {
			return None;
		}
		let to = dir.path.as_ref();
		// The same subtree vanished from somewhere else too: which of the two moved is a guess.
		let twin = gone.by_signature.get(&signature).is_some_and(|others| {
			others
				.iter()
				.any(|other| *other != from && same_rows(baseline, from, other))
		});
		let (to_parent, to_name) = (parent_path(to), leaf(to));
		let via = match to_parent.is_empty() {
			true => leaf(from).to_string(),
			false => format!("{to_parent}/{}", leaf(from)),
		};
		let passes_free =
			parent_path(from) == to_parent || leaf(from) == to_name || !remote.occupied(&via);
		let action =
			(!twin && passes_free && !remote.occupied(to)).then(|| SyncAction::MoveRemote {
				from_path: from.to_string(),
				to_path: to.to_string(),
				kind: NodeKind::Dir,
				remote_uuid: uuid,
			})?;
		(action, Matched::Signature { full })
	} else {
		return None;
	};
	let (from_key, to_key) = (collision_key(from), collision_key(action.rel_path()));
	let to = action.rel_path();
	let clear = from_key != to_key
		&& !is_under(&to_key, &from_key)
		&& !is_under(&from_key, &to_key)
		&& !baseline.occupied(to)
		&& ![from, to].into_iter().any(|end| touches_held(end, held))
		&& baseline.subtree_all_synced(from);
	clear.then_some((action, matched))
}

/// What a directory holds, relative to it, as a digest: every path under it in path order, each
/// with what it adds to the signature ([`SignatureEntry`]). Two directories that hold the same tree
/// have the same signature. Two with the same signature are only NOMINATED as holding the same
/// tree: [`same_tree`] and [`same_rows`] compare the two subtrees path for path, and decide.
///
/// A digest rather than the tree itself, so that what a fold holds for a pushed move is a few
/// bytes per candidate directory: a copy of each candidate's subtree was what every directory
/// above a path held again, the tree's depth over.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct Signature([u8; 32]);

/// What one path under a directory adds to the directory's signature: its kind and, for a file,
/// its content hash and size. `None` for a path no signature can be read over (see
/// [`row_signature_entry`] and [`local_signature_entry`]).
type SignatureEntry = Option<(NodeKind, Option<Blake3Hash>, u64)>;

/// What a baseline row adds to its directory's signature: `None` where the baseline cannot vouch for
/// the row — one that is not `Synced`, or a file row without a hash or a size.
fn row_signature_entry(row: &BaselineEntry) -> SignatureEntry {
	if row.state != BaselineState::Synced {
		return None;
	}
	Some(match row.kind {
		NodeKind::File => (NodeKind::File, Some(row.content_hash?), row.size?),
		NodeKind::Dir => (NodeKind::Dir, None, 0),
	})
}

/// What a local node adds to its directory's signature: `None` for a file without a hash.
fn local_signature_entry(node: &LocalNode) -> SignatureEntry {
	Some(match node.kind {
		NodeKind::File => (NodeKind::File, Some(node.content_hash?), node.size),
		NodeKind::Dir => (NodeKind::Dir, None, 0),
	})
}

/// A [`Signature`] being read, a path at a time in path order. A path that adds `None` spoils it.
struct SignatureReader {
	hasher: Option<blake3::Hasher>,
	/// Whether a file was among the paths read.
	files: bool,
}

impl SignatureReader {
	fn new() -> Self {
		Self {
			hasher: Some(blake3::Hasher::new()),
			files: false,
		}
	}

	/// Add the path `rel` below the directory, with what it adds. Each field is written whole and
	/// the path with its length first, so no two sequences of paths read the same bytes.
	fn add(&mut self, rel: &str, entry: SignatureEntry) {
		let Some(hasher) = &mut self.hasher else {
			return;
		};
		let Some((kind, hash, size)) = entry else {
			self.hasher = None;
			return;
		};
		hasher.update(&rel.len().to_le_bytes());
		hasher.update(rel.as_bytes());
		hasher.update(&kind.as_i64().to_le_bytes());
		match hash {
			Some(hash) => hasher.update(&[1]).update(hash.as_ref()),
			None => hasher.update(&[0]),
		};
		hasher.update(&size.to_le_bytes());
		self.files |= kind == NodeKind::File;
	}

	/// The signature read, and whether a file was among it; `None` once spoilt.
	fn finish(self) -> Option<(Signature, bool)> {
		let hasher = self.hasher?;
		Some((Signature(*hasher.finalize().as_bytes()), self.files))
	}
}

/// The signature of every directory in `roots`, and whether it holds a file, in ONE walk of each
/// outermost root's subtree: `walk` hands `add` every path strictly under the root it is given, in
/// path order, with what the path adds. Each path is added to the signature of every root it is
/// under, so a nested root's paths are read once rather than once per root above them.
///
/// A root's subtree is the run of paths from `root/` to `root0` (see [`subtree_bounds`]), and two
/// roots' runs are nested or apart, so the roots are taken up in the order their runs start: a
/// root is opened at the first path inside its run and closed at the first path past it, and the
/// open ones are always the roots the path in hand is under, outermost first.
fn read_signatures<'r>(
	roots: impl IntoIterator<Item = &'r str>,
	mut walk: impl FnMut(&str, &mut dyn FnMut(&str, SignatureEntry)),
) -> HashMap<&'r str, Option<(Signature, bool)>> {
	let mut roots: Vec<&str> = roots.into_iter().collect();
	roots.sort_unstable_by(|a, b| run_start(a).cmp(run_start(b)));
	roots.dedup();
	let mut read = HashMap::with_capacity(roots.len());
	let mut rest = roots.as_slice();
	while let Some((&outer, after)) = rest.split_first() {
		let (nested, next) = after.split_at(after.partition_point(|root| is_under(root, outer)));
		let mut nested = nested.iter().copied().peekable();
		let mut open = vec![(outer, SignatureReader::new())];
		walk(outer, &mut |path, entry| {
			// Never the outer root: the walk is its subtree.
			while open.len() > 1
				&& let Some((root, reader)) = open.pop_if(|(root, _)| !is_under(path, root))
			{
				read.insert(root, reader.finish());
			}
			while let Some(root) = nested.next_if(|root| run_start(root).le(path.bytes())) {
				match is_under(path, root) {
					true => open.push((root, SignatureReader::new())),
					// Its run lies before this path, with nothing in it.
					false => {
						read.insert(root, SignatureReader::new().finish());
					}
				}
			}
			for (root, reader) in &mut open {
				if let Some(rel) = path.strip_prefix(*root) {
					reader.add(rel, entry);
				}
			}
		});
		for (root, reader) in open {
			read.insert(root, reader.finish());
		}
		for root in nested {
			read.insert(root, SignatureReader::new().finish());
		}
		rest = next;
	}
	read
}

/// Where the run of paths under `root` starts in path order: `root/`.
fn run_start(root: &str) -> impl Iterator<Item = u8> + Clone + '_ {
	root.bytes().chain([b'/'])
}

/// The signature of the local directory at `root` less everything at or under `excluded`; `None`
/// when a file it holds has no hash.
fn local_signature(
	local: &SideRef<'_, LocalNode>,
	root: &str,
	excluded: &BTreeSet<String>,
) -> Option<Signature> {
	let mut reader = SignatureReader::new();
	for (path, node) in local.under(root) {
		if !at_or_under_root(excluded, &path) {
			reader.add(path.strip_prefix(root)?, local_signature_entry(&node));
		}
	}
	reader.finish().map(|(signature, _)| signature)
}

/// Whether the baseline's subtree at `from` and the local one at `to`, less everything at or under
/// `excluded`, hold the same tree: path for path in path order, each the same below its root and
/// adding the same to its signature. What decides a match equal signatures only nominate.
fn same_tree(
	baseline: &Baseline,
	from: &str,
	local: &SideRef<'_, LocalNode>,
	to: &str,
	excluded: &BTreeSet<String>,
) -> bool {
	same_entries(
		row_entries(baseline, from),
		from,
		local
			.under(to)
			.filter(|(path, _)| !at_or_under_root(excluded, path))
			.map(|(path, node)| (local_signature_entry(&node), path)),
		to,
	)
}

/// Whether the baseline's subtrees at `a` and `b` hold the same tree (see [`same_tree`]).
fn same_rows(baseline: &Baseline, a: &str, b: &str) -> bool {
	same_entries(row_entries(baseline, a), a, row_entries(baseline, b), b)
}

/// Every row strictly under `root`, with what it adds to the signature, in path order.
fn row_entries<'b>(
	baseline: &'b Baseline,
	root: &'b str,
) -> impl Iterator<Item = (SignatureEntry, Cow<'static, str>)> + 'b {
	baseline
		.subtree(root)
		.map(|row| (row_signature_entry(&row), Cow::Owned(row.rel_path)))
}

/// Whether `left` under `left_root` and `right` under `right_root` are the same paths below their
/// roots, in the same order, each adding the same.
fn same_entries<'l, 'r>(
	mut left: impl Iterator<Item = (SignatureEntry, Cow<'l, str>)>,
	left_root: &str,
	mut right: impl Iterator<Item = (SignatureEntry, Cow<'r, str>)>,
	right_root: &str,
) -> bool {
	loop {
		match (left.next(), right.next()) {
			(None, None) => return true,
			(Some((left_entry, left_path)), Some((right_entry, right_path)))
				if left_entry == right_entry
					&& left_path.strip_prefix(left_root) == right_path.strip_prefix(right_root) => {}
			_ => return false,
		}
	}
}

/// The move sources gone locally whose subtree the baseline vouches for, each with its signature
/// (see [`gone_signatures`]).
struct Gone<'s> {
	by_path: HashMap<&'s str, Signature>,
	/// The sources with each signature.
	by_signature: HashMap<Signature, Vec<&'s str>>,
}

impl<'s> Gone<'s> {
	/// The source that holds what the local directory `to` holds less `excluded`, whose signature is
	/// `signature`, if one does.
	fn explaining(
		&self,
		baseline: &Baseline,
		signature: Signature,
		local: &SideRef<'_, LocalNode>,
		to: &str,
		excluded: &BTreeSet<String>,
	) -> Option<&'s str> {
		self.by_signature
			.get(&signature)?
			.iter()
			.copied()
			.find(|from| same_tree(baseline, from, local, to, excluded))
	}
}

/// Every one of `sources` gone locally whose subtree the baseline vouches for, with its signature:
/// what a pushed move is matched from, and what makes one a guess. Read in one walk of the rows
/// under the outermost of them (see [`read_signatures`]).
fn gone_signatures<'s>(
	baseline: &Baseline,
	local: &SideRef<'_, LocalNode>,
	sources: &'s [MoveSource],
) -> Gone<'s> {
	let read = read_signatures(
		sources
			.iter()
			.filter(|source| !source.local_dir && !local.holds(&source.path))
			.map(|source| source.path.as_str()),
		|root, add| {
			for row in baseline.subtree(root) {
				add(&row.rel_path, row_signature_entry(&row));
			}
		},
	);
	let mut gone = Gone {
		by_path: HashMap::new(),
		by_signature: HashMap::new(),
	};
	for (path, read) in read {
		// A subtree without a file says nothing about identity.
		let Some((signature, true)) = read else {
			continue;
		};
		gone.by_path.insert(path, signature);
		gone.by_signature.entry(signature).or_default().push(path);
	}
	gone
}

/// A new local directory and the signatures a pushed move into it is matched by: everything it holds,
/// and — when a directory nested in it is a move of its own — what it holds without those ones.
struct NewLocalDir<'a> {
	path: Cow<'a, str>,
	full: Signature,
	/// What it holds less the nested directories that are moves of their own, and those
	/// directories.
	without_nested_moves: Option<(Signature, BTreeSet<String>)>,
	/// The source its full signature was found to match, path for path, which a match of that
	/// source here need not check again.
	explained_by: Option<&'a str>,
	/// Whether it is a move of its own: one of its signatures is a source's.
	moved: bool,
}

impl NewLocalDir<'_> {
	/// Whether the source `from`, whose signature is `signature`, moved here — and then whether it
	/// was the directory's full signature that matched.
	fn matched(
		&self,
		baseline: &Baseline,
		local: &SideRef<'_, LocalNode>,
		from: &str,
		signature: Signature,
	) -> Option<bool> {
		if self.full == signature
			&& (self.explained_by == Some(from)
				|| same_tree(baseline, from, local, &self.path, &BTreeSet::new()))
		{
			return Some(true);
		}
		let (without, excluded) = self.without_nested_moves.as_ref()?;
		(*without == signature && same_tree(baseline, from, local, &self.path, excluded))
			.then_some(false)
	}
}

/// The new local directories, and each by every signature it is matched by.
struct NewLocalDirs<'a> {
	dirs: Vec<NewLocalDir<'a>>,
	by_signature: HashMap<Signature, Vec<usize>>,
}

impl NewLocalDirs<'_> {
	/// Every directory the source `from`, whose signature is `signature`, moved to, with whether its
	/// full signature matched.
	fn matching<'d>(
		&'d self,
		baseline: &'d Baseline,
		local: &'d SideRef<'_, LocalNode>,
		from: &'d str,
		signature: Signature,
	) -> impl Iterator<Item = (&'d NewLocalDir<'d>, bool)> {
		self.by_signature
			.get(&signature)
			.into_iter()
			.flatten()
			.filter_map(move |&at| {
				let dir = self.dirs.get(at)?;
				Some((dir, dir.matched(baseline, local, from, signature)?))
			})
	}
}

/// The local directories the baseline does not record, with their signatures (see [`NewLocalDir`]).
/// A nested directory is a move of its own when either of its signatures is that of a synced
/// directory gone locally (`gone`, see [`gone_signatures`]); `mv z new; mv b new/b` then matches
/// `new` to `z` and `new/b` to `b`, while a subdirectory that moved along inside its parent still
/// leaves the parent's full signature to match. A directory whose full signature matches a gone one
/// gets no reduced signature at all: the full match already explains it.
///
/// The full signatures are read in one walk of each outermost new directory (see
/// [`read_signatures`]), the reduced ones each in a walk of its directory; then the directories are
/// taken up in reverse path order, which puts every one after the directories nested in it, so a
/// chain of such moves reduces from the inside out.
fn new_local_dir_signatures<'a>(
	baseline: &Baseline,
	local: &'a SideRef<'a, LocalNode>,
	gone: &Gone<'a>,
	dirs: &FoldDirs<'a>,
) -> NewLocalDirs<'a> {
	let mut paths: Vec<Cow<'a, str>> = Vec::new();
	dirs.visit_local_new(local, baseline, |path| paths.push(path));
	let read = read_signatures(paths.iter().map(AsRef::as_ref), |root, add| {
		for (path, node) in local.under(root) {
			add(&path, local_signature_entry(&node));
		}
	});
	let fulls: Vec<Option<Signature>> = paths
		.iter()
		.map(|path| Some(read.get(path.as_ref()).copied()??.0))
		.collect();
	drop(read);
	// A directory under which a file has no hash has no signature to match.
	let mut new_dirs: Vec<NewLocalDir<'a>> = paths
		.into_iter()
		.zip(fulls)
		.filter_map(|(path, full)| {
			Some(NewLocalDir {
				path,
				full: full?,
				without_nested_moves: None,
				explained_by: None,
				moved: false,
			})
		})
		.collect();
	new_dirs.sort_unstable_by(|a, b| a.path.cmp(&b.path));
	for at in (0..new_dirs.len()).rev() {
		let (upto, after) = new_dirs.split_at_mut(at + 1);
		let Some(dir) = upto.last_mut() else {
			continue;
		};
		// A directory whose whole tree already matches one gone locally is that move, its nested
		// directories included: stripping one would let a deleted directory holding the rest claim it.
		dir.explained_by = gone.explaining(baseline, dir.full, local, &dir.path, &BTreeSet::new());
		dir.moved = dir.explained_by.is_some();
		if dir.moved {
			continue;
		}
		// The new directories nested in it: the run of them from `dir/` on, in path order.
		let bounds = subtree_bounds(&dir.path);
		let excluded: BTreeSet<String> = after
			.iter()
			.skip_while(|inner| *inner.path < *bounds.start)
			.take_while(|inner| *inner.path < *bounds.end)
			.filter(|inner| inner.moved)
			.map(|inner| inner.path.to_string())
			.collect();
		if excluded.is_empty() {
			continue;
		}
		let Some(without) = local_signature(local, &dir.path, &excluded) else {
			continue;
		};
		dir.moved = gone
			.explaining(baseline, without, local, &dir.path, &excluded)
			.is_some();
		dir.without_nested_moves = Some((without, excluded));
	}
	let mut by_signature: HashMap<Signature, Vec<usize>> = HashMap::new();
	for (at, dir) in new_dirs.iter().enumerate() {
		by_signature.entry(dir.full).or_default().push(at);
		if let Some((without, _)) = &dir.without_nested_moves
			&& *without != dir.full
		{
			by_signature.entry(*without).or_default().push(at);
		}
	}
	NewLocalDirs {
		dirs: new_dirs,
		by_signature,
	}
}

/// Whether every directory above `to` exists on `side`, or is one the reconcile creates there
/// because the other side has it new: nothing holds that path on `side` or in the baseline, under any
/// spelling, and no other directory move of this fold is headed there (`pending`, collision keys) —
/// that one runs first and brings the parent. The engine runs those creates just before the move
/// that needs them.
fn parents_ready<N: NodesAt>(
	side: &N,
	baseline: &Baseline,
	pending: &HashSet<String>,
	to: &str,
	is_dir: impl Fn(&N::Node) -> bool,
) -> bool {
	let mut parent = parent_path(to);
	while !parent.is_empty() {
		match side.at(parent) {
			// An existing directory stands on existing ones.
			Some(node) => return is_dir(&node),
			None if side.occupied(parent)
				|| baseline.occupied(parent)
				|| pending.contains(&collision_key(parent)) =>
			{
				return false;
			}
			None => parent = parent_path(parent),
		}
	}
	true
}

/// The last component of a `/`-joined relative path.
fn leaf(rel_path: &str) -> &str {
	rel_path.rsplit_once('/').map_or(rel_path, |(_, name)| name)
}

/// The parent part of a `/`-joined relative path (`""` at the top level).
fn parent_path(rel_path: &str) -> &str {
	rel_path.rsplit_once('/').map_or("", |(parent, _)| parent)
}

/// What a pass must NOT act on, beyond what the three inputs themselves say.
#[derive(Debug, Default)]
pub(crate) struct PassHolds {
	/// Remote uuids this engine already sent to the trash, whose removal the cache has not applied
	/// yet. Only the DELETION of those uuids is suppressed, not their paths: a
	/// delete-then-recreate uploads a new file at the very path that was just trashed, and
	/// freezing it would stall that.
	pub(crate) trashed: HashSet<Uuid>,
	/// Remote paths the cache is showing mid-transition (see [`RemoteView::held_paths`]).
	pub(crate) held_remote: BTreeSet<String>,
}

/// Which paths one [`reconcile`] decides.
///
/// A pass that read both sides whole knows every path and decides every path. A change-scoped pass
/// read only what its changelists named and DERIVED the rest from the baseline rows
/// (see [`derive`](super::derive)), so almost every path in its maps is one whose two nodes came
/// out of its own row — and the reconcile's answer at such a path is "the three inputs agree",
/// which is no action at all. Deciding them again is what the reconcile costs at rest, and this is
/// what takes that cost away.
///
/// # What [`Changed`](Self::Changed) requires of its caller
///
/// Every path where the local map, the remote map or the baseline row is not what
/// [`derive::carried`](super::derive) put there must be in the set. That is not a convention a
/// caller has to remember: `carried` hands back BOTH of a row's nodes or neither, and a row it
/// cannot carry goes into the dirty set in the same statement — so a derivation's own set already
/// names every path whose nodes are not its row's, and what the pass adds to it is every path its
/// observations moved off that row (see `derive::Derived::decided`).
///
/// One kind of path leaves the set again: one a proven directory move carried, once the fold has
/// shown both of its nodes to be the ones its row describes — the local one by the move's
/// signature, the remote one because the pass recorded nothing there (see [`fold_dir_moves`]).
/// That is a proof that the three inputs agree there rather than an observation of it, and
/// nothing else takes a path back out.
///
/// A path left out of the set is a path nothing decides. That can never INVENT an absence — a
/// deletion is only ever planned at a path this loop visits — but it can DELAY one, which is
/// invariant I1's safe direction, and the reason the requirement above is written in terms of
/// evidence rather than of paths.
#[derive(Debug, Clone, Copy)]
pub(crate) enum PassPaths<'a> {
	/// Decide every path the three inputs hold.
	Whole,
	/// Decide only these, plus the rows under a held path (which cost their count and nothing
	/// else — see [`reconcile_keys`]).
	Changed(&'a BTreeSet<String>),
}

/// One reconciled pass.
pub(crate) struct Plan {
	pub(crate) actions: Vec<SyncAction>,
	/// How many paths the pass deliberately left alone (an unresolvable name, or a deletion this
	/// engine has already made). A pass that skips what it would otherwise have done is not the
	/// same as a pass with nothing to do, and the report has to be able to tell them apart.
	pub(crate) deferred_paths: usize,
}

/// The paths [`reconcile`] decides, in path order — siblings together, so its baseline cursor
/// resolves one directory and answers for everything in it.
///
/// [`Whole`](PassPaths::Whole) is every key either side holds, plus the baseline rows NEITHER side
/// holds any more. That third set is the only part of the baseline whose paths the two side maps
/// do not already carry — a row both sides still hold is keyed by a `String` that exists, and
/// borrowing it is the difference between one allocation per row and three — and it is exactly the
/// set the both-absent arm retires. So the tree is walked for it against one reused buffer and
/// only a row in neither side is copied: a converged pair copies no path here. A staged side lends
/// no path, though, so over a whole pass's staged sides this set holds a copy of every path: a
/// whole pass reconciles them window by window instead (see `stage::reconcile`).
///
/// [`Changed`](PassPaths::Changed) is that same union intersected with the pass's own set. Nothing
/// is left unretired by the intersection: the both-absent arm retires a row whose path neither
/// side holds any more, and a path this leaves out is one whose two nodes were CARRIED from its
/// own row, or one a proven directory fold showed equal to its row (see [`fold_dir_moves`]) —
/// either way it is on both sides. A row is retired by the pass that observes its absence, and
/// observing that absence is what put the path in the set.
fn reconcile_keys<'m>(
	paths: PassPaths<'m>,
	baseline: &Baseline,
	local: &'m impl Nodes<Node = LocalNode>,
	remote: &'m impl Nodes<Node = RemoteNode>,
	held: &BTreeSet<String>,
) -> BTreeSet<Cow<'m, str>> {
	let PassPaths::Changed(changed) = paths else {
		// The remote's paths first, and the local side's put in one at a time: `insert` keeps the key
		// a set already holds, so where both sides hold a path the key is the remote map's, lent, and
		// the local side's copy — a staged side reads each path out of its stage — is dropped as soon
		// as it is made. Only the paths the local side alone holds are kept as copies, where
		// collecting both sides would hold a copy of every local path at once.
		let mut keys: BTreeSet<Cow<'m, str>> = remote.paths().collect();
		for path in local.paths() {
			keys.insert(path);
		}
		baseline.visit_row_paths(|path| {
			if !local.holds(path) && !remote.holds(path) {
				keys.insert(Cow::Owned(path.to_string()));
			}
		});
		return keys;
	};
	let mut keys: BTreeSet<Cow<'m, str>> = BTreeSet::new();
	// Intersected with the three inputs, not taken as given: a path in none of them is a path the
	// whole-tree key set does not hold either, and counting one as withheld under a held path
	// would report a deferral a whole read never reports.
	for path in changed {
		if local.holds(path) || remote.holds(path) || baseline.contains_key(path) {
			keys.insert(Cow::Borrowed(path.as_str()));
		}
	}
	// A held path withholds its whole subtree, and every path withheld is COUNTED
	// ([`Plan::deferred_paths`]). The rows under one are otherwise absent from this set — they are
	// carried, so nothing observed them — and they are the only paths whose absence would change a
	// number rather than an action. Held paths are the rows that record one side only and the ones
	// the cache is showing twice: few, and their subtrees with them.
	for root in held {
		if local.holds(root) || remote.holds(root) || baseline.contains_key(root) {
			keys.insert(Cow::Owned(root.clone()));
		}
		baseline.visit_subtree_paths(root, |path| {
			keys.insert(Cow::Owned(path.to_string()));
		});
	}
	keys
}

/// Reconcile a pair's three inputs into an ordered action plan. `baseline`/`local`/`remote` are all
/// keyed by the same NFC-normalized relative path; `holds` is what the pass must leave alone, and
/// `paths` which paths it decides (see [`PassPaths`]).
///
/// A whole pass's staged sides are reconciled window by window into the pass's stage instead (see
/// `stage::reconcile`), which plans what this plans over them: the per-path decision
/// ([`decide_path`]), the move join's ([`pull_move`], [`push_move`]) and the plan's tail
/// ([`finish_plan`]) are this one's.
pub(crate) fn reconcile(
	mode: super::SyncMode,
	baseline: &Baseline,
	local: &impl Nodes<Node = LocalNode>,
	remote: &impl Nodes<Node = RemoteNode>,
	holds: &PassHolds,
	paths: PassPaths<'_>,
) -> Plan {
	let mut actions = Vec::new();
	let mut consumed = HashSet::new();
	let keys = reconcile_keys(paths, baseline, local, remote, &holds.held_remote);
	// What the pass decides, against the rows it decides it from. Not the sides' sizes: a carried
	// side counts its nodes by asking the rows about every path it recorded — every row under a
	// directory moved away — and that read would be paid only with debug logging on, unseen.
	tracing::debug!(
		"reconcile: mode {mode:?} — {} path(s) to decide, {} baseline row(s)",
		keys.len(),
		baseline.len()
	);

	// Consume the paths the view could not resolve before anything else looks at them, so neither
	// move detection nor the per-path reconcile acts on a name the cache is showing twice. (A path
	// this engine has just written to needs no such hold: the engine folds its own unacknowledged
	// writes into the view instead — see `PendingWrites::fold_into`.)
	//
	// A held path with nothing on either side is skipped by nobody below, but it IS being withheld
	// and the report has to say so.
	let mut deferred_paths = holds
		.held_remote
		.iter()
		.filter(|path| !keys.contains(path.as_str()))
		.count();
	if !holds.held_remote.is_empty() {
		for key in &keys {
			if is_under_held_path(key, &holds.held_remote) {
				tracing::debug!(
					"reconcile: skipping {key:?} — the cache is listing that name twice, so the view cannot resolve it"
				);
				consumed.insert(key.to_string());
				deferred_paths += 1;
			}
		}
	}

	// Resolve moves next; their endpoints are then excluded from the per-path reconcile so a move
	// is never also emitted as a delete + create. This runs in EVERY mode, backup modes included: a
	// rename on the source side is one item that changed name, and mirroring it as a metadata-only
	// re-parent/rename keeps the backup a faithful copy instead of accumulating the old name beside
	// the new one forever. It does not weaken the backup guarantee — a move deletes nothing on the
	// destination, and a real deletion (content that reappears nowhere) is still suppressed by the
	// `delete_ok` gate below.
	detect_moves(
		mode,
		baseline,
		local,
		remote,
		paths,
		&mut actions,
		&mut consumed,
	);

	// The keys come out of the set in path order, so siblings arrive together: the cursor resolves
	// one directory and answers for everything under it.
	let mut rows = baseline.cursor();
	for key in &keys {
		let key: &str = key;
		if consumed.contains(key) {
			continue;
		}
		let base = rows.get(key);
		decide_path(
			mode,
			key,
			base.as_ref(),
			|| (local.at(key), remote.at(key)),
			&mut actions,
		);
	}
	finish_plan(actions, deferred_paths, holds)
}

/// Decide one path from the three inputs at it, and nothing else: what [`reconcile`] plans at every
/// path it decides that no move took, and what the windows of a whole pass's staged reconcile plan
/// at each of theirs (see `stage::reconcile`). `nodes` reads the path's local and remote nodes, and
/// is not asked at a path the baseline holds in conflict.
pub(super) fn decide_path<'n>(
	mode: super::SyncMode,
	key: &str,
	base: Option<&BaselineEntry>,
	nodes: impl FnOnce() -> (Option<Cow<'n, LocalNode>>, Option<Cow<'n, RemoteNode>>),
	actions: &mut Vec<SyncAction>,
) {
	// A held conflict is never acted on until the caller resolves it — but it IS re-reported every
	// pass, so a caller watching the reports keeps seeing what is outstanding. Its subtree is
	// suppressed once the plan is made, along with any conflict surfaced by this pass (see
	// [`finish_plan`]).
	if base.is_some_and(|b| b.state.is_conflict()) {
		actions.push(SyncAction::Conflict {
			rel_path: key.to_string(),
		});
		return;
	}
	let (local_at, remote_at) = nodes();
	let (local_node, remote_node) = (local_at.as_deref(), remote_at.as_deref());

	// A path adopted from the destination at a mode switch (see [`adopt_destination_rows`]). The
	// row is never a baseline for classification — whatever still holds the path reads as newly
	// created — and in a one-way mode it holds off the destination-side deletion for as long as the
	// source has nothing there.
	let mut base = base;
	if base.is_some_and(|b| b.state == BaselineState::Adopted) {
		let source_holds = match mode {
			super::SyncMode::LocalToRemote | super::SyncMode::LocalBackup => local_node.is_some(),
			super::SyncMode::RemoteToLocal | super::SyncMode::RemoteBackup => remote_node.is_some(),
			// Both sides are sources; the surviving copy flows back to the other one.
			super::SyncMode::TwoWay => local_node.is_some() || remote_node.is_some(),
		};
		if local_node.is_some() || remote_node.is_some() {
			if !source_holds {
				tracing::trace!(
					"reconcile: leaving {key:?} alone — the destination's copy was adopted at a mode switch and the source is still empty here"
				);
				return;
			}
			base = None;
		}
		// Neither side holds the path any more: the real row falls through below, where the
		// both-absent arm retires it.
	}

	// A backup mode is its mirror mode minus deletions on the destination — the ONE difference, so
	// the two share an arm and the deletion policy is read off the mode itself.
	let delete_ok = mode.propagates_deletes();
	match mode {
		super::SyncMode::LocalToRemote | super::SyncMode::LocalBackup => {
			push_to_remote(key, local_node, remote_node, base, delete_ok, actions)
		}
		super::SyncMode::RemoteToLocal | super::SyncMode::RemoteBackup => {
			pull_to_local(key, local_node, remote_node, base, delete_ok, actions)
		}
		super::SyncMode::TwoWay => reconcile_two_way(key, local_node, remote_node, base, actions),
	}
}

/// The plan, from the actions a reconcile planned in the order it planned them — the moves it
/// detected, then each path's in path order — and the paths it deferred: a deletion of a remote
/// item this engine already trashed dropped and counted as deferred, everything under a conflict
/// dropped, and the rest ordered for the apply. The tail of [`reconcile`]; a whole pass's staged
/// reconcile finishes its plan the same way in its stage (see `stage::Screen`).
pub(super) fn finish_plan(
	mut actions: Vec<SyncAction>,
	mut deferred_paths: usize,
	holds: &PassHolds,
) -> Plan {
	if !holds.trashed.is_empty() {
		actions.retain(|action| {
			let trashed = already_trashed(action, &holds.trashed);
			deferred_paths += usize::from(trashed);
			!trashed
		});
	}

	suppress_conflicted_subtrees(&mut actions);
	order_actions(&mut actions);
	tracing::debug!(
		"reconcile: planned {} action(s), {deferred_paths} path(s) deferred",
		actions.len()
	);
	Plan {
		actions,
		deferred_paths,
	}
}

/// Apply phases, lowest first. A "replace-delete" (a delete whose path is also created this pass —
/// a file<->dir type flip) has to precede the create at that path, but a DIRECTORY delete is
/// recursive (the server trashes a dir's whole subtree; a local delete quarantines it), so it must
/// still wait until everything moving OUT of that directory has moved.
///
/// A directory move runs before all of them: the rest of the plan already names its subtree by the
/// path it moves to (see [`fold_dir_moves`]).
pub(super) const PHASE_DIR_MOVE: u8 = 0;
pub(super) const PHASE_FILE_REPLACE_DELETE: u8 = 1;
pub(super) const PHASE_CREATE: u8 = 2;
pub(super) const PHASE_MOVE: u8 = 3;
pub(super) const PHASE_DIR_REPLACE_DELETE: u8 = 4;
pub(super) const PHASE_TRANSFER: u8 = 5;
pub(super) const PHASE_DELETE: u8 = 6;
pub(super) const PHASE_CONFLICT: u8 = 7;

/// The apply phase of an action given the paths the pass deletes and creates again
/// ([`replaced_paths`]). See the `PHASE_*` constants; other deletes run LAST (child-before-parent)
/// so a directory delete cannot strand an item an earlier move/transfer still needs.
fn action_phase(action: &SyncAction, replaced: &HashSet<String>) -> u8 {
	match action {
		SyncAction::MoveRemote {
			kind: NodeKind::Dir,
			..
		}
		| SyncAction::MoveLocal {
			kind: NodeKind::Dir,
			..
		} => PHASE_DIR_MOVE,
		// Replace-delete of a FILE: removing it strands nothing, so it goes first — the create at
		// that path would otherwise hit the still-present old item.
		SyncAction::DeleteLocal {
			kind: NodeKind::File,
			..
		}
		| SyncAction::TrashRemote {
			kind: NodeKind::File,
			..
		} if replaced.contains(action.rel_path()) => PHASE_FILE_REPLACE_DELETE,
		// Baseline bookkeeping, no transfer/dependency — order is irrelevant.
		SyncAction::AdoptBaseline { .. } => PHASE_CREATE,
		// Directories that may host a move/transfer destination — created early, parent-before-child.
		SyncAction::CreateLocalDir { .. } | SyncAction::CreateRemoteDir { .. } => PHASE_CREATE,
		// Re-parent/rename in place: sources still exist (deletes run later), destinations now exist.
		SyncAction::MoveLocal { .. } | SyncAction::MoveRemote { .. } => PHASE_MOVE,
		// Replace-delete of a DIRECTORY: recursive, so it runs only after the moves that carry
		// items out of it — but still before the transfer that recreates the path as a file.
		SyncAction::DeleteLocal { .. } | SyncAction::TrashRemote { .. }
			if replaced.contains(action.rel_path()) =>
		{
			PHASE_DIR_REPLACE_DELETE
		}
		// Content transfers into already-created parents.
		SyncAction::UploadFile { .. } | SyncAction::DownloadFile { .. } => PHASE_TRANSFER,
		// Destructive last, child-before-parent (see `order_actions`).
		SyncAction::DeleteLocal { .. } | SyncAction::TrashRemote { .. } => PHASE_DELETE,
		// Conflicts are never executed (the engine splits them out); order is irrelevant.
		SyncAction::Conflict { .. } => PHASE_CONFLICT,
	}
}

/// The paths a deletion of this pass empties and a create of the same pass fills again: the paths
/// of its type flips (see [`SyncAction::is_create`]). A lookup for either half tells whether the
/// action is part of one; a first sync, all creates, and a mass deletion, all deletions, hold none.
pub(super) fn replaced_paths<'a>(
	actions: impl IntoIterator<Item = &'a SyncAction> + Clone,
) -> HashSet<String> {
	let paths = |deletions: bool| {
		actions
			.clone()
			.into_iter()
			.filter(move |a| {
				if deletions {
					a.is_delete()
				} else {
					a.is_create()
				}
			})
			.map(SyncAction::rel_path)
	};
	// Index the smaller side and probe it with the other: a first sync is all creates and a mass
	// deletion all deletions, and neither indexes a single path.
	let (deletions, creates) = (paths(true).count(), paths(false).count());
	let index_deletions = deletions <= creates;
	let mut index: HashSet<&str> = HashSet::with_capacity(deletions.min(creates));
	index.extend(paths(index_deletions));
	paths(!index_deletions)
		.filter(|path| index.contains(path))
		// Owned, not borrowed from `actions`: `order_actions` sorts them while the set is still read.
		// It holds only the type flips, usually none.
		.map(str::to_owned)
		.collect()
}

/// The oracles the tests hold a pass to: the plan it folds in its stage, and the confirmations it
/// makes a page of rows at a time.
#[cfg(test)]
pub(super) mod test_support;

/// Order the plan so it applies safely: file replace-deletes → creates (parent-before-child) →
/// moves → directory replace-deletes → transfers → the remaining deletions (child-before-parent).
/// Deletions run last so a cascading directory delete never removes a path an earlier move/transfer
/// depends on; within a delete phase, child-before-parent keeps a server-side cascade or a local
/// subtree quarantine from racing its own children.
///
/// A file<->directory type flip at one path resolves in a SINGLE pass: the replace-delete phases
/// put the old item's removal ahead of the create at that path, with the directory case held back
/// until after the moves because trashing a directory takes its whole subtree with it.
fn order_actions(actions: &mut [SyncAction]) {
	let replaced = replaced_paths(&*actions);
	actions.sort_by(|a, b| {
		let (pa, pb) = (action_phase(a, &replaced), action_phase(b, &replaced));
		if pa != pb {
			pa.cmp(&pb)
		} else if pa == PHASE_DIR_REPLACE_DELETE || pa == PHASE_DELETE {
			// Deletes that cascade: child-before-parent (descending path).
			b.rel_path().cmp(a.rel_path())
		} else {
			// Everything else: parent-before-child (ascending path).
			a.rel_path().cmp(b.rel_path())
		}
	});
}

#[cfg(test)]
mod tests;
