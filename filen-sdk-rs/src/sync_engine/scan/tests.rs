use std::fs;

use super::super::side::{Nodes, NodesAt, test_support::sorted_entries};

use rand::{Rng, SeedableRng, rngs::StdRng};
use uuid::Uuid;

use super::*;
use crate::sync_engine::{
	baseline::BaselineState,
	ignore::{IgnoreLevel, parse_user_ignore},
};

fn temp_root() -> std::path::PathBuf {
	let dir = std::env::temp_dir().join(format!("filen_scan_test_{}", Uuid::new_v4()));
	fs::create_dir_all(&dir).unwrap();
	dir
}

/// A scan with no user level, reading the `.filenignore` files on disk.
fn scan_plain(root: &Path, baseline: &HashMap<String, BaselineEntry>) -> LocalScan {
	scan_local(
		ScanRoot {
			path: root,
			pair_root: PairRoot::Directory,
		},
		&tree(baseline),
		IgnoreRules::default(),
		RuleFiles::Read,
	)
	.0
}

fn sorted_paths(scan: &LocalScan) -> Vec<String> {
	let mut paths: Vec<String> = scan
		.nodes
		.whole()
		.paths()
		.map(|path| path.into_owned())
		.collect();
	paths.sort_unstable();
	paths
}

#[test]
fn normalize_rel_path_keeps_ascii_and_composes_decomposed_names() {
	let key = |path: &str| normalize_rel_path(Path::new(path));
	assert_eq!(key("a/b.txt").as_deref(), Some("a/b.txt"));
	assert_eq!(key("single").as_deref(), Some("single"));
	assert_eq!(key("").as_deref(), Some(""));
	assert_eq!(key("cafe\u{301}").as_deref(), Some("caf\u{e9}"));
	assert_eq!(
		key("Docs/re\u{301}sume\u{301}/caf\u{e9}/x.txt").as_deref(),
		Some("Docs/r\u{e9}sum\u{e9}/caf\u{e9}/x.txt")
	);
	// Hangul jamo compose too, and an already composed name is kept byte for byte.
	assert_eq!(
		key("\u{1100}\u{1161}/\u{ac00}").as_deref(),
		Some("\u{ac00}/\u{ac00}")
	);
	assert_eq!(key("../escape"), None);
	assert_eq!(key("/abs"), None);
}

#[cfg(unix)]
#[test]
fn normalize_rel_path_refuses_a_non_utf8_component() {
	use std::os::unix::ffi::OsStrExt;
	let path = Path::new(OsStr::from_bytes(b"ok/bad\xff/leaf"));
	assert_eq!(normalize_rel_path(path), None);
}

#[test]
fn collision_key_folds_case() {
	assert_eq!(collision_key("A/B.txt"), "a/b.txt");
	assert_eq!(collision_key("already/low.txt"), "already/low.txt");
	assert_eq!(collision_key("Dir/İstanbul.TXT"), "dir/i\u{307}stanbul.txt");
	assert_eq!(collision_key("ẞ/Ä"), "ß/ä");
}

#[test]
fn collision_key_folds_ascii_like_the_full_unicode_fold() {
	let unicode_fold = |s: &str| s.chars().flat_map(char::to_lowercase).collect::<String>();
	let every_ascii_char = (0u8..=127).map(char::from).collect::<String>();
	for path in [
		every_ascii_char.as_str(),
		"MiXeD/Case/Path.TXT",
		"ZZ/@[`{/~_-",
	] {
		assert_eq!(collision_key(path), unicode_fold(path), "{path:?}");
	}
}

#[test]
fn scans_a_tree_into_normalized_nodes() {
	let root = temp_root();
	fs::write(root.join("a.txt"), b"hello").unwrap();
	fs::create_dir(root.join("sub")).unwrap();
	fs::write(root.join("sub").join("b.txt"), b"world").unwrap();

	let scan = scan_plain(&root, &HashMap::new());
	assert!(
		scan.complete,
		"a clean tree scans completely: {:?}",
		scan.errors
	);

	let mut paths: Vec<String> = scan
		.nodes
		.whole()
		.paths()
		.map(|path| path.into_owned())
		.collect();
	paths.sort();
	assert_eq!(paths, vec!["a.txt", "sub", "sub/b.txt"]);

	let nodes = scan.nodes.whole();
	let a = nodes.at("a.txt").unwrap();
	assert_eq!(a.kind, NodeKind::File);
	assert_eq!(a.size, 5);
	assert!(
		a.content_hash.is_some(),
		"files are hashed without a baseline"
	);
	assert_eq!(nodes.at("sub").unwrap().kind, NodeKind::Dir);
	assert!(
		nodes.at("sub").unwrap().content_hash.is_none(),
		"dirs have no hash"
	);

	fs::remove_dir_all(&root).ok();
}

#[test]
fn fast_path_reuses_baseline_hash_when_size_and_mtime_match() {
	let root = temp_root();
	let file = root.join("a.txt");
	fs::write(&file, b"hello").unwrap();
	let meta = fs::metadata(&file).unwrap();
	let mtime = FilenMetaExt::modified(&meta).timestamp_millis();

	// A baseline whose (size, mtime) match the file but carries a SENTINEL hash that is NOT the
	// real content hash. The fast-path must hand back the sentinel without re-hashing.
	let sentinel = Blake3Hash::from([0xAB; 32]);
	let baseline = HashMap::from([(
		"a.txt".to_string(),
		BaselineEntry {
			rel_path: "a.txt".to_string(),
			kind: NodeKind::File,
			remote_uuid: None,
			content_hash: Some(sentinel),
			size: Some(5),
			local_mtime: Some(mtime),
			remote_modified: None,
			state: BaselineState::Synced,
			local_kind: None,
			remote_kind: None,
			remote_hash: None,
			remote_size: None,
			remote_stable_uuid: None,
			agreed_hash: None,
		},
	)]);

	let scan = scan_plain(&root, &baseline);
	assert_eq!(
		scan.nodes.whole().at("a.txt").unwrap().content_hash,
		Some(sentinel),
		"unchanged (size, mtime) reuses the baseline hash — no re-hash"
	);

	// A baseline with a stale size forces a real re-hash (sentinel must NOT survive).
	let stale = HashMap::from([(
		"a.txt".to_string(),
		BaselineEntry {
			size: Some(999),
			..baseline["a.txt"].clone()
		},
	)]);
	let rescan = scan_plain(&root, &stale);
	assert_ne!(
		rescan.nodes.whole().at("a.txt").unwrap().content_hash,
		Some(sentinel),
		"a diverged size triggers a fresh hash"
	);

	fs::remove_dir_all(&root).ok();
}

/// A file written through a handle that is still open has a new size on disk while NTFS's copy in
/// its parent's index, which the directory listing returns, still holds the old one. The scan stats
/// it fresh, so the fast path misses and the new content is hashed. The write is not flushed:
/// `FlushFileBuffers` may bring the parent's copy up to date, and the listing would then pass too.
#[cfg(windows)]
#[test]
fn a_file_grown_through_a_handle_still_open_is_hashed_again() {
	let root = temp_root();
	let path = root.join("held.log");
	let before = vec![b'a'; 100];
	fs::write(&path, &before).unwrap();
	let meta = fs::metadata(&path).unwrap();
	let baseline = HashMap::from([(
		"held.log".to_string(),
		BaselineEntry {
			rel_path: "held.log".to_string(),
			kind: NodeKind::File,
			remote_uuid: None,
			content_hash: Some(Blake3Hash::from(blake3::hash(&before))),
			size: Some(meta.len()),
			local_mtime: Some(FilenMetaExt::modified(&meta).timestamp_millis()),
			remote_modified: None,
			state: BaselineState::Synced,
			local_kind: None,
			remote_kind: None,
			remote_hash: None,
			remote_size: None,
			remote_stable_uuid: None,
			agreed_hash: None,
		},
	)]);

	let mut held = fs::OpenOptions::new().append(true).open(&path).unwrap();
	std::io::Write::write_all(&mut held, &[b'b'; 1000]).unwrap();
	let mut after = before;
	after.extend_from_slice(&[b'b'; 1000]);

	let scan = scan_plain(&root, &baseline);
	let nodes = scan.nodes.whole();
	let node = nodes.at("held.log").unwrap();
	assert_eq!(node.size, 1100);
	assert_eq!(
		node.content_hash,
		Some(Blake3Hash::from(blake3::hash(&after)))
	);

	drop(held);
	fs::remove_dir_all(&root).ok();
}

/// Times the directory listing's own metadata against a fresh `symlink_metadata` per entry on 1, 4
/// and 8 threads, over 100 directories of 1000 files, and prints both. Run it on Windows with
/// `cargo test -p filen-sdk-rs --lib -F sync-engine sync_engine::scan::tests::stat_cost_probe --
/// --ignored --nocapture`.
///
/// It asserts nothing, so drop it once a reading has been taken on Windows.
#[cfg(windows)]
#[test]
#[ignore = "timing probe: prints the Windows listing's metadata cost against a fresh stat's"]
fn stat_cost_probe() {
	let root = tempfile::tempdir().unwrap();
	let mut paths = Vec::new();
	for d in 0..100 {
		let dir = root.path().join(format!("d{d}"));
		fs::create_dir(&dir).unwrap();
		for f in 0..1000 {
			let path = dir.join(format!("f{f}"));
			fs::write(&path, b"x").unwrap();
			paths.push(path);
		}
	}
	let list = || {
		let start = std::time::Instant::now();
		for dir in fs::read_dir(root.path()).unwrap() {
			for entry in fs::read_dir(dir.unwrap().path()).unwrap() {
				entry.unwrap().metadata().unwrap();
			}
		}
		start.elapsed()
	};
	// The first listing warms the cache.
	list();
	println!(
		"entries {} listing+cached metadata {:?}",
		paths.len(),
		list()
	);
	for threads in [1, 4, 8] {
		let start = std::time::Instant::now();
		std::thread::scope(|scope| {
			for chunk in paths.chunks(paths.len().div_ceil(threads)) {
				scope.spawn(move || {
					for path in chunk {
						fs::symlink_metadata(path).unwrap();
					}
				});
			}
		});
		println!(
			"fresh symlink_metadata, {threads} threads: {:?}",
			start.elapsed()
		);
	}
}

#[test]
fn missing_root_is_reported_incomplete() {
	let root = std::env::temp_dir().join(format!("filen_scan_absent_{}", Uuid::new_v4()));
	let scan = scan_plain(&root, &HashMap::new());
	assert!(
		!scan.complete,
		"a missing root scans incomplete (guards mass-delete)"
	);
	assert!(scan.nodes.whole().is_empty());
	assert!(!scan.errors.is_empty());
}

#[test]
fn in_flight_download_temp_files_are_excluded() {
	let root = temp_root();
	fs::write(root.join("keep.txt"), b"x").unwrap();
	// What `download_file_to_path` writes into the tree while a transfer is in flight.
	fs::write(
		root.join("dee76e0e-0000-0000-0000-000000000000.filendl"),
		b"half",
	)
	.unwrap();
	// A DIRECTORY with that suffix is a legitimate user item and stays.
	fs::create_dir(root.join("notes.filendl")).unwrap();

	let scan = scan_plain(&root, &HashMap::new());
	let mut paths: Vec<String> = scan
		.nodes
		.whole()
		.paths()
		.map(|path| path.into_owned())
		.collect();
	paths.sort();
	assert_eq!(
		paths,
		vec!["keep.txt", "notes.filendl"],
		"a partial download must never be seen as a local file to upload"
	);
	assert!(
		scan.complete,
		"skipping our own staging file is not an error"
	);

	fs::remove_dir_all(&root).ok();
}

