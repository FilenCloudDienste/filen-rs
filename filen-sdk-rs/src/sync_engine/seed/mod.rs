//! The seeding stage: before a pass plans, record as synced the files both sides already agree on
//! that the remote cannot prove it agrees on.
//!
//! A file uploaded by a client that writes no BLAKE3 (filen-sync, the older SDKs) has a remote with
//! no content hash, so the direct comparison the reconcile makes between the two sides never holds
//! for it. With a baseline row the planner has a fallback for that (`plan::remote_matches_local`,
//! `plan::classify_remote_file`: the row's local hash and remote version are the evidence). Without
//! one — a pair migrated from filen-sync, whose files were never synced by this engine — every such
//! file reads as created on both sides: a conflict in two-way, a transfer in a one-way mode.
//!
//! So before the pass plans, every path with no row where both sides hold a FILE, the remote has no
//! BLAKE3 and the local one has been hashed is weighed here, and where the evidence holds it gets a
//! `Synced` row. The planner then reads the path as it reads any synced one, through those same
//! fallbacks, and nothing is planned there. The evidence, in the order it is weighed:
//!
//! - the filen-sync state the pair imported (see [`legacy`](super::legacy)) lists the path as
//!   synced, and nothing has moved since: the remote is still the version that engine last listed
//!   there, and the local file has the size it last saw and the mtime it recorded: filen-sync
//!   recorded `Math.floor(stats.mtimeMs)`, so the local mtime matches to the millisecond as this
//!   engine reads it or as filen-sync read it (see the remote stamps below). A stale version or a
//!   local edit since is no evidence here, and the path is weighed by what follows;
//! - both files are empty: the content is provably the same, whatever the stamps say;
//! - the sizes are equal and the remote's `lastModified` is the local mtime, read one of the ways a
//!   client that stamped it can have read it:
//!   - to the millisecond, as this engine reads it ([`LocalNode::mtime_millis`]);
//!   - as filen-sync read it, the stamp it gave every file it uploaded: `Math.floor(stats.mtimeMs)`,
//!     where Node computes `mtimeMs` as `sec * 1000 + nsec / 1e6` in doubles. That is the
//!     millisecond below where this engine rounds up to the next one (on Unix); and where the
//!     nanoseconds lie a fraction of a microsecond short of a millisecond, as the mtimes filen-sync
//!     set from float seconds on the files it downloaded can, the double rounds up to it before
//!     the floor. So the stamp is computed as Node computes it, in `f64`, and only then floored;
//!   - in whole seconds, where the local mtime has no sub-second part: a file system that keeps
//!     whole seconds only (older HFS+, some network shares), or a writer that set it so, drops the
//!     remote stamp's milliseconds, so a remote stamp anywhere in that second matches. FAT keeps
//!     two-second steps, so there only a remote stamp in the second it kept does. A local mtime
//!     with any sub-second part is never compared at seconds.
//!
//!   A remote stamp of 0 is a missing one and never matches. There is no tolerance beyond these: a
//!   miss goes the safe way, a conflict or a transfer, where a tolerance would adopt a different
//!   file.
//!
//! A file whose remote item another row already records — by its version or its whole-life id —
//! is never seeded: that is the same file renamed on both sides, which the move detection carries
//! as a move, and a second `Synced` row for one remote item is the one shape it and the
//! directory-move fold must never read (see `plan::visit_move_sources`).
//!
//! Only a real pass writes what it seeds into the store. A dry run weighs the same evidence through
//! the same [`seed`], over the same paths, and reads each row it would write into its own view of
//! the rows instead ([`Seeding::Preview`]), so it plans what the pass after it would. The adoption
//! read of a mode switch and the benchmark weigh nothing ([`Seeding::Skip`]).

use std::{borrow::Cow, cell::RefCell, collections::HashMap};

use filen_types::fs::StableUuid;
use num_traits::ToPrimitive;
use uuid::Uuid;

