//! The remote half of a change-scoped pass: apply what the cache ANNOUNCED to the derived remote
//! view, instead of reading the whole subtree back.
//!
//! The rule this module keeps is the remote twin of [`observe`](super::observe)'s: absence is only
//! ever produced by evidence about that very item. Exactly one construct here takes something out
//! of the view without putting it back — [`Gone`](super::changes::Gone), which only an actual cache
//! removal event can mint — and exactly one other vacates a path: a placement that MOVES an item,
//! whose evidence is that the item is demonstrably somewhere else now. Everything the delta does
//! not mention is left as the derived map had it.
//!
//! What cannot be DERIVED asks for a whole-tree read rather than being guessed at
//! ([`FullPassReason::RemoteUnplaceable`]): an item whose ancestry the cache does not know, a
//! tracked item renamed to a name no local path can hold, a directory displaced at its own path.
//! Each of those would otherwise have to invent where something went, and an invented answer on
//! this side ends in a deletion.
//!
//! Entries are applied IN ORDER. Two of them can name the same path — a create over a path a
//! removal just vacated, a rename onto a name a move freed — and the LAST one is the current
//! state, so the order the cache dispatched them in is the order they are applied in.

use std::{
	collections::{BTreeSet, HashMap},
	mem,
	path::Path,
};

use filen_types::fs::StableUuid;
use unicode_normalization::UnicodeNormalization;
use uuid::Uuid;

use super::{
	baseline::{NodeKind, SyncedPaths},
	changes::{FullPassReason, RemoteChange, RemoteDeltaEntry},
	ignore::rule_file_dir,
	plan::{self, RemoteNode, in_quarantine, is_safe_name, join_path},
	rows::Baseline,
	side::{Nodes, NodesAt, Side},
};
use crate::cache::{RemoteItem, SearchResult, hydrate_by_uuids, read_ancestors};

/// Reads the cached ancestry of one item: the item itself and every ancestor of it, in any order.
///
/// An EMPTY answer means the cache does not know the item — never that the item has no ancestors
/// (see [`read_ancestors`]). A hook rather than a cache handle, so applying a delta stays a pure
/// function of the delta, the view and the baseline; [`cache_ancestry`] is the reader the engine
/// passes, on the blocking thread the rest of the derivation runs on.
pub(super) type Ancestry<'a> = &'a mut dyn FnMut(Uuid) -> rusqlite::Result<Vec<RemoteItem>>;

/// What the announced changes did to the derived remote view.
#[derive(Debug)]
pub(super) struct RemoteObservation {
	/// The view after the delta, keyed exactly as `plan::place_remote_items` keys it.
	pub(super) nodes: Side<RemoteNode>,
	/// Where this delta PLACED each item it moved — the overlay
	/// [`path_of`](Self::path_of) answers over, and nothing else.
	///
	/// The whole index used to be built here, from the derived map, before a single change was
	/// applied: one `String` clone and one hash per node of the tree, to answer a handful of
	/// lookups. The derived map is the baseline's own rows keyed by the path each row sits on, so
	/// [`placed`](Self::placed) — the store's own index, seeked once for the ids this delta names
	/// — already answers for every item this delta has not touched.
	moved: HashMap<Uuid, String>,
	/// Every path the delta touched, including the ones it vacated. These are the remote side's
	/// dirty paths: they go into the local observation's dirty set (a path dirty on either side is
	/// re-observed on both) and they are the prefixes the pass's carried facts are pruned by.
	/// A directory carries its subtree, so only the root of a moved or removed subtree is listed.
	pub(super) touched: BTreeSet<String>,
	/// Every path whose node the delta actually added, replaced or removed — the EXACT keys, one
	/// per node, where [`touched`](Self::touched) names only the root of a subtree.
	///
	/// The two answer different questions and cannot be one set. `touched` says where to LOOK, and
	/// a subtree root says it for everything under it — expanding it would send the local half off
	/// to re-stat every descendant of a removed directory. This says what to DECIDE, and the
	/// reconcile needs every key that moved by name (see
	/// [`PassPaths::Changed`](super::plan::PassPaths::Changed)).
	pub(super) changed: BTreeSet<String>,
	/// Paths withheld from this pass, in the sense [`RemoteView::held_paths`](super::plan::RemoteView::held_paths)
	/// gives them: the cache is showing a transition at that path and no action may be planned on
	/// it. Merged into the view's own held set by the caller.
	pub(super) held_paths: BTreeSet<String>,
	/// Directories whose `.filenignore` the delta changed. Their subtrees have to be RE-DERIVED,
	/// not carried: what the rules hide below them changed, and an item the previous pass hid has
	/// no baseline row to derive from. The producer collapses a remote rule-file event to a full
	/// pass today ([`FullPassReason::RulesChanged`]), so this is normally empty; it is what lets
	/// that collapse be relaxed, exactly as `observe_local` maps a dirty rule file to its
	/// directory for the local side.
	pub(super) rule_dirs: BTreeSet<String>,
	/// A removal that named a successor, by that successor's uuid: the path it left behind, until
	/// something puts the successor somewhere (see [`settle_superseded`](Self::settle_superseded)).
	superseded: HashMap<Uuid, String>,
	/// Where the baseline records each item this delta can name, resolved BEFORE the first entry
	/// is applied (see [`delta_uuids`]).
	///
	/// Owned rather than borrowed so that every method reaching [`path_of`](Self::path_of) keeps
	/// the signature it had. It holds one entry per id the delta names, not one per row of the
	/// pair, which is the whole point: the resident `Uuid -> row` map this replaces cost 36.75 MiB
	/// at a million rows and was consulted a handful of times a pass.
	placed: SyncedPaths,
}

