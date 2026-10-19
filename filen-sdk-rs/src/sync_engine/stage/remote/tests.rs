use filen_types::{crypto::Blake3Hash, fs::StableUuid};
use rand::{Rng, SeedableRng, rngs::StdRng, seq::SliceRandom};

use super::{
	super::{
		super::{
			changes::tests::{cacheable_dir, cacheable_file},
			ignore::{IgnoreRules, IgnoreSource, Origin},
			plan::{ViewFilter, place_remote_items},
			rows::Baseline,
			scan::QUARANTINE_DIR,
			side::Nodes,
		},
		assert_views_alike, paging_of,
		tests::row,
	},
	*,
};
use crate::{
	alloc_meter,
	cache::{
		CacheState, read_capped_subtree_snapshot, read_subtree_snapshot, read_subtree_snapshot_into,
	},
};

const CASES: u64 = 200;
const ROOT: Uuid = Uuid::from_u128(1);
/// A parent no row is.
const MISSING: Uuid = Uuid::from_u128(2);
/// The account root of a cache the tests write, with [`ROOT`] a directory under it, so that the
/// sync root has a row of its own to move.
const ACCOUNT: Uuid = Uuid::from_u128(3);

/// The names a generated snapshot draws from: case twins, plain and non-ASCII; two spellings NFC
/// folds into one, so two siblings drawing them are held back; every unsafe name; the engine's
/// own quarantine directory; and names the filter's rules hide.
const NAMES: &[&str] = &[
	"a",
	"A",
	"b.txt",
	"B.TXT",
	"\u{c4}",
	"\u{e4}",
	"e\u{301}",
	"\u{e9}",
	QUARANTINE_DIR,
	"build",
	"x.tmp",
	".DS_Store",
	"..",
	".",
	"",
	"s/l",
	"b\\s",
	"n\0l",
];

/// A generated snapshot: its items in the order a read hands them over, and its undecodable
/// records.
struct Snapshot {
	items: Vec<(bool, RemoteItem)>,
	undecodable: Vec<UndecodableItem>,
}