use super::{
	baseline::{
		BaselineEntry, BaselineState, BaselineStore, LegacySynced, NodeKind, PairId, SyncedPaths,
	},
	plan::RemoteNode,
	rows::Baseline,
	scan::LocalNode,
	side::NodesAt,
};

/// How many seeded rows go into the store a transaction. A migrated pair can have a hashless file
/// per item of its tree, and one transaction for the lot would hold the baseline file's write lock
/// while the stage is read — every other pair's write waiting on it. Each row stands on its own
/// (it records an agreement the pass observed), so a cut between two chunks loses nothing.
const SEED_CHUNK: usize = 1024;

/// Whether a pass's read may write the rows this stage seeds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Seeding {
	/// A real pass: seed, then plan from the rows as the store holds them.
	Write,
	/// A dry run: weigh what a real pass would, and plan from the rows it would seed, written into
	/// the read's own view of the rows and never into the store.
	Preview,
	/// A read that weighs nothing: the adoption read of a mode switch, the benchmark.
	Skip,
}

/// Which evidence recorded a path as synced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Evidence {
	/// The imported filen-sync state lists the path as synced at the remote's version and the
	/// local file's size and mtime.
	Imported,
	/// Both files are empty.
	Empty,
	/// The same size, and the remote's `lastModified` is the local mtime, read to the millisecond,
	/// as filen-sync read it, or in whole seconds where it has no sub-second part.
	SizeAndMtime,
}

/// How many paths a seeding recorded, by the evidence that recorded each.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct SeedCounts {
	pub(super) imported: usize,
	pub(super) empty: usize,
	pub(super) size_and_mtime: usize,
}

impl SeedCounts {
	fn count(&mut self, evidence: Evidence) {
		match evidence {
			Evidence::Imported => self.imported += 1,
			Evidence::Empty => self.empty += 1,
			Evidence::SizeAndMtime => self.size_and_mtime += 1,
		}
	}

	pub(super) fn total(&self) -> usize {
		self.imported + self.empty + self.size_and_mtime
	}
}

/// Whether the two nodes at a rowless path are what this stage weighs: a file on both sides, the
/// remote with no BLAKE3 and the local one hashed. A remote with a hash is never one — the direct
/// comparison is the only test there.
pub(super) fn candidate(local: &LocalNode, remote: &RemoteNode) -> bool {
	local.kind == NodeKind::File
		&& remote.kind == NodeKind::File
		&& remote.content_hash.is_none()
		&& local.content_hash.is_some()
}

/// The evidence that the two files of a [`candidate`] hold the same content, if there is any (see
/// the module docs), where `imported` is what the pair's imported filen-sync state recorded at the
/// path. Equality on every stamp, each read only the ways the module docs list: no tolerance.
pub(super) fn evidence(
	local: &LocalNode,
	remote: &RemoteNode,
	imported: Option<&LegacySynced>,
) -> Option<Evidence> {
	if imported.is_some_and(|synced| {
		synced.remote_uuid == remote.remote_uuid
			&& synced.size == local.size
			&& same_millisecond(local, synced.local_mtime)
	}) {
		return Some(Evidence::Imported);
	}
	if local.size != remote.size {
		return None;
	}
	if local.size == 0 {
		return Some(Evidence::Empty);
	}
	(remote.modified_millis != 0 && same_mtime(local, remote.modified_millis))
		.then_some(Evidence::SizeAndMtime)
}

/// Whether a remote `lastModified` of `remote_millis` is `local`'s mtime, read to the millisecond,
/// as filen-sync read it, or in whole seconds where it has no sub-second part (see the module docs).
fn same_mtime(local: &LocalNode, remote_millis: i64) -> bool {
	if same_millisecond(local, remote_millis) {
		return true;
	}
	mtime_parts(local)
		.is_some_and(|(secs, nanos)| nanos == 0 && remote_millis.div_euclid(1000) == secs)
}