/// The outcome of applying a delta: the derived view, or the reason this pass has to read the
/// remote whole after all.
#[derive(Debug)]
pub(super) enum RemoteObserved {
	Applied(Box<RemoteObservation>),
	Full(FullPassReason),
}

/// Every remote id applying `delta` can ask the baseline about, for the caller to resolve first
/// (see [`SyncedPaths`]).
///
/// The ONE place that enumeration lives, and it has to be exhaustive: [`RemoteObservation::path_of`]
/// reads an unresolved id as "held nowhere", and a move whose source reads as held nowhere is a
/// move whose source is never vacated — the item then sits at two paths in the view, and the pass
/// plans against a tree that does not exist. Every id here is one an entry NAMES:
///
/// - an upsert's own uuid and its PARENT's, which `parent_path` resolves before it will place it,
/// - a rename's uuid, which is all a metadata event carries,
/// - a removal's uuid, and the successor a versioning edit names, which `settle_superseded` asks
///   about after every entry has been applied.
///
/// Duplicates are left in: the store deduplicates before it binds anything, and a directory whose
/// hundred children changed names one parent a hundred times.
pub(super) fn delta_uuids(delta: &[RemoteDeltaEntry]) -> Vec<Uuid> {
	let mut out = Vec::with_capacity(delta.len() * 2);
	for entry in delta {
		match &entry.change {
			RemoteChange::Upsert(item) => {
				out.push(item.uuid);
				out.push(item.parent);
			}
			RemoteChange::Renamed { uuid, .. } => out.push(*uuid),
			RemoteChange::Gone(gone) => {
				out.push(gone.uuid());
				out.extend(gone.successor());
			}
		}
	}
	out
}

/// Apply `delta` to `nodes` — the remote view derived from the baseline rows — in dispatch order.
///
/// `root` is the pair's remote root uuid, the item every derived path is relative to. `baseline` is
/// the resident tree, asked where the view does not know an item's parent, before the cache is.
///
/// A delta that EMPTIES the view is not applied: an emptied remote is the one shape the guard
/// weighs against whole-tree evidence (`remote_emptied`), so it is a full-pass trigger rather than
/// a derivation (plan 3.5).
pub(super) fn observe_remote(
	root: Uuid,
	baseline: &Baseline,
	nodes: Side<RemoteNode>,
	delta: &[RemoteDeltaEntry],
	placed: SyncedPaths,
	ancestry: Ancestry<'_>,
) -> RemoteObserved {
	let mut out = RemoteObservation::new(nodes, placed);
	for entry in delta {
		if let Err(reason) = out.apply(root, baseline, &entry.change, &mut *ancestry) {
			return RemoteObserved::Full(reason);
		}
	}
	out.settle_superseded(baseline);
	if out.nodes.of(baseline).is_empty() && baseline.has_remote_rows() {
		return RemoteObserved::Full(FullPassReason::RemoteEmptied);
	}
	RemoteObserved::Applied(Box::new(out))
}

/// The cached ancestry of `uuid` read out of the cache DB at `db`, in the projection the view
/// reads: one indexed recursive walk of `items.parent` ([`read_ancestors`]), then the payloads of
/// the chain it named ([`hydrate_by_uuids`]).
///
/// Two read connections per call, which is why it is only ever asked about an item whose parent
/// chain neither the view nor the baseline knows — a subtree created or moved in out of view.
pub(super) fn cache_ancestry(db: &Path, uuid: Uuid) -> rusqlite::Result<Vec<RemoteItem>> {
	let chain = read_ancestors(db, uuid)?;
	Ok(hydrate_by_uuids(db, &chain)?.iter().map(as_item).collect())
}

/// One hydrated cache payload in the view's projection — the same mapping
/// `changes::RemoteChange::from_event` makes from an event's payload, so an item hydrated here and
/// the same item announced are one node.
fn as_item(result: &SearchResult) -> RemoteItem {
	match result {
		SearchResult::Dir(dir) => RemoteItem {
			uuid: dir.uuid,
			parent: dir.parent,
			name: dir.name.to_string(),
			// A directory has no whole-life id and no content; the view tells the two kinds apart
			// by exactly that.
			stable_uuid: None,
			hash: None,
			size: 0,
			modified_millis: dir.created.map_or(0, |at| at.timestamp_millis()),
		},
		SearchResult::File(file) => RemoteItem {
			uuid: file.uuid,
			parent: file.parent,
			name: file.name.to_string(),
			stable_uuid: Some(file.stable_uuid),
			hash: file.hash,
			size: file.size,
			modified_millis: file.last_modified.timestamp_millis(),
		},
	}
}

impl RemoteObservation {
	fn new(nodes: Side<RemoteNode>, placed: SyncedPaths) -> Self {
		Self {
			nodes,
			moved: HashMap::new(),
			touched: BTreeSet::new(),
			changed: BTreeSet::new(),
			held_paths: BTreeSet::new(),
			rule_dirs: BTreeSet::new(),
			superseded: HashMap::new(),
			placed,
		}
	}