/// What `seed` generates: a tree drawn from [`NAMES`], with items under a file, under a parent
/// no row is, under an undecodable directory, now and then a directory with an undecodable
/// record of its own, a cycle of two directories, the root's own row under a directory of the
/// tree, and a chain deeper than a chain may run, with an unsafe name somewhere along it or
/// none. Everything the cache can list and everything a `ViewBuilder` answers for.
fn snapshot(seed: u64) -> Snapshot {
	let mut rng = StdRng::seed_from_u64(seed);
	let mut next = 100u128;
	let mut fresh = || {
		next += 1;
		Uuid::from_u128(next)
	};
	let item = |uuid, parent, name: &str, is_dir: bool, rng: &mut StdRng| {
		let hash = rng.random_range(0..3u8);
		RemoteItem {
			uuid,
			parent,
			name: name.to_owned(),
			stable_uuid: (!is_dir).then(|| StableUuid::new_for_test(uuid)),
			hash: (!is_dir && hash > 0).then(|| Blake3Hash::from([hash; 32])),
			size: if is_dir { 0 } else { rng.random_range(0..10) },
			modified_millis: rng.random_range(0..10),
		}
	};
	let (mut dirs, mut files, mut undecodable_dirs) = (vec![ROOT], Vec::new(), Vec::new());
	let (mut items, mut undecodable) = (Vec::new(), Vec::new());
	for _ in 0..rng.random_range(1..=40) {
		let is_dir = rng.random_bool(0.4);
		let pick = |from: &Vec<Uuid>, rng: &mut StdRng| from[rng.random_range(0..from.len())];
		let parent = match rng.random_range(0..20) {
			0 => MISSING,
			1 if !files.is_empty() => pick(&files, &mut rng),
			2 if !undecodable_dirs.is_empty() => pick(&undecodable_dirs, &mut rng),
			_ => pick(&dirs, &mut rng),
		};
		let uuid = fresh();
		let name = NAMES[rng.random_range(0..NAMES.len())];
		items.push((is_dir, item(uuid, parent, name, is_dir, &mut rng)));
		if is_dir {
			dirs.push(uuid);
		} else {
			files.push(uuid);
		}
		if rng.random_range(0..6) == 0 {
			let is_dir = rng.random_bool(0.5);
			let uuid = fresh();
			let parent = match rng.random_range(0..6) {
				0 => MISSING,
				_ => dirs[rng.random_range(0..dirs.len())],
			};
			undecodable.push(UndecodableItem {
				uuid,
				parent,
				stable_uuid: (!is_dir).then(|| StableUuid::new_for_test(uuid)),
			});
			if is_dir {
				undecodable_dirs.push(uuid);
			}
		}
	}
	// A directory the cache also holds an undecodable record of. The cache's own listing never
	// hands over both (a decodable row hides the record), but the builder answers for it, and so
	// must the walk: nothing under it is placed.
	if rng.random_range(0..4) == 0
		&& let Some((_, dir)) = items.iter().find(|(is_dir, _)| *is_dir)
	{
		undecodable.push(UndecodableItem {
			uuid: dir.uuid,
			parent: dir.parent,
			stable_uuid: None,
		});
	}
	if rng.random_range(0..6) == 0 {
		let (one, other) = (fresh(), fresh());
		items.push((true, item(one, other, "cycle", true, &mut rng)));
		items.push((true, item(other, one, "cycle", true, &mut rng)));
		items.push((false, item(fresh(), one, "in.txt", false, &mut rng)));
	}
	// The root's own row, under a directory of its subtree or under itself: a cache that holds
	// the root moved into its own subtree lists it with the rest. Every chain ends at the root,
	// so whatever the row is placed as holds nothing: the root's children are the root's.
	if rng.random_range(0..6) == 0 {
		let parent = dirs[rng.random_range(0..dirs.len())];
		items.push((true, item(ROOT, parent, "loop", true, &mut rng)));
	}
	if rng.random_range(0..8) == 0 {
		let depth = rng.random_range(MAX_REMOTE_DEPTH - 4..=MAX_REMOTE_DEPTH + 4);
		let unsafe_at = rng.random_range(0..depth * 2);
		let mut parent = dirs[rng.random_range(0..dirs.len())];
		for at in 0..depth {
			let uuid = fresh();
			let name = if at == unsafe_at { ".." } else { "deep" };
			items.push((true, item(uuid, parent, name, true, &mut rng)));
			parent = uuid;
		}
		items.push((false, item(fresh(), parent, "foot.txt", false, &mut rng)));
	}
	items.shuffle(&mut rng);
	Snapshot { items, undecodable }
}

/// The view `ViewBuilder` builds from `snapshot`, its rows streamed in in order.
fn resident(snapshot: &Snapshot) -> RemoteView {
	let mut builder = ViewBuilder::with_capacity(ROOT, 0);
	builder.undecodable(snapshot.undecodable.clone());
	for (is_dir, item) in &snapshot.items {
		builder.item(*is_dir, item.clone());
	}
	builder.finish()
}

/// The view staged from `snapshot`, placed and read as `paging` says.
fn staged(snapshot: &Snapshot, paging: Paging) -> (tempfile::TempDir, RemoteView) {
	let (dir, stage) = Stage::in_temp_dir();
	let mut sink = RemoteSink::new(stage, paging).unwrap();
	sink.undecodable(snapshot.undecodable.clone());
	for (is_dir, item) in &snapshot.items {
		sink.item(*is_dir, item.clone());
	}
	(dir, sink.finish(ROOT).unwrap())
}

/// Root rules that hide `build/` and `*.tmp`, over the built-in defaults, which hide every
/// `.DS_Store`; and rows under two of those, so a default hit there is a root and elsewhere is
/// counted on its own.
fn rules_and_rows() -> (IgnoreRules, Baseline) {
	let (source, errors) =
		IgnoreSource::parse("build/\n*.tmp\n", Origin::File { dir: "" }).unwrap();
	assert!(errors.is_empty(), "{errors:?}");
	let mut rules = IgnoreRules::new(None);
	rules.insert_file(String::new(), source);
	(
		rules,
		Baseline::from_rows([row(".DS_Store"), row("a/.DS_Store")]),
	)
}

