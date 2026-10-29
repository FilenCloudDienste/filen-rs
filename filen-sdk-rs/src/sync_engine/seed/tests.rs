use std::{cell::Cell, collections::HashMap};

use filen_types::{crypto::Blake3Hash, fs::StableUuid};
use uuid::Uuid;

use super::*;
use crate::sync_engine::{
	SyncMode,
	baseline::{LegacyItem, LegacySide},
};

const STAMP: i64 = 1_700_000_000_123;

fn hash(byte: u8) -> Blake3Hash {
	Blake3Hash::from([byte; 32])
}

fn local(rel: &str, size: u64, mtime_millis: i64) -> LocalNode {
	LocalNode {
		rel_path: rel.to_owned(),
		kind: NodeKind::File,
		size,
		mtime_millis,
		mtime_nanos: 0,
		content_hash: Some(hash(7)),
	}
}

/// A file another client uploaded with no BLAKE3.
fn hashless(rel: &str, uuid: u128, size: u64, modified_millis: i64) -> RemoteNode {
	RemoteNode {
		rel_path: rel.to_owned(),
		kind: NodeKind::File,
		remote_uuid: Uuid::from_u128(uuid),
		stable_uuid: Some(StableUuid::new_for_test(Uuid::from_u128(uuid + 1000))),
		content_hash: None,
		size,
		modified_millis,
	}
}

#[test]
fn the_same_size_and_the_exact_millisecond_is_evidence() {
	assert_eq!(
		evidence(&local("a", 5, STAMP), &hashless("a", 1, 5, STAMP), None),
		Some(Evidence::SizeAndMtime)
	);
}

#[test]
fn a_millisecond_either_way_is_no_evidence() {
	for remote in [STAMP + 1, STAMP - 1] {
		assert_eq!(
			evidence(&local("a", 5, STAMP), &hashless("a", 1, 5, remote), None),
			None,
			"remote stamp {remote}"
		);
	}
}

#[test]
fn a_size_off_by_one_is_no_evidence() {
	assert_eq!(
		evidence(&local("a", 5, STAMP), &hashless("a", 1, 6, STAMP), None),
		None
	);
	assert_eq!(
		evidence(&local("a", 6, STAMP), &hashless("a", 1, 5, STAMP), None),
		None
	);
}

#[test]
fn a_missing_remote_stamp_never_matches_a_file_with_content() {
	assert_eq!(
		evidence(&local("a", 5, 0), &hashless("a", 1, 5, 0), None),
		None
	);
}

/// A second on the remote stamps below, in epoch millis.
const SECOND: i64 = 1_743_472_771_000;

/// A hashed local file of 5 bytes read at `mtime_millis`, the file system holding it `mtime_nanos`
/// past that.
fn read_at(mtime_millis: i64, mtime_nanos: i32) -> LocalNode {
	LocalNode {
		mtime_nanos,
		..local("a", 5, mtime_millis)
	}
}

/// The evidence for `local` against a hashless remote of 5 bytes stamped `remote_millis`.
fn weighed(local: &LocalNode, remote_millis: i64) -> Option<Evidence> {
	evidence(local, &hashless("a", 1, 5, remote_millis), None)
}

/// 440.9999 ms into the second, read at 440 as Windows reads it: Node's `mtimeMs` is the double
/// `...440.9999`, which rounds up to `...441` before filen-sync floored it.
#[test]
fn filen_sync_s_stamp_rounded_up_to_the_next_millisecond_is_evidence() {
	let file = read_at(SECOND + 440, 999_900);
	assert_eq!(weighed(&file, SECOND + 441), Some(Evidence::SizeAndMtime));
	assert_eq!(weighed(&file, SECOND + 440), Some(Evidence::SizeAndMtime));
	assert_eq!(weighed(&file, SECOND + 442), None);
	assert_eq!(weighed(&file, SECOND + 439), None);
}

/// The same with the nanoseconds a float-seconds `utimes` leaves, `1743472771440999961`, read at
/// the millisecond below.
#[test]
fn filen_sync_s_stamp_of_a_float_seconds_mtime_is_evidence() {
	let file = read_at(SECOND + 440, 999_961);
	assert_eq!(weighed(&file, SECOND + 441), Some(Evidence::SizeAndMtime));
	assert_eq!(weighed(&file, SECOND + 442), None);
}

