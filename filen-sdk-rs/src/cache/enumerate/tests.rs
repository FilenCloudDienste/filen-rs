use std::borrow::Cow;

use chrono::{DateTime, Utc};
use filen_types::{
	api::v3::dir::color::DirColor, auth::FileEncryptionVersion, crypto::Blake3Hash, fs::StableUuid,
};
use uuid::Uuid;

use crate::{
	cache::CacheState,
	crypto::file::FileKey,
	fs::{dir::cache::CacheableDir, file::cache::CacheableFile},
};

use super::*;

fn temp_db_path() -> PathBuf {
	std::env::temp_dir().join(format!("filen_enumerate_test_{}.db", Uuid::new_v4()))
}

fn ms(millis: i64) -> DateTime<Utc> {
	DateTime::from_timestamp_millis(millis).unwrap()
}

fn test_dir(uuid: Uuid, parent: Uuid, name: &str) -> CacheableDir<'static> {
	CacheableDir {
		uuid,
		parent,
		color: DirColor::Custom(Cow::Borrowed("#123456")),
		favorited: true,
		timestamp: ms(1_700_000_000_000),
		name: Cow::Owned(name.to_string()),
		created: Some(ms(1_700_000_000_001)),
	}
}

fn test_file(uuid: Uuid, parent: Uuid, name: &str) -> CacheableFile<'static> {
	CacheableFile {
		uuid,
		stable_uuid: StableUuid::new_for_test(uuid),
		parent,
		chunks_size: 7,
		chunks: 2,
		favorited: false,
		region: Cow::Borrowed("eu-central-1"),
		bucket: Cow::Borrowed("bucket-x"),
		timestamp: ms(1_700_000_000_002),
		name: Cow::Owned(name.to_string()),
		size: 1234,
		mime: Cow::Borrowed("image/png"),
		key: FileKey::from_str_with_version(&"b".repeat(64), FileEncryptionVersion::V3).unwrap(),
		last_modified: ms(1_700_000_000_003),
		created: Some(ms(1_700_000_000_004)),
		hash: Some(Blake3Hash::from([7u8; 32])),
	}
}

/// account_root → { f1, A(dir) → { a1, AA(dir) → { aa1 } }, B(dir) → { b1 } }.
struct Fixture {
	path: PathBuf,
	root: Uuid,
	a: CacheableDir<'static>,
	aa: CacheableDir<'static>,
	b: CacheableDir<'static>,
	f1: CacheableFile<'static>,
	a1: CacheableFile<'static>,
	aa1: CacheableFile<'static>,
	b1: CacheableFile<'static>,
	// Held open: the read connection works alongside the writer (WAL).
	state: CacheState,
}

fn fixture() -> Fixture {
	let path = temp_db_path();
	let root = Uuid::new_v4();
	let mut state = CacheState::new_on_path(&path, root);
	let a = test_dir(Uuid::new_v4(), root, "A");
	let aa = test_dir(Uuid::new_v4(), a.uuid, "AA");
	let b = test_dir(Uuid::new_v4(), root, "B");
	let f1 = test_file(Uuid::new_v4(), root, "f1.txt");
	let a1 = test_file(Uuid::new_v4(), a.uuid, "a1.txt");
	let aa1 = test_file(Uuid::new_v4(), aa.uuid, "aa1.txt");
	let b1 = test_file(Uuid::new_v4(), b.uuid, "b1.txt");
	state.upsert_dirs([&a, &aa, &b].into_iter()).unwrap();
	state
		.upsert_files([&f1, &a1, &aa1, &b1].into_iter())
		.unwrap();
	Fixture {
		path,
		root,
		a,
		aa,
		b,
		f1,
		a1,
		aa1,
		b1,
		state,
	}
}

fn uuids(items: impl IntoIterator<Item = Uuid>) -> std::collections::HashSet<Uuid> {
	items.into_iter().collect()
}

/// Counts what a sink was handed, for the read that fails partway.
#[derive(Default)]
struct Counting {
	items: usize,
	/// Set if the undecodable records arrived AFTER an item, which the contract forbids: a
	/// sink that places on arrival cannot place anything until it has them.
	undecodable_late: bool,
}

impl SnapshotSink for Counting {
	fn undecodable(&mut self, _items: Vec<UndecodableItem>) {
		self.undecodable_late |= self.items > 0;
	}

	fn item(&mut self, _is_dir: bool, _item: RemoteItem) {
		self.items += 1;
	}
}