#[test]
fn a_name_the_remote_would_reject_is_reported_once_but_still_observed() {
	let root = temp_root();
	fs::write(root.join("keep.txt"), b"x").unwrap();
	// A leading space is refused by the SDK's own name validator, so no upload of either can
	// ever succeed, yet every platform stores it as written. A reserved device name or a
	// trailing dot would not do: Win32 opens the console for `CON` and strips the dot.
	fs::write(root.join(" notes.txt"), b"rejected").unwrap();
	fs::create_dir(root.join(" bad")).unwrap();
	fs::write(root.join(" bad").join("inner.txt"), b"child").unwrap();
	fs::create_dir_all(root.join(" bad").join("deeper")).unwrap();
	fs::write(root.join(" bad").join("deeper").join("x.txt"), b"deep").unwrap();

	let scan = scan_plain(&root, &HashMap::new());
	let mut paths: Vec<String> = scan
		.nodes
		.whole()
		.paths()
		.map(|path| path.into_owned())
		.collect();
	paths.sort();
	// The scan reports the disk as it is: a rejected name is unpushable, not invisible. Dropping
	// it here would make a rename INTO such a name look like a local deletion one layer up.
	assert_eq!(
		paths,
		vec![
			" bad",
			" bad/deeper",
			" bad/deeper/x.txt",
			" bad/inner.txt",
			" notes.txt",
			"keep.txt"
		]
	);
	assert!(
		scan.nodes
			.whole()
			.at(" notes.txt")
			.unwrap()
			.content_hash
			.is_some(),
		"a rejected name is hashed like any other file, so a rename of a synced file into one \
			 is still recognized as that file"
	);

	let reported: Vec<_> = scan.invalid_names.keys().cloned().collect();
	assert_eq!(
		reported,
		vec![" bad", " notes.txt"],
		"each rejected name is reported ONCE, not once per descendant"
	);
	assert!(
		scan.invalid_names[" bad"].contains("start with a space"),
		"the report carries the validator's own reason: {:?}",
		scan.invalid_names[" bad"]
	);

	// Crucially the scan still COMPLETED: an unsyncable name is not missing evidence, so the
	// delete guard must not start holding every deletion because of it.
	assert!(scan.complete, "{:?}", scan.errors);
	assert!(scan.errors.is_empty(), "{:?}", scan.errors);

	fs::remove_dir_all(&root).ok();
}

#[test]
fn a_valid_name_that_merely_contains_a_rejected_one_is_kept() {
	let root = temp_root();
	for name in ["CONSOLE", "console.txt", "CON.txt", "a.b", ".hidden"] {
		fs::write(root.join(name), b"x").unwrap();
	}
	let scan = scan_plain(&root, &HashMap::new());
	assert!(
		scan.invalid_names.is_empty(),
		"none of these are rejected by the SDK validator: {:?}",
		scan.invalid_names
	);
	assert_eq!(scan.nodes.whole().len(), 5);
	fs::remove_dir_all(&root).ok();
}

#[test]
fn quarantine_dir_is_excluded() {
	let root = temp_root();
	fs::write(root.join("keep.txt"), b"x").unwrap();
	let quarantine = root.join(QUARANTINE_DIR);
	fs::create_dir(&quarantine).unwrap();
	fs::write(quarantine.join("trashed.txt"), b"old").unwrap();

	let scan = scan_plain(&root, &HashMap::new());
	let paths: Vec<String> = scan
		.nodes
		.whole()
		.paths()
		.map(|path| path.into_owned())
		.collect();
	assert_eq!(
		paths,
		vec!["keep.txt"],
		"the quarantine subtree is not scanned"
	);

	fs::remove_dir_all(&root).ok();
}

#[cfg(unix)]
#[test]
fn an_unreadable_directory_is_reported_with_its_path() {
	use std::os::unix::fs::PermissionsExt;

	let root = temp_root();
	fs::write(root.join("keep.txt"), b"x").unwrap();
	fs::create_dir(root.join("locked")).unwrap();
	fs::write(root.join("locked").join("inner.txt"), b"y").unwrap();
	fs::set_permissions(root.join("locked"), fs::Permissions::from_mode(0o000)).unwrap();

	let scan = scan_plain(&root, &HashMap::new());
	fs::set_permissions(root.join("locked"), fs::Permissions::from_mode(0o755)).unwrap();

	assert!(!scan.complete, "an unreadable subtree is missing evidence");
	let reported: Vec<String> = scan.reported_errors().collect();
	assert_eq!(reported.len(), 1, "{reported:?}");
	assert!(
		reported[0].starts_with("local scan: locked: "),
		"the line names the path it is about: {reported:?}"
	);

	fs::remove_dir_all(&root).ok();
}

/// The built-in defaults hide a `.DS_Store` in every folder. Each one is pruned, but only the
/// ones the baseline still tracks become ignored ROOTS: recording the rest gives the pass a root
/// under every directory, and each is left out of the report and deletes no row when the pass
/// untracks. A tracked one — a file with a row, or a directory with a row under it — is a root
/// exactly as before.
#[test]
fn a_default_rule_hit_is_a_root_only_where_a_row_sits_at_or_under_it() {
	let root = temp_root();
	for dir in ["a", "b", "c"] {
		fs::create_dir(root.join(dir)).unwrap();
		fs::write(root.join(dir).join(".DS_Store"), b"x").unwrap();
		fs::write(root.join(dir).join("keep.txt"), b"y").unwrap();
	}
	// A default-ignored DIRECTORY, which is the case that has to look at a whole subtree.
	fs::create_dir(root.join("a").join(".Trashes")).unwrap();
	fs::write(root.join("a").join(".Trashes").join("t.bin"), b"z").unwrap();

	let untracked = scan_plain(&root, &HashMap::new());
	assert!(
		untracked.ignored.is_empty(),
		"nothing was ever synced there, so none of it is a root: {:?}",
		untracked.ignored
	);
	assert_eq!(
		untracked.ignored_default_untracked, 4,
		"3 files and 1 directory"
	);
	assert!(
		!untracked.nodes.whole().holds("a/.DS_Store")
			&& !untracked.nodes.whole().holds("a/.Trashes"),
		"they are still hidden, and still not descended into"
	);

	let row = |rel_path: &str, kind: NodeKind| BaselineEntry {
		rel_path: rel_path.to_string(),
		kind,
		remote_uuid: None,
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
	};
	let baseline = HashMap::from([
		(
			"b/.DS_Store".to_string(),
			row("b/.DS_Store", NodeKind::File),
		),
		(
			"a/.Trashes/t.bin".to_string(),
			row("a/.Trashes/t.bin", NodeKind::File),
		),
	]);

	let tracked = scan_plain(&root, &baseline);
	assert_eq!(
		tracked.ignored.keys().collect::<Vec<_>>(),
		vec!["a/.Trashes", "b/.DS_Store"],
		"a row AT the path and a row UNDER it both make a root"
	);
	assert_eq!(
		tracked.ignored_default_untracked, 2,
		"the other two `.DS_Store`s are still untracked"
	);

	fs::remove_dir_all(&root).ok();
}

/// A FILE that is removed between the walk listing it and the scan reading it is not missing
/// evidence: the walk covered the whole tree and the entry is genuinely gone. Calling such a
/// scan incomplete holds every deletion of the pair, and a temp file written and removed while
/// the walk runs (a build, an editor saving) is enough to do it.
#[test]
fn a_file_that_vanishes_after_the_walk_listed_it_keeps_the_scan_complete() {
	let root = temp_root();
	fs::write(root.join("keep.txt"), b"x").unwrap();
	fs::write(root.join("temp.txt"), b"y").unwrap();

	let baseline = HashMap::new();
	let scan = scan_watched(
		&root,
		&tree(&baseline),
		// Removed the moment the walker hands the entry over, before the scan stats it.
		&mut |path| {
			if path.ends_with("temp.txt") {
				fs::remove_file(path).unwrap();
			}
			ControlFlow::Continue(())
		},
	);

	assert!(
		scan.complete,
		"an entry the walk itself listed and that is gone now is not missing evidence: {:?}",
		scan.errors
	);
	assert!(
		scan.errors.is_empty(),
		"nothing to report either — it is not an error that a file was deleted: {:?}",
		scan.errors
	);
	assert_eq!(
		sorted_paths(&scan),
		vec!["keep.txt"],
		"what is gone is gone: it is absent, which is what the next pass acts on"
	);

	fs::remove_dir_all(&root).ok();
}

/// A directory the walk has gone into and that is something else by the time it is read was
/// caught mid-change: entries may have been listed under what it was, so neither a node at its
/// path nor its absence is evidence, and the scan is partial instead. Nothing is wrong with the
/// tree, so the read reports nothing. There are more of them than the walk stats inline, so the
/// workers' stats meet it too.
// Windows does not remove a directory the walker holds open.
#[cfg(unix)]
#[test]
fn a_directory_replaced_by_a_file_once_listed_leaves_the_scan_incomplete() {
	let root = temp_root();
	fs::write(root.join("keep.txt"), b"x").unwrap();
	let swapped = INLINE_STATS + 16;
	for i in 0..swapped {
		fs::create_dir(root.join(format!("swap{i:03}"))).unwrap();
	}

	let baseline = HashMap::new();
	// A name recreated while its directory is still being listed may be listed again, with its
	// new type (POSIX leaves that open): each one is swapped once.
	let mut done = HashSet::new();
	let scan = scan_watched(
		&root,
		&tree(&baseline),
		// The walker has opened the directory by the time it hands it over.
		&mut |path| {
			if path
				.file_name()
				.is_some_and(|name| name.to_string_lossy().starts_with("swap"))
				&& done.insert(path.to_owned())
			{
				fs::remove_dir(path).unwrap();
				fs::write(path, b"now a file").unwrap();
			}
			ControlFlow::Continue(())
		},
	);

	assert!(!scan.complete);
	assert!(scan.errors.is_empty(), "{:?}", scan.errors);
	assert_swapped_from(&scan, NodeKind::Dir);

	fs::remove_dir_all(&root).ok();
}

/// The mirror case: a file the walk listed, and so did not go into, that is a directory by the
/// time it is read had none of its entries listed. A node with nothing under it would read every
/// synced child as a deletion, so the scan is partial instead, and again reports nothing.
#[test]
fn a_file_replaced_by_a_directory_once_listed_leaves_the_scan_incomplete() {
	let root = temp_root();
	fs::write(root.join("keep.txt"), b"x").unwrap();
	let swapped = INLINE_STATS + 16;
	for i in 0..swapped {
		fs::write(root.join(format!("swap{i:03}")), b"a file").unwrap();
	}

	let baseline = HashMap::new();
	// Swapped once each, as above.
	let mut done = HashSet::new();
	let scan = scan_watched(&root, &tree(&baseline), &mut |path| {
		if path
			.file_name()
			.is_some_and(|name| name.to_string_lossy().starts_with("swap"))
			&& done.insert(path.to_owned())
		{
			fs::remove_file(path).unwrap();
			fs::create_dir(path).unwrap();
		}
		ControlFlow::Continue(())
	});

	assert!(!scan.complete);
	assert!(scan.errors.is_empty(), "{:?}", scan.errors);
	assert_swapped_from(&scan, NodeKind::File);

	fs::remove_dir_all(&root).ok();
}

/// An empty window over `root`, putting into a map.
fn map_window(root: &Path) -> Window<'_, MapSink> {
	Window {
		root,
		listed: VecDeque::new(),
		front: 0,
		sink: MapSink::with_capacity(1),
		errors: Vec::new(),
		complete: true,
		invalid_names: BTreeMap::new(),
	}
}

/// Settle the file at `root/name`, listed as `listed`, whose hash read failed as access denied, the
/// way a stat taken before the read leaves it: the window it went through.
fn settle_denied_read<'r>(
	root: &'r Path,
	name: &str,
	listed: &fs::Metadata,
) -> Window<'r, MapSink> {
	let read = restat_failed_read(
		reading(listed, |_, _| {
			Err(std::io::Error::from(ErrorKind::PermissionDenied))
		}),
		|| fs::symlink_metadata(root.join(name)),
	);
	let mut window = map_window(root);
	window
		.push(Listed::Entry {
			rel_path: name.to_owned(),
			listed_type: listed.file_type(),
			blocked_rule_file: false,
			read: Some(Ok(read)),
		})
		.unwrap();
	window
}