/// 440.6 ms into the second, read at 441 as Unix reads it: filen-sync floored Node's
/// `...440.6` to `...440`.
#[test]
fn filen_sync_s_floor_below_a_millisecond_this_engine_rounds_up_is_evidence() {
	let file = read_at(SECOND + 441, -400_000);
	assert_eq!(weighed(&file, SECOND + 440), Some(Evidence::SizeAndMtime));
	assert_eq!(weighed(&file, SECOND + 441), Some(Evidence::SizeAndMtime));
	assert_eq!(weighed(&file, SECOND + 439), None);
	assert_eq!(weighed(&file, SECOND + 442), None);
}

#[test]
fn a_whole_second_local_mtime_matches_a_remote_stamp_anywhere_in_that_second() {
	let file = read_at(SECOND, 0);
	for remote in [SECOND, SECOND + 1, SECOND + 441, SECOND + 999] {
		assert_eq!(
			weighed(&file, remote),
			Some(Evidence::SizeAndMtime),
			"remote stamp {remote}"
		);
	}
	for remote in [SECOND - 1, SECOND + 1000] {
		assert_eq!(weighed(&file, remote), None, "remote stamp {remote}");
	}
}

/// The second is the one the mtime is in, before the epoch too.
#[test]
fn a_whole_second_before_the_epoch_matches_the_stamps_of_that_second() {
	let file = read_at(-1000, 0);
	assert_eq!(weighed(&file, -1), Some(Evidence::SizeAndMtime));
	assert_eq!(weighed(&file, -999), Some(Evidence::SizeAndMtime));
	assert_eq!(weighed(&file, -1001), None);
}

#[test]
fn a_local_mtime_with_a_sub_second_part_is_never_compared_at_seconds() {
	for (file, what) in [
		(read_at(SECOND + 440, 0), "440 ms in"),
		(read_at(SECOND, 1), "a nanosecond past the second"),
		// 999.6 ms in, which Unix reads as the next whole second.
		(
			read_at(SECOND + 1000, -400_000),
			"rounded up to the next second",
		),
	] {
		for remote in [SECOND + 1, SECOND + 441, SECOND + 1441] {
			assert_eq!(
				weighed(&file, remote),
				None,
				"{what}, remote stamp {remote}"
			);
		}
	}
}

#[test]
fn a_size_off_by_one_is_no_evidence_however_the_stamps_are_read() {
	for (file, remote) in [
		(read_at(SECOND + 440, 999_900), SECOND + 441),
		(read_at(SECOND + 441, -400_000), SECOND + 440),
		(read_at(SECOND, 0), SECOND + 441),
	] {
		assert_eq!(
			evidence(&file, &hashless("a", 1, 6, remote), None),
			None,
			"remote stamp {remote}"
		);
	}
}

#[test]
fn a_missing_remote_stamp_never_matches_however_the_local_mtime_is_read() {
	for file in [read_at(0, 0), read_at(0, 400_000), read_at(1, -600_000)] {
		assert_eq!(weighed(&file, 0), None, "{file:?}");
	}
}

#[test]
fn two_empty_files_agree_whatever_their_stamps() {
	assert_eq!(
		evidence(
			&local("a", 0, STAMP),
			&hashless("a", 1, 0, STAMP + 5000),
			None
		),
		Some(Evidence::Empty)
	);
	assert_eq!(
		evidence(&local("a", 0, STAMP), &hashless("a", 1, 0, 0), None),
		Some(Evidence::Empty)
	);
}

#[test]
fn only_a_hashed_local_file_and_a_hashless_remote_file_are_candidates() {
	let file = local("a", 5, STAMP);
	let remote = hashless("a", 1, 5, STAMP);
	assert!(candidate(&file, &remote));
	assert!(
		!candidate(
			&LocalNode {
				kind: NodeKind::Dir,
				..file.clone()
			},
			&remote
		),
		"a local directory"
	);
	assert!(
		!candidate(
			&file,
			&RemoteNode {
				kind: NodeKind::Dir,
				..remote.clone()
			}
		),
		"a remote directory"
	);
	assert!(
		!candidate(
			&file,
			&RemoteNode {
				content_hash: Some(hash(7)),
				..remote.clone()
			}
		),
		"a remote with a hash"
	);
	assert!(
		!candidate(
			&LocalNode {
				content_hash: None,
				..file
			},
			&remote
		),
		"a local file with no hash"
	);
}