/// The staged view places, holds and records exactly what `ViewBuilder` does, before the rules
/// and after them, over 200 generated snapshots read a few children and a few rows a statement
/// as well as a full page: the expectation is not written down here, it is whatever the
/// builder answers.
#[test]
fn the_staged_view_is_the_one_the_builder_builds() {
	let (rules, rows) = rules_and_rows();
	let filter = ViewFilter {
		rules: &rules,
		baseline: &rows,
	};
	let (mut held, mut skipped, mut collided, mut hidden, mut untracked) = (0, 0, 0, 0, 0);
	let mut looped = 0;
	for paging in [paging_of(1), paging_of(2), paging_of(7), Paging::PASS] {
		for seed in 0..CASES {
			let snapshot = snapshot(seed);
			looped += usize::from(snapshot.items.iter().any(|(_, item)| item.uuid == ROOT));
			let at = |step: &str| format!("{paging:?} seed {seed}: {step}");
			let resident = resident(&snapshot);
			let (_dir, staged) = staged(&snapshot, paging);
			assert_views_alike(&at("as placed"), &resident, &staged);
			held += resident.held_paths.len();
			skipped += resident.skipped.len();

			for filter in [None, Some(filter)] {
				let mut resident = self::resident(&snapshot);
				resident.filter(filter);
				let (_dir, mut staged) = self::staged(&snapshot, paging);
				staged.filter(filter);
				assert_views_alike(&at("filtered"), &resident, &staged);
				collided += usize::from(resident.has_collisions);
				hidden += resident.ignored.len();
				untracked += resident.ignored_default_untracked;
			}
		}
	}
	// A corpus that stopped producing these would let the equality above pass by proving
	// nothing about them.
	assert!(held > 100, "only {held} held path(s)");
	assert!(skipped > 1000, "only {skipped} record(s)");
	assert!(collided > 100, "only {collided} collision(s)");
	assert!(hidden > 100, "only {hidden} ignored root(s)");
	assert!(untracked > 100, "only {untracked} untracked default hit(s)");
	assert!(looped > 50, "only {looped} root row(s) under the tree");
}

/// The same, through the cache's own read: generated snapshots written into a cache DB and read
/// back as a pass reads them, copied into the stage inside the read's transaction on one side
/// and collected into two `Vec`s for `place_remote_items` on the other. What the cache lists is
/// a subtree of the root, so an orphan and a cycle never reach either side, but the root's own
/// row does once a snapshot moves it under a directory of its subtree.
#[test]
fn the_staged_view_of_a_cache_read_is_the_one_the_builder_builds() {
	let mut items_read = 0;
	for seed in 0..32 {
		let snapshot = snapshot(seed);
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("cache.db");
		let mut cache = CacheState::new_on_path(&path, ACCOUNT);
		cache
			.upsert_dirs([cacheable_dir(ROOT, ACCOUNT, "root")].into_iter())
			.unwrap();
		let (dirs, files): (Vec<_>, Vec<_>) =
			snapshot.items.iter().partition(|(is_dir, _)| *is_dir);
		cache
			.upsert_dirs(
				dirs.iter()
					.map(|(_, item)| cacheable_dir(item.uuid, item.parent, &item.name)),
			)
			.unwrap();
		cache
			.upsert_files(
				files
					.iter()
					.map(|(_, item)| cacheable_file(item.uuid, item.parent, &item.name)),
			)
			.unwrap();
		cache
			.replace_undecodable(ROOT, &snapshot.undecodable)
			.unwrap();

		let read = read_subtree_snapshot(&path, ROOT).unwrap();
		items_read += read.dirs.len() + read.files.len();
		let mut resident = place_remote_items(ROOT, &read.dirs, &read.files, &read.undecodable);
		let (_stage_dir, stage) = Stage::in_temp_dir();
		let mut sink = RemoteSink::new(stage, Paging::PASS).unwrap();
		let watermark = read_subtree_snapshot_into(&path, ROOT, &mut sink).unwrap();
		assert_eq!(watermark, read.watermark, "seed {seed}");
		let mut staged = sink.finish(ROOT).unwrap();
		assert_views_alike(&format!("seed {seed}: as placed"), &resident, &staged);
		resident.filter(None);
		staged.filter(None);
		assert_views_alike(&format!("seed {seed}: filtered"), &resident, &staged);
	}
	assert!(items_read > 300, "only {items_read} item(s) read");
}