/// Whether a stamp of `millis` is `local`'s mtime to the millisecond, as this engine reads it or as
/// filen-sync read it ([`node_mtime_millis`]). filen-sync recorded that stamp both in the state the
/// pair imported and as the `lastModified` of every file it uploaded.
fn same_millisecond(local: &LocalNode, millis: i64) -> bool {
	millis == local.mtime_millis
		|| mtime_parts(local).and_then(|(secs, nanos)| node_mtime_millis(secs, nanos))
			== Some(millis)
}

/// `local`'s mtime as the file system holds it: whole seconds since the epoch, and the nanoseconds
/// past them. `None` only past the range of an `i64` of seconds.
fn mtime_parts(local: &LocalNode) -> Option<(i64, u32)> {
	let nanos = local.mtime_millis.rem_euclid(1000) * 1_000_000 + i64::from(local.mtime_nanos);
	let secs = local
		.mtime_millis
		.div_euclid(1000)
		.checked_add(nanos.div_euclid(1_000_000_000))?;
	Some((secs, u32::try_from(nanos.rem_euclid(1_000_000_000)).ok()?))
}

/// `Math.floor(stats.mtimeMs)` for an mtime of `secs` and `nanos`, the stamp filen-sync gave a file
/// it uploaded: Node computes `mtimeMs` as `sec * 1000 + nsec / 1e6` in doubles, which can round up
/// to the next millisecond before the floor, so it is computed the same way here. `None` past the
/// range of an `i64`.
fn node_mtime_millis(secs: i64, nanos: u32) -> Option<i64> {
	(secs.to_f64()? * 1000.0 + f64::from(nanos) / 1_000_000.0)
		.floor()
		.to_i64()
}

/// The row that records `rel_path` as synced, as `apply::adopt_outcome` records a converged file,
/// except that the content is the local side's: the remote has no hash to give. Both markers are
/// the local hash, so the row awaits no confirmation and a later pass carries it like any other.
///
/// The local mtime is the scan's, the read the hash belongs to — for the reason `adopt_outcome`
/// gives: a fresh stat paired with the scan's hash could hide an edit made since the scan forever.
pub(super) fn seeded_row(rel_path: &str, local: &LocalNode, remote: &RemoteNode) -> BaselineEntry {
	BaselineEntry {
		rel_path: rel_path.to_owned(),
		kind: NodeKind::File,
		remote_uuid: Some(remote.remote_uuid),
		content_hash: local.content_hash,
		size: Some(local.size),
		local_mtime: Some(local.mtime_millis),
		remote_modified: Some(remote.modified_millis),
		state: BaselineState::Synced,
		local_kind: None,
		remote_kind: None,
		remote_hash: None,
		remote_size: None,
		remote_stable_uuid: remote.stable_uuid,
		agreed_hash: local.content_hash,
	}
}

/// What the seeding stage reads and writes: the pair's imported filen-sync state, where the rows
/// record the remote items it would seed, and the rows it seeds. [`BaselineStore`] is the one
/// store; the tests count what is asked of it.
pub(super) trait SeedStore {
	/// See [`BaselineStore::has_legacy_synced`].
	fn has_legacy_synced(&self, pair: PairId) -> rusqlite::Result<bool>;
	/// See [`BaselineStore::legacy_synced`].
	fn legacy_synced(&self, pair: PairId, rel_path: &str)
	-> rusqlite::Result<Option<LegacySynced>>;
	/// See [`BaselineStore::synced_paths`].
	fn synced_paths(
		&self,
		pair: PairId,
		uuids: &[Uuid],
		lineages: &[StableUuid],
	) -> rusqlite::Result<SyncedPaths>;
	/// See [`BaselineStore::seed_rows`]; the rows are handed over, so a store that keeps them
	/// copies none.
	fn seed_rows(&self, pair: PairId, rows: Vec<BaselineEntry>) -> rusqlite::Result<()>;
}