	/// Where the view holds `uuid`, or `None` when it holds it nowhere.
	///
	/// Asked of two sources in turn: what this delta placed ([`moved`](Self::moved)), then where
	/// the baseline rows record it ([`placed`](Self::placed)), whose rows ARE the derived map —
	/// `observe_remote` is only ever handed the map `derive::from_baseline` built from the very
	/// tree it is passed, and each of those nodes carries its row's uuid at its row's path.
	///
	/// Either candidate is then checked against `nodes`, which is what makes the second source
	/// safe rather than a guess. A row the derivation could not carry, a node the delta has since
	/// detached, a uuid a later item displaced at its path and — on a view that is NOT derived from
	/// this baseline, which only a test builds — a row that never described this view at all: each
	/// reads as "held nowhere", exactly as a map rebuilt from `nodes` would answer.
	fn path_of(&self, baseline: &Baseline, uuid: Uuid) -> Option<String> {
		let candidate = match self.moved.get(&uuid) {
			Some(path) => path.clone(),
			None => self.placed.path_by_uuid(uuid)?,
		};
		self.nodes
			.of(baseline)
			.at(&candidate)
			.is_some_and(|node| node.remote_uuid == uuid)
			.then_some(candidate)
	}

	fn apply(
		&mut self,
		root: Uuid,
		baseline: &Baseline,
		change: &RemoteChange,
		ancestry: Ancestry<'_>,
	) -> Result<(), FullPassReason> {
		match change {
			RemoteChange::Upsert(item) => {
				let parent = self.parent_path(root, baseline, item, ancestry)?;
				let name = item.name.nfc().collect::<String>();
				self.place(baseline, item.uuid, &parent, &name, node_of(item))
			}
			RemoteChange::Renamed {
				uuid,
				name,
				content,
			} => {
				let (parent, mut node) = self.renamed_at(root, baseline, *uuid, ancestry)?;
				if let Some(content) = content {
					node.content_hash = content.hash;
					node.size = content.size;
					node.modified_millis = content.modified_millis;
				}
				let name = name.nfc().collect::<String>();
				self.place(baseline, *uuid, &parent, &name, node)
			}
			RemoteChange::Gone(gone) => {
				let uuid = gone.uuid();
				// Nothing in the view under that uuid: an item this pass's view never held, or a
				// removal already applied. Both are no-ops — and neither says anything about
				// whatever else may sit at the path.
				let Some(path) = self.path_of(baseline, uuid) else {
					return Ok(());
				};
				let removed = self.detach(baseline, &path);
				// The server takes a directory's subtree with it, and so does the view.
				if removed.is_some_and(|node| node.kind == NodeKind::Dir) {
					self.drop_subtree(baseline, &path);
				}
				self.touch(&path);
				if let Some(successor) = gone.successor() {
					// A versioning edit re-mints the file under a new uuid: the file is not gone.
					// Its upsert may be later in this delta, or in the next one (see
					// `settle_superseded`).
					self.superseded.insert(successor, path);
				}
				Ok(())
			}
		}
	}

	/// The path of `item`'s parent directory: the view first, then the baseline, then — for a
	/// subtree created or moved in out of view, which neither of those has ever seen — the cache's
	/// own ancestry walk.
	fn parent_path(
		&self,
		root: Uuid,
		baseline: &Baseline,
		item: &RemoteItem,
		ancestry: Ancestry<'_>,
	) -> Result<String, FullPassReason> {
		if item.parent == root {
			return Ok(String::new());
		}
		if let Some(path) = self.path_of(baseline, item.parent) {
			return Ok(path);
		}
		// A row the derived map left out (a conflicted row, a row whose path the rules hide) still
		// says where its item sits, and asking it costs no read.
		if let Some(path) = self.placed.path_by_uuid(item.parent) {
			return Ok(path);
		}
		let chain = read_chain(item.uuid, ancestry)?;
		ancestor_path(root, &chain, item.parent).ok_or(FullPassReason::RemoteUnplaceable)
	}

	/// Where a metadata patch's item sits and what it is: a rename moves nothing, so the item keeps
	/// the directory it is in — and the directory of an item the view does not hold is the cache's
	/// to answer, since a metadata event carries no parent at all.
	fn renamed_at(
		&self,
		root: Uuid,
		baseline: &Baseline,
		uuid: Uuid,
		ancestry: Ancestry<'_>,
	) -> Result<(String, RemoteNode), FullPassReason> {
		if let Some(at) = self.path_of(baseline, uuid) {
			let node = self
				.nodes
				.of(baseline)
				.at(&at)
				.ok_or(FullPassReason::RemoteUnplaceable)?
				.into_owned();
			return Ok((dir_of(&at).to_owned(), node));
		}
		let chain = read_chain(uuid, ancestry)?;
		let item = chain
			.iter()
			.find(|item| item.uuid == uuid)
			.ok_or(FullPassReason::RemoteUnplaceable)?;
		let parent =
			ancestor_path(root, &chain, item.parent).ok_or(FullPassReason::RemoteUnplaceable)?;
		Ok((parent, node_of(item)))
	}