/// The case above with the swap between the stat and the hash read: the entry still reads as a
/// file, and opening it fails (as access denied on Windows). The entry is stat'd again then, and
/// taken as caught mid-change.
#[test]
fn a_file_whose_read_fails_once_it_became_a_directory_leaves_the_scan_incomplete() {
	let root = temp_root();
	let path = root.join("swap");
	fs::write(&path, b"a file").unwrap();
	let listed = fs::symlink_metadata(&path).unwrap();
	fs::remove_file(&path).unwrap();
	fs::create_dir(&path).unwrap();

	let window = settle_denied_read(&root, "swap", &listed);

	assert!(!window.complete);
	assert!(window.errors.is_empty(), "{:?}", window.errors);
	assert_eq!(window.sink.nodes.len(), 0);
	assert_eq!(window.sink.claims, Vec::<String>::new());

	fs::remove_dir_all(&root).ok();
}

/// The same failed read of an entry that is still a file is a real error, reported as one.
#[test]
fn a_file_whose_read_fails_while_still_a_file_is_an_io_error() {
	let root = temp_root();
	let path = root.join("denied");
	fs::write(&path, b"a file").unwrap();
	let listed = fs::symlink_metadata(&path).unwrap();

	let window = settle_denied_read(&root, "denied", &listed);

	assert!(!window.complete);
	assert!(
		matches!(
			window.errors.as_slice(),
			[ScanError::Io { rel_path, source }]
				if rel_path == "denied" && source.kind() == ErrorKind::PermissionDenied
		),
		"{:?}",
		window.errors
	);
	assert_eq!(window.sink.claims, vec!["denied".to_owned()]);

	fs::remove_dir_all(&root).ok();
}

/// A file the walk listed that is a symlink by the time it is read is neither a file nor a
/// directory to the read, which stats the entry itself. Dropped as unsyncable, it would read as
/// the file's deletion, so the scan is partial instead, and again reports nothing.
#[cfg(unix)]
#[test]
fn a_file_replaced_by_a_symlink_once_listed_leaves_the_scan_incomplete() {
	let root = temp_root();
	fs::write(root.join("keep.txt"), b"x").unwrap();
	let swapped = INLINE_STATS + 16;
	for i in 0..swapped {
		fs::write(root.join(format!("swap{i:03}")), b"a file").unwrap();
	}

	let baseline = HashMap::new();
	// Swapped once each, as above. A swapped name listed again is a link to `keep.txt`, and so a
	// file again.
	let mut done = HashSet::new();
	let scan = scan_watched(&root, &tree(&baseline), &mut |path| {
		if path
			.file_name()
			.is_some_and(|name| name.to_string_lossy().starts_with("swap"))
			&& done.insert(path.to_owned())
		{
			fs::remove_file(path).unwrap();
			std::os::unix::fs::symlink(root.join("keep.txt"), path).unwrap();
		}
		ControlFlow::Continue(())
	});

	assert!(!scan.complete);
	assert!(scan.errors.is_empty(), "{:?}", scan.errors);
	let nodes = sorted_entries(&scan.nodes.whole());
	assert!(
		nodes.iter().any(|(path, _)| path == "keep.txt"),
		"{nodes:?}"
	);

	fs::remove_dir_all(&root).ok();
}

/// What a swap test's scan keeps: `keep.txt`, and no node of the type a swapped entry was
/// listed with. A swapped name listed again with its new type may be a node of that type.
fn assert_swapped_from(scan: &LocalScan, listed: NodeKind) {
	let nodes = sorted_entries(&scan.nodes.whole());
	assert!(
		nodes.iter().any(|(path, _)| path == "keep.txt"),
		"{nodes:?}"
	);
	assert!(
		!nodes
			.iter()
			.any(|(path, node)| path.starts_with("swap") && node.kind == listed),
		"{nodes:?}"
	);
}

/// A directory the walk listed that is gone by the time it is read had its entries listed from
/// what it was: the scan is partial, and the read reports nothing, as for one replaced by a
/// file. There are more of them than the walk stats inline, so the workers' stats meet it too.
// Windows does not remove a directory the walker holds open.
#[cfg(unix)]
#[test]
fn a_directory_removed_once_listed_leaves_the_scan_incomplete() {
	let root = temp_root();
	fs::write(root.join("keep.txt"), b"x").unwrap();
	let removed = INLINE_STATS + 16;
	for i in 0..removed {
		fs::create_dir(root.join(format!("gone{i:03}"))).unwrap();
	}

	let baseline = HashMap::new();
	let scan = scan_watched(&root, &tree(&baseline), &mut |path| {
		if path
			.file_name()
			.is_some_and(|name| name.to_string_lossy().starts_with("gone"))
		{
			fs::remove_dir(path).unwrap();
		}
		ControlFlow::Continue(())
	});

	assert!(!scan.complete);
	assert!(scan.errors.is_empty(), "{:?}", scan.errors);
	assert_eq!(sorted_paths(&scan), vec!["keep.txt"]);

	fs::remove_dir_all(&root).ok();
}

/// A DIRECTORY the walk listed but could not descend into is the opposite case: its children
/// were never listed, so their absence is not something the walk observed. Trusting it reads
/// every file under a directory that was moved or removed mid-walk as a deletion — and the
/// destination, listed before the move, is not in the scan either, so the pass cannot see the
/// move. One held pass costs nothing; the next one folds it. Nothing is wrong with the tree, so
/// nothing is reported.
// Windows does not remove a directory the walker holds open.
#[cfg(unix)]
#[test]
fn a_directory_the_walk_could_not_descend_into_leaves_the_scan_incomplete() {
	let root = temp_root();
	fs::write(root.join("keep.txt"), b"x").unwrap();
	// The walk opens a directory as it hands the entry over, so the entry that vanishes
	// BETWEEN listing and descent is one of the siblings already buffered in an open stream.
	let outer = root.join("outer");
	fs::create_dir(&outer).unwrap();
	for i in 0..64 {
		let dir = outer.join(format!("d{i:02}"));
		fs::create_dir(&dir).unwrap();
		fs::write(dir.join("inner.txt"), b"z").unwrap();
	}

	let baseline = HashMap::new();
	let mut swept = false;
	let scan = scan_watched(
		&root,
		&tree(&baseline),
		// The first time a child of `outer` is listed, every child of `outer` goes — while the
		// walk still holds that directory's open stream and has yet to descend into any of them.
		&mut |path| {
			if !swept && path.parent() == Some(outer.as_path()) {
				swept = true;
				for child in fs::read_dir(&outer).unwrap() {
					fs::remove_dir_all(child.unwrap().path()).unwrap();
				}
			}
			ControlFlow::Continue(())
		},
	);

	assert!(
		!scan.complete,
		"the children of a directory the walk never descended into were not observed absent"
	);
	assert!(scan.errors.is_empty(), "{:?}", scan.errors);

	fs::remove_dir_all(&root).ok();
}

/// A file whose directory is replaced by a file once the walk listed it fails its read with
/// ENOTDIR: that directory changed under the walk, as one gone before the walk descends into it
/// does, so the scan is partial and nothing is reported. There are more of them than the walk
/// stats inline, so the workers' stats meet it too. Only the stat is met here: a hash read cannot
/// be made to fail that way from the outside, between the stat and the read.
// Windows does not remove a directory the walker holds open.
#[cfg(unix)]
#[test]
fn a_file_whose_directory_is_replaced_by_a_file_leaves_the_scan_incomplete() {
	let root = temp_root();
	fs::write(root.join("keep.txt"), b"x").unwrap();
	let swapped = INLINE_STATS + 16;
	for i in 0..swapped {
		let dir = root.join(format!("swap{i:03}"));
		fs::create_dir(&dir).unwrap();
		fs::write(dir.join("inner.txt"), b"z").unwrap();
	}

	let baseline = HashMap::new();
	// A name recreated while its directory is still being listed may be listed again: each
	// directory is swapped once.
	let mut done = HashSet::new();
	let scan = scan_watched(&root, &tree(&baseline), &mut |path| {
		if path.ends_with("inner.txt")
			&& let Some(dir) = path.parent()
			&& done.insert(dir.to_owned())
		{
			fs::remove_dir_all(dir).unwrap();
			fs::write(dir, b"now a file").unwrap();
		}
		ControlFlow::Continue(())
	});

	assert_eq!(done.len(), swapped);
	assert!(!scan.complete);
	assert!(scan.errors.is_empty(), "{:?}", scan.errors);
	assert!(
		!sorted_paths(&scan)
			.iter()
			.any(|path| path.ends_with("inner.txt")),
		"{:?}",
		sorted_paths(&scan)
	);

	fs::remove_dir_all(&root).ok();
}

/// `A.txt` and `a.txt` in `root`, two names that fold together; `None` where the filesystem
/// folds case and holds one file for the two.
///
/// No pair of names such a filesystem keeps apart folds together either, since it folds the
/// way the key does, so no walk there can meet a twin. The cases the twin tests guard are
/// reachable wherever the filesystem is case-sensitive, which is where a twin can exist at all.
#[cfg(unix)]
fn case_twins(root: &Path) -> Option<[std::path::PathBuf; 2]> {
	let (upper, lower) = ("A.txt", "a.txt");
	assert_eq!(
		collision_key(upper),
		collision_key(lower),
		"the fixture needs two names that fold together"
	);
	let twins = [root.join(upper), root.join(lower)];
	fs::write(&twins[0], b"x").unwrap();
	fs::write(&twins[1], b"y").unwrap();
	// Told apart by content, not by counting `root`'s entries: a caller may pass a directory
	// that holds more than the twins.
	(fs::read(&twins[0]).unwrap() != fs::read(&twins[1]).unwrap()).then_some(twins)
}

/// A twin whose first spelling claimed the folded name and then bailed — a file the scan could
/// not read — must still refuse the pass. The claim is what says the name is taken; forgetting
/// it lets the second spelling through as an ordinary new file, and since the server folds case
/// too, uploading it lands on top of the remote copy the first spelling is synced as.
#[cfg(unix)]
#[test]
fn a_twin_whose_first_spelling_could_not_be_read_is_still_a_collision() {
	use std::os::unix::fs::PermissionsExt;

	let root = temp_root();
	let Some([upper, lower]) = case_twins(&root) else {
		fs::remove_dir_all(&root).ok();
		return;
	};

	let baseline = HashMap::new();
	let mut blocked = false;
	let scan = scan_watched(
		&root,
		&tree(&baseline),
		// Whichever of the two the walk lists FIRST is made unreadable before the scan hashes
		// it, so it claims the folded name and then bails. Which one that is depends on the
		// directory's order, and the answer must not.
		&mut |path| {
			if !blocked && (path == upper || path == lower) {
				blocked = true;
				fs::set_permissions(path, fs::Permissions::from_mode(0o000)).unwrap();
			}
			ControlFlow::Continue(())
		},
	);

	assert!(
		scan.errors
			.iter()
			.any(|error| matches!(error, ScanError::DuplicateName { .. })),
		"the pass must still be refused over the collision: {:?}",
		scan.errors
	);
	assert!(
		sorted_paths(&scan).is_empty(),
		"and neither spelling may be planned: {:?}",
		sorted_paths(&scan)
	);

	fs::set_permissions(&upper, fs::Permissions::from_mode(0o644)).ok();
	fs::set_permissions(&lower, fs::Permissions::from_mode(0o644)).ok();
	fs::remove_dir_all(&root).ok();
}