/// A cache holding the sync root under its own child `d` lists the root's row with the rest,
/// under `d`. The builder ends every chain at the root, so it places that row at `d/loop` and the
/// root's children once, at the top; a walk that went into the row would read them again under
/// it, at every second level down to the deepest a chain may run.
#[test]
fn a_sync_root_listed_under_its_own_child_is_walked_once() {
	let d = Uuid::from_u128(10);
	let dir = tempfile::tempdir().unwrap();
	let path = dir.path().join("cache.db");
	let mut cache = CacheState::new_on_path(&path, ACCOUNT);
	cache
		.upsert_dirs(
			[
				cacheable_dir(ROOT, ACCOUNT, "root"),
				cacheable_dir(d, ROOT, "d"),
			]
			.into_iter(),
		)
		.unwrap();
	cache
		.upsert_files([cacheable_file(Uuid::from_u128(11), ROOT, "f.txt")].into_iter())
		.unwrap();
	cache
		.upsert_dirs([cacheable_dir(ROOT, d, "loop")].into_iter())
		.unwrap();

	// Capped, and read before the uncapped streamed read below: a read that loses its stop
	// rule cycles without end, which must fail here, not hang.
	let read = read_capped_subtree_snapshot(&path, ROOT, 16).unwrap();
	assert_eq!(
		read.dirs.len() + read.files.len(),
		3,
		"the read lists the root's row"
	);
	let resident = place_remote_items(ROOT, &read.dirs, &read.files, &read.undecodable);
	let (_stage_dir, stage) = Stage::in_temp_dir();
	let mut sink = RemoteSink::new(stage, Paging::PASS).unwrap();
	read_subtree_snapshot_into(&path, ROOT, &mut sink).unwrap();
	let staged = sink.finish(ROOT).unwrap();
	assert_views_alike("the root under its own child", &resident, &staged);
	let paths: Vec<String> = staged
		.nodes
		.whole()
		.iter()
		.map(|(path, _)| path.into_owned())
		.collect();
	assert_eq!(paths, ["d", "d/loop", "f.txt"]);
	assert_eq!(staged.skipped, []);
}

/// A row the stage cannot take stops the copy, and placing it returns that row's error: a copy
/// with a row missing is not the snapshot. Forced with triggers that refuse two of four rows,
/// as a full disk would.
#[test]
fn a_row_the_stage_cannot_take_fails_the_copy_with_its_error() {
	let (_dir, stage) = Stage::in_temp_dir();
	let mut sink = RemoteSink::new(stage, Paging::PASS).unwrap();
	sink.batch
		.conn()
		.execute_batch(
			"CREATE TRIGGER refuse_b BEFORE INSERT ON remote_raw WHEN NEW.name = 'b'
				 BEGIN SELECT RAISE(FAIL, 'no room for b'); END;
				 CREATE TRIGGER refuse_c BEFORE INSERT ON remote_raw WHEN NEW.name = 'c'
				 BEGIN SELECT RAISE(FAIL, 'no room for c'); END;",
		)
		.unwrap();
	sink.undecodable(Vec::new());
	for (n, name) in (10..).zip(["a", "b", "c", "d"]) {
		sink.item(
			false,
			RemoteItem {
				uuid: Uuid::from_u128(n),
				parent: ROOT,
				name: name.to_owned(),
				stable_uuid: Some(StableUuid::new_for_test(Uuid::from_u128(n))),
				hash: None,
				size: 1,
				modified_millis: 0,
			},
		);
	}
	assert_eq!(
		sink.rows, 1,
		"only the row before the refused one is copied"
	);
	let Err(error) = sink.finish(ROOT) else {
		panic!("a copy missing a row was placed");
	};
	assert!(
		error.to_string().contains("no room for b"),
		"the first refusal is the error: {error}"
	);
}