/// A read that fails partway returns the ERROR, and the part of the subtree the sink took is
/// all it ever gets: a streamed read can no more hand back a tree with holes than the
/// materialized one could. Forced the only way a row can fail on its own — an `items.type` the
/// projection does not know, which is what a corrupt row would look like in the field.
///
/// What is deliberately NOT asserted is how much the sink took: the corrupted row is one of
/// the fixture's seven, so no item count can tell a read that stopped at it from one that
/// streamed every other row and failed at the end.
#[test]
fn a_read_that_fails_partway_hands_back_the_error_and_no_subtree() {
	let f = fixture();
	f.state
		.db
		.execute(
			"UPDATE items SET type = 0 WHERE uuid = ?1",
			params![f.b1.uuid],
		)
		.unwrap();

	let mut sink = Counting::default();
	let error = read_subtree_snapshot_into(&f.path, f.root, &mut sink).unwrap_err();
	assert!(
		!sink.undecodable_late,
		"the undecodable records come before the first item, failed read or not"
	);
	assert!(
		matches!(error, rusqlite::Error::FromSqlConversionFailure(..)),
		"the unreadable item type is what stopped the read: {error:?}"
	);
	assert!(
		read_subtree_snapshot(&f.path, f.root).is_err(),
		"and the materialized read over the same rows fails with it"
	);
}

#[test]
fn account_root_snapshot_returns_the_whole_subtree_split_by_type() {
	let f = fixture();
	let snapshot = read_subtree_snapshot(&f.path, f.root).unwrap();

	// Every descendant of the account root, dirs and files separated, across both the A/AA
	// branch and the sibling B branch. The root node itself is never returned (the engine syncs
	// its contents, not the root).
	assert_eq!(
		uuids(snapshot.dirs.iter().map(|d| d.uuid)),
		uuids([f.a.uuid, f.aa.uuid, f.b.uuid]),
		"all descendant dirs, anchor excluded"
	);
	assert_eq!(
		uuids(snapshot.files.iter().map(|file| file.uuid)),
		uuids([f.f1.uuid, f.a1.uuid, f.aa1.uuid, f.b1.uuid]),
		"all descendant files across every depth and both branches"
	);
}

#[test]
fn subdir_snapshot_is_scoped_to_that_root_only() {
	let f = fixture();
	// Anchored at A: only A's subtree (AA, a1, aa1) — never B's b1, never the account-root f1,
	// and never A itself.
	let snapshot = read_subtree_snapshot(&f.path, f.a.uuid).unwrap();
	assert_eq!(
		uuids(snapshot.dirs.iter().map(|d| d.uuid)),
		uuids([f.aa.uuid])
	);
	assert_eq!(
		uuids(snapshot.files.iter().map(|file| file.uuid)),
		uuids([f.a1.uuid, f.aa1.uuid]),
		"A's own file plus its grandchild, nothing from sibling B"
	);
}

/// The view's projection, hydrated from the same rows the full payload comes from: a file
/// carries its name, lineage, hash, size and modified stamp, a directory its name and created
/// stamp under the same field, with no lineage, no hash and no size.
#[test]
fn items_hydrate_in_the_view_projection() {
	let f = fixture();
	let snapshot = read_subtree_snapshot(&f.path, f.root).unwrap();
	let got_file = snapshot
		.files
		.iter()
		.find(|file| file.uuid == f.a1.uuid)
		.expect("a1 present");
	assert_eq!(
		*got_file,
		RemoteItem {
			uuid: f.a1.uuid,
			parent: f.a.uuid,
			name: "a1.txt".to_string(),
			stable_uuid: Some(f.a1.stable_uuid),
			hash: f.a1.hash,
			size: f.a1.size,
			modified_millis: f.a1.last_modified.timestamp_millis(),
		}
	);
	let got_dir = snapshot
		.dirs
		.iter()
		.find(|d| d.uuid == f.aa.uuid)
		.expect("AA present");
	assert_eq!(
		*got_dir,
		RemoteItem {
			uuid: f.aa.uuid,
			parent: f.a.uuid,
			name: "AA".to_string(),
			stable_uuid: None,
			hash: None,
			size: 0,
			modified_millis: f.aa.created.unwrap().timestamp_millis(),
		}
	);
}

#[test]
fn empty_subtree_yields_no_items() {
	let f = fixture();
	// AA has a file child (aa1) but no dir children — anchor at aa1's grandparent's leaf: use a
	// brand-new empty dir to prove an empty result.
	let empty = test_dir(Uuid::new_v4(), f.root, "empty");
	// Re-upsert through a fresh borrow of state would need &mut; instead anchor at a uuid with
	// no children at all: aa1 is a file, so its "subtree" is empty.
	let snapshot = read_subtree_snapshot(&f.path, f.aa1.uuid).unwrap();
	assert!(snapshot.dirs.is_empty() && snapshot.files.is_empty());
	// Anchoring at a uuid that isn't even in the cache is likewise empty (not an error).
	let absent = read_subtree_snapshot(&f.path, empty.uuid).unwrap();
	assert!(absent.dirs.is_empty() && absent.files.is_empty());
}