/// A twin that vanishes once the walk has listed and stat'ed it, before its hash is read, is
/// gone, as any file that vanishes then is: the other spelling is a node, and the pass is not
/// refused over a twin that is no longer there. A name is taken once its entry is read, not
/// before its hash is.
#[cfg(unix)]
#[test]
fn a_twin_that_vanishes_before_its_hash_is_read_is_no_collision() {
	let root = temp_root();
	if case_twins(&root).is_none() {
		fs::remove_dir_all(&root).ok();
		return;
	}

	let mut asked: Vec<String> = Vec::new();
	let Ok((scan, _)) = scan_local_watched(
		ScanRoot {
			path: &root,
			pair_root: PairRoot::Directory,
		},
		"",
		&Baseline::default(),
		IgnoreRules::default(),
		&RuleFiles::Read,
		Hooks {
			on_listed: &mut |_| ControlFlow::Continue(()),
			stop: None,
			// Asked about each file between its stat and its hash read, with no baseline to
			// take a hash from: the second of the two is removed there.
			moved_hash: Some(&mut |rel_path, _, _| {
				asked.push(rel_path.to_owned());
				if asked.len() == 2 {
					fs::remove_file(root.join(rel_path)).unwrap();
				}
				None
			}),
			max_read_workers: MAX_READ_WORKERS,
		},
		MapSink::with_capacity(4),
	);

	let [first, _] = asked.as_slice() else {
		panic!("the walk reads both twins: {asked:?}")
	};
	assert!(scan.complete, "{:?}", scan.errors);
	assert!(scan.errors.is_empty(), "{:?}", scan.errors);
	assert_eq!(sorted_paths(&scan), vec![first.clone()]);

	fs::remove_dir_all(&root).ok();
}

/// A twin whose hash cannot be read, reached once the other spelling holds the name, reports
/// both what stopped the read and the collision: the pass is refused over the twin, and told
/// why that file was not read.
#[cfg(unix)]
#[test]
fn a_twin_whose_second_spelling_could_not_be_read_reports_the_read_and_the_collision() {
	use std::os::unix::fs::PermissionsExt;

	let root = temp_root();
	let Some(twins) = case_twins(&root) else {
		fs::remove_dir_all(&root).ok();
		return;
	};

	let baseline = HashMap::new();
	let mut listed: Vec<String> = Vec::new();
	let scan = scan_watched(
		&root,
		&tree(&baseline),
		// The second of the two the walk lists is made unreadable before the scan hashes it.
		&mut |path| {
			if twins.iter().any(|twin| twin == path) {
				let name = path.file_name().and_then(OsStr::to_str).unwrap();
				listed.push(name.to_owned());
				if listed.len() == 2 {
					fs::set_permissions(path, fs::Permissions::from_mode(0o000)).unwrap();
				}
			}
			ControlFlow::Continue(())
		},
	);

	for twin in &twins {
		fs::set_permissions(twin, fs::Permissions::from_mode(0o644)).ok();
	}
	let [first, second] = listed.as_slice() else {
		panic!("the walk lists both twins: {listed:?}")
	};
	assert!(!scan.complete);
	let [
		ScanError::Io {
			rel_path: unread, ..
		},
		ScanError::DuplicateName { rel_path: named },
	] = scan.errors.as_slice()
	else {
		panic!("the read error, then the collision: {:?}", scan.errors)
	};
	assert_eq!(unread, second);
	assert_eq!(named, &format!("{first} / {second}"));
	assert_eq!(sorted_paths(&scan), vec![first.clone()]);

	fs::remove_dir_all(&root).ok();
}

/// A root that goes away DURING the walk — a removed volume, an unreachable share — answers
/// `NotFound` for every entry it had already listed, which on its own is indistinguishable from
/// each of those entries being deleted. One stat after the walk tells the two apart; without it
/// the scan reports a complete observation of an empty tree, and every row of the pair reads as
/// a deletion.
// Windows does not remove a directory the walker holds open.
#[cfg(unix)]
#[test]
fn a_root_that_vanishes_during_the_walk_leaves_the_scan_incomplete() {
	let root = temp_root();
	// Flat on purpose: no directory to descend into, so the root check is the only thing that
	// can catch this.
	for name in ["a.txt", "b.txt", "c.txt"] {
		fs::write(root.join(name), b"x").unwrap();
	}

	let baseline = HashMap::new();
	let mut gone = false;
	let scan = scan_watched(&root, &tree(&baseline), &mut |path| {
		if !gone && path.parent() == Some(root.as_path()) {
			gone = true;
			fs::remove_dir_all(&root).unwrap();
		}
		ControlFlow::Continue(())
	});

	assert!(
		!scan.complete,
		"the root itself is gone: nothing the walk listed is evidence of a deletion"
	);
	assert!(
		matches!(
			scan.errors.as_slice(),
			[ScanError::Io { rel_path, source }]
				if rel_path.is_empty() && source.kind() == ErrorKind::NotFound
		),
		"a missing pair root is a removed volume, and is reported: {:?}",
		scan.errors
	);

	fs::remove_dir_all(&root).ok();
}

#[test]
fn a_name_collision_is_not_reported_as_an_error_line() {
	let scan = LocalScan {
		nodes: Side::default(),
		complete: false,
		errors: vec![
			ScanError::DuplicateName {
				rel_path: "A.txt / a.txt".to_string(),
			},
			ScanError::NonUtf8Name {
				lossy_path: "bad\u{FFFD}".to_string(),
			},
			ScanError::Io {
				rel_path: String::new(),
				source: std::io::Error::from(std::io::ErrorKind::NotFound),
			},
		],
		invalid_names: BTreeMap::new(),
		aliased_dirs: BTreeMap::new(),
		ignored: BTreeMap::new(),
		ignored_default_untracked: 0,
		ignore_blocked: BTreeSet::new(),
	};
	let reported: Vec<String> = scan.reported_errors().collect();
	assert_eq!(
		reported,
		vec![
			"local scan: bad\u{FFFD}: the name is not valid UTF-8".to_string(),
			format!(
				"local scan: local root: {}",
				std::io::Error::from(std::io::ErrorKind::NotFound)
			),
		],
		"a collision refuses the pass with its own line; everything else is reported"
	);
}

#[cfg(unix)]
#[test]
fn a_dangling_symlink_is_reported_and_a_loop_inside_the_root_is_an_alias_both_complete() {
	use std::os::unix::fs::symlink;

	let root = temp_root();
	fs::write(root.join("keep.txt"), b"x").unwrap();
	symlink(root.join("nowhere"), root.join("dangling")).unwrap();
	symlink(root.join("itself"), root.join("itself")).unwrap();
	fs::create_dir(root.join("cycle")).unwrap();
	symlink(root.join("cycle"), root.join("cycle").join("self")).unwrap();

	let scan = scan_plain(&root, &HashMap::new());
	assert!(
		scan.complete,
		"a link that leads nowhere hides nothing, so deletions must not be held for it: {:?}",
		scan.errors
	);
	let mut paths: Vec<String> = scan
		.nodes
		.whole()
		.paths()
		.map(|path| path.into_owned())
		.collect();
	paths.sort();
	assert_eq!(paths, vec!["cycle", "keep.txt"]);

	let mut errored: Vec<&str> = scan
		.errors
		.iter()
		.map(|e| match e {
			ScanError::Io { rel_path, .. } => rel_path.as_str(),
			other => panic!("unexpected scan error {other:?}"),
		})
		.collect();
	errored.sort_unstable();
	assert_eq!(errored, vec!["dangling", "itself"]);
	assert_eq!(scan.reported_errors().count(), 2);
	// The loop leads back onto a directory inside the root, so the alias rule skips it before
	// it is entered, and the loop check that would report it is never made.
	assert_eq!(
		scan.aliased_dirs,
		BTreeMap::from([("cycle/self".to_string(), "cycle".to_string())])
	);

	fs::remove_dir_all(&root).ok();
}

/// A loop the alias rule does not catch: through a link leading outside the root, back onto that
/// link's target. The walk finds it only once the scan has admitted the looping link, and the
/// link is neither recorded nor has its rule file read: as an empty directory it would be created
/// on the remote, and a synced one would read as emptied.
#[cfg(unix)]
#[test]
fn a_loop_through_a_link_leading_outside_the_root_is_reported_and_not_recorded() {
	use std::os::unix::fs::{PermissionsExt, symlink};

	let root = temp_root();
	let outside = temp_root();
	fs::create_dir(outside.join("o")).unwrap();
	fs::write(outside.join("o").join("f.txt"), b"f").unwrap();
	// Read for `away`, and for the loop too if the scan read it before finding the loop.
	fs::write(outside.join(FILENIGNORE), b"*.tmp").unwrap();
	fs::set_permissions(outside.join(FILENIGNORE), fs::Permissions::from_mode(0o000)).unwrap();
	symlink(&outside, outside.join("o").join("back")).unwrap();
	symlink(&outside, root.join("away")).unwrap();

	let unreadable = fs::read(outside.join(FILENIGNORE)).is_err();
	let scan = scan_plain(&root, &HashMap::new());
	fs::remove_dir_all(&root).ok();
	fs::remove_dir_all(&outside).ok();
	assert!(
		unreadable,
		"run as non-root: the case needs permissions enforced"
	);

	assert!(scan.complete, "{:?}", scan.errors);
	let mut paths: Vec<String> = scan
		.nodes
		.whole()
		.paths()
		.map(|path| path.into_owned())
		.collect();
	paths.sort();
	assert_eq!(paths, vec!["away", "away/o", "away/o/f.txt"]);
	assert_eq!(scan.ignore_blocked, BTreeSet::from(["away".to_string()]));
	match scan.errors.as_slice() {
		[
			ScanError::Io {
				rel_path: looped,
				source,
			},
			ScanError::Io {
				rel_path: rule_file,
				..
			},
		] => {
			assert_eq!(looped, "away/o/back");
			assert!(
				source.to_string().starts_with("symlink loops back onto "),
				"{source}"
			);
			assert_eq!(rule_file, &format!("away/{FILENIGNORE}"));
		}
		errors => panic!("expected the loop and one rule file, got {errors:?}"),
	}
}

#[cfg(unix)]
#[test]
fn a_symlink_to_a_directory_inside_the_root_is_scanned_once_under_its_real_path() {
	use std::os::unix::fs::symlink;

	let root = temp_root();
	let outside = temp_root();
	fs::write(outside.join("far.txt"), b"far").unwrap();
	fs::create_dir_all(root.join("real").join("sub")).unwrap();
	fs::write(root.join("real").join("a.txt"), b"a").unwrap();
	fs::write(root.join("real").join("sub").join("b.txt"), b"b").unwrap();
	// Links named to sort before and after their target, so walk order cannot decide the winner.
	symlink(root.join("real"), root.join("alias")).unwrap();
	symlink(root.join("real"), root.join("zlias")).unwrap();
	// Nested inside its own target's parent, and spelled relative.
	symlink("sub", root.join("real").join("inner")).unwrap();
	// A link to a file inside the root stays a copy; a link leading outside is walked.
	symlink(root.join("real").join("a.txt"), root.join("copy.txt")).unwrap();
	symlink(&outside, root.join("away")).unwrap();

	let scan = scan_plain(&root, &HashMap::new());
	assert!(scan.complete, "{:?}", scan.errors);
	assert!(scan.errors.is_empty(), "{:?}", scan.errors);
	let mut paths: Vec<String> = scan
		.nodes
		.whole()
		.paths()
		.map(|path| path.into_owned())
		.collect();
	paths.sort();
	assert_eq!(
		paths,
		vec![
			"away",
			"away/far.txt",
			"copy.txt",
			"real",
			"real/a.txt",
			"real/sub",
			"real/sub/b.txt",
		]
	);
	assert_eq!(
		scan.aliased_dirs,
		BTreeMap::from([
			("alias".to_string(), "real".to_string()),
			("real/inner".to_string(), "real/sub".to_string()),
			("zlias".to_string(), "real".to_string()),
		])
	);
	let mut blocked: Vec<&String> = scan
		.invalid_names
		.keys()
		.chain(scan.aliased_dirs.keys())
		.collect();
	blocked.sort();
	assert_eq!(blocked, vec!["alias", "real/inner", "zlias"]);

	fs::remove_dir_all(&root).ok();
	fs::remove_dir_all(&outside).ok();
}