#[test]
fn a_seeded_row_records_the_local_content_as_agreed_and_is_carried() {
	let file = local("docs/a.txt", 5, STAMP);
	let remote = hashless("docs/a.txt", 1, 5, STAMP);
	let row = seeded_row("docs/a.txt", &file, &remote);
	assert_eq!(row.rel_path, "docs/a.txt");
	assert_eq!(row.kind, NodeKind::File);
	assert_eq!(row.state, BaselineState::Synced);
	assert_eq!(row.content_hash, Some(hash(7)));
	assert_eq!(row.agreed_hash, Some(hash(7)));
	assert_eq!(row.remote_uuid, Some(Uuid::from_u128(1)));
	assert_eq!(
		row.remote_stable_uuid,
		Some(StableUuid::new_for_test(Uuid::from_u128(1001)))
	);
	assert_eq!(row.size, Some(5));
	assert_eq!(row.local_mtime, Some(STAMP));
	assert_eq!(row.remote_modified, Some(STAMP));
	assert!(row.carryable());
	assert!(!row.awaits_confirmation());
}

/// A store holding one pair, and its id.
fn store() -> (BaselineStore, PairId) {
	let store = BaselineStore::open_in_memory().unwrap();
	let (pair, _) = store
		.create_pair("/local", Uuid::from_u128(99), SyncMode::TwoWay)
		.unwrap();
	(store, pair)
}

fn seed_all(
	store: &BaselineStore,
	pair: PairId,
	local: &[LocalNode],
	remote: &[RemoteNode],
	skip: impl Fn(&str) -> bool,
) -> SeedCounts {
	let baseline = store.baseline(pair).unwrap();
	let local: HashMap<String, LocalNode> = local
		.iter()
		.map(|node| (node.rel_path.clone(), node.clone()))
		.collect();
	seed(
		store,
		pair,
		&baseline,
		&local,
		remote
			.iter()
			.map(|node| (Cow::from(node.rel_path.as_str()), Cow::Borrowed(node))),
		skip,
	)
	.unwrap()
}

#[test]
fn seeding_writes_a_row_for_each_agreeing_rowless_file_and_counts_it_by_evidence() {
	let (store, pair) = store();
	let counts = seed_all(
		&store,
		pair,
		&[
			local("same.bin", 5, STAMP),
			local("empty.bin", 0, STAMP),
			local("off.bin", 5, STAMP),
		],
		&[
			hashless("same.bin", 1, 5, STAMP),
			hashless("empty.bin", 2, 0, 0),
			hashless("off.bin", 3, 5, STAMP + 1),
			// Nothing on the local side to agree with.
			hashless("remote-only.bin", 4, 5, STAMP),
		],
		|_| false,
	);
	assert_eq!(
		counts,
		SeedCounts {
			imported: 0,
			empty: 1,
			size_and_mtime: 1,
		}
	);
	assert_eq!(counts.total(), 2);
	let paths: Vec<String> = store
		.entries(pair)
		.unwrap()
		.into_iter()
		.map(|row| row.rel_path)
		.collect();
	assert_eq!(paths, ["empty.bin", "same.bin"]);
	assert_eq!(
		store.entry(pair, "same.bin").unwrap(),
		Some(seeded_row(
			"same.bin",
			&local("same.bin", 5, STAMP),
			&hashless("same.bin", 1, 5, STAMP)
		))
	);
}

#[test]
fn a_path_that_has_a_row_is_left_exactly_as_it_was() {
	let (store, pair) = store();
	let existing = BaselineEntry {
		state: BaselineState::Conflicted,
		..seeded_row("a.bin", &local("a.bin", 9, 1), &hashless("a.bin", 50, 9, 1))
	};
	store.upsert_entry(pair, &existing).unwrap();
	let counts = seed_all(
		&store,
		pair,
		&[local("a.bin", 5, STAMP)],
		&[hashless("a.bin", 1, 5, STAMP)],
		|_| false,
	);
	assert_eq!(counts, SeedCounts::default());
	assert_eq!(store.entries(pair).unwrap(), [existing]);
}