	/// Put the item at `parent`/`name`, vacating wherever it was.
	fn place(
		&mut self,
		baseline: &Baseline,
		uuid: Uuid,
		parent: &str,
		name: &str,
		mut node: RemoteNode,
	) -> Result<(), FullPassReason> {
		if !is_safe_name(name) {
			// No local path can hold that name. A whole-tree read records such an item as
			// unsyncable (`RemoteView::skipped`), which is what keeps its absence from `nodes` from
			// reading as a deletion — and a `skipped` record is the one thing a derivation cannot
			// produce, since it is a statement about the whole remote. An item the view never held
			// is simply not syncable yet and needs nothing.
			return if self.path_of(baseline, uuid).is_some() {
				Err(FullPassReason::RemoteUnplaceable)
			} else {
				Ok(())
			};
		}
		let path = join_path(parent, name);
		// The engine's own bin is in no view (`place_remote_items`), so an item moved into it
		// leaves, exactly as a whole-tree read would show it leaving.
		if in_quarantine(&path) {
			self.vacate(baseline, uuid);
			return Ok(());
		}
		// A held path is one the pass may not act on; a third item arriving on it changes nothing
		// about that.
		if self.held_paths.contains(&path) {
			return self.vacate_unplaced(baseline, uuid);
		}
		if let Some(occupant) = self.displaced(baseline, &path, uuid, node.stable_uuid) {
			if occupant == NodeKind::Dir {
				// A directory displaced at its own path takes a subtree's worth of rows with it,
				// and nothing announced here says where they went.
				return Err(FullPassReason::RemoteUnplaceable);
			}
			// Two byte-identical names under one parent cannot both exist on the server, so what
			// the cache is showing is a transition. Withhold that one path for this pass, exactly
			// as the view withholds it, and let the pass run.
			self.hold(baseline, &path);
			return self.vacate_unplaced(baseline, uuid);
		}
		match self.path_of(baseline, uuid) {
			Some(from) if from != path => {
				let moved = self.detach(baseline, &from);
				// A directory's subtree moved with it; the cache lists the children under the old
				// path until it catches up, so the view re-keys them here (`fold_move` does the
				// same for this engine's own moves).
				if moved.is_some_and(|node| node.kind == NodeKind::Dir) {
					self.rekey_subtree(baseline, &from, &path);
				}
				self.touch(&from);
			}
			_ => {}
		}
		node.rel_path = path.clone();
		self.insert(path.clone(), node);
		self.touch(&path);
		Ok(())
	}

	/// What sits at `path` when it is NOT the item being placed there and not another version of
	/// it: the server re-mints a file's uuid on every content edit, and the whole-life id is what
	/// says the new item is the same file.
	fn displaced(
		&self,
		baseline: &Baseline,
		path: &str,
		uuid: Uuid,
		lineage: Option<StableUuid>,
	) -> Option<NodeKind> {
		// The side is BOUND rather than read through a temporary: `at` hands back a `Cow` borrowed
		// from the reader, which a carried side builds on the spot.
		let nodes = self.nodes.of(baseline);
		let current = nodes.at(path)?;
		let same_file = current.stable_uuid.is_some() && current.stable_uuid == lineage;
		(current.remote_uuid != uuid && !same_file).then_some(current.kind)
	}

	/// Take the node at `path` out of the view.
	///
	/// One of the two places a node enters or leaves the view, which is why
	/// [`changed`](Self::changed) is recorded here rather than at each of the callers that vacate,
	/// drop or re-key a subtree.
	///
	/// Nothing is taken out of [`moved`](Self::moved): a stale entry there names a path the view no
	/// longer holds this item at, and [`path_of`](Self::path_of) checks every candidate against
	/// `nodes` before it answers.
	fn detach(&mut self, baseline: &Baseline, path: &str) -> Option<RemoteNode> {
		let node = self.nodes.remove(baseline, path)?;
		self.changed.insert(path.to_owned());
		Some(node)
	}

	/// Take this item off the path the view has it on, because it is demonstrably not there any
	/// more — and where it is a directory, take the subtree that went with it.
	///
	/// Only for a destination the derivation KNOWS: a path that is no part of any view, which is
	/// what a whole-tree read would show too. [`vacate_unplaced`](Self::vacate_unplaced) is the
	/// form for a destination it could not work out.
	fn vacate(&mut self, baseline: &Baseline, uuid: Uuid) {
		let Some(from) = self.path_of(baseline, uuid) else {
			return;
		};
		if self
			.detach(baseline, &from)
			.is_some_and(|node| node.kind == NodeKind::Dir)
		{
			self.drop_subtree(baseline, &from);
		}
		self.touch(&from);
	}

	/// Vacate an item this pass could NOT place at a path. A directory whose subtree the view still
	/// lists cannot leave that way: its children moved with it, to paths nothing announced names,
	/// and dropping them would read as a tree of deletions — the absence this side must never
	/// invent. Only a whole-tree read can say where they went.
	fn vacate_unplaced(&mut self, baseline: &Baseline, uuid: Uuid) -> Result<(), FullPassReason> {
		if let Some(from) = self.path_of(baseline, uuid) {
			let carries_subtree = self
				.nodes
				.of(baseline)
				.at(&from)
				.is_some_and(|node| node.kind == NodeKind::Dir)
				&& self
					.nodes
					.of(baseline)
					.paths()
					.any(|key| plan::is_under(&key, &from));
			if carries_subtree {
				return Err(FullPassReason::RemoteUnplaceable);
			}
		}
		self.vacate(baseline, uuid);
		Ok(())
	}

	/// Put `node` at `path`, displacing whatever the view had there. The other half of
	/// [`detach`](Self::detach), and the other place [`changed`](Self::changed) is recorded.
	fn insert(&mut self, path: String, node: RemoteNode) {
		let uuid = node.remote_uuid;
		self.changed.insert(path.clone());
		// Whatever this displaces needs no record: the path now names a different uuid, and that is
		// what [`path_of`](Self::path_of) checks before it answers for the displaced one.
		self.nodes.insert(path.clone(), node);
		self.moved.insert(uuid, path);
	}

	/// Withhold `path` for this pass: nothing stays on it, and nothing may be planned for it.
	fn hold(&mut self, baseline: &Baseline, path: &str) {
		self.detach(baseline, path);
		self.held_paths.insert(path.to_owned());
		self.touch(path);
	}

	/// Everything strictly under `path` leaves the view with it.
	fn drop_subtree(&mut self, baseline: &Baseline, path: &str) {
		let under = self.nodes.subtree_paths(baseline, path);
		for key in under {
			self.detach(baseline, &key);
		}
	}

