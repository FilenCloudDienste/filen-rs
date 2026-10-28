//! The two path-keyed maps a change-scoped pass reconciles from, assembled out of the resident
//! baseline and the pass's own observations.
//!
//! A whole-tree pass reads both sides and knows everything. A change-scoped pass reads the paths
//! its changelists name and DERIVES the rest from the rows the last pass wrote: a row is the record
//! of an observation, and carrying it forward is what makes "nothing was announced here" mean
//! "nothing changed here".
//!
//! # A row answers for both sides or for neither
//!
//! The rule that keeps invariant I1 — absence is only ever produced by a stat or an event on that
//! path or an ancestor — is enforced in ONE place, [`carried`]: a row stands in for a pass only
//! when it records BOTH sides, and every row that does not is put in the dirty set (re-observed on
//! disk) and in the held set (no action may be planned at it or under it) in the same breath. There
//! is no path through this module that leaves a row out of a map without holding its path, so a
//! half-recorded row cannot read downstream as a deletion. A path the rules HIDE is the one other
//! row a pass cannot answer for, and [`merge_local`] takes it out of the local map under the same
//! rule: dropped so the two halves of a hidden path agree, held so the pair of absences over a
//! live row is not read as a convergent delete.
//!
//! The rows that record one side only are the ones the engine writes that way on purpose: a
//! `KeepLocal` resolution clears the local half and a `KeepRemote` one clears the remote half
//! (`engine::resolution_entry`), an [`Adopted`](super::baseline::BaselineState::Adopted) row
//! records whichever side the destination held, and a
//! [`Conflicted`](super::baseline::BaselineState::Conflicted) /
//! [`Overwritten`](super::baseline::BaselineState::Overwritten) row's two halves describe a
//! divergence rather than an agreement — an `Overwritten` row's remote half names the version this
//! engine BURIED, not what the remote holds now. Deriving either side from any of them would be a
//! guess.
//!
//! # What derivation cannot see
//!
//! A path the baseline has no row for is invisible here: a directory that exists on disk and was
//! never synced (a destination-only item's parent, say) is in a whole-tree scan's nodes and in no
//! derived map. That defers the CREATE such a path would plan to the next full pass; it can never
//! produce a deletion, because a path in neither map is a path the reconcile never decides.
//!
//! A directory's timestamps are the other gap, and the same shape. A row written for a pushed
//! directory records no local mtime and no remote stamp at all (`apply::dir_entry`), so a derived
//! directory node carries `0` where a whole-tree read carries the filesystem's mtime and the
//! server's created stamp; and a directory that is not itself dirty keeps the mtime its row
//! recorded even when a change inside it moved the real one. Neither stamp is evidence any part of
//! a pass reads — `plan::classify_local` and `plan::classify_remote` answer `Unchanged` for a
//! directory pair whatever their stamps say — and the tests below pin that by planning against the
//! derived maps and the whole-tree ones and comparing the plans.
//!
//! A file's mtime past the millisecond is the last: a row records milliseconds, so a derived file
//! carries no [`mtime_nanos`](LocalNode::mtime_nanos). Only the seeding stage reads it, and that
//! never weighs a path with a row.

use std::{collections::BTreeSet, mem};

use super::{
	baseline::{BaselineEntry, NodeKind},
	engine::written_node,
	observe::{LocalObservation, LocalObservations},
	plan::{self, RemoteNode},
	rows::Baseline,
	scan::LocalNode,
	side::{FromRow, Nodes, Side},
};

/// What a pass reconciles, before its observations are merged in.
#[derive(Debug)]
pub(super) struct Derived {
	/// The local side: one node per row that records local evidence. [`merge_local`] then corrects
	/// it with what the pass observed on disk.
	pub(super) local: Side<LocalNode>,
	/// The remote side: one node per row that records a remote item. It is the input
	/// `remote::observe_remote` applies the announced changes to.
	pub(super) remote: Side<RemoteNode>,
	/// Every path this pass must look at: what the changelist named, plus every row that cannot
	/// stand in for itself.
	pub(super) dirty: BTreeSet<String>,
	/// Paths no action may be planned at or under, because no derived map describes them: the rows
	/// that record one side only, and the paths an observation found HIDDEN with a row still
	/// carried there ([`merge_local`]). Merged into the pass's
	/// [`held_remote`](super::plan::PassHolds::held_remote), which is what makes the missing side a
	/// deferral rather than an absence.
	pub(super) held: BTreeSet<String>,
	/// Every path the reconcile must DECIDE — what the pass hands it as
	/// [`PassPaths::Changed`](super::plan::PassPaths::Changed).
	///
	/// Not [`dirty`](Self::dirty) under another name. `dirty` says what to LOOK AT and answers for
	/// a whole subtree: one entry for a directory stands for the walk of everything under it. This
	/// says what to DECIDE and is exact, because a path missing from it is a path the reconcile
	/// does not visit — so it starts as the dirty set and grows by every key an observation
	/// actually moved off the node its row carries ([`merge_local`], `remote::RemoteObservation`,
	/// the cache-lag fold).
	pub(super) decided: BTreeSet<String>,
}