#[test]
fn a_path_the_pass_does_not_act_on_is_skipped() {
	let (store, pair) = store();
	let counts = seed_all(
		&store,
		pair,
		&[
			local("held/a.bin", 5, STAMP),
			local("blocked/b.bin", 5, STAMP),
			local("c.bin", 5, STAMP),
		],
		&[
			hashless("held/a.bin", 1, 5, STAMP),
			hashless("blocked/b.bin", 2, 5, STAMP),
			hashless("c.bin", 3, 5, STAMP),
		],
		|path| path.starts_with("held/") || path.starts_with("blocked/"),
	);
	assert_eq!(counts.total(), 1);
	let paths: Vec<String> = store
		.entries(pair)
		.unwrap()
		.into_iter()
		.map(|row| row.rel_path)
		.collect();
	assert_eq!(paths, ["c.bin"]);
}

/// A file renamed on both sides: the row at its old path still records its remote item, so the new
/// path is left to the move detection rather than seeded as a second row for that item.
#[test]
fn a_file_whose_remote_version_a_row_records_elsewhere_is_not_seeded() {
	let (store, pair) = store();
	let old = seeded_row(
		"old.bin",
		&local("old.bin", 5, STAMP),
		&hashless("old.bin", 1, 5, STAMP),
	);
	store.upsert_entry(pair, &old).unwrap();
	let counts = seed_all(
		&store,
		pair,
		&[local("new.bin", 5, STAMP), local("other.bin", 5, STAMP)],
		&[
			hashless("new.bin", 1, 5, STAMP),
			hashless("other.bin", 2, 5, STAMP),
		],
		|_| false,
	);
	assert_eq!(counts.total(), 1);
	let paths: Vec<String> = store
		.entries(pair)
		.unwrap()
		.into_iter()
		.map(|row| row.rel_path)
		.collect();
	assert_eq!(paths, ["old.bin", "other.bin"]);
}

/// A remote file re-uploaded since a row recorded it keeps its whole-life id: a row recording that
/// lineage at another path claims it as well.
#[test]
fn a_file_whose_lineage_a_row_records_elsewhere_is_not_seeded() {
	let (store, pair) = store();
	let old = BaselineEntry {
		remote_uuid: Some(Uuid::from_u128(50)),
		..seeded_row(
			"old.bin",
			&local("old.bin", 5, STAMP),
			&hashless("old.bin", 1, 5, STAMP),
		)
	};
	store.upsert_entry(pair, &old).unwrap();
	let counts = seed_all(
		&store,
		pair,
		&[local("new.bin", 5, STAMP)],
		&[hashless("new.bin", 1, 5, STAMP)],
		|_| false,
	);
	assert_eq!(counts, SeedCounts::default());
	assert_eq!(store.entries(pair).unwrap(), [old]);
}

#[test]
fn more_rows_than_a_chunk_are_all_written() {
	let (store, pair) = store();
	let count = SEED_CHUNK * 2 + 3;
	let paths: Vec<String> = (0..count).map(|n| format!("f{n:05}.bin")).collect();
	let locals: Vec<LocalNode> = paths.iter().map(|path| local(path, 5, STAMP)).collect();
	let remotes: Vec<RemoteNode> = paths
		.iter()
		.zip(1u128..)
		.map(|(path, uuid)| hashless(path, uuid, 5, STAMP))
		.collect();
	let counts = seed_all(&store, pair, &locals, &remotes, |_| false);
	assert_eq!(counts.size_and_mtime, count);
	assert_eq!(store.entries(pair).unwrap().len(), count);
}