/// A link synced as a copy of the file it pointed at, whose target is then deleted, is a
/// dangling link to a FILE: reported, but the scan stays complete, so the target's deletion (and
/// every other one) is not held for as long as the link exists.
#[cfg(unix)]
#[test]
fn a_dangling_file_symlink_the_baseline_tracks_keeps_the_scan_complete() {
	use std::os::unix::fs::symlink;

	let root = temp_root();
	symlink(root.join("target.txt"), root.join("link.txt")).unwrap();
	let file_row = |rel_path: &str| BaselineEntry {
		rel_path: rel_path.to_string(),
		kind: NodeKind::File,
		remote_uuid: None,
		content_hash: None,
		size: Some(1),
		local_mtime: Some(1),
		remote_modified: None,
		state: BaselineState::Synced,
		local_kind: None,
		remote_kind: None,
		remote_hash: None,
		remote_size: None,
		remote_stable_uuid: None,
		agreed_hash: None,
	};
	let baseline = HashMap::from([
		("link.txt".to_string(), file_row("link.txt")),
		("target.txt".to_string(), file_row("target.txt")),
	]);

	let scan = scan_plain(&root, &baseline);
	assert!(
		scan.complete,
		"a dead link to a synced file must not hold every deletion: {:?}",
		scan.errors
	);
	assert_eq!(scan.reported_errors().count(), 1, "{:?}", scan.errors);
	assert!(
		scan.nodes.whole().is_empty(),
		"{:?}",
		scan.nodes.whole().paths().collect::<Vec<_>>()
	);

	fs::remove_dir_all(&root).ok();
}

#[cfg(unix)]
#[test]
fn a_dangling_symlink_the_baseline_tracks_marks_the_scan_incomplete() {
	use std::os::unix::fs::symlink;

	let root = temp_root();
	// A link to a directory that was synced through it, and is now gone (an unmounted drive).
	symlink(root.join("unmounted"), root.join("media")).unwrap();
	let baseline = HashMap::from([(
		"media".to_string(),
		BaselineEntry {
			rel_path: "media".to_string(),
			kind: NodeKind::Dir,
			remote_uuid: None,
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
		},
	)]);

	let scan = scan_plain(&root, &baseline);
	assert!(
		!scan.complete,
		"what was synced behind the link must not read as deleted"
	);
	assert_eq!(scan.reported_errors().count(), 1, "{:?}", scan.errors);

	fs::remove_dir_all(&root).ok();
}

#[test]
fn an_ignored_entry_is_pruned_and_reported_top_most() {
	let root = temp_root();
	fs::write(root.join(FILENIGNORE), "build/\n*.log\n").unwrap();
	fs::create_dir_all(root.join("build").join("deep")).unwrap();
	fs::write(root.join("build").join("deep").join("a.txt"), b"a").unwrap();
	fs::write(root.join("build").join("b.log"), b"b").unwrap();
	fs::write(root.join("x.log"), b"x").unwrap();
	fs::write(root.join("keep.txt"), b"k").unwrap();
	fs::create_dir_all(root.join("sub").join("tmp")).unwrap();
	fs::write(root.join("sub").join(FILENIGNORE), "/tmp\n").unwrap();
	fs::write(root.join("sub").join("tmp").join("t.txt"), b"t").unwrap();
	fs::write(root.join("sub").join(".DS_Store"), b"junk").unwrap();
	fs::create_dir(root.join("tmp")).unwrap();

	let (scan, rules) = scan_local(
		ScanRoot {
			path: &root,
			pair_root: PairRoot::Directory,
		},
		&Baseline::default(),
		IgnoreRules::default(),
		RuleFiles::Read,
	);
	assert!(scan.complete, "{:?}", scan.errors);
	assert!(scan.errors.is_empty(), "{:?}", scan.errors);
	assert_eq!(
		sorted_paths(&scan),
		vec![".filenignore", "keep.txt", "sub", "sub/.filenignore", "tmp"]
	);
	// `build/b.log` matches `*.log` too, but the walk never went into `build`.
	let by = |level: IgnoreLevel, pattern: &str| IgnoreDecision {
		level,
		pattern: pattern.to_string(),
	};
	let root_file = |pattern: &str| by(IgnoreLevel::File { dir: String::new() }, pattern);
	assert_eq!(
		scan.ignored,
		BTreeMap::from([
			("build".to_string(), root_file("build/")),
			(
				"sub/tmp".to_string(),
				by(
					IgnoreLevel::File {
						dir: "sub".to_string()
					},
					"/tmp"
				)
			),
			("x.log".to_string(), root_file("*.log")),
		])
	);
	// `sub/.DS_Store` is hidden and pruned like the rest, but only the built-in defaults hide it
	// and this baseline tracks nothing there, so it is counted rather than carried as a root.
	assert_eq!(scan.ignored_default_untracked, 1);
	assert!(scan.ignore_blocked.is_empty());
	assert!(
		rules.decide("sub/tmp", true).is_some(),
		"the rules come back holding the files the scan read"
	);

	// When the remote copies are the ones that count, the files on disk are ordinary items.
	let (skipped, rules) = scan_local(
		ScanRoot {
			path: &root,
			pair_root: PairRoot::Directory,
		},
		&Baseline::default(),
		IgnoreRules::default(),
		RuleFiles::Only(BTreeSet::new()),
	);
	assert!(skipped.nodes.whole().holds("build/deep/a.txt"));
	// With no rule file read, the built-in defaults are all that hides anything, and nothing was
	// synced at the one entry they hide.
	assert!(
		skipped.ignored.is_empty(),
		"{:?} is hidden but untracked, so it is no root",
		skipped.ignored
	);
	assert_eq!(skipped.ignored_default_untracked, 1);
	assert!(rules.decide("x.log", false).is_none());
	// Except the ones named: a synced rule file the remote has lost still governs.
	let (only, rules) = scan_local(
		ScanRoot {
			path: &root,
			pair_root: PairRoot::Directory,
		},
		&Baseline::default(),
		IgnoreRules::default(),
		RuleFiles::Only(BTreeSet::from(["sub".to_string()])),
	);
	assert!(only.nodes.whole().holds("build/deep/a.txt"));
	assert_eq!(only.ignored.keys().collect::<Vec<_>>(), vec!["sub/tmp"]);
	assert_eq!(only.ignored_default_untracked, 1);
	assert!(rules.decide("x.log", false).is_none());

	fs::remove_dir_all(&root).ok();
}

/// A device-wide user rule that hides every dot file except the root's rule file still hides the
/// nested one from syncing, but not from being READ: hiding a rule file is about whether it syncs,
/// and its directory's rules apply either way.
#[test]
fn a_user_level_dotfile_rule_still_reads_rule_files() {
	let root = temp_root();
	fs::write(root.join(FILENIGNORE), "*.tmp\n").unwrap();
	fs::write(root.join(".a"), b"a").unwrap();
	fs::write(root.join("x.tmp"), b"x").unwrap();
	fs::create_dir(root.join("d")).unwrap();
	fs::write(root.join("d").join(FILENIGNORE), "*.log\n").unwrap();
	fs::write(root.join("d").join(".b"), b"b").unwrap();
	fs::write(root.join("d").join("y.log"), b"y").unwrap();
	fs::write(root.join("d").join("keep.txt"), b"k").unwrap();

	let user = parse_user_ignore(".*\n!/.filenignore").unwrap();
	let (scan, rules) = scan_local(
		ScanRoot {
			path: &root,
			pair_root: PairRoot::Directory,
		},
		&Baseline::default(),
		IgnoreRules::new(Some(user)),
		RuleFiles::Read,
	);
	assert!(scan.complete, "{:?}", scan.errors);
	assert!(scan.errors.is_empty(), "{:?}", scan.errors);
	assert_eq!(sorted_paths(&scan), vec![".filenignore", "d", "d/keep.txt"]);
	let by = |level: IgnoreLevel, pattern: &str| IgnoreDecision {
		level,
		pattern: pattern.to_string(),
	};
	assert_eq!(
		scan.ignored,
		BTreeMap::from([
			(".a".to_string(), by(IgnoreLevel::User, ".*")),
			("d/.b".to_string(), by(IgnoreLevel::User, ".*")),
			("d/.filenignore".to_string(), by(IgnoreLevel::User, ".*")),
			(
				"d/y.log".to_string(),
				by(
					IgnoreLevel::File {
						dir: "d".to_string()
					},
					"*.log"
				)
			),
			(
				"x.tmp".to_string(),
				by(IgnoreLevel::File { dir: String::new() }, "*.tmp")
			),
		])
	);
	assert!(scan.ignore_blocked.is_empty());
	assert!(
		rules.decide("d/z.log", false).is_some(),
		"the hidden rule file's rules apply"
	);

	fs::remove_dir_all(&root).ok();
}

#[test]
fn ignored_case_twins_do_not_refuse_the_pass() {
	let root = temp_root();
	fs::write(root.join("Twin.txt"), b"a").unwrap();
	fs::write(root.join("twin.txt"), b"b").unwrap();
	// A case-insensitive volume (the macOS and Windows defaults) keeps one file under both
	// names, and there is nothing to collide.
	if fs::read_dir(&root).unwrap().count() == 2 {
		let unruled = scan_plain(&root, &HashMap::new());
		assert!(!unruled.complete, "without a rule the twins collide");

		fs::write(root.join(FILENIGNORE), "TWIN.TXT\n").unwrap();
		let scan = scan_plain(&root, &HashMap::new());
		assert!(scan.complete, "{:?}", scan.errors);
		assert!(scan.errors.is_empty(), "{:?}", scan.errors);
		assert_eq!(sorted_paths(&scan), vec![FILENIGNORE]);
		assert_eq!(
			scan.ignored.keys().collect::<Vec<_>>(),
			vec!["Twin.txt", "twin.txt"]
		);
	}
	fs::remove_dir_all(&root).ok();
}

#[cfg(unix)]
#[test]
fn an_unusable_filenignore_blocks_its_directory_and_keeps_the_scan_complete() {
	use std::os::unix::fs::PermissionsExt;

	let root = temp_root();
	let locked = root.join("locked");
	fs::create_dir(&locked).unwrap();
	fs::write(locked.join(FILENIGNORE), "*\n").unwrap();
	fs::write(locked.join("a.txt"), b"a").unwrap();
	fs::set_permissions(locked.join(FILENIGNORE), fs::Permissions::from_mode(0o000)).unwrap();
	// One bad line: reported, and the rest of the file still applies.
	fs::create_dir(root.join("bad")).unwrap();
	fs::write(root.join("bad").join(FILENIGNORE), "[z-a]\n*.o\n").unwrap();
	fs::write(root.join("bad").join("x.o"), b"o").unwrap();

	let scan = scan_plain(&root, &HashMap::new());
	fs::set_permissions(locked.join(FILENIGNORE), fs::Permissions::from_mode(0o644)).unwrap();

	assert!(
		scan.complete,
		"what the rules would cover was withheld, not missed: {:?}",
		scan.errors
	);
	assert_eq!(scan.ignore_blocked, BTreeSet::from(["locked".to_string()]));
	assert!(
		scan.nodes.whole().holds("locked/a.txt"),
		"{:?}",
		scan.nodes.whole().paths().collect::<Vec<_>>()
	);
	assert_eq!(scan.ignored.keys().collect::<Vec<_>>(), vec!["bad/x.o"]);
	let reported: Vec<String> = scan.reported_errors().collect();
	assert_eq!(reported.len(), 2, "{reported:?}");
	assert!(
		reported
			.iter()
			.any(|line| line.starts_with("local scan: locked/.filenignore: ")),
		"{reported:?}"
	);
	assert!(
		reported
			.iter()
			.any(|line| line.starts_with("local scan: bad/.filenignore:1: ")),
		"{reported:?}"
	);

	fs::remove_dir_all(&root).ok();
}

/// A body the remote side would refuse (over the size cap, or not UTF-8) blocks its directory on
/// disk too, so the device that wrote it sees the same error every other device does.
#[test]
fn a_rule_file_the_remote_would_refuse_blocks_its_directory_on_disk_too() {
	let root = temp_root();
	fs::create_dir(root.join("latin")).unwrap();
	fs::write(root.join("latin").join(FILENIGNORE), b"caf\xe9/\n").unwrap();
	fs::write(root.join("latin").join("a.txt"), b"a").unwrap();
	fs::create_dir(root.join("huge")).unwrap();
	let too_large = usize::try_from(MAX_RULE_FILE_BYTES).unwrap() + 1;
	fs::write(root.join("huge").join(FILENIGNORE), vec![b'#'; too_large]).unwrap();

	let scan = scan_plain(&root, &HashMap::new());
	assert!(scan.complete, "{:?}", scan.errors);
	assert_eq!(
		scan.ignore_blocked,
		BTreeSet::from(["huge".to_string(), "latin".to_string()])
	);
	let reported: Vec<String> = scan.reported_errors().collect();
	assert_eq!(reported.len(), 2, "{reported:?}");
	for dir in ["huge", "latin"] {
		let prefix = format!("local scan: {dir}/.filenignore: ");
		assert!(
			reported.iter().any(|line| line.starts_with(&prefix)),
			"{prefix}: {reported:?}"
		);
	}

	fs::remove_dir_all(&root).ok();
}