	/// Re-key everything the cache still lists under `from` to sit under `to`.
	fn rekey_subtree(&mut self, baseline: &Baseline, from: &str, to: &str) {
		let under = self.nodes.subtree_paths(baseline, from);
		for key in under {
			let Some(mut node) = self.detach(baseline, &key) else {
				continue;
			};
			let Some(moved) = plan::moved_path(&key, from, to) else {
				continue;
			};
			node.rel_path = moved.clone();
			self.insert(moved, node);
		}
	}

	/// Record a path the delta acted on — and, where it is a rule file, the directory whose rules
	/// it decides.
	fn touch(&mut self, path: &str) {
		if let Some(dir) = rule_file_dir(path) {
			self.rule_dirs.insert(dir.to_owned());
		}
		self.touched.insert(path.to_owned());
	}

	/// A removal that named a successor is a re-mint, not a deletion. If the successor's own upsert
	/// was in this delta the path already holds it; if it lands in the NEXT delta, the path is
	/// withheld rather than left showing an absence — which is the absence a pass would act on, and
	/// the file is still there.
	fn settle_superseded(&mut self, baseline: &Baseline) {
		for (successor, path) in mem::take(&mut self.superseded) {
			if self.path_of(baseline, successor).is_some() {
				continue;
			}
			self.hold(baseline, &path);
		}
	}
}

/// The view node an announced item describes. A file is told from a directory by its whole-life id,
/// as everywhere else in the view.
fn node_of(item: &RemoteItem) -> RemoteNode {
	RemoteNode {
		// Filled in by `place`, which is what decides the path.
		rel_path: String::new(),
		kind: if item.stable_uuid.is_some() {
			NodeKind::File
		} else {
			NodeKind::Dir
		},
		remote_uuid: item.uuid,
		stable_uuid: item.stable_uuid,
		content_hash: item.hash,
		size: item.size,
		modified_millis: item.modified_millis,
	}
}

/// The directory part of a root-relative path; `""` for a path at the pair root.
fn dir_of(path: &str) -> &str {
	path.rsplit_once('/').map_or("", |(dir, _)| dir)
}

fn read_chain(uuid: Uuid, ancestry: Ancestry<'_>) -> Result<Vec<RemoteItem>, FullPassReason> {
	ancestry(uuid).map_err(|e| {
		tracing::debug!("remote delta: reading the cached ancestry of {uuid} failed: {e}");
		FullPassReason::RemoteUnplaceable
	})
}

/// The path `target` sits at according to `chain`, walked up to `root` — `None` when the chain does
/// not reach the root (an item the cache does not know, one outside this pair, or a cached parent
/// cycle), which is UNKNOWN and never "at the root".
fn ancestor_path(root: Uuid, chain: &[RemoteItem], target: Uuid) -> Option<String> {
	let by_uuid: HashMap<Uuid, &RemoteItem> = chain.iter().map(|item| (item.uuid, item)).collect();
	let mut parts: Vec<String> = Vec::new();
	let mut at = target;
	while at != root {
		let item = by_uuid.get(&at)?;
		let name = item.name.nfc().collect::<String>();
		if !is_safe_name(&name) {
			return None;
		}
		parts.push(name);
		at = item.parent;
		// A cached parent cycle: the walk has already seen more items than the chain holds.
		if parts.len() > by_uuid.len() {
			return None;
		}
	}
	parts.reverse();
	Some(parts.join("/"))
}

#[cfg(test)]
mod tests {
	use std::borrow::Cow;

	use uuid::Uuid;

	use super::*;
	use crate::{
		cache::{CacheEvent, CacheEventType, DirEvent, FileEvent},
		fs::file::{cache::CacheableFile, meta::DecryptedFileMeta},
		sync_engine::{
			baseline::{BaselineEntry, BaselineState},
			changes::{
				PairChanges,
				tests::{cache_event, cacheable_dir, cacheable_file, dt, file_key},
			},
			ignore::FILENIGNORE,
		},
	};

	/// The pair's remote root: every derived path is relative to it.
	fn root() -> Uuid {
		Uuid::from_u128(1_000)
	}

	fn item_file(uuid: Uuid, parent: Uuid, name: &str) -> RemoteItem {
		RemoteItem {
			uuid,
			parent,
			name: name.to_owned(),
			stable_uuid: Some(StableUuid::new_for_test(uuid)),
			hash: None,
			size: 7,
			modified_millis: 1_234,
		}
	}

	fn item_dir(uuid: Uuid, parent: Uuid, name: &str) -> RemoteItem {
		RemoteItem {
			uuid,
			parent,
			name: name.to_owned(),
			stable_uuid: None,
			hash: None,
			size: 0,
			modified_millis: 99,
		}
	}

	fn placed(path: &str, item: &RemoteItem) -> (String, RemoteNode) {
		let mut node = node_of(item);
		node.rel_path = path.to_owned();
		(path.to_owned(), node)
	}

	fn file_node(path: &str, uuid: Uuid) -> (String, RemoteNode) {
		placed(path, &item_file(uuid, Uuid::nil(), path))
	}

	fn dir_node(path: &str, uuid: Uuid) -> (String, RemoteNode) {
		placed(path, &item_dir(uuid, Uuid::nil(), path))
	}

	fn view(nodes: impl IntoIterator<Item = (String, RemoteNode)>) -> Side<RemoteNode> {
		nodes.into_iter().collect()
	}

	/// The pair state a change-scoped pass starts from: the resident rows, and the derived map
	/// `derive::from_baseline` makes of exactly those rows.
	///
	/// The two are built together because that pairing is what `observe_remote` is handed — the
	/// engine takes `derived.remote` and the very baseline it came from, and
	/// `RemoteObservation::path_of` reads the tree's uuid index for every node the delta leaves
	/// alone. A map over a baseline that knows nothing about it is a shape no pass produces.
	fn derived(
		nodes: impl IntoIterator<Item = (String, RemoteNode)>,
	) -> (Baseline, Side<RemoteNode>) {
		let nodes = view(nodes);
		let baseline = Baseline::from_rows(nodes.whole().iter().map(|(_, node)| {
			let mut entry = row(&node.rel_path, node.kind, Some(node.remote_uuid));
			entry.remote_stable_uuid = node.stable_uuid;
			entry
		}));
		(baseline, nodes)
	}