/// A preview weighs what a seeding weighs and hands back the rows it would write, in the order it
/// would write them, writing none.
#[test]
fn a_preview_hands_back_the_rows_a_seeding_would_write_and_writes_none() {
	let (store, pair) = store();
	let old = seeded_row(
		"old.bin",
		&local("old.bin", 5, STAMP),
		&hashless("old.bin", 1, 5, STAMP),
	);
	store.upsert_entry(pair, &old).unwrap();
	let baseline = store.baseline(pair).unwrap();
	let locals: HashMap<String, LocalNode> = ["same.bin", "empty.bin", "renamed.bin", "off.bin"]
		.into_iter()
		.map(|rel| {
			let size = if rel == "empty.bin" { 0 } else { 5 };
			(rel.to_owned(), local(rel, size, STAMP))
		})
		.collect();
	let remotes = [
		hashless("same.bin", 2, 5, STAMP),
		hashless("empty.bin", 3, 0, 0),
		// Recorded at `old.bin` already.
		hashless("renamed.bin", 1, 5, STAMP),
		hashless("off.bin", 4, 5, STAMP + 1),
	];
	let preview = Preview::new(&store);
	let counts = seed(
		&preview,
		pair,
		&baseline,
		&locals,
		remotes
			.iter()
			.map(|node| (Cow::from(node.rel_path.as_str()), Cow::Borrowed(node))),
		|_| false,
	)
	.unwrap();
	assert_eq!(
		counts,
		SeedCounts {
			imported: 0,
			empty: 1,
			size_and_mtime: 1,
		}
	);
	assert_eq!(
		preview.into_rows(),
		[
			seeded_row("same.bin", &locals["same.bin"], &remotes[0]),
			seeded_row("empty.bin", &locals["empty.bin"], &remotes[1]),
		]
	);
	assert_eq!(store.entries(pair).unwrap(), [old]);
}

/// What an imported filen-sync state recorded at a path: remote version `uuid`, local `size` and
/// `local_mtime`.
fn synced(uuid: u128, size: u64, local_mtime: i64) -> LegacySynced {
	LegacySynced {
		remote_uuid: Uuid::from_u128(uuid),
		size,
		local_mtime,
	}
}

/// The local file was touched after the remote was stamped, so the stamps alone say nothing.
const LATER: i64 = STAMP + 5000;

#[test]
fn imported_state_that_still_holds_is_evidence_where_the_stamps_differ() {
	let (file, remote) = (local("a", 5, LATER), hashless("a", 1, 5, STAMP));
	assert_eq!(evidence(&file, &remote, None), None);
	assert_eq!(
		evidence(&file, &remote, Some(&synced(1, 5, LATER))),
		Some(Evidence::Imported)
	);
}

#[test]
fn imported_state_for_another_remote_version_falls_through_to_the_stamps() {
	assert_eq!(
		evidence(
			&local("a", 5, STAMP),
			&hashless("a", 1, 5, STAMP),
			Some(&synced(2, 5, STAMP))
		),
		Some(Evidence::SizeAndMtime)
	);
	assert_eq!(
		evidence(
			&local("a", 5, LATER),
			&hashless("a", 1, 5, STAMP),
			Some(&synced(2, 5, LATER))
		),
		None
	);
}

#[test]
fn imported_state_a_byte_or_a_millisecond_off_the_local_file_is_no_evidence() {
	let (file, remote) = (local("a", 5, LATER), hashless("a", 1, 5, STAMP));
	for (size, mtime) in [(4, LATER), (6, LATER), (5, LATER - 1), (5, LATER + 1)] {
		assert_eq!(
			evidence(&file, &remote, Some(&synced(1, size, mtime))),
			None,
			"imported size {size}, mtime {mtime}"
		);
	}
}

/// 440.6 ms into the second, read at 441 as Unix reads it: filen-sync recorded the local mtime as
/// `Math.floor(stats.mtimeMs)`, `...440`.
#[test]
fn imported_state_holding_filen_sync_s_floor_below_a_millisecond_this_engine_rounds_up_is_evidence()
{
	let (file, remote) = (read_at(SECOND + 441, -400_000), hashless("a", 1, 5, STAMP));
	for mtime in [SECOND + 440, SECOND + 441] {
		assert_eq!(
			evidence(&file, &remote, Some(&synced(1, 5, mtime))),
			Some(Evidence::Imported),
			"imported mtime {mtime}"
		);
	}
	for mtime in [SECOND + 439, SECOND + 442] {
		assert_eq!(
			evidence(&file, &remote, Some(&synced(1, 5, mtime))),
			None,
			"imported mtime {mtime}"
		);
	}
}

/// 440.9999 ms into the second, read at 440: Node's `mtimeMs` rounds up to `...441` before the floor.
#[test]
fn imported_state_holding_filen_sync_s_stamp_rounded_up_to_the_next_millisecond_is_evidence() {
	let (file, remote) = (read_at(SECOND + 440, 999_900), hashless("a", 1, 5, STAMP));
	assert_eq!(
		evidence(&file, &remote, Some(&synced(1, 5, SECOND + 441))),
		Some(Evidence::Imported)
	);
	assert_eq!(
		evidence(&file, &remote, Some(&synced(1, 5, SECOND + 442))),
		None
	);
}

