use std::collections::HashMap;

use filen_types::{crypto::Blake3Hash, fs::StableUuid};
use uuid::Uuid;

use super::*;
use crate::sync_engine::SyncMode;

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
		evidence(&local("a", 5, STAMP), &hashless("a", 1, 5, STAMP)),
		Some(Evidence::SizeAndMtime)
	);
}

#[test]
fn a_millisecond_either_way_is_no_evidence() {
	for remote in [STAMP + 1, STAMP - 1] {
		assert_eq!(
			evidence(&local("a", 5, STAMP), &hashless("a", 1, 5, remote)),
			None,
			"remote stamp {remote}"
		);
	}
}

#[test]
fn a_size_off_by_one_is_no_evidence() {
	assert_eq!(
		evidence(&local("a", 5, STAMP), &hashless("a", 1, 6, STAMP)),
		None
	);
	assert_eq!(
		evidence(&local("a", 6, STAMP), &hashless("a", 1, 5, STAMP)),
		None
	);
}

#[test]
fn a_missing_remote_stamp_never_matches_a_file_with_content() {
	assert_eq!(evidence(&local("a", 5, 0), &hashless("a", 1, 5, 0)), None);
}

#[test]
fn two_empty_files_agree_whatever_their_stamps() {
	assert_eq!(
		evidence(&local("a", 0, STAMP), &hashless("a", 1, 0, STAMP + 5000)),
		Some(Evidence::Empty)
	);
	assert_eq!(
		evidence(&local("a", 0, STAMP), &hashless("a", 1, 0, 0)),
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