/// Only a FILE named exactly `.filenignore` holds rules. A directory of that name is an ordinary
/// item (Windows reports reading one as access denied, not as a directory), and another spelling is
/// an ordinary file even where the volume opens it under the rule file's name, as the remote and a
/// case-sensitive device read it.
#[test]
fn only_a_file_named_exactly_filenignore_holds_rules() {
	let root = temp_root();
	fs::create_dir_all(root.join("docs").join(FILENIGNORE)).unwrap();
	fs::write(root.join("docs").join(FILENIGNORE).join("x.txt"), b"x").unwrap();
	fs::create_dir(root.join("proj")).unwrap();
	fs::write(root.join("proj").join(".FilenIgnore"), b"secret\n").unwrap();
	fs::write(root.join("proj").join("secret"), b"s").unwrap();

	let scan = scan_plain(&root, &HashMap::new());
	assert!(scan.complete, "{:?}", scan.errors);
	assert!(scan.errors.is_empty(), "{:?}", scan.errors);
	assert!(scan.ignore_blocked.is_empty(), "{:?}", scan.ignore_blocked);
	assert!(scan.ignored.is_empty(), "{:?}", scan.ignored);
	assert_eq!(
		sorted_paths(&scan),
		vec![
			"docs",
			"docs/.filenignore",
			"docs/.filenignore/x.txt",
			"proj",
			"proj/.FilenIgnore",
			"proj/secret"
		]
	);

	fs::remove_dir_all(&root).ok();
}

#[test]
fn a_whitelist_inside_an_ignored_directory_has_no_effect() {
	let root = temp_root();
	fs::write(root.join(FILENIGNORE), "dir/\n!dir/keep\n").unwrap();
	fs::create_dir(root.join("dir")).unwrap();
	fs::write(root.join("dir").join(FILENIGNORE), "!keep\nother\n").unwrap();
	fs::write(root.join("dir").join("keep"), b"k").unwrap();

	let (scan, rules) = scan_local(
		ScanRoot {
			path: &root,
			pair_root: PairRoot::Directory,
		},
		&Baseline::default(),
		IgnoreRules::default(),
		RuleFiles::Read,
	);
	assert_eq!(sorted_paths(&scan), vec![FILENIGNORE]);
	assert_eq!(scan.ignored.keys().collect::<Vec<_>>(), vec!["dir"]);
	assert!(
		rules.decide("dir/other", false).is_none(),
		"the file inside the ignored directory is never read"
	);

	fs::remove_dir_all(&root).ok();
}

#[test]
fn the_engines_own_files_stay_outside_every_rule() {
	let root = temp_root();
	fs::write(root.join(FILENIGNORE), "!.filen-sync-trash/\n*.filendl\n").unwrap();
	fs::create_dir(root.join(QUARANTINE_DIR)).unwrap();
	fs::write(root.join(QUARANTINE_DIR).join("old.txt"), b"old").unwrap();
	fs::write(root.join("dee76e0e.filendl"), b"half").unwrap();

	let scan = scan_plain(&root, &HashMap::new());
	assert_eq!(sorted_paths(&scan), vec![FILENIGNORE]);
	assert!(
		scan.ignored.is_empty(),
		"an internal file is not an ignored item: {:?}",
		scan.ignored
	);

	fs::remove_dir_all(&root).ok();
}

/// filen-sync's trash folder is never read, at the root or nested, whatever a rule says — but only
/// an entry named exactly that: a longer name or another case is the user's.
#[test]
fn the_scan_skips_filen_syncs_trash_at_any_depth_and_only_by_its_exact_name() {
	let root = temp_root();
	fs::write(root.join(FILENIGNORE), "!.filen.trash.local/\n").unwrap();
	fs::create_dir(root.join(LEGACY_TRASH_DIR)).unwrap();
	fs::write(root.join(LEGACY_TRASH_DIR).join("old.txt"), b"old").unwrap();
	let nested = root.join("Photos").join(LEGACY_TRASH_DIR);
	fs::create_dir_all(nested.join("deep")).unwrap();
	fs::write(nested.join("deep").join("x.jpg"), b"x").unwrap();
	// A file with the folder's name is reserved too: the name is, whatever it names.
	fs::create_dir(root.join("docs")).unwrap();
	fs::write(root.join("docs").join(LEGACY_TRASH_DIR), b"f").unwrap();
	fs::write(root.join(".filen.trash.local.txt"), b"mine").unwrap();
	// In a directory of its own: a case-insensitive file system would take it for the one above.
	fs::create_dir_all(root.join("other").join(".Filen.Trash.Local")).unwrap();
	fs::write(
		root.join("other").join(".Filen.Trash.Local").join("y.txt"),
		b"y",
	)
	.unwrap();

	let scan = scan_plain(&root, &HashMap::new());
	assert!(scan.complete, "{:?}", scan.errors);
	assert_eq!(
		sorted_paths(&scan),
		vec![
			".filen.trash.local.txt",
			FILENIGNORE,
			"Photos",
			"docs",
			"other",
			"other/.Filen.Trash.Local",
			"other/.Filen.Trash.Local/y.txt",
		]
	);
	assert!(
		scan.ignored.is_empty(),
		"filen-sync's trash is not an ignored item: {:?}",
		scan.ignored
	);

	fs::remove_dir_all(&root).ok();
}

/// The account's data directory is a top-level name, matched case-insensitively, of a pair rooted
/// at the drive only.
#[test]
fn only_the_top_of_a_drive_rooted_pair_holds_the_account_data_directory() {
	for path in [".filen", ".filen/chats/a.bin", ".FILEN", ".Filen/x"] {
		assert!(is_account_data(path, PairRoot::Drive), "{path}");
		assert!(!is_account_data(path, PairRoot::Directory), "{path}");
	}
	for path in [
		".filen.txt",
		".filenx/a",
		"docs/.filen",
		"docs/.filen/a",
		"",
	] {
		assert!(!is_account_data(path, PairRoot::Drive), "{path}");
	}
}

/// A drive-rooted pair's scan never reads the account's data directory, whatever a rule says, nor
/// reports it ignored. A `.filen` deeper down, a longer name, and the same tree under a pair rooted
/// at a directory are the user's.
#[test]
fn a_drive_rooted_scan_skips_the_account_data_directory_at_its_top_only() {
	let root = temp_root();
	fs::write(root.join(FILENIGNORE), "!.filen/\n").unwrap();
	fs::create_dir_all(root.join(ACCOUNT_DATA_DIR).join("chats")).unwrap();
	fs::write(
		root.join(ACCOUNT_DATA_DIR).join("chats").join("a.bin"),
		b"a",
	)
	.unwrap();
	fs::create_dir_all(root.join("Photos").join(ACCOUNT_DATA_DIR)).unwrap();
	fs::write(
		root.join("Photos").join(ACCOUNT_DATA_DIR).join("x.jpg"),
		b"x",
	)
	.unwrap();
	fs::write(root.join(".filen.txt"), b"mine").unwrap();
	let scan = |pair_root| {
		scan_local(
			ScanRoot {
				path: &root,
				pair_root,
			},
			&Baseline::default(),
			IgnoreRules::default(),
			RuleFiles::Read,
		)
		.0
	};

	let drive = scan(PairRoot::Drive);
	assert!(drive.complete, "{:?}", drive.errors);
	assert_eq!(
		sorted_paths(&drive),
		vec![
			".filen.txt",
			FILENIGNORE,
			"Photos",
			"Photos/.filen",
			"Photos/.filen/x.jpg",
		]
	);
	assert!(
		drive.ignored.is_empty(),
		"the account's data directory is not an ignored item: {:?}",
		drive.ignored
	);
	assert_eq!(
		sorted_paths(&scan(PairRoot::Directory)),
		vec![
			".filen",
			".filen.txt",
			".filen/chats",
			".filen/chats/a.bin",
			FILENIGNORE,
			"Photos",
			"Photos/.filen",
			"Photos/.filen/x.jpg",
		]
	);

	fs::remove_dir_all(&root).ok();
}

/// A case variant of the name is the account's directory too: the drive root's names are deduped
/// case-insensitively, so it could only ever be merged into that directory.
#[test]
fn a_drive_rooted_scan_skips_a_case_variant_of_the_account_data_directory() {
	let root = temp_root();
	fs::create_dir(root.join(".Filen")).unwrap();
	fs::write(root.join(".Filen").join("y.txt"), b"y").unwrap();
	fs::write(root.join("kept.txt"), b"k").unwrap();
	let (scan, _) = scan_local(
		ScanRoot {
			path: &root,
			pair_root: PairRoot::Drive,
		},
		&Baseline::default(),
		IgnoreRules::default(),
		RuleFiles::Read,
	);
	assert_eq!(sorted_paths(&scan), vec!["kept.txt"]);

	fs::remove_dir_all(&root).ok();
}

/// A subtree walk from the root of a drive-rooted pair leaves the account's data directory out as
/// a whole walk does.
#[test]
fn a_subtree_walk_from_a_drive_rooted_pairs_root_skips_the_account_data_directory() {
	let root = temp_root();
	fs::create_dir(root.join(ACCOUNT_DATA_DIR)).unwrap();
	fs::write(root.join(ACCOUNT_DATA_DIR).join("a.bin"), b"a").unwrap();
	fs::write(root.join("kept.txt"), b"k").unwrap();
	let (scan, _) = scan_subtree(
		ScanRoot {
			path: &root,
			pair_root: PairRoot::Drive,
		},
		"",
		&Baseline::default(),
		IgnoreRules::default(),
		&RuleFiles::Read,
		&mut |_, _, _| None,
		&|_| false,
	)
	.unwrap();
	assert_eq!(sorted_paths(&scan), vec!["kept.txt"]);

	fs::remove_dir_all(&root).ok();
}

/// A subtree walk keys against the pair ROOT and starts at its own directory. The rules that go
/// by depth still mean the root: the quarantine bin is the root's own child, so a user directory
/// of that name nested deeper is an ordinary item.
#[test]
fn a_subtree_walk_keys_against_the_root_and_starts_at_its_own_directory() {
	let root = temp_root();
	fs::write(root.join("top.txt"), b"t").unwrap();
	fs::create_dir(root.join(QUARANTINE_DIR)).unwrap();
	fs::write(root.join(QUARANTINE_DIR).join("trashed.txt"), b"old").unwrap();
	let nested = root.join("sub").join(QUARANTINE_DIR);
	fs::create_dir_all(&nested).unwrap();
	fs::write(nested.join("mine.txt"), b"m").unwrap();

	let (scan, _) = scan_subtree(
		ScanRoot {
			path: &root,
			pair_root: PairRoot::Directory,
		},
		"sub",
		&Baseline::default(),
		IgnoreRules::default(),
		&RuleFiles::Read,
		&mut |_, _, _| None,
		&|_| false,
	)
	.expect("a walk nothing stops");

	assert!(scan.complete, "{:?}", scan.errors);
	assert_eq!(
		sorted_paths(&scan),
		vec![
			"sub",
			"sub/.filen-sync-trash",
			"sub/.filen-sync-trash/mine.txt"
		],
		"the start directory is a node, its keys are root-relative, and only the ROOT's own \
			 quarantine bin is the engine's"
	);

	fs::remove_dir_all(&root).ok();
}