/// The imported state is never read at whole seconds: filen-sync recorded the same whole second
/// this engine reads there.
#[test]
fn imported_state_is_never_compared_at_seconds() {
	let (file, remote) = (read_at(SECOND, 0), hashless("a", 1, 5, STAMP));
	assert_eq!(
		evidence(&file, &remote, Some(&synced(1, 5, SECOND + 441))),
		None
	);
}

/// Import a filen-sync state into `pair` that lists each of `files` — path, remote version, size
/// and local mtime — as synced, replacing what it held.
fn import(store: &BaselineStore, pair: PairId, files: &[(&str, u128, i64, i64)]) {
	store
		.replace_legacy_synced::<rusqlite::Error>(pair, |put| {
			for &(rel_path, uuid, size, mtime) in files {
				let item = |side| LegacyItem {
					rel_path,
					raw: rel_path,
					is_file: true,
					size: Some(size),
					side,
				};
				put(&item(LegacySide::Remote {
					uuid: Some(Uuid::from_u128(uuid)),
				}))?;
				put(&item(LegacySide::Local { mtime: Some(mtime) }))?;
			}
			Ok(())
		})
		.unwrap();
}

#[test]
fn the_store_reads_back_what_the_pair_imported_and_nothing_of_another_pair() {
	let (store, pair) = store();
	let (other, _) = store
		.create_pair("/other", Uuid::from_u128(98), SyncMode::TwoWay)
		.unwrap();
	assert!(!store.has_legacy_synced(pair).unwrap());
	import(&store, other, &[("a.bin", 1, 5, LATER)]);
	assert!(!store.has_legacy_synced(pair).unwrap());
	assert_eq!(store.legacy_synced(pair, "a.bin").unwrap(), None);
	import(&store, pair, &[("a.bin", 2, 6, STAMP)]);
	assert!(store.has_legacy_synced(pair).unwrap());
	assert_eq!(
		store.legacy_synced(pair, "a.bin").unwrap(),
		Some(synced(2, 6, STAMP))
	);
	assert_eq!(store.legacy_synced(pair, "b.bin").unwrap(), None);
}

#[test]
fn seeding_records_a_file_the_imported_state_lists_and_counts_it_as_imported() {
	let (store, pair) = store();
	import(
		&store,
		pair,
		&[("a.bin", 1, 5, LATER), ("b.bin", 9, 5, LATER)],
	);
	let counts = seed_all(
		&store,
		pair,
		&[local("a.bin", 5, LATER), local("b.bin", 5, LATER)],
		// `b.bin` was edited remotely since: a new version, and stamps that say nothing.
		&[
			hashless("a.bin", 1, 5, STAMP),
			hashless("b.bin", 2, 5, STAMP),
		],
		|_| false,
	);
	assert_eq!(
		counts,
		SeedCounts {
			imported: 1,
			empty: 0,
			size_and_mtime: 0,
		}
	);
	assert_eq!(
		store.entries(pair).unwrap(),
		[seeded_row(
			"a.bin",
			&local("a.bin", 5, LATER),
			&hashless("a.bin", 1, 5, STAMP)
		)]
	);
}

#[test]
fn imported_state_at_a_path_that_has_a_row_is_ignored() {
	let (store, pair) = store();
	let existing = BaselineEntry {
		state: BaselineState::Conflicted,
		..seeded_row("a.bin", &local("a.bin", 9, 1), &hashless("a.bin", 50, 9, 1))
	};
	store.upsert_entry(pair, &existing).unwrap();
	import(&store, pair, &[("a.bin", 1, 5, LATER)]);
	let counts = seed_all(
		&store,
		pair,
		&[local("a.bin", 5, LATER)],
		&[hashless("a.bin", 1, 5, STAMP)],
		|_| false,
	);
	assert_eq!(counts, SeedCounts::default());
	assert_eq!(store.entries(pair).unwrap(), [existing]);
}