impl SeedStore for BaselineStore {
	fn has_legacy_synced(&self, pair: PairId) -> rusqlite::Result<bool> {
		BaselineStore::has_legacy_synced(self, pair)
	}

	fn legacy_synced(
		&self,
		pair: PairId,
		rel_path: &str,
	) -> rusqlite::Result<Option<LegacySynced>> {
		BaselineStore::legacy_synced(self, pair, rel_path)
	}

	fn synced_paths(
		&self,
		pair: PairId,
		uuids: &[Uuid],
		lineages: &[StableUuid],
	) -> rusqlite::Result<SyncedPaths> {
		BaselineStore::synced_paths(self, pair, uuids, lineages)
	}

	fn seed_rows(&self, pair: PairId, rows: Vec<BaselineEntry>) -> rusqlite::Result<()> {
		BaselineStore::seed_rows(self, pair, &rows)
	}
}

/// A [`SeedStore`] that writes nothing: it reads what the stage asks of `store` and keeps the rows
/// the stage would write there, for a dry run to read into its own view (see [`Seeding::Preview`]).
///
/// Each chunk's rows are asked about against the store as the pass found it and against the rows
/// kept from the chunks before it, as a real pass reads them against the store with those chunks
/// written in. The remote view places each remote uuid at one path, but not each lineage: a remote
/// file a later chunk holds under a lineage an earlier chunk's row records is dropped by the pass,
/// and so here.
pub(super) struct Preview<'a> {
	store: &'a BaselineStore,
	kept: RefCell<Kept>,
}

/// The rows a [`Preview`] kept, and which of them records each remote uuid and lineage.
#[derive(Default)]
struct Kept {
	rows: Vec<BaselineEntry>,
	by_uuid: HashMap<Uuid, usize>,
	by_lineage: HashMap<StableUuid, usize>,
}

impl<'a> Preview<'a> {
	pub(super) fn new(store: &'a BaselineStore) -> Self {
		Self {
			store,
			kept: RefCell::new(Kept::default()),
		}
	}

	/// The rows the stage would have written, in the order it would have written them.
	pub(super) fn into_rows(self) -> Vec<BaselineEntry> {
		self.kept.into_inner().rows
	}
}

impl SeedStore for Preview<'_> {
	fn has_legacy_synced(&self, pair: PairId) -> rusqlite::Result<bool> {
		self.store.has_legacy_synced(pair)
	}

	fn legacy_synced(
		&self,
		pair: PairId,
		rel_path: &str,
	) -> rusqlite::Result<Option<LegacySynced>> {
		self.store.legacy_synced(pair, rel_path)
	}

	fn synced_paths(
		&self,
		pair: PairId,
		uuids: &[Uuid],
		lineages: &[StableUuid],
	) -> rusqlite::Result<SyncedPaths> {
		let mut out = self.store.synced_paths(pair, uuids, lineages)?;
		let kept = self.kept.borrow();
		let by_uuid = uuids
			.iter()
			.filter_map(|uuid| kept.by_uuid.get(uuid).map(|&at| (Some(*uuid), None, at)));
		let by_lineage = lineages.iter().filter_map(|lineage| {
			kept.by_lineage
				.get(lineage)
				.map(|&at| (None, Some(*lineage), at))
		});
		for (uuid, lineage, at) in by_uuid.chain(by_lineage) {
			if let Some(row) = kept.rows.get(at) {
				out.offer(&row.rel_path, uuid, lineage);
			}
		}
		Ok(out)
	}

	fn seed_rows(&self, _pair: PairId, rows: Vec<BaselineEntry>) -> rusqlite::Result<()> {
		let mut kept = self.kept.borrow_mut();
		for row in rows {
			let at = kept.rows.len();
			if let Some(uuid) = row.remote_uuid {
				kept.by_uuid.entry(uuid).or_insert(at);
			}
			if let Some(lineage) = row.remote_stable_uuid {
				kept.by_lineage.entry(lineage).or_insert(at);
			}
			kept.rows.push(row);
		}
		Ok(())
	}
}