/// A subtree walk whose start is removed once the walk has opened it finds every entry it lists
/// gone and the start gone after it. That start is a directory below the root that changed under
/// the walk, as any other is: the scan is partial and nothing is reported.
// Windows does not remove a directory the walker holds open.
#[cfg(unix)]
#[test]
fn a_subtree_start_removed_during_the_walk_leaves_the_scan_incomplete_and_unreported() {
	let root = temp_root();
	let start = root.join("sub");
	fs::create_dir(&start).unwrap();
	for name in ["a.txt", "b.txt", "c.txt"] {
		fs::write(start.join(name), b"x").unwrap();
	}

	let (scan, _) = scan_subtree(
		ScanRoot {
			path: &root,
			pair_root: PairRoot::Directory,
		},
		"sub",
		&Baseline::default(),
		IgnoreRules::default(),
		&RuleFiles::Read,
		&mut |_, _, _| None,
		&|listed| {
			if listed == 1 {
				fs::remove_dir_all(&start).unwrap();
			}
			false
		},
	)
	.expect("a walk nothing stops");

	assert!(!start.exists());
	assert!(
		!scan.complete,
		"what the walk listed under a start that is gone is evidence of nothing"
	);
	assert!(scan.errors.is_empty(), "{:?}", scan.errors);

	fs::remove_dir_all(&root).ok();
}

/// A subtree start removed after the caller stat'd it and before the walk read it is the same
/// change caught a moment earlier: the walk's first read answers `NotFound` at the start, and
/// the scan is partial with nothing reported.
#[test]
fn a_subtree_start_gone_before_the_walk_reads_it_leaves_the_scan_incomplete_and_unreported() {
	let root = temp_root();

	let (scan, _) = scan_subtree(
		ScanRoot {
			path: &root,
			pair_root: PairRoot::Directory,
		},
		"sub",
		&Baseline::default(),
		IgnoreRules::default(),
		&RuleFiles::Read,
		&mut |_, _, _| None,
		&|_| false,
	)
	.expect("a walk nothing stops");

	assert!(
		!scan.complete,
		"a start that is gone lists nothing that is evidence of a deletion"
	);
	assert!(scan.errors.is_empty(), "{:?}", scan.errors);

	fs::remove_dir_all(&root).ok();
}

/// A subtree walk is sized to the walk and not to the pair: the pair's row count is what the
/// WHOLE tree holds, and a table reserved for it around one directory's entries is the tree's
/// worth of memory spent on a fraction of it.
#[test]
fn a_subtree_walk_is_sized_to_the_walk_and_a_whole_one_to_the_pair() {
	let root = temp_root();
	fs::create_dir(root.join("sub")).unwrap();
	fs::write(root.join("sub").join("a.txt"), b"a").unwrap();
	let rows = 4096;
	let baseline = Baseline::from_rows((0..rows).map(|at| BaselineEntry {
		rel_path: format!("elsewhere/{at}.txt"),
		kind: NodeKind::File,
		remote_uuid: None,
		content_hash: Some(Blake3Hash::from([1; 32])),
		size: Some(1),
		local_mtime: Some(1),
		remote_modified: None,
		state: BaselineState::Synced,
		local_kind: None,
		remote_kind: None,
		remote_hash: None,
		remote_size: None,
		remote_stable_uuid: None,
		agreed_hash: None,
	}));
	assert_eq!(baseline.len(), rows);

	let (subtree, _) = scan_subtree(
		ScanRoot {
			path: &root,
			pair_root: PairRoot::Directory,
		},
		"sub",
		&baseline,
		IgnoreRules::default(),
		&RuleFiles::Read,
		&mut |_, _, _| None,
		&|_| false,
	)
	.expect("a walk nothing stops");
	let whole = scan_local(
		ScanRoot {
			path: &root,
			pair_root: PairRoot::Directory,
		},
		&baseline,
		IgnoreRules::default(),
		RuleFiles::Read,
	)
	.0;

	assert_eq!(sorted_paths(&subtree), vec!["sub", "sub/a.txt"]);
	let subtree_room = subtree.nodes.into_whole().capacity();
	assert!(
		subtree_room < rows,
		"a walk of one directory reserved room for {subtree_room} node(s), the {rows}-row pair's \
			 worth"
	);
	let whole_room = whole.nodes.into_whole().capacity();
	assert!(
		whole_room >= rows,
		"the whole-tree walk still starts with room for the pair: {whole_room}"
	);

	fs::remove_dir_all(&root).ok();
}

/// A whole walk into the pass's stage finds what a walk into a map finds — the nodes, every fact
/// the walk reports and every error — over generated trees: nested directories, a non-ASCII
/// name, a name the remote rejects, entries a `.filenignore` hides, a fast path that takes a
/// hash off the baseline rather than the file, and, where a file can be made unreadable, one
/// whose hash cannot be read: a claim, which neither side holds as a node.
///
/// Collisions are the sinks' own test (`stage::local`): a case-insensitive volume — this one, on
/// macOS — cannot hold the twins a walk would have to meet.
#[test]
fn a_whole_walk_into_the_stage_finds_what_a_walk_into_a_map_finds() {
	// No two of these fold together, so any volume holds every one of them.
	const NAMES: &[&str] = &[
		"a",
		"b",
		"docs",
		"notes.md",
		"x.log",
		"\u{c4}rger",
		" lead",
		" bad",
		"z.txt",
		"keep",
	];
	#[cfg(unix)]
	let mut unreadable = 0usize;
	for seed in 0..16u64 {
		let mut rng = StdRng::seed_from_u64(seed);
		let root = temp_root();
		let mut dirs = vec![root.clone()];
		let mut files = Vec::new();
		for _ in 0..rng.random_range(8..40) {
			let parent = dirs[rng.random_range(0..dirs.len())].clone();
			let path = parent.join(NAMES[rng.random_range(0..NAMES.len())]);
			if path.exists() {
				continue;
			}
			if rng.random_range(0..3) == 0 {
				fs::create_dir(&path).unwrap();
				dirs.push(path);
			} else {
				let size = rng.random_range(0..64);
				fs::write(&path, vec![b'x'; size]).unwrap();
				files.push(path);
			}
		}
		if rng.random_range(0..2) == 0 {
			fs::write(root.join(FILENIGNORE), "*.log\n").unwrap();
		}
		// Half the cases take every hash the baseline vouches for off the baseline: a hash
		// no file has, so that only the fast path can have produced it.
		let baseline = if rng.random_range(0..2) == 0 {
			Baseline::default()
		} else {
			let walked = scan_local(
				ScanRoot {
					path: &root,
					pair_root: PairRoot::Directory,
				},
				&Baseline::default(),
				IgnoreRules::default(),
				RuleFiles::Read,
			)
			.0;
			Baseline::from_rows(sorted_entries(&walked.nodes.whole()).into_iter().map(
				|(rel_path, node)| BaselineEntry {
					rel_path,
					kind: node.kind,
					remote_uuid: None,
					content_hash: Some(Blake3Hash::from([0xAB; 32])),
					size: Some(node.size),
					local_mtime: Some(node.mtime_millis),
					remote_modified: None,
					state: BaselineState::Synced,
					local_kind: None,
					remote_kind: None,
					remote_hash: None,
					remote_size: None,
					remote_stable_uuid: None,
					agreed_hash: None,
				},
			))
		};
		#[cfg(unix)]
		let locked = (!files.is_empty() && rng.random_range(0..2) == 0).then(|| {
			use std::os::unix::fs::PermissionsExt;
			let file = files[rng.random_range(0..files.len())].clone();
			fs::set_permissions(&file, fs::Permissions::from_mode(0o000)).unwrap();
			file
		});

		let (map, _) = scan_local(
			ScanRoot {
				path: &root,
				pair_root: PairRoot::Directory,
			},
			&baseline,
			IgnoreRules::default(),
			RuleFiles::Read,
		);
		let (_dir, stage) = Stage::in_temp_dir();
		let (staged, _) = scan_local_staged(
			ScanRoot {
				path: &root,
				pair_root: PairRoot::Directory,
			},
			&baseline,
			IgnoreRules::default(),
			RuleFiles::Read,
			stage,
			&AtomicBool::new(false),
		)
		.unwrap();

		#[cfg(unix)]
		if let Some(file) = locked {
			use std::os::unix::fs::PermissionsExt;
			fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
			unreadable += usize::from(!map.complete);
		}
		assert!(matches!(staged.nodes, Side::Staged(_)), "seed {seed}");
		assert_eq!(
			sorted_entries(&staged.nodes.whole()),
			sorted_entries(&map.nodes.whole()),
			"seed {seed}: nodes"
		);
		assert_eq!(staged.complete, map.complete, "seed {seed}: complete");
		let errors = |scan: &LocalScan| -> Vec<String> {
			scan.errors.iter().map(ToString::to_string).collect()
		};
		assert_eq!(errors(&staged), errors(&map), "seed {seed}: errors");
		assert_eq!(staged.invalid_names, map.invalid_names, "seed {seed}");
		assert_eq!(staged.aliased_dirs, map.aliased_dirs, "seed {seed}");
		assert_eq!(staged.ignored, map.ignored, "seed {seed}");
		assert_eq!(
			staged.ignored_default_untracked, map.ignored_default_untracked,
			"seed {seed}"
		);
		assert_eq!(staged.ignore_blocked, map.ignore_blocked, "seed {seed}");
		let held = Nodes::len(&staged.nodes.whole());
		assert!(
			held > 4,
			"seed {seed}: a tree of {held} node(s) proves little"
		);
		fs::remove_dir_all(&root).ok();
	}
	#[cfg(unix)]
	assert!(
		unreadable > 2,
		"only {unreadable} case(s) with an unreadable file"
	);
}

/// A map sink that also keeps everything put into it, in the order it was put.
struct Recording<'a> {
	map: MapSink,
	put: &'a mut Vec<Found>,
}

impl ScanSink for Recording<'_> {
	type Error = Infallible;

	fn put(&mut self, found: Found) -> Result<Option<Collision>, Infallible> {
		self.put.push(found.clone());
		self.map.put(found)
	}

	fn finish(self) -> Result<Side<LocalNode>, Infallible> {
		self.map.finish()
	}
}