/// Staging a 20,000-item snapshot, placing it and reading it all back holds a page of it, not
/// the tree: the builder holds a few MiB for the same items.
#[test]
fn staging_and_placing_a_snapshot_holds_a_page_of_it() {
	const DIRS: u128 = 100;
	const FILES: u128 = 20_000;
	let items: Vec<(bool, RemoteItem)> = (0..DIRS)
		.map(|n| {
			(
				true,
				RemoteItem {
					uuid: Uuid::from_u128(1_000 + n),
					parent: ROOT,
					name: format!("d{n:03}"),
					stable_uuid: None,
					hash: None,
					size: 0,
					modified_millis: 0,
				},
			)
		})
		.chain((0..FILES).map(|n| {
			(
				false,
				RemoteItem {
					uuid: Uuid::from_u128(100_000 + n),
					parent: Uuid::from_u128(1_000 + n % DIRS),
					name: format!("f{n:05}.txt"),
					stable_uuid: Some(StableUuid::new_for_test(Uuid::from_u128(100_000 + n))),
					hash: Some(Blake3Hash::from([7; 32])),
					size: 1,
					modified_millis: 1,
				},
			)
		}))
		.collect();
	let builder_peak = alloc_meter::peak_bytes(|| {
		let mut builder = ViewBuilder::with_capacity(ROOT, 0);
		builder.undecodable(Vec::new());
		for (is_dir, item) in &items {
			builder.item(*is_dir, item.clone());
		}
		builder.finish().nodes.whole().len()
	});
	let (_dir, stage) = Stage::in_temp_dir();
	let (read, peak) = alloc_meter::peak_bytes(|| {
		let mut sink = RemoteSink::new(stage, Paging::PASS).unwrap();
		sink.undecodable(Vec::new());
		// Moved in one at a time, as the read hands them over: the `Vec` they come from is the
		// test's, and counted before the meter starts.
		for (is_dir, item) in &items {
			sink.item(*is_dir, item.clone());
		}
		let view = sink.finish(ROOT).unwrap();
		view.nodes.whole().iter().count()
	});
	assert_eq!(read, items.len());
	assert_eq!(builder_peak.0, items.len());
	assert!(
		peak < 1024 * 1024,
		"staging, placing and reading back the snapshot held {peak} bytes at once, where the \
			 builder held {}",
		builder_peak.1
	);
}

/// What a pass's journal asks of the view: where it holds each uuid. A staged view answers off
/// its uuid index as a walk of the map answers, for items it holds, moved or not, and for ones
/// it does not.
#[test]
fn a_staged_view_finds_items_by_uuid_as_a_walk_does() {
	let snapshot = Snapshot {
		items: vec![
			(
				true,
				RemoteItem {
					uuid: Uuid::from_u128(10),
					parent: ROOT,
					name: "docs".to_owned(),
					stable_uuid: None,
					hash: None,
					size: 0,
					modified_millis: 0,
				},
			),
			(
				false,
				RemoteItem {
					uuid: Uuid::from_u128(11),
					parent: Uuid::from_u128(10),
					name: "a.txt".to_owned(),
					stable_uuid: Some(StableUuid::new_for_test(Uuid::from_u128(11))),
					hash: None,
					size: 3,
					modified_millis: 0,
				},
			),
		],
		undecodable: Vec::new(),
	};
	let resident = resident(&snapshot);
	let (_dir, mut staged) = staged(&snapshot, Paging::PASS);
	let asked = HashSet::from([10, 11, 12].map(Uuid::from_u128));
	let found = |view: &RemoteView| -> BTreeMap<Uuid, String> {
		view.nodes
			.whole()
			.paths_of(&asked)
			.into_iter()
			.map(|(uuid, path)| (uuid, path.into_owned()))
			.collect()
	};
	assert_eq!(
		found(&staged),
		BTreeMap::from([
			(Uuid::from_u128(10), "docs".to_owned()),
			(Uuid::from_u128(11), "docs/a.txt".to_owned()),
		])
	);
	assert_eq!(found(&staged), found(&resident));
	staged
		.nodes
		.rekey_subtree(&Baseline::default(), "docs", "notes", |_, _| {});
	assert_eq!(
		found(&staged),
		BTreeMap::from([
			(Uuid::from_u128(10), "notes".to_owned()),
			(Uuid::from_u128(11), "notes/a.txt".to_owned()),
		])
	);
}