/// Weigh each of `candidates` — remote nodes with the paths they are at — and write a row for every
/// one the evidence records as synced, [`SEED_CHUNK`] rows a transaction.
///
/// The imported filen-sync state is looked up at a path only once it is a [`candidate`], and only
/// when the pair imported any: a pair that did not pays one read per call.
///
/// A path is left alone when `skip` says the pass does not act on it, when `baseline` has a row
/// there, when `local` holds nothing there, or when the two nodes are no [`candidate`]. A row the
/// evidence holds for is dropped when the store already records its remote uuid or lineage at
/// another path (see the module docs); that is asked once a chunk, of the rows about to be
/// written, so a pass seeding nothing asks nothing. The rows are written through the store's own
/// statement, so nothing a pass's snapshot holds is overwritten: every path written is one that
/// snapshot has no row at, and the pass holds the pair's gate.
pub(super) fn seed<'a>(
	store: &impl SeedStore,
	pair: PairId,
	baseline: &Baseline,
	local: &impl NodesAt<Node = LocalNode>,
	candidates: impl Iterator<Item = (Cow<'a, str>, Cow<'a, RemoteNode>)>,
	skip: impl Fn(&str) -> bool,
) -> rusqlite::Result<SeedCounts> {
	let mut counts = SeedCounts::default();
	let mut rows: Vec<(BaselineEntry, Evidence)> = Vec::new();
	let any_imported = store.has_legacy_synced(pair)?;
	for (path, remote) in candidates {
		if skip(&path) || baseline.contains_key(&path) {
			continue;
		}
		let Some(local) = local.at(&path) else {
			continue;
		};
		if !candidate(&local, &remote) {
			continue;
		}
		let imported = if any_imported {
			store.legacy_synced(pair, &path)?
		} else {
			None
		};
		let Some(evidence) = evidence(&local, &remote, imported.as_ref()) else {
			continue;
		};
		rows.push((seeded_row(&path, &local, &remote), evidence));
		if rows.len() == SEED_CHUNK {
			write_unclaimed(store, pair, &mut rows, &mut counts)?;
		}
	}
	if !rows.is_empty() {
		write_unclaimed(store, pair, &mut rows, &mut counts)?;
	}
	Ok(counts)
}

/// Write the `rows` no row in the store already records the remote item of, by uuid or by lineage,
/// counting each by its evidence, and empty `rows`.
fn write_unclaimed(
	store: &impl SeedStore,
	pair: PairId,
	rows: &mut Vec<(BaselineEntry, Evidence)>,
	counts: &mut SeedCounts,
) -> rusqlite::Result<()> {
	let uuids: Vec<Uuid> = rows.iter().filter_map(|(row, _)| row.remote_uuid).collect();
	let lineages: Vec<StableUuid> = rows
		.iter()
		.filter_map(|(row, _)| row.remote_stable_uuid)
		.collect();
	let claimed = store.synced_paths(pair, &uuids, &lineages)?;
	let unclaimed: Vec<BaselineEntry> = rows
		.drain(..)
		.filter_map(|(row, evidence)| {
			let recorded_at = row
				.remote_uuid
				.and_then(|uuid| claimed.path_by_uuid(uuid))
				.or_else(|| {
					row.remote_stable_uuid
						.and_then(|lineage| claimed.path_by_lineage(lineage))
				});
			if let Some(recorded_at) = recorded_at {
				tracing::debug!(
					"seed[pair {pair}]: not recording {:?} as synced: {recorded_at:?} records its \
					 remote item",
					row.rel_path
				);
				return None;
			}
			counts.count(evidence);
			Some(row)
		})
		.collect();
	if unclaimed.is_empty() {
		return Ok(());
	}
	store.seed_rows(pair, unclaimed)
}

#[cfg(test)]
mod tests;