/// Imported state that lists a path exactly as it is now is still weighed only at a candidate: a
/// remote with a hash, a directory on either side and a local file with no hash are left alone.
#[test]
fn imported_state_that_still_holds_seeds_no_path_that_is_no_candidate() {
	let (store, pair) = store();
	import(
		&store,
		pair,
		&[
			("hashed.bin", 1, 5, LATER),
			("remote-dir", 2, 5, LATER),
			("local-dir", 3, 5, LATER),
			("unhashed.bin", 4, 5, LATER),
		],
	);
	let counts = seed_all(
		&store,
		pair,
		&[
			local("hashed.bin", 5, LATER),
			local("remote-dir", 5, LATER),
			LocalNode {
				kind: NodeKind::Dir,
				..local("local-dir", 5, LATER)
			},
			LocalNode {
				content_hash: None,
				..local("unhashed.bin", 5, LATER)
			},
		],
		&[
			RemoteNode {
				content_hash: Some(hash(8)),
				..hashless("hashed.bin", 1, 5, STAMP)
			},
			RemoteNode {
				kind: NodeKind::Dir,
				..hashless("remote-dir", 2, 5, STAMP)
			},
			hashless("local-dir", 3, 5, STAMP),
			hashless("unhashed.bin", 4, 5, STAMP),
		],
		|_| false,
	);
	assert_eq!(counts, SeedCounts::default());
	assert_eq!(store.entries(pair).unwrap(), []);
}

/// A [`BaselineStore`] that counts what the seeding stage asks it about imported state.
struct Counting<'a> {
	store: &'a BaselineStore,
	asked_any: Cell<usize>,
	looked_up: Cell<usize>,
}

impl<'a> Counting<'a> {
	fn new(store: &'a BaselineStore) -> Self {
		Self {
			store,
			asked_any: Cell::new(0),
			looked_up: Cell::new(0),
		}
	}
}

impl SeedStore for Counting<'_> {
	fn has_legacy_synced(&self, pair: PairId) -> rusqlite::Result<bool> {
		self.asked_any.set(self.asked_any.get() + 1);
		self.store.has_legacy_synced(pair)
	}

	fn legacy_synced(
		&self,
		pair: PairId,
		rel_path: &str,
	) -> rusqlite::Result<Option<LegacySynced>> {
		self.looked_up.set(self.looked_up.get() + 1);
		self.store.legacy_synced(pair, rel_path)
	}

	fn synced_paths(
		&self,
		pair: PairId,
		uuids: &[Uuid],
		lineages: &[StableUuid],
	) -> rusqlite::Result<SyncedPaths> {
		self.store.synced_paths(pair, uuids, lineages)
	}

	fn seed_rows(&self, pair: PairId, rows: Vec<BaselineEntry>) -> rusqlite::Result<()> {
		self.store.seed_rows(pair, &rows)
	}
}

/// Seed `pair` from three remote files, two of them candidates, through a [`Counting`] store, and
/// return how often it was asked whether the pair imported anything and how many paths it looked
/// up.
fn lookups_seeding(store: &BaselineStore, pair: PairId) -> (usize, usize) {
	let counting = Counting::new(store);
	let baseline = store.baseline(pair).unwrap();
	let local: HashMap<String, LocalNode> = ["a.bin", "b.bin", "c.bin"]
		.into_iter()
		.map(|rel| (rel.to_owned(), local(rel, 5, STAMP)))
		.collect();
	let remote = [
		hashless("a.bin", 1, 5, STAMP),
		hashless("b.bin", 2, 5, STAMP),
		// Hashed: no candidate, so nothing is looked up for it.
		RemoteNode {
			content_hash: Some(hash(7)),
			..hashless("c.bin", 3, 5, STAMP)
		},
	];
	seed(
		&counting,
		pair,
		&baseline,
		&local,
		remote
			.iter()
			.map(|node| (Cow::from(node.rel_path.as_str()), Cow::Borrowed(node))),
		|_| false,
	)
	.unwrap();
	(counting.asked_any.get(), counting.looked_up.get())
}

#[test]
fn a_pair_that_imported_nothing_looks_up_no_path() {
	let (store, pair) = store();
	assert_eq!(lookups_seeding(&store, pair), (1, 0));
}

#[test]
fn a_pair_that_imported_state_looks_up_its_candidates_alone() {
	let (store, pair) = store();
	import(&store, pair, &[("z.bin", 1, 5, STAMP)]);
	assert_eq!(lookups_seeding(&store, pair), (1, 2));
}