/// Reading entries on worker threads — their stats, and the hashes the fast path missed —
/// changes nothing a walk reports: what it puts and in which order, which entry of a twin wins,
/// every error in its order, and whether it completed. The tree holds more entries than the walk
/// holds while one is read, a name the remote rejects (a file, and a directory with entries
/// under it), an ignored file, a file removed once listed, files and a `.filenignore` that cannot
/// be read, and, where the volume keeps them apart, case twins. Every directory holds a symlink
/// to a file, one to a directory inside the root and one to a directory outside it, and two
/// dangling ones, so the links past [`INLINE_STATS`] that resolve are stat-ed on a worker. It
/// walks with no baseline, and once with a baseline whose every hash only the fast path can
/// produce.
#[cfg(unix)]
#[test]
fn a_walk_reading_entries_on_workers_finds_what_one_reading_them_inline_finds() {
	use std::os::unix::fs::{PermissionsExt, symlink};

	let root = temp_root();
	let outside = temp_root();
	fs::write(outside.join("o1.txt"), b"outside 1").unwrap();
	fs::write(outside.join("o2.txt"), b"outside 2").unwrap();
	for dir in 0..5 {
		let dir_path = root.join(format!("d{dir}"));
		fs::create_dir(&dir_path).unwrap();
		for file in 0..1100 {
			fs::write(
				dir_path.join(format!("f{file:04}.txt")),
				format!("{dir}/{file}"),
			)
			.unwrap();
		}
		symlink(dir_path.join("f0000.txt"), dir_path.join("link.txt")).unwrap();
		symlink(root.join("d0").join("deep"), dir_path.join("alias")).unwrap();
		symlink(&outside, dir_path.join("ext")).unwrap();
		symlink(root.join("nowhere"), dir_path.join("dangling")).unwrap();
		symlink(root.join("nowhere_dir"), dir_path.join("dangling_dir")).unwrap();
	}
	fs::create_dir(root.join("d0").join("deep")).unwrap();
	fs::write(root.join("d0").join("deep").join("leaf.txt"), b"leaf").unwrap();
	fs::write(root.join("bad."), b"rejected name").unwrap();
	fs::create_dir_all(root.join("rej.").join("sub")).unwrap();
	fs::write(
		root.join("rej.").join("child.txt"),
		b"under a rejected name",
	)
	.unwrap();
	fs::write(root.join("rej.").join("sub").join("bad."), b"under it too").unwrap();
	fs::write(root.join(FILENIGNORE), "*.log\n").unwrap();
	fs::write(root.join("d1").join("x.log"), b"ignored").unwrap();
	// Alone in its directory, so putting it back between the walks leaves every other
	// directory's order as it was. Removing it changes the directory's mtime, which a walk
	// reading on workers may read after the removal: that one mtime is left out of the
	// comparison.
	let gone_dir = root.join("gone");
	fs::create_dir(&gone_dir).unwrap();
	let gone = gone_dir.join("temp.txt");
	let restore_gone = || fs::write(&gone, b"gone").unwrap();
	restore_gone();
	let locked = [
		root.join("d1").join("f0100.txt"),
		root.join("d3").join("f0500.txt"),
		root.join("d3").join("f0501.txt"),
	];
	let rule_file = root.join("d2").join(FILENIGNORE);
	fs::write(&rule_file, "*.tmp\n").unwrap();
	for path in locked.iter().chain([&rule_file]) {
		fs::set_permissions(path, fs::Permissions::from_mode(0o000)).unwrap();
	}
	let twins = case_twins(&root.join("d4")).is_some();

	let walk = |baseline: &Baseline, max_read_workers: usize| {
		let mut put = Vec::new();
		let Ok((scan, _)) = scan_local_watched(
			ScanRoot {
				path: &root,
				pair_root: PairRoot::Directory,
			},
			"",
			baseline,
			IgnoreRules::default(),
			&RuleFiles::Read,
			Hooks {
				on_listed: &mut |path| {
					if path == gone {
						fs::remove_file(path).unwrap();
					}
					ControlFlow::Continue(())
				},
				stop: None,
				moved_hash: None,
				max_read_workers,
			},
			Recording {
				map: MapSink::with_capacity(1024),
				put: &mut put,
			},
		);
		restore_gone();
		(scan, put)
	};
	let errors =
		|scan: &LocalScan| -> Vec<String> { scan.errors.iter().map(ToString::to_string).collect() };
	let without_gone_mtime = |found: &Found| match found {
		Found::Node(node) if node.rel_path == "gone" => Found::Node(LocalNode {
			mtime_millis: 0,
			..node.clone()
		}),
		found => found.clone(),
	};
	let assert_same = |(pooled, pooled_put): &(LocalScan, Vec<Found>),
	                   (inline, inline_put): &(LocalScan, Vec<Found>)| {
		assert!(
			inline_put.len() > READ_WINDOW,
			"the tree must outgrow the window: {} put",
			inline_put.len()
		);
		assert_eq!(
			pooled_put
				.iter()
				.map(without_gone_mtime)
				.collect::<Vec<_>>(),
			inline_put
				.iter()
				.map(without_gone_mtime)
				.collect::<Vec<_>>(),
			"what is put, and in which order"
		);
		assert_eq!(errors(pooled), errors(inline));
		assert_eq!(pooled.complete, inline.complete);
		assert_eq!(pooled.invalid_names, inline.invalid_names);
		assert_eq!(pooled.aliased_dirs, inline.aliased_dirs);
		assert_eq!(pooled.ignored, inline.ignored);
		assert_eq!(pooled.ignore_blocked, inline.ignore_blocked);
		let nodes = |scan: &LocalScan| {
			let mut nodes = sorted_entries(&scan.nodes.whole());
			for (_, node) in nodes.iter_mut().filter(|(rel_path, _)| rel_path == "gone") {
				node.mtime_millis = 0;
			}
			nodes
		};
		assert_eq!(nodes(pooled), nodes(inline));
	};

	let inline = walk(&Baseline::default(), 0);
	let pooled = walk(&Baseline::default(), MAX_READ_WORKERS);
	assert_same(&pooled, &inline);

	// Every hash the baseline vouches for is one no file has, so only the fast path can have
	// produced it.
	let fake = Blake3Hash::from([0xAB; 32]);
	let row = |rel_path: String, kind, size, local_mtime| BaselineEntry {
		rel_path,
		kind,
		remote_uuid: None,
		content_hash: (kind == NodeKind::File).then_some(fake),
		size,
		local_mtime,
		remote_modified: None,
		state: BaselineState::Synced,
		local_kind: None,
		remote_kind: None,
		remote_hash: None,
		remote_size: None,
		remote_stable_uuid: None,
		agreed_hash: None,
	};
	let baseline = Baseline::from_rows(sorted_entries(&inline.0.nodes.whole()).into_iter().map(
		|(rel_path, node)| {
			row(
				rel_path,
				node.kind,
				Some(node.size),
				Some(node.mtime_millis),
			)
		},
	));
	let inline_fast = walk(&baseline, 0);
	let pooled_fast = walk(&baseline, MAX_READ_WORKERS);
	assert_same(&pooled_fast, &inline_fast);

	for path in locked.iter().chain([&rule_file]) {
		fs::set_permissions(path, fs::Permissions::from_mode(0o644)).unwrap();
	}
	let (inline, _) = inline;
	assert_eq!(
		inline
			.errors
			.iter()
			.filter(|error| matches!(error, ScanError::Io { .. }))
			.count(),
		locked.len() + 1 + 2 * 5,
		"each unreadable file, the unreadable rule file and the dangling links: {:?}",
		inline.errors
	);
	assert_eq!(
		inline
			.errors
			.iter()
			.any(|error| matches!(error, ScanError::DuplicateName { .. })),
		twins,
		"{:?}",
		inline.errors
	);
	assert!(!inline.complete);
	assert_eq!(
		inline.invalid_names.keys().collect::<Vec<_>>(),
		vec!["bad.", "rej."]
	);
	assert_eq!(
		inline.aliased_dirs.keys().cloned().collect::<Vec<_>>(),
		(0..5)
			.map(|dir| format!("d{dir}/alias"))
			.collect::<Vec<_>>()
	);
	assert_eq!(inline.ignored.keys().collect::<Vec<_>>(), vec!["d1/x.log"]);
	assert_eq!(inline.ignore_blocked.iter().collect::<Vec<_>>(), vec!["d2"]);
	let nodes = sorted_entries(&inline.nodes.whole());
	for linked in ["d3/link.txt", "d3/ext/o1.txt", "d3/ext/o2.txt"] {
		assert!(
			nodes.iter().any(|(rel_path, _)| rel_path == linked),
			"{linked} is walked through its link"
		);
	}
	let files = sorted_entries(&pooled_fast.0.nodes.whole())
		.into_iter()
		.filter(|(_, node)| node.kind == NodeKind::File)
		.collect::<Vec<_>>();
	assert!(files.len() > READ_WINDOW);
	assert!(
		files
			.iter()
			.all(|(_, node)| node.content_hash == Some(fake)),
		"every file takes the baseline's hash on the fast path"
	);

	fs::remove_dir_all(&root).ok();
	fs::remove_dir_all(&outside).ok();
}

/// A root holding `files` files and nothing else.
fn flat_tree(files: usize) -> std::path::PathBuf {
	let root = temp_root();
	for file in 0..files {
		fs::write(root.join(format!("f{file:04}.txt")), file.to_string()).unwrap();
	}
	root
}

/// A map sink that counts what it takes, and sets `stop` as it takes its `at`th entry.
struct StopAt<'a> {
	map: MapSink,
	put: usize,
	at: usize,
	stop: &'a AtomicBool,
}

impl ScanSink for StopAt<'_> {
	type Error = Infallible;

	fn put(&mut self, found: Found) -> Result<Option<Collision>, Infallible> {
		self.put += 1;
		if self.put == self.at {
			self.stop.store(true, Ordering::Relaxed);
		}
		self.map.put(found)
	}

	fn finish(self) -> Result<Side<LocalNode>, Infallible> {
		self.map.finish()
	}
}

/// A walk its caller stops before it lists anything stages nothing, and says it is partial. The
/// tree is directories alone, which the walk reads on its own thread and puts as it lists them,
/// so a walk that went on listing would stage them.
#[test]
fn a_staged_walk_stopped_before_it_starts_stages_nothing() {
	let root = temp_root();
	for dir in ["a", "b", "c"] {
		fs::create_dir(root.join(dir)).unwrap();
	}
	let (_dir, stage) = Stage::in_temp_dir();
	let (scan, _) = scan_local_staged(
		ScanRoot {
			path: &root,
			pair_root: PairRoot::Directory,
		},
		&Baseline::default(),
		IgnoreRules::default(),
		RuleFiles::Read,
		stage,
		&AtomicBool::new(true),
	)
	.unwrap();
	assert!(!scan.complete);
	assert_eq!(Nodes::len(&scan.nodes.whole()), 0);
	fs::remove_dir_all(&root).ok();
}

/// A walk cut short while it lists waits for none of the reads it handed over. With one worker
/// and no baseline every file is read there, and the walk puts nothing before it waits on a
/// read, so a walk that waited would have put every file listed before the cut.
#[test]
fn a_walk_cut_short_while_listing_puts_none_of_the_reads_under_way() {
	let root = flat_tree(300);
	let mut listed = 0usize;
	let Ok((scan, _)) = scan_local_watched(
		ScanRoot {
			path: &root,
			pair_root: PairRoot::Directory,
		},
		"",
		&Baseline::default(),
		IgnoreRules::default(),
		&RuleFiles::Read,
		Hooks {
			on_listed: &mut |path| {
				listed += usize::from(path != root);
				if listed == 200 {
					ControlFlow::Break(())
				} else {
					ControlFlow::Continue(())
				}
			},
			stop: None,
			moved_hash: None,
			max_read_workers: 1,
		},
		Recording {
			map: MapSink::with_capacity(300),
			put: &mut Vec::new(),
		},
	);
	assert!(!scan.complete);
	assert_eq!(Nodes::len(&scan.nodes.whole()), 0);
	fs::remove_dir_all(&root).ok();
}

/// A walk stopped once it has listed everything waits for no more of its reads. One worker
/// answers the reads in the order they were handed over, so each answer puts exactly one file,
/// and the walk stops at the one put as the stop is set.
#[test]
fn a_walk_stopped_while_it_waits_on_its_reads_waits_for_no_more_of_them() {
	let root = flat_tree(300);
	let stop = AtomicBool::new(false);
	let Ok((scan, _)) = scan_local_watched(
		ScanRoot {
			path: &root,
			pair_root: PairRoot::Directory,
		},
		"",
		&Baseline::default(),
		IgnoreRules::default(),
		&RuleFiles::Read,
		Hooks {
			on_listed: &mut |_| ControlFlow::Continue(()),
			stop: Some(&stop),
			moved_hash: None,
			max_read_workers: 1,
		},
		StopAt {
			map: MapSink::with_capacity(300),
			put: 0,
			at: 100,
			stop: &stop,
		},
	);
	assert!(!scan.complete);
	assert_eq!(Nodes::len(&scan.nodes.whole()), 100);
	fs::remove_dir_all(&root).ok();
}

/// A walk of `root` into a map, with `on_listed` called for each entry it lists.
fn scan_watched(
	root: &Path,
	baseline: &Baseline,
	on_listed: &mut dyn FnMut(&Path) -> ControlFlow<()>,
) -> LocalScan {
	let Ok((scan, _)) = scan_local_watched(
		ScanRoot {
			path: root,
			pair_root: PairRoot::Directory,
		},
		"",
		baseline,
		IgnoreRules::default(),
		&RuleFiles::Read,
		Hooks {
			on_listed,
			stop: None,
			moved_hash: None,
			max_read_workers: MAX_READ_WORKERS,
		},
		MapSink::with_capacity(1024),
	);
	scan
}

/// The rows a test spells as a path-keyed map, as the pass's resident baseline.
fn tree(rows: &HashMap<String, BaselineEntry>) -> Baseline {
	Baseline::from_rows(rows.values().cloned())
}