/// Carry the baseline forward into the two maps, and grow `dirty` by every row that cannot be
/// carried.
///
/// `dirty` comes in as the pass's own change-scoped set (`PassScope`'s local paths) and comes back
/// with the rows that record one side only added to it — which is what makes the "always in the
/// dirty set" rule a property of this function instead of a check every caller has to remember.
pub(super) fn from_baseline(baseline: &Baseline, dirty: BTreeSet<String>) -> Derived {
	let mut out = Derived {
		// Nothing is carried INTO anything: a path no producer touches is answered from its own
		// row when it is asked about, so the pass starts with no per-tree data at all.
		local: Side::carried(),
		remote: Side::carried(),
		// The pass decides everything it set out to look at, whether or not it found a change
		// there: a path the changelist named and the disk answered "unchanged" for costs one
		// no-op decision, and leaving it out would need the observation to be consulted first.
		decided: dirty.clone(),
		dirty,
		held: BTreeSet::new(),
	};
	// The rows that stand in for NEITHER side, from the tree's own index
	// ([`Baseline::uncarryable_paths`]) rather than from a visit of every row. Each is re-observed
	// on disk (dirty), decided, and held — the three in ONE statement, so a row cannot come to be
	// left out of a side without its path being held, which is what keeps a half-recorded row from
	// reading downstream as a deletion. The rows that CAN be carried are not touched at all: they
	// answer for themselves when a consumer asks about them.
	for path in baseline.uncarryable_paths() {
		out.dirty.insert(path.clone());
		out.held.insert(path.clone());
		out.decided.insert(path);
	}
	out
}

/// The two sides derive their halves from one row through [`carried`], so neither can come to
/// carry a row the other does not.
///
/// Both halves are built to hand one back, which is deliberate: the pair is what makes the rule
/// un-splittable. The cost is per LOOKUP rather than per row, and a change-scoped pass looks at
/// the paths it decided — where materializing the tree cost one of these per row in it.
impl FromRow for LocalNode {
	fn from_row(row: &BaselineEntry) -> Option<Self> {
		carried(row).map(|(local, _)| local)
	}
}

impl FromRow for RemoteNode {
	fn from_row(row: &BaselineEntry) -> Option<Self> {
		carried(row).map(|(_, remote)| remote)
	}
}

/// The two nodes a row stands in for, or `None` when it stands in for neither.
///
/// Both halves together, deliberately: a row that describes one side and not the other is exactly
/// the shape that fabricates an absence on the other, and returning them as a pair means no caller
/// can take the half it likes. See the module docs for which rows record one side only.
fn carried(row: &BaselineEntry) -> Option<(LocalNode, RemoteNode)> {
	// The RULE is [`BaselineEntry::carryable`], on the row, because the resident tree indexes the
	// rows that fail it and a carried side answers "does this side hold the path" off that index:
	// three readers, one field list (see there). What is left is the EXTRACTION, below.
	if !row.carryable() {
		return None;
	}
	extracted(row)
}

/// The two nodes a row's FIELDS describe, with no judgement about whether they may be carried.
///
/// Split from the gate on purpose. Inlined behind it, every row the rule refuses would extract
/// nothing BY CONSTRUCTION, so a test holding the rule and the extraction together could only
/// assert `false == false` over most of its corpus — it would pass whatever either one did. Apart,
/// the field half of the rule has a second implementation here, the `?`s, and
/// `the_carryable_rule_and_the_extraction_agree_on_every_row_shape` can fail in BOTH directions: a
/// rule grown looser than these fields, which would derive a side from a row that describes none,
/// and a rule grown stricter, which would strand its path in `dirty` and `held` on every pass.
fn extracted(row: &BaselineEntry) -> Option<(LocalNode, RemoteNode)> {
	// Names the remote item this row last recorded; `None` for a row whose remote half was cleared
	// or never written.
	let remote = written_node(Some(row))?;
	let local = match row.kind {
		// A directory has no content and no stamp a pass reads (see the module docs).
		NodeKind::Dir => LocalNode {
			rel_path: row.rel_path.clone(),
			kind: NodeKind::Dir,
			size: 0,
			mtime_millis: row.local_mtime.unwrap_or(0),
			mtime_nanos: 0,
			content_hash: None,
		},
		// A file's every field IS read — the hash classifies it, the `(size, mtime)` pair is the
		// scanner's fast path, and the remote stamp is written back into the rows an adopt writes.
		// A row missing any of them describes no side fully, so it describes neither.
		NodeKind::File => {
			row.remote_modified?;
			LocalNode {
				rel_path: row.rel_path.clone(),
				kind: NodeKind::File,
				size: row.size?,
				mtime_millis: row.local_mtime?,
				mtime_nanos: 0,
				content_hash: Some(row.content_hash?),
			}
		}
	};
	Some((local, remote))
}