	fn row(rel_path: &str, kind: NodeKind, remote_uuid: Option<Uuid>) -> BaselineEntry {
		BaselineEntry {
			rel_path: rel_path.to_owned(),
			kind,
			remote_uuid,
			content_hash: None,
			size: None,
			local_mtime: None,
			remote_modified: None,
			state: BaselineState::Synced,
			local_kind: None,
			remote_kind: None,
			remote_hash: None,
			remote_size: None,
			remote_stable_uuid: None,
			agreed_hash: None,
		}
	}

	fn meta(name: &str) -> DecryptedFileMeta<'static> {
		DecryptedFileMeta {
			name: Cow::Owned(name.to_owned()),
			size: 7,
			mime: Cow::Borrowed("text/plain"),
			key: file_key(),
			last_modified: dt(500),
			created: None,
			hash: None,
		}
	}

	/// The delta a batch of cache events produces, taken exactly as a pass takes it — the only way
	/// a `Gone` can be minted, which is the point of its private fields.
	fn delta(events: &[CacheEvent<'static>]) -> Vec<RemoteDeltaEntry> {
		let changes = PairChanges::new();
		changes.note_tree_size(4_000);
		changes.note_remote_batch(&mut events.iter());
		let entries = changes.take().take_remote();
		assert!(
			!entries.is_empty(),
			"the producer collapsed the batch instead of recording it"
		);
		entries
	}

	fn no_ancestry() -> impl FnMut(Uuid) -> rusqlite::Result<Vec<RemoteItem>> {
		|uuid: Uuid| -> rusqlite::Result<Vec<RemoteItem>> {
			panic!("no ancestry read was expected, but one was made for {uuid}")
		}
	}

	/// `observe_remote` as a pass reaches it: the ids the delta names resolved off the baseline
	/// first, exactly as the engine resolves them off the store.
	///
	/// `also` is for the ids a test goes on to ASK about itself (`out.path_of(..)`), which a real
	/// pass would only ask about because the delta named them.
	fn observed_with(
		baseline: &Baseline,
		nodes: Side<RemoteNode>,
		delta: &[RemoteDeltaEntry],
		also: &[Uuid],
		ancestry: Ancestry<'_>,
	) -> RemoteObserved {
		let mut uuids = delta_uuids(delta);
		uuids.extend_from_slice(also);
		let placed = baseline.synced_paths(&uuids, &[]);
		observe_remote(root(), baseline, nodes, delta, placed, ancestry)
	}

	fn applied(observed: RemoteObserved) -> RemoteObservation {
		match observed {
			RemoteObserved::Applied(observation) => *observation,
			RemoteObserved::Full(reason) => {
				panic!("expected a derived view, got a full pass: {reason}")
			}
		}
	}

	fn paths(out: &RemoteObservation) -> Vec<String> {
		let mut paths: Vec<String> = out
			.nodes
			.whole()
			.paths()
			.map(|path| path.into_owned())
			.collect();
		paths.sort_unstable();
		paths
	}

	fn listed(set: &BTreeSet<String>) -> Vec<&str> {
		set.iter().map(String::as_str).collect()
	}

	/// Two entries can name one path — a create over a path a removal just vacated, and the same
	/// pair the other way round. What the cache dispatched LAST is the state, in both orders.
	#[test]
	fn entries_naming_one_path_are_applied_in_dispatch_order() {
		let file = Uuid::from_u128(1);
		let born = || {
			cache_event(
				Some(1),
				CacheEventType::File(FileEvent::New(cacheable_file(file, root(), "a.txt"))),
			)
		};
		let died = || cache_event(Some(2), CacheEventType::File(FileEvent::Removed(file)));

		let empty = Baseline::default();
		let out = applied(observed_with(
			&empty,
			Side::default(),
			&delta(&[born(), died()]),
			&[file],
			&mut no_ancestry(),
		));
		assert!(out.nodes.whole().is_empty(), "{:?}", out.nodes);
		assert!(out.path_of(&empty, file).is_none());
		assert_eq!(listed(&out.touched), vec!["a.txt"]);

		let out = applied(observed_with(
			&empty,
			Side::default(),
			&delta(&[died(), born()]),
			&[file],
			&mut no_ancestry(),
		));
		assert_eq!(paths(&out), vec!["a.txt"], "the create is the last word");
		assert_eq!(out.path_of(&empty, file).as_deref(), Some("a.txt"));
	}

	/// A subtree created — or moved in — out of view: neither the derived map nor the baseline has
	/// ever seen its parent chain, so the cache's own indexed walk is what says where it sits. One
	/// walk, not one read per ancestor.
	#[test]
	fn a_subtree_created_out_of_view_takes_its_path_from_the_cached_ancestry() {
		let outer = Uuid::from_u128(11);
		let inner = Uuid::from_u128(12);
		let file = Uuid::from_u128(13);
		let chain = vec![
			item_file(file, inner, "f.txt"),
			item_dir(inner, outer, "sub"),
			item_dir(outer, root(), "out"),
		];
		let events = [cache_event(
			Some(1),
			CacheEventType::File(FileEvent::New(cacheable_file(file, inner, "f.txt"))),
		)];

		let mut reads = 0usize;
		let mut ancestry = |uuid: Uuid| {
			reads += 1;
			assert_eq!(uuid, file, "the chain is asked for by the item's own uuid");
			Ok(chain.clone())
		};
		let empty = Baseline::default();
		let out = applied(observed_with(
			&empty,
			Side::default(),
			&delta(&events),
			&[],
			&mut ancestry,
		));

		assert_eq!(reads, 1, "one indexed walk answers the whole chain");
		assert_eq!(paths(&out), vec!["out/sub/f.txt"]);
		assert_eq!(out.path_of(&empty, file).as_deref(), Some("out/sub/f.txt"));
	}

	/// An item gone takes everything under it: the server trashes a directory's subtree with it,
	/// and so does the view.
	#[test]
	fn a_gone_directory_takes_its_subtree_with_it() {
		let dir = Uuid::from_u128(21);
		let sub = Uuid::from_u128(22);
		let keep = Uuid::from_u128(25);
		let (baseline, nodes) = derived([
			dir_node("d", dir),
			file_node("d/a.txt", Uuid::from_u128(23)),
			dir_node("d/s", sub),
			file_node("d/s/b.txt", Uuid::from_u128(24)),
			file_node("keep.txt", keep),
		]);
		let events = [cache_event(
			Some(1),
			CacheEventType::Dir(DirEvent::Removed(dir)),
		)];

		let out = applied(observed_with(
			&baseline,
			nodes,
			&delta(&events),
			&[keep, dir, sub],
			&mut no_ancestry(),
		));

		assert_eq!(paths(&out), vec!["keep.txt"]);
		assert_eq!(out.path_of(&baseline, keep).as_deref(), Some("keep.txt"));
		assert!(out.path_of(&baseline, dir).is_none());
		assert!(out.path_of(&baseline, sub).is_none());
		assert_eq!(
			listed(&out.touched),
			vec!["d"],
			"the root of the removed subtree is the dirty path; a walk of it covers the rest"
		);
	}

	/// A rename onto a name another item holds cannot be true of the server — it has no two
	/// byte-identical names under one parent — so it is the cache showing a transition. The one
	/// path is withheld, exactly as the whole-tree view withholds it, and the pass runs.
	#[test]
	fn a_rename_onto_an_occupied_path_withholds_that_path() {
		let held = Uuid::from_u128(31);
		let mover = Uuid::from_u128(32);
		let (baseline, nodes) = derived([
			file_node("a.txt", held),
			file_node("b.txt", mover),
			file_node("c.txt", Uuid::from_u128(33)),
		]);
		let events = [cache_event(
			Some(1),
			CacheEventType::File(FileEvent::MetadataChanged {
				uuid: mover,
				meta: meta("a.txt"),
			}),
		)];

		let out = applied(observed_with(
			&baseline,
			nodes,
			&delta(&events),
			&[],
			&mut no_ancestry(),
		));

		assert_eq!(paths(&out), vec!["c.txt"]);
		assert_eq!(listed(&out.held_paths), vec!["a.txt"]);
		assert_eq!(listed(&out.touched), vec!["a.txt", "b.txt"]);
	}

	/// The server re-mints a file's uuid on every content edit, so a new uuid landing on a path a
	/// file of the SAME lineage holds is a new version of it rather than a second item: it
	/// replaces, and nothing is withheld.
	#[test]
	fn a_new_version_of_the_same_file_replaces_the_path_it_lands_on() {
		let old = Uuid::from_u128(41);
		let new = Uuid::from_u128(42);
		let lineage = StableUuid::new_for_test(old);
		let mut previous = item_file(old, root(), "a.txt");
		previous.stable_uuid = Some(lineage);
		let (baseline, nodes) = derived([placed("a.txt", &previous)]);
		let successor = CacheableFile {
			stable_uuid: lineage,
			..cacheable_file(new, root(), "a.txt")
		};
		let events = [cache_event(
			Some(1),
			CacheEventType::File(FileEvent::New(successor)),
		)];

		let out = applied(observed_with(
			&baseline,
			nodes,
			&delta(&events),
			&[old],
			&mut no_ancestry(),
		));

		assert_eq!(out.nodes.whole().at("a.txt").unwrap().remote_uuid, new);
		assert!(out.path_of(&baseline, old).is_none());
		assert_eq!(out.path_of(&baseline, new).as_deref(), Some("a.txt"));
		assert!(out.held_paths.is_empty(), "{:?}", out.held_paths);
	}

	/// A removal that names a successor is a re-mint, not a deletion. Until something puts the
	/// successor somewhere the path is withheld — showing it as absent is the fabricated deletion
	/// this side must never produce.
	#[test]
	fn a_removal_that_names_a_successor_withholds_its_path() {
		let old = Uuid::from_u128(51);
		let new = Uuid::from_u128(52);
		let lineage = StableUuid::new_for_test(old);
		let pair = || {
			derived([
				file_node("a.txt", old),
				file_node("keep.txt", Uuid::from_u128(53)),
			])
		};
		let trashed = || {
			cache_event(
				Some(1),
				CacheEventType::File(FileEvent::Trashed {
					uuid: old,
					stable_uuid: lineage,
					new_uuid: Some(new),
				}),
			)
		};

		let (baseline, nodes) = pair();
		let out = applied(observed_with(
			&baseline,
			nodes,
			&delta(&[trashed()]),
			&[],
			&mut no_ancestry(),
		));
		assert_eq!(paths(&out), vec!["keep.txt"]);
		assert_eq!(listed(&out.held_paths), vec!["a.txt"]);

		// The successor's own upsert is usually in the same delta, and then the path simply holds
		// the new version.
		let successor = CacheableFile {
			stable_uuid: lineage,
			..cacheable_file(new, root(), "a.txt")
		};
		let (baseline, nodes) = pair();
		let out = applied(observed_with(
			&baseline,
			nodes,
			&delta(&[
				trashed(),
				cache_event(Some(2), CacheEventType::File(FileEvent::New(successor))),
			]),
			&[],
			&mut no_ancestry(),
		));
		assert_eq!(out.nodes.whole().at("a.txt").unwrap().remote_uuid, new);
		assert!(out.held_paths.is_empty(), "{:?}", out.held_paths);
	}

	/// A directory move carries the subtree the cache still lists under its old path.
	#[test]
	fn a_moved_directory_carries_its_subtree_to_the_new_path() {
		let top = Uuid::from_u128(61);
		let dir = Uuid::from_u128(62);
		let file = Uuid::from_u128(63);
		let (baseline, nodes) = derived([
			dir_node("top", top),
			dir_node("d", dir),
			file_node("d/a.txt", file),
		]);
		let events = [cache_event(
			Some(1),
			CacheEventType::Dir(DirEvent::Move(cacheable_dir(dir, top, "d"))),
		)];

		let out = applied(observed_with(
			&baseline,
			nodes,
			&delta(&events),
			&[file],
			&mut no_ancestry(),
		));

		assert_eq!(paths(&out), vec!["top", "top/d", "top/d/a.txt"]);
		assert_eq!(out.path_of(&baseline, file).as_deref(), Some("top/d/a.txt"));
		assert_eq!(
			out.nodes.whole().at("top/d/a.txt").unwrap().rel_path,
			"top/d/a.txt"
		);
		assert_eq!(listed(&out.touched), vec!["d", "top/d"]);
	}

	/// A rule file's CONTENTS decide what is hidden for its whole directory, so a change to one
	/// re-derives that directory — the remote twin of `observe_local` mapping a dirty rule file to
	/// its directory. The producer collapses such an event to a full pass today
	/// (`FullPassReason::RulesChanged`), so the entry is built here as a relaxed one would record
	/// it.
	#[test]
	fn a_changed_rule_file_re_derives_its_directory() {
		let dir = Uuid::from_u128(71);
		let rule = Uuid::from_u128(72);
		let (baseline, nodes) = derived([dir_node("d", dir)]);
		let entries = vec![RemoteDeltaEntry {
			id: Some(1),
			change: RemoteChange::Upsert(item_file(rule, dir, FILENIGNORE)),
		}];

		let out = applied(observed_with(
			&baseline,
			nodes,
			&entries,
			&[],
			&mut no_ancestry(),
		));

		assert_eq!(listed(&out.rule_dirs), vec!["d"]);
		assert!(out.nodes.whole().holds(&format!("d/{FILENIGNORE}")));
	}

	/// A directory the derivation cannot place — the name it was renamed to is taken — cannot
	/// simply leave the view: its children moved with it, to paths nothing announced names, and
	/// dropping them would read as a tree of deletions.
	#[test]
	fn a_directory_that_cannot_be_placed_asks_for_a_full_pass() {
		let dir = Uuid::from_u128(101);
		let (baseline, nodes) = derived([
			dir_node("d", dir),
			file_node("d/a.txt", Uuid::from_u128(102)),
			file_node("taken", Uuid::from_u128(103)),
		]);
		let entries = vec![RemoteDeltaEntry {
			id: Some(1),
			change: RemoteChange::Renamed {
				uuid: dir,
				name: "taken".to_owned(),
				content: None,
			},
		}];

		match observed_with(&baseline, nodes, &entries, &[], &mut no_ancestry()) {
			RemoteObserved::Full(reason) => assert_eq!(reason, FullPassReason::RemoteUnplaceable),
			RemoteObserved::Applied(out) => {
				panic!("a subtree was dropped instead: {:?}", out.nodes)
			}
		}
	}

	/// A delta that empties the view is not applied: an emptied remote is the shape a backend fault
	/// takes, and the guard weighs it against a whole-tree read, never against a derivation.
	#[test]
	fn a_delta_that_empties_the_view_asks_for_a_full_pass() {
		let file = Uuid::from_u128(81);
		let baseline = Baseline::from_rows([row("a.txt", NodeKind::File, Some(file))]);
		let nodes = view([file_node("a.txt", file)]);
		let events = [cache_event(
			Some(1),
			CacheEventType::File(FileEvent::Removed(file)),
		)];

		match observed_with(&baseline, nodes, &delta(&events), &[], &mut no_ancestry()) {
			RemoteObserved::Full(reason) => assert_eq!(reason, FullPassReason::RemoteEmptied),
			RemoteObserved::Applied(out) => panic!("{:?}", out.nodes),
		}
	}

	/// An empty ancestry means the cache does not know the item — UNKNOWN, never "at the root".
	/// Deriving a path from it would be a guess, and a guess on this side ends in a deletion.
	#[test]
	fn an_item_the_cache_cannot_place_asks_for_a_full_pass() {
		let orphan = Uuid::from_u128(91);
		let stranger = Uuid::from_u128(92);
		let events = [cache_event(
			Some(1),
			CacheEventType::File(FileEvent::New(cacheable_file(orphan, stranger, "f.txt"))),
		)];
		let mut ancestry = |_: Uuid| Ok(Vec::new());

		match observed_with(
			&Baseline::default(),
			Side::default(),
			&delta(&events),
			&[],
			&mut ancestry,
		) {
			RemoteObserved::Full(reason) => assert_eq!(reason, FullPassReason::RemoteUnplaceable),
			RemoteObserved::Applied(out) => panic!("{:?}", out.nodes),
		}
	}
}