/// A record a listing could not decode rides along with the snapshot until the uuid is gone or
/// decodes again: a removal drops it, a decodable upsert of the same uuid hides it, and the next
/// listing of its root replaces it.
#[test]
fn undecodable_records_are_listed_until_removed_decoded_or_relisted() {
	let mut f = fixture();
	let garbled_file = UndecodableItem {
		uuid: Uuid::new_v4(),
		parent: f.a.uuid,
		stable_uuid: Some(StableUuid::new_for_test(Uuid::new_v4())),
	};
	let garbled_dir = UndecodableItem {
		uuid: Uuid::new_v4(),
		parent: f.root,
		stable_uuid: None,
	};
	let decodes_later = UndecodableItem {
		uuid: f.b1.uuid,
		parent: f.b.uuid,
		stable_uuid: Some(f.b1.stable_uuid),
	};
	f.state
		.replace_undecodable(f.root, &[garbled_file, garbled_dir, decodes_later])
		.unwrap();
	let listed = |f: &Fixture| {
		let mut got = read_subtree_snapshot(&f.path, f.root).unwrap().undecodable;
		got.sort_by_key(|item| item.uuid);
		got
	};
	let mut expected = vec![garbled_file, garbled_dir];
	expected.sort_by_key(|item| item.uuid);
	assert_eq!(
		listed(&f),
		expected,
		"b1 is a cached item, so its stale record is not reported"
	);

	f.state
		.delete_items(std::iter::once(garbled_file.uuid))
		.unwrap();
	assert_eq!(listed(&f), vec![garbled_dir], "a removal drops the record");

	f.state.replace_undecodable(f.root, &[]).unwrap();
	assert!(listed(&f).is_empty(), "a clean relisting clears the root");
}

/// The full payloads a pass acts on, read back by uuid: the ones named come whole, an unknown
/// uuid is simply absent rather than an error, and asking for nothing reads nothing at all.
#[test]
fn hydrate_by_uuids_returns_the_full_payloads_of_the_items_named() {
	let f = fixture();
	let got = hydrate_by_uuids(&f.path, &[f.a1.uuid, f.aa.uuid, Uuid::new_v4()]).unwrap();
	assert_eq!(
		got.len(),
		2,
		"the unknown uuid is absent, not an error: {got:?}"
	);
	let file = got
		.iter()
		.find_map(|item| match item {
			SearchResult::File(file) => Some(file),
			SearchResult::Dir(_) => None,
		})
		.expect("a1 present");
	assert_eq!(*file, f.a1, "full file payload round-trips");
	let dir = got
		.iter()
		.find_map(|item| match item {
			SearchResult::Dir(dir) => Some(dir),
			SearchResult::File(_) => None,
		})
		.expect("AA present");
	assert_eq!(*dir, f.aa, "full dir payload round-trips");
	// No uuids, so no connection is opened: a path that does not exist would otherwise error.
	assert!(
		hydrate_by_uuids(Path::new("/no/such/cache.db"), &[])
			.unwrap()
			.is_empty()
	);
}

/// Corrupt parent links never make the read repeat a row or run forever: the anchor listed
/// under its own child is read once (as that child's child) and not descended into again, and
/// a cycle that does not pass through the anchor cannot be reached from it. Anchored inside a
/// two-dir cycle, or at a self-parented item, the read returns each member once and stops.
#[test]
fn every_reachable_item_is_read_once_whatever_cycles_the_parents_hold() {
	let path = temp_db_path();
	let account = Uuid::from_u128(1);
	let anchor = Uuid::from_u128(2);
	let d = Uuid::from_u128(3);
	let f = Uuid::from_u128(4);
	let g = Uuid::from_u128(5);
	let (x, y) = (Uuid::from_u128(6), Uuid::from_u128(7));
	let z = Uuid::from_u128(8);
	let mut state = CacheState::new_on_path(&path, account);
	state
		.upsert_dirs(
			[
				test_dir(anchor, account, "anchor"),
				test_dir(d, anchor, "d"),
				test_dir(x, y, "x"),
				test_dir(y, x, "y"),
				test_dir(z, z, "z"),
			]
			.iter(),
		)
		.unwrap();
	state
		.upsert_files([test_file(f, anchor, "f"), test_file(g, d, "g")].iter())
		.unwrap();
	state
		.upsert_dirs([test_dir(anchor, d, "loop")].iter())
		.unwrap();

	let read = |root: Uuid| {
		// Capped: a read that loses its stop rule cycles without end, which must fail the
		// test, not hang it.
		let snapshot = read_capped_subtree_snapshot(&path, root, 16).unwrap();
		let mut got: Vec<Uuid> = snapshot
			.dirs
			.iter()
			.chain(&snapshot.files)
			.map(|item| item.uuid)
			.collect();
		got.sort_unstable();
		got
	};
	assert_eq!(
		read(anchor),
		[anchor, d, f, g],
		"anchored at the looped root"
	);
	assert_eq!(read(x), [x, y], "anchored inside the detached cycle");
	assert_eq!(read(z), [z], "anchored at the self-parented dir");
}

#[test]
fn watermark_is_none_on_a_fresh_cache_and_reflects_a_set_value() {
	let f = fixture();
	// init.sql seeds the watermark row NULL; nothing has applied an event.
	assert_eq!(
		read_subtree_snapshot(&f.path, f.root).unwrap().watermark,
		None
	);

	f.state.set_watermark(4242).unwrap();
	assert_eq!(
		read_subtree_snapshot(&f.path, f.root).unwrap().watermark,
		Some(4242),
		"the snapshot reads the same watermark the worker advances"
	);
}