/// Correct `local` with what the pass observed on disk — plan 3.4's step 3, local half.
///
/// The observations are keyed by the path they are ABOUT, which answers for everything under it
/// (`observe::LocalObservations::observed`), so they are applied nearest-ancestor-first: a
/// `BTreeMap` hands them over in path order and a path under an earlier key was never observed
/// separately.
///
/// A dirty path with no observation is NOT absent — it is a path this pass got no evidence for —
/// so its row stays carried, which is why this only ever acts on what was observed.
///
/// Every key it writes or drops goes into [`Derived::decided`] as it goes, which is what lets the
/// reconcile skip the rest: the nodes it did not touch are still the ones their rows carry.
///
/// A walk's nodes are MOVED into the local side, which leaves its scan empty. They are the largest
/// thing a scoped pass reads, and nothing after this reads them there: what still asks about a
/// walk — the facts, the assembly check — asks what it found rather than the nodes it found, and
/// the check takes its bounds before this runs (`engine::assembly_bounds`).
pub(super) fn merge_local(
	derived: &mut Derived,
	baseline: &Baseline,
	observed: &mut LocalObservations,
) {
	// Room for every node the walks found, before the first one lands: grown into from empty, the
	// table doubles its way up, and the allocator keeps every table it outgrew resident.
	derived.local.reserve(
		observed
			.observed
			.values()
			.map(|observation| match observation {
				LocalObservation::Dir(scan) => scan.nodes.of(baseline).len(),
				LocalObservation::File { .. }
				| LocalObservation::Absent(_)
				| LocalObservation::Hidden(_) => 0,
			})
			.sum(),
	);
	for (at, observation) in &mut observed.observed {
		// Asked of the observation before the arm below borrows its walk to empty it.
		let pruned: BTreeSet<String> = observation.uncovered_roots().cloned().collect();
		match observation {
			// Still a node, even where the remote would reject its name: the caller screens the
			// path out through the scan's `invalid_names`, and dropping it here would read as a
			// local deletion.
			LocalObservation::File { node, .. } => {
				derived.decided.insert(node.rel_path.clone());
				derived.local.insert(node.rel_path.clone(), node.clone());
			}
			LocalObservation::Dir(scan) => {
				// The walk of a directory is evidence for everything it reached — and only for
				// that. What it pruned (an ignored entry, a symlinked directory it reads under its
				// real path) it never looked at, and an incomplete walk may have missed anything,
				// so both keep their rows.
				if scan.complete {
					drop_rows(derived, baseline, at, &pruned);
				}
				// The decided key is the one copy: the path and the node themselves move.
				for (rel_path, node) in mem::take(&mut scan.nodes).into_entries(baseline) {
					derived.decided.insert(rel_path.clone());
					derived.local.insert(rel_path, node);
				}
			}
			// The one construct that means deletion: a `stat` on this very path answered NotFound,
			// so every row at or under it has no local node.
			LocalObservation::Absent(absence) => {
				drop_rows(derived, baseline, absence.path(), &BTreeSet::new());
			}
			// The rules hide it, and a walk of the directory above it would have pruned it, so the
			// local map must not go on carrying a node here: the view is filtered with the very same
			// rules a few lines later (`engine::prepare_scoped`), and a path that reads
			// local-present/remote-absent is a path the reconcile plans a local delete at.
			//
			// Dropped is not absent, though. Where a row was carried here the path is HELD as well:
			// two absences over a live row read as a convergent delete and retire it, and what sits
			// under a hidden root is the untrack-on-ignore path's to remove, which retries its own
			// delete every pass. What the rules normally hide has no row at all — the pass that
			// first hid it untracked what was there — and drops, holds and costs nothing here.
			LocalObservation::Hidden(_) => {
				if drop_rows(derived, baseline, at, &BTreeSet::new()) {
					derived.held.insert(at.clone());
				}
			}
		}
	}
}

/// Take the rows at or under `at` out of the local map, except those at or under a path the walk
/// pruned. Answers whether it took any, which is what tells a hidden path with rows still behind
/// it from one the rules have always hidden.
///
/// Driven by the baseline's own subtree rather than by the map, so it costs the dirty subtree and
/// not the whole tree. Nodes an EARLIER observation inserted are never under `at`: an observation
/// answers for everything below its key, and `observe_local` files them in ancestor-first order —
/// rewriting a dirty rule file onto its directory BEFORE it walks the set, so that directory is
/// never filed after an entry it holds — so no two of them nest.
fn drop_rows(
	derived: &mut Derived,
	baseline: &Baseline,
	at: &str,
	pruned: &BTreeSet<String>,
) -> bool {
	let mut dropped = false;
	if !at.is_empty()
		&& !plan::at_or_under_root(pruned, at)
		&& derived.local.remove(baseline, at).is_some()
	{
		derived.decided.insert(at.to_owned());
		dropped = true;
	}
	for row in baseline.subtree(at) {
		if !plan::at_or_under_root(pruned, &row.rel_path) && derived.local.forget(&row) {
			derived.decided.insert(row.rel_path);
			dropped = true;
		}
	}
	dropped
}

#[cfg(test)]
mod tests;
