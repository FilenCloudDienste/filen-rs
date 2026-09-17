//! Ignore-rule tests: `.filenignore` files, the device-wide user patterns and the built-in defaults.
//!
//! An ignored path is invisible to sync on both sides: nothing is uploaded, downloaded, moved or
//! deleted for it. A synced path that becomes ignored stops syncing and loses its baseline rows, so
//! removing the rule later syncs it like a first sync.
use std::{
	borrow::Cow,
	collections::{BTreeMap, BTreeSet},
	sync::{Arc, Mutex},
	time::Duration,
};

use filen_macros::shared_test_runtime;
use filen_sdk_rs::{
	auth::Client,
	fs::{
		HasName, HasUUID,
		categories::{DirType, Normal},
		dir::RemoteDirectory,
		file::RemoteFile,
	},
	io::client_impl::IoSharedClientExt,
	sync_engine::{
		GuardReason, IgnoreLevel, IgnoredPath, SyncEngine, SyncEvent, SyncMode, SyncReport,
		WatchConfig,
	},
};

use crate::harness::*;
use crate::helpers::*;

/// Every item under `root`, by root-relative path: `Some` for a file, `None` for a directory.
async fn remote_tree(
	client: &Client,
	root: &RemoteDirectory,
) -> BTreeMap<String, Option<RemoteFile>> {
	let mut tree = BTreeMap::new();
	let mut stack = vec![(String::new(), root.clone())];
	while let Some((prefix, dir)) = stack.pop() {
		let (dirs, files) = client
			.list_dir(
				&DirType::<Normal>::Dir(Cow::Borrowed(&dir)),
				None::<&fn(u64, Option<u64>)>,
			)
			.await
			.unwrap();
		for file in files {
			tree.insert(format!("{prefix}{}", file.name().unwrap()), Some(file));
		}
		for sub in dirs {
			let path = format!("{prefix}{}", sub.name().unwrap());
			stack.push((format!("{path}/"), sub));
			tree.insert(path, None);
		}
	}
	tree
}

async fn remote_paths(client: &Client, root: &RemoteDirectory) -> BTreeSet<String> {
	remote_tree(client, root).await.into_keys().collect()
}

fn paths(list: &[&str]) -> BTreeSet<String> {
	list.iter().map(|p| p.to_string()).collect()
}

/// The remote directory at `rel` under the single client's root.
async fn remote_dir_at(sc: &SingleClient, rel: &str) -> RemoteDirectory {
	let tree = remote_tree(&sc.cache.client, &sc.resources.dir).await;
	assert!(
		matches!(tree.get(rel), Some(None)),
		"no remote directory at {rel}"
	);
	let mut dir = sc.resources.dir.clone();
	for name in rel.split('/') {
		let (dirs, _) = sc
			.cache
			.client
			.list_dir(
				&DirType::<Normal>::Dir(Cow::Borrowed(&dir)),
				None::<&fn(u64, Option<u64>)>,
			)
			.await
			.unwrap();
		dir = dirs.into_iter().find(|d| d.name() == Some(name)).unwrap();
	}
	dir
}

/// Create `name` under `parent` on the remote and wait until the cache observes it.
async fn create_remote_dir(
	cache: &TestCache,
	parent: &RemoteDirectory,
	name: &str,
) -> RemoteDirectory {
	let dir = cache
		.client
		.create_dir(&DirType::<Normal>::Dir(Cow::Borrowed(parent)), name)
		.await
		.unwrap();
	assert!(
		poll_for_item(cache.db_path(), dir.uuid(), CACHE_CONVERGE_TIMEOUT).await,
		"the cache never observed the remote directory {name}"
	);
	dir
}

/// Upload `data` as `name` under `parent` and wait until the cache observes it.
async fn upload_remote(
	cache: &TestCache,
	parent: &RemoteDirectory,
	name: &str,
	data: &[u8],
) -> RemoteFile {
	let builder = cache.client.make_file_builder(name, parent.uuid()).unwrap();
	let file = cache.client.upload_file(builder, data).await.unwrap();
	assert!(
		poll_for_item(cache.db_path(), file.uuid(), CACHE_CONVERGE_TIMEOUT).await,
		"the cache never observed the remote file {name}"
	);
	file
}

async fn remote_bytes(sc: &SingleClient, rel: &str) -> Vec<u8> {
	let tree = remote_tree(&sc.cache.client, &sc.resources.dir).await;
	let file = tree
		.get(rel)
		.and_then(Option::as_ref)
		.unwrap_or_else(|| panic!("no remote file at {rel}"));
	sc.cache.client.download_file(file).await.unwrap()
}

fn clean(report: &SyncReport) {
	assert!(report.errors.is_empty(), "pass errors: {report:?}");
	assert!(report.refused.is_none(), "pass refused: {report:?}");
}

fn by_root_file(rel_path: &str, pattern: &str, tracked: bool) -> IgnoredPath {
	IgnoredPath {
		rel_path: rel_path.to_string(),
		level: IgnoreLevel::File { dir: String::new() },
		pattern: pattern.to_string(),
		tracked,
	}
}

// ===========================================================================
// Built-in defaults
// ===========================================================================

/// OS junk the built-in defaults name is never uploaded, and a path only they hide is not reported.
#[shared_test_runtime]
async fn filter_defaults_not_uploaded() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	write_file(&sc.local, ".DS_Store", b"finder");
	write_file(&sc.local, "a/Thumbs.db", b"thumbs");
	write_file(&sc.local, "a/x.txt", b"content");

	let report = sc.sync().await;
	clean(&report);
	assert!(report.ignored.is_empty(), "{report:?}");
	assert_eq!(
		remote_paths(&sc.cache.client, &sc.resources.dir).await,
		paths(&["a", "a/x.txt"])
	);

	sc.cleanup();
}

// ===========================================================================
// .filenignore files
// ===========================================================================

/// A local root `.filenignore` of `build/`, a local `build/o.bin` and a remote `build/r.bin`, in a
/// two-way pair.
async fn stage_build_on_both_sides(sc: &SingleClient) {
	let build = create_remote_dir(&sc.cache, &sc.resources.dir, "build").await;
	upload_remote(&sc.cache, &build, "r.bin", b"remote build output").await;
	write_file(&sc.local, ".filenignore", b"build/\n");
	write_file(&sc.local, "build/o.bin", b"local build output");
}

/// The root `.filenignore` syncs, and the directory it names is hidden on both sides: the local
/// build output never reaches the remote and the remote one never reaches the disk.
#[shared_test_runtime]
async fn filter_root_filenignore_both_sides() {
	let sc = single_client(SyncMode::TwoWay).await;
	stage_build_on_both_sides(&sc).await;

	let first = sc.sync().await;
	clean(&first);
	assert_eq!(first.uploaded, 1, "only .filenignore uploads: {first:?}");
	assert_eq!(first.downloaded, 0, "{first:?}");
	assert_eq!(first.ignored, vec![by_root_file("build", "build/", false)]);
	let steady = sc.sync().await;
	clean(&steady);
	assert_eq!((steady.uploaded, steady.downloaded), (0, 0), "{steady:?}");

	assert_eq!(
		remote_paths(&sc.cache.client, &sc.resources.dir).await,
		paths(&[".filenignore", "build", "build/r.bin"])
	);
	assert_eq!(remote_bytes(&sc, ".filenignore").await, b"build/\n");
	assert!(
		!sc.local.join("build/r.bin").exists(),
		"remote build output downloaded"
	);
	assert!(read_eq(&sc.local, "build/o.bin", b"local build output"));

	sc.cleanup();
}

/// A deeper `.filenignore` re-includes with `!` what a shallower one ignores, only below itself.
#[shared_test_runtime]
async fn filter_nested_and_whitelist() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	write_file(&sc.local, ".filenignore", b"*.log\n");
	write_file(&sc.local, "logs/.filenignore", b"!keep.log\n");
	write_file(&sc.local, "a.log", b"a");
	write_file(&sc.local, "logs/keep.log", b"keep");
	write_file(&sc.local, "logs/b.log", b"b");

	let report = sc.sync().await;
	clean(&report);
	assert_eq!(
		report.ignored,
		vec![
			by_root_file("a.log", "*.log", false),
			by_root_file("logs/b.log", "*.log", false)
		]
	);
	assert_eq!(
		remote_paths(&sc.cache.client, &sc.resources.dir).await,
		paths(&[".filenignore", "logs", "logs/.filenignore", "logs/keep.log"])
	);

	sc.cleanup();
}

/// git's rule: nothing under an ignored directory can be re-included.
#[shared_test_runtime]
async fn filter_parent_excluded_cannot_reinclude() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	write_file(&sc.local, ".filenignore", b"secret/\n!secret/ok.txt\n");
	write_file(&sc.local, "secret/ok.txt", b"still hidden");
	write_file(&sc.local, "public.txt", b"synced");

	let report = sc.sync().await;
	clean(&report);
	assert_eq!(
		report.ignored,
		vec![by_root_file("secret", "secret/", false)]
	);
	assert_eq!(
		remote_paths(&sc.cache.client, &sc.resources.dir).await,
		paths(&[".filenignore", "public.txt"])
	);

	sc.cleanup();
}

/// A `.filenignore` another device put on the remote is downloaded and applied before this device
/// has a copy: the remote file it hides is not downloaded, and the local file it hides is not
/// uploaded, on the first pass and after.
#[shared_test_runtime]
async fn filter_remote_only_filenignore_applies() {
	let sc = single_client(SyncMode::TwoWay).await;
	let x = create_remote_dir(&sc.cache, &sc.resources.dir, "x").await;
	upload_remote(&sc.cache, &x, ".filenignore", b"*.tmp\n").await;
	upload_remote(&sc.cache, &x, "a.tmp", b"remote scratch").await;
	write_file(&sc.local, "x/b.tmp", b"local scratch");

	let hidden = || {
		["x/a.tmp", "x/b.tmp"]
			.map(|rel_path| IgnoredPath {
				rel_path: rel_path.to_string(),
				level: IgnoreLevel::File {
					dir: "x".to_string(),
				},
				pattern: "*.tmp".to_string(),
				tracked: false,
			})
			.to_vec()
	};
	let first = sc.sync().await;
	clean(&first);
	assert_eq!(
		first.downloaded, 1,
		"only x/.filenignore downloads: {first:?}"
	);
	assert_eq!(first.uploaded, 0, "{first:?}");
	assert_eq!(first.ignored, hidden());
	let second = sc.sync().await;
	clean(&second);
	assert_eq!((second.uploaded, second.downloaded), (0, 0), "{second:?}");
	assert_eq!(second.ignored, hidden());

	assert!(read_eq(&sc.local, "x/.filenignore", b"*.tmp\n"));
	assert!(
		!sc.local.join("x/a.tmp").exists(),
		"an ignored remote file downloaded"
	);
	assert_eq!(
		remote_paths(&sc.cache.client, &sc.resources.dir).await,
		paths(&["x", "x/.filenignore", "x/a.tmp"])
	);

	sc.cleanup();
}

/// `plan_pair` and the pass agree on what is ignored, and nothing is planned for it.
#[shared_test_runtime]
async fn filter_plan_pair_reports_ignored() {
	let sc = single_client(SyncMode::TwoWay).await;
	stage_build_on_both_sides(&sc).await;

	let plan = sc.engine.plan_pair(sc.pair).await.unwrap();
	assert_eq!(
		plan.ignored,
		vec![by_root_file("build", "build/", false)],
		"{plan}"
	);
	assert!(
		!plan.actions.iter().any(|a| a.rel_path.starts_with("build")),
		"an action was planned under an ignored directory: {plan}"
	);
	let report = sc.sync().await;
	clean(&report);
	assert_eq!(report.ignored, plan.ignored);

	sc.cleanup();
}

// ===========================================================================
// A synced path becomes ignored, then is re-included
// ===========================================================================

/// Sync `cache/{same,gone,edit}.bin`, ignore `cache/`, then delete the local `gone.bin` and edit
/// the remote `edit.bin`. Asserts that the pass adding the rule untracks `cache` and touches
/// nothing, and that neither later change is propagated.
async fn stage_untracked_cache(sc: &SingleClient) {
	write_file(&sc.local, "cache/same.bin", b"same");
	write_file(&sc.local, "cache/gone.bin", b"gone");
	write_file(&sc.local, "cache/edit.bin", b"v1");
	let seed = sc.sync().await;
	clean(&seed);
	assert_eq!(seed.uploaded, 3, "{seed:?}");

	write_file(&sc.local, ".filenignore", b"cache/\n");
	let untracking = sc.sync().await;
	clean(&untracking);
	assert_eq!(
		untracking.uploaded, 1,
		"only .filenignore uploads: {untracking:?}"
	);
	assert_eq!(
		untracking.ignored,
		vec![by_root_file("cache", "cache/", true)]
	);
	assert_eq!(
		(untracking.locally_deleted, untracking.remotely_trashed),
		(0, 0),
		"{untracking:?}"
	);

	std::fs::remove_file(sc.local.join("cache/gone.bin")).unwrap();
	let cache_dir = remote_dir_at(sc, "cache").await;
	upload_remote(&sc.cache, &cache_dir, "edit.bin", b"v2 from the remote").await;

	let ignored = sc.sync().await;
	clean(&ignored);
	assert_eq!(
		ignored.ignored,
		vec![by_root_file("cache", "cache/", false)]
	);
	assert_eq!(
		(
			ignored.uploaded,
			ignored.downloaded,
			ignored.locally_deleted,
			ignored.remotely_trashed
		),
		(0, 0, 0, 0),
		"an ignored path was synced: {ignored:?}"
	);
	assert!(ignored.conflicts.is_empty(), "{ignored:?}");
	assert!(
		!sc.local.join("cache/gone.bin").exists(),
		"gone.bin re-downloaded"
	);
	assert!(
		read_eq(&sc.local, "cache/edit.bin", b"v1"),
		"the remote edit was pulled"
	);
	assert_eq!(
		remote_bytes(sc, "cache/gone.bin").await,
		b"gone",
		"the local delete propagated"
	);
	assert_eq!(
		remote_bytes(sc, "cache/edit.bin").await,
		b"v2 from the remote"
	);
}

/// A synced directory that becomes ignored stops syncing both ways, and neither copy is touched.
#[shared_test_runtime]
async fn filter_newly_ignored_stops_syncing() {
	let sc = single_client(SyncMode::TwoWay).await;
	stage_untracked_cache(&sc).await;
	sc.cleanup();
}

/// Removing the rule syncs the directory like a first sync: identical copies simply resume, a file
/// only one side holds is copied, and a file the two sides changed apart is a conflict.
#[shared_test_runtime]
async fn filter_unignore_syncs_like_a_first_sync() {
	let sc = single_client(SyncMode::TwoWay).await;
	stage_untracked_cache(&sc).await;

	write_file(&sc.local, ".filenignore", b"# nothing ignored\n");
	let report = sc.sync().await;
	clean(&report);
	assert!(report.ignored.is_empty(), "{report:?}");
	assert_eq!(
		report.conflict_paths().collect::<Vec<_>>(),
		vec!["cache/edit.bin"],
		"{report:?}"
	);
	assert_eq!(
		report.uploaded, 1,
		"only the edited .filenignore uploads: {report:?}"
	);
	assert_eq!(report.downloaded, 1, "only gone.bin downloads: {report:?}");
	assert_eq!(
		(report.locally_deleted, report.remotely_trashed),
		(0, 0),
		"{report:?}"
	);
	assert!(read_eq(&sc.local, "cache/gone.bin", b"gone"));
	assert!(read_eq(&sc.local, "cache/same.bin", b"same"));
	assert!(
		read_eq(&sc.local, "cache/edit.bin", b"v1"),
		"a conflict side was overwritten"
	);
	assert_eq!(
		remote_bytes(&sc, "cache/edit.bin").await,
		b"v2 from the remote"
	);

	sc.cleanup();
}

/// In a one-way mirror, removing a rule holds the deletion of what only the destination holds under
/// it, as a first sync holds one, until the deletion is approved: another client's upload the rule
/// hid is not trashed the moment the rule goes.
#[shared_test_runtime]
async fn filter_unignore_holds_a_mirror_deletion_like_a_first_sync() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	write_file(&sc.local, "keep.txt", b"tracked");
	clean(&sc.sync().await);
	sc.engine.set_user_ignore("*.psd").await.unwrap();
	upload_remote(
		&sc.cache,
		&sc.resources.dir,
		"a.psd",
		b"another client's layers",
	)
	.await;
	let hidden = sc.sync().await;
	clean(&hidden);
	assert_eq!(hidden.remotely_trashed, 0, "{hidden:?}");
	assert_eq!(
		hidden.ignored,
		vec![IgnoredPath {
			rel_path: "a.psd".to_string(),
			level: IgnoreLevel::User,
			pattern: "*.psd".to_string(),
			tracked: false,
		}]
	);

	sc.engine.set_user_ignore("").await.unwrap();
	for _ in 0..2 {
		let held = sc.sync().await;
		clean(&held);
		assert!(
			matches!(
				held.guard,
				Some(GuardReason::FirstSyncWithDeletions { deletions: 1 })
			),
			"{held:?}"
		);
		assert_eq!(held.remotely_trashed, 0, "{held:?}");
	}
	assert_eq!(
		remote_paths(&sc.cache.client, &sc.resources.dir).await,
		paths(&["a.psd", "keep.txt"])
	);

	let token = sc
		.sync()
		.await
		.deletion_token
		.expect("the deletion is still held");
	sc.engine.approve_deletions(sc.pair, &token).await;
	let approved = sc.sync().await;
	clean(&approved);
	assert_eq!(approved.remotely_trashed, 1, "{approved:?}");
	assert_eq!(
		remote_paths(&sc.cache.client, &sc.resources.dir).await,
		paths(&["keep.txt"])
	);

	sc.cleanup();
}

/// A file moved out of an ignored directory is uploaded at its new path, and the remote copy at
/// the ignored path stays.
#[shared_test_runtime]
async fn filter_move_out_of_ignored() {
	let sc = single_client(SyncMode::TwoWay).await;
	write_file(&sc.local, "a/x.bin", b"moved out");
	clean(&sc.sync().await);
	write_file(&sc.local, ".filenignore", b"a/\n");
	clean(&sc.sync().await);

	move_file(&sc.local, "a/x.bin", "b/x.bin");
	let report = sc.sync().await;
	clean(&report);
	assert_eq!(report.uploaded, 1, "{report:?}");
	assert_eq!(
		(report.moved_remote, report.remotely_trashed),
		(0, 0),
		"the remote copy under the ignored directory was touched: {report:?}"
	);
	assert_eq!(
		remote_paths(&sc.cache.client, &sc.resources.dir).await,
		paths(&[".filenignore", "a", "a/x.bin", "b", "b/x.bin"])
	);

	sc.cleanup();
}

// ===========================================================================
// A directory deleted on one side over ignored content on the other
// ===========================================================================

/// Sync `proj/a.txt` with a local-only `proj/extra`, trash `proj` on the remote, and run a pass.
async fn trash_proj_over_local_extra(sc: &SingleClient, extra: &str) -> SyncReport {
	write_file(&sc.local, "proj/a.txt", b"visible");
	write_file(&sc.local, &format!("proj/{extra}"), b"ignored extra");
	clean(&sc.sync().await);
	let a_txt = remote_tree(&sc.cache.client, &sc.resources.dir)
		.await
		.remove("proj/a.txt")
		.flatten()
		.expect("proj/a.txt never uploaded");

	let mut proj = remote_dir_at(sc, "proj").await;
	// A pass keeps its own recent uploads in its remote view until a snapshot lists them (or the
	// cache announces them after the write). Trashing the directory before that would leave them
	// folded back in, so let a pass read them first.
	let mut uploaded: Vec<_> = remote_tree(&sc.cache.client, &proj)
		.await
		.into_values()
		.flatten()
		.map(|file| file.uuid())
		.collect();
	uploaded.push(proj.uuid());
	assert!(uploaded.contains(&a_txt.uuid()));
	for &uuid in &uploaded {
		assert!(
			poll_for_item(sc.cache.db_path(), uuid, CACHE_CONVERGE_TIMEOUT).await,
			"the cache never observed the upload"
		);
	}
	clean(&sc.sync().await);
	sc.cache.client.trash_dir(&mut proj).await.unwrap();
	for uuid in uploaded {
		assert!(
			poll_for_item_absent(sc.cache.db_path(), uuid, CACHE_CONVERGE_TIMEOUT).await,
			"the cache never observed the trash"
		);
	}
	let report = sc.sync().await;
	clean(&report);
	assert!(
		bytes_recoverable_anywhere(&sc.local, b"visible"),
		"a.txt was not quarantined"
	);
	report
}

/// A `.filenignore` rule keeps the directory, holding only the ignored content, and the withheld
/// delete counts as deferred.
#[shared_test_runtime]
async fn filter_dir_delete_keeps_ignored_children() {
	let sc = single_client(SyncMode::TwoWay).await;
	write_file(&sc.local, ".filenignore", b"node_modules/\n");
	let report = trash_proj_over_local_extra(&sc, "node_modules/m.js").await;

	assert!(
		!sc.local.join("proj/a.txt").exists(),
		"a.txt not deleted: {report:?}"
	);
	assert!(read_eq(
		&sc.local,
		"proj/node_modules/m.js",
		b"ignored extra"
	));
	assert!(report.deferred_paths >= 1, "{report:?}");
	assert_eq!(
		remote_paths(&sc.cache.client, &sc.resources.dir).await,
		paths(&[".filenignore"])
	);

	sc.cleanup();
}

/// The directory's own `.filenignore` keeps it too, pass after pass: that rule file is kept with the
/// directory, or the next pass would read no rule and take the ignored content down.
#[shared_test_runtime]
async fn filter_dir_delete_keeps_ignored_children_by_its_own_rule() {
	let sc = single_client(SyncMode::TwoWay).await;
	// Something outside proj stays on the remote, so the trash does not empty it and trip the guard.
	write_file(&sc.local, "keep.txt", b"stays");
	write_file(&sc.local, "proj/.filenignore", b"node_modules/\n");
	let report = trash_proj_over_local_extra(&sc, "node_modules/m.js").await;
	assert!(report.deferred_paths >= 1, "{report:?}");
	let again = sc.sync().await;
	clean(&again);
	assert!(again.deferred_paths >= 1, "{again:?}");

	assert!(
		!sc.local.join("proj/a.txt").exists(),
		"a.txt not deleted: {report:?}"
	);
	assert!(read_eq(&sc.local, "proj/.filenignore", b"node_modules/\n"));
	assert!(read_eq(
		&sc.local,
		"proj/node_modules/m.js",
		b"ignored extra"
	));
	assert_eq!(
		remote_paths(&sc.cache.client, &sc.resources.dir).await,
		paths(&["keep.txt"])
	);

	sc.cleanup();
}

/// A mode that pulls keeps the directory by its own rule file as well, although the remote copy of
/// that file went with the trashed directory: the local copy still has its row.
#[shared_test_runtime]
async fn filter_pull_dir_delete_keeps_ignored_children_by_its_own_rule() {
	let sc = single_client(SyncMode::RemoteToLocal).await;
	upload_remote(&sc.cache, &sc.resources.dir, "keep.txt", b"stays").await;
	let mut proj = create_remote_dir(&sc.cache, &sc.resources.dir, "proj").await;
	let rule = upload_remote(&sc.cache, &proj, ".filenignore", b"node_modules/\n").await;
	let a_txt = upload_remote(&sc.cache, &proj, "a.txt", b"visible").await;
	let first = sc.sync().await;
	clean(&first);
	assert_eq!(first.downloaded, 3, "{first:?}");
	write_file(&sc.local, "proj/node_modules/m.js", b"ignored extra");
	clean(&sc.sync().await);

	sc.cache.client.trash_dir(&mut proj).await.unwrap();
	for uuid in [proj.uuid(), rule.uuid(), a_txt.uuid()] {
		assert!(
			poll_for_item_absent(sc.cache.db_path(), uuid, CACHE_CONVERGE_TIMEOUT).await,
			"the cache never observed the trash"
		);
	}
	for _ in 0..2 {
		let report = sc.sync().await;
		clean(&report);
		assert!(report.deferred_paths >= 1, "{report:?}");
	}

	assert!(!sc.local.join("proj/a.txt").exists(), "a.txt not deleted");
	assert!(bytes_recoverable_anywhere(&sc.local, b"visible"));
	assert!(read_eq(&sc.local, "proj/.filenignore", b"node_modules/\n"));
	assert!(read_eq(
		&sc.local,
		"proj/node_modules/m.js",
		b"ignored extra"
	));

	sc.cleanup();
}

/// A mode that pushes keeps the remote directory by its own rule file after a local `rm -r`: the
/// remote copy of that file is read while its row stands.
#[shared_test_runtime]
async fn filter_push_dir_delete_keeps_ignored_children_by_its_own_rule() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	write_file(&sc.local, "keep.txt", b"stays");
	write_file(&sc.local, "proj/.filenignore", b"node_modules/\n");
	write_file(&sc.local, "proj/a.txt", b"visible");
	let first = sc.sync().await;
	clean(&first);
	assert_eq!(first.uploaded, 3, "{first:?}");
	let proj = remote_dir_at(&sc, "proj").await;
	let modules = create_remote_dir(&sc.cache, &proj, "node_modules").await;
	upload_remote(&sc.cache, &modules, "m.js", b"ignored extra").await;
	let quiet = sc.sync().await;
	clean(&quiet);
	assert_eq!(quiet.remotely_trashed, 0, "{quiet:?}");

	std::fs::remove_dir_all(sc.local.join("proj")).unwrap();
	for _ in 0..2 {
		let report = sc.sync().await;
		clean(&report);
		assert!(report.deferred_paths >= 1, "{report:?}");
	}

	assert_eq!(
		remote_paths(&sc.cache.client, &sc.resources.dir).await,
		paths(&[
			"keep.txt",
			"proj",
			"proj/.filenignore",
			"proj/node_modules",
			"proj/node_modules/m.js"
		])
	);

	sc.cleanup();
}

/// Content only the built-in defaults hide does not keep the directory.
#[shared_test_runtime]
async fn filter_dir_delete_takes_default_junk() {
	let sc = single_client(SyncMode::TwoWay).await;
	// Something outside proj stays on the remote, so the trash does not empty it and trip the guard.
	write_file(&sc.local, "keep.txt", b"stays");
	let report = trash_proj_over_local_extra(&sc, ".DS_Store").await;

	assert!(!sc.local.join("proj").exists(), "proj survived: {report:?}");
	assert!(
		bytes_recoverable_anywhere(&sc.local, b"ignored extra"),
		".DS_Store was not quarantined with its directory"
	);
	assert_eq!(report.deferred_paths, 0, "{report:?}");

	sc.cleanup();
}

// ===========================================================================
// User level
// ===========================================================================

/// The user patterns apply to every pair, and a pair's own `.filenignore` re-includes only there.
#[shared_test_runtime]
async fn filter_user_level_all_pairs() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let root = resources.dir.uuid();
	let cache = TestCache::new(&resources.client, root).await;
	wait_for_converged_resync(&cache.messages, root, 0, CACHE_CONVERGE_TIMEOUT).await;
	let r1 = create_remote_dir(&cache, &resources.dir, "p1").await;
	let r2 = create_remote_dir(&cache, &resources.dir, "p2").await;
	let local1 = fresh_local_dir("filter_user1");
	let local2 = fresh_local_dir("filter_user2");
	for local in [&local1, &local2] {
		write_file(local, "a.psd", b"layers");
		write_file(local, "b.txt", b"text");
	}
	write_file(&local2, ".filenignore", b"!*.psd\n");

	let engine = SyncEngine::open(cache.client.clone(), temp_cache_path())
		.await
		.unwrap();
	engine.set_user_ignore("*.psd").await.unwrap();
	let pair1 = engine
		.add_pair(local1.clone(), r1.uuid(), SyncMode::LocalToRemote)
		.await
		.unwrap();
	let pair2 = engine
		.add_pair(local2.clone(), r2.uuid(), SyncMode::LocalToRemote)
		.await
		.unwrap();
	let rep1 = engine.sync_once(pair1).await.unwrap();
	let rep2 = engine.sync_once(pair2).await.unwrap();
	clean(&rep1);
	clean(&rep2);
	assert_eq!(
		rep1.ignored,
		vec![IgnoredPath {
			rel_path: "a.psd".to_string(),
			level: IgnoreLevel::User,
			pattern: "*.psd".to_string(),
			tracked: false,
		}]
	);
	assert!(rep2.ignored.is_empty(), "{rep2:?}");

	assert_eq!(remote_paths(&cache.client, &r1).await, paths(&["b.txt"]));
	assert_eq!(
		remote_paths(&cache.client, &r2).await,
		paths(&[".filenignore", "a.psd", "b.txt"])
	);

	std::fs::remove_dir_all(&local1).ok();
	std::fs::remove_dir_all(&local2).ok();
}

/// Patterns match case-insensitively, non-ASCII letters included.
#[shared_test_runtime]
async fn filter_case_insensitive() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	sc.engine.set_user_ignore("*.JPG\nÄ*\n").await.unwrap();
	write_file(&sc.local, "img.jpg", b"pixels");
	write_file(&sc.local, "ärger.txt", b"umlaut");
	write_file(&sc.local, "kept.txt", b"kept");

	let report = sc.sync().await;
	clean(&report);
	assert_eq!(report.uploaded, 1, "{report:?}");
	assert_eq!(
		remote_paths(&sc.cache.client, &sc.resources.dir).await,
		paths(&["kept.txt"])
	);

	sc.cleanup();
}

// ===========================================================================
// Watch
// ===========================================================================

/// A running watch applies a `.filenignore` written while it runs: the file it names is never
/// uploaded, and the passes are driven by file events, not the safety net.
#[shared_test_runtime]
async fn filter_watch_applies_a_new_filenignore() {
	const SETTLE: Duration = Duration::from_secs(90);

	let sc = single_client(SyncMode::TwoWay).await;
	write_file(&sc.local, "a.tmp", b"synced before");
	let engine = Arc::new(
		SyncEngine::open(sc.cache.client.clone(), temp_cache_path())
			.await
			.unwrap(),
	);
	let pair = engine
		.add_pair(sc.local.clone(), sc.remote, SyncMode::TwoWay)
		.await
		.unwrap();
	let seed = engine.sync_once(pair).await.unwrap();
	clean(&seed);
	assert_eq!(seed.uploaded, 1, "{seed:?}");

	let reports: Arc<Mutex<Vec<SyncReport>>> = Arc::default();
	let sink = reports.clone();
	let handle = engine
		.clone()
		.watch_with(
			pair,
			WatchConfig {
				debounce: Duration::from_secs(1),
				// Far beyond the test: every pass below is triggered by a file event.
				safety_net: Duration::from_secs(3600),
			},
			Box::new(move |event| {
				if let SyncEvent::PassCompleted { report } = event {
					sink.lock().unwrap().push(report);
				}
			}),
		)
		.await
		.unwrap();
	let uploaded = || {
		reports
			.lock()
			.unwrap()
			.iter()
			.map(|r| r.uploaded)
			.sum::<usize>()
	};
	assert!(
		poll_until(SETTLE, || !reports.lock().unwrap().is_empty()).await,
		"the watch never ran its initial pass"
	);

	write_file(&sc.local, ".filenignore", b"new.tmp\n");
	assert!(
		poll_until(SETTLE, || uploaded() >= 1).await,
		"the watch never uploaded .filenignore"
	);
	write_file(&sc.local, "new.tmp", b"ignored");
	write_file(&sc.local, "after.txt", b"proves a pass ran");
	assert!(
		poll_until(SETTLE, || uploaded() >= 2).await,
		"the watch never uploaded after.txt"
	);
	drop(handle);

	assert_eq!(uploaded(), 2, "{:?}", reports.lock().unwrap());
	for report in reports.lock().unwrap().iter() {
		clean(report);
	}
	assert_eq!(
		remote_paths(&sc.cache.client, &sc.resources.dir).await,
		paths(&[".filenignore", "a.tmp", "after.txt"])
	);

	sc.cleanup();
}

// ===========================================================================
// Mass-delete guard
// ===========================================================================

/// A root `.filenignore` that hides everything but two files leaves the filtered remote view empty
/// once those two are trashed, while the remote still holds what the rule hides. The pass reads the
/// remote as it is, not as vanished, so the guard does not hold the two deletions it plans.
#[shared_test_runtime]
async fn filter_hiding_everything_is_not_an_emptied_remote() {
	let sc = single_client(SyncMode::TwoWay).await;
	write_file(&sc.local, "a.txt", b"hidden later");
	write_file(&sc.local, "gone1.txt", b"one");
	write_file(&sc.local, "gone2.txt", b"two");
	clean(&sc.sync().await);
	// A pass keeps its own recent uploads in its remote view until a snapshot lists them, so let the
	// cache list them and a pass read them before anything is trashed.
	let mut tree = remote_tree(&sc.cache.client, &sc.resources.dir).await;
	for file in tree.values().flatten() {
		assert!(
			poll_for_item(sc.cache.db_path(), file.uuid(), CACHE_CONVERGE_TIMEOUT).await,
			"the cache never observed the upload"
		);
	}
	clean(&sc.sync().await);
	for name in ["gone1.txt", "gone2.txt"] {
		let mut file = tree.remove(name).flatten().expect("uploaded");
		sc.cache.client.trash_file(&mut file).await.unwrap();
		assert!(
			poll_for_item_absent(sc.cache.db_path(), file.uuid(), CACHE_CONVERGE_TIMEOUT).await,
			"the cache never observed the trash"
		);
	}

	write_file(&sc.local, ".filenignore", b"*\n!gone*\n");
	let report = sc.sync().await;
	clean(&report);
	assert!(report.guard.is_none(), "{report:?}");
	assert_eq!(report.locally_deleted, 2, "{report:?}");
	assert!(read_eq(&sc.local, "a.txt", b"hidden later"));
	assert_eq!(
		remote_paths(&sc.cache.client, &sc.resources.dir).await,
		paths(&["a.txt"])
	);

	sc.cleanup();
}

// ===========================================================================
// Un-ignoring in the one-way modes
// ===========================================================================

/// Removing a rule in a pushing mirror syncs the path like a first sync: the source overwrites the
/// destination's copy of a file both sides hold, while the destination-only file the rule hid is
/// reported as a held deletion and trashed only once the deletion is approved.
#[shared_test_runtime]
async fn filter_unignore_local_to_remote_holds_the_destination_deletion() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	write_file(&sc.local, "keep.txt", b"tracked");
	write_file(&sc.local, "assets/photo.bin", b"v1");
	let seed = sc.sync().await;
	clean(&seed);
	assert_eq!(seed.uploaded, 2, "{seed:?}");

	write_file(&sc.local, ".filenignore", b"assets/\n");
	let untracking = sc.sync().await;
	clean(&untracking);
	assert_eq!(
		untracking.ignored,
		vec![by_root_file("assets", "assets/", true)]
	);
	assert_eq!(untracking.remotely_trashed, 0, "{untracking:?}");

	// Another client changes the destination while the rule hides it: an edit of the file this
	// device still holds, and a file only the remote has.
	let assets = remote_dir_at(&sc, "assets").await;
	let destination_copy =
		upload_remote(&sc.cache, &assets, "photo.bin", b"v2 from the remote").await;
	upload_remote(&sc.cache, &assets, "extra.bin", b"another client's").await;
	let ignored = sc.sync().await;
	clean(&ignored);
	assert_eq!(
		(ignored.uploaded, ignored.remotely_trashed),
		(0, 0),
		"an ignored path was synced: {ignored:?}"
	);
	assert_eq!(
		ignored.ignored,
		vec![by_root_file("assets", "assets/", false)]
	);

	write_file(&sc.local, ".filenignore", b"# nothing ignored\n");
	let unignored = sc.sync().await;
	clean(&unignored);
	assert!(unignored.ignored.is_empty(), "{unignored:?}");
	assert_eq!(
		unignored.uploaded, 2,
		"the edited .filenignore and the source's own copy of photo.bin: {unignored:?}"
	);
	assert!(
		matches!(
			unignored.guard,
			Some(GuardReason::FirstSyncWithDeletions { deletions: 1 })
		),
		"{unignored:?}"
	);
	assert_eq!(unignored.remotely_trashed, 0, "{unignored:?}");
	assert_eq!(
		remote_bytes(&sc, "assets/photo.bin").await,
		b"v1",
		"the source overwrites the destination's copy"
	);
	// Overwrites it, never destroys it: the edit this device never saw is still on the path's version
	// chain, which is what makes a one-way overwrite of an unseen copy safe to do unasked.
	let head = remote_tree(&sc.cache.client, &sc.resources.dir)
		.await
		.remove("assets/photo.bin")
		.flatten()
		.expect("no remote file at assets/photo.bin");
	let versions = sc.cache.client.list_file_versions(&head).await.unwrap();
	assert!(
		versions.iter().any(|v| v.uuid() == destination_copy.uuid()),
		"the destination's copy did not survive as a server version: {versions:?}"
	);

	let held = sc.sync().await;
	clean(&held);
	assert_eq!(
		held.remotely_trashed, 0,
		"the hold stands until it is approved: {held:?}"
	);
	let token = held.deletion_token.expect("the deletion is still held");
	sc.engine.approve_deletions(sc.pair, &token).await;
	let approved = sc.sync().await;
	clean(&approved);
	assert_eq!(approved.remotely_trashed, 1, "{approved:?}");
	assert_eq!(
		remote_paths(&sc.cache.client, &sc.resources.dir).await,
		paths(&[".filenignore", "assets", "assets/photo.bin", "keep.txt"])
	);

	sc.cleanup();
}

/// The mirror image in a pulling mirror: the remote copy overwrites the local one, whose bytes are
/// quarantined first, and the local-only file the rule hid survives until the held deletion is
/// approved. The rules come from the remote copies here, so the rule file is edited there.
#[shared_test_runtime]
async fn filter_unignore_remote_to_local_holds_the_destination_deletion() {
	let sc = single_client(SyncMode::RemoteToLocal).await;
	upload_remote(&sc.cache, &sc.resources.dir, "keep.txt", b"tracked").await;
	let assets = create_remote_dir(&sc.cache, &sc.resources.dir, "assets").await;
	upload_remote(&sc.cache, &assets, "photo.bin", b"v1").await;
	let seed = sc.sync().await;
	clean(&seed);
	assert_eq!(seed.downloaded, 2, "{seed:?}");

	upload_remote(&sc.cache, &sc.resources.dir, ".filenignore", b"assets/\n").await;
	let untracking = sc.sync().await;
	clean(&untracking);
	assert_eq!(
		untracking.downloaded, 1,
		"only .filenignore downloads: {untracking:?}"
	);
	assert_eq!(
		untracking.ignored,
		vec![by_root_file("assets", "assets/", true)]
	);
	assert_eq!(untracking.locally_deleted, 0, "{untracking:?}");

	// The destination changes while the rule hides it: an edit of the file the remote still holds,
	// and a file only this device has.
	write_file(&sc.local, "assets/photo.bin", b"edited here");
	write_file(&sc.local, "assets/extra.bin", b"made here");
	let ignored = sc.sync().await;
	clean(&ignored);
	assert_eq!(
		(ignored.downloaded, ignored.locally_deleted),
		(0, 0),
		"an ignored path was synced: {ignored:?}"
	);
	assert_eq!(
		ignored.ignored,
		vec![by_root_file("assets", "assets/", false)]
	);

	upload_remote(
		&sc.cache,
		&sc.resources.dir,
		".filenignore",
		b"# nothing ignored\n",
	)
	.await;
	let unignored = sc.sync().await;
	clean(&unignored);
	assert!(unignored.ignored.is_empty(), "{unignored:?}");
	assert_eq!(
		unignored.downloaded, 2,
		"the edited .filenignore and the source's own copy of photo.bin: {unignored:?}"
	);
	assert!(
		matches!(
			unignored.guard,
			Some(GuardReason::FirstSyncWithDeletions { deletions: 1 })
		),
		"{unignored:?}"
	);
	assert_eq!(unignored.locally_deleted, 0, "{unignored:?}");
	assert!(
		read_eq(&sc.local, "assets/photo.bin", b"v1"),
		"the source overwrites the destination's copy"
	);
	assert!(
		bytes_recoverable_anywhere(&sc.local, b"edited here"),
		"the local copy was destroyed rather than quarantined"
	);

	let held = sc.sync().await;
	clean(&held);
	assert_eq!(
		held.locally_deleted, 0,
		"the hold stands until it is approved: {held:?}"
	);
	let token = held.deletion_token.expect("the deletion is still held");
	sc.engine.approve_deletions(sc.pair, &token).await;
	let approved = sc.sync().await;
	clean(&approved);
	assert_eq!(approved.locally_deleted, 1, "{approved:?}");
	assert!(!sc.local.join("assets/extra.bin").exists());
	assert!(
		bytes_recoverable_anywhere(&sc.local, b"made here"),
		"the deleted copy is recoverable"
	);

	sc.cleanup();
}

// ===========================================================================
// Rules that cannot be read
// ===========================================================================

/// A `.filenignore` this process cannot read blocks its directory for the pass: nothing under it is
/// uploaded, trashed or quarantined, and the error is reported. The scan still completed, so a
/// deletion elsewhere is propagated rather than held for approval. The next pass recovers once the
/// file can be read again.
#[cfg(unix)]
#[shared_test_runtime]
async fn filter_unreadable_local_filenignore_blocks_its_directory() {
	use std::os::unix::fs::PermissionsExt;

	let sc = single_client(SyncMode::TwoWay).await;
	write_file(&sc.local, "gone.txt", b"deleted later");
	write_file(&sc.local, "locked/.filenignore", b"*.tmp\n");
	write_file(&sc.local, "locked/a.txt", b"synced before");
	let seed = sc.sync().await;
	clean(&seed);
	assert_eq!(seed.uploaded, 3, "{seed:?}");

	let rules = sc.local.join("locked/.filenignore");
	std::fs::set_permissions(&rules, std::fs::Permissions::from_mode(0o000)).unwrap();
	write_file(&sc.local, "locked/b.txt", b"written while blocked");
	std::fs::remove_file(sc.local.join("locked/a.txt")).unwrap();
	std::fs::remove_file(sc.local.join("gone.txt")).unwrap();

	let blocked = sc.sync().await;
	std::fs::set_permissions(&rules, std::fs::Permissions::from_mode(0o644)).unwrap();
	assert!(blocked.refused.is_none(), "{blocked:?}");
	assert_eq!(blocked.errors.len(), 1, "{blocked:?}");
	assert!(
		blocked.errors[0].starts_with("local scan: locked/.filenignore: "),
		"{blocked:?}"
	);
	assert_eq!(
		(
			blocked.uploaded,
			blocked.downloaded,
			blocked.locally_deleted
		),
		(0, 0, 0),
		"nothing under rules that could not be read may sync: {blocked:?}"
	);
	assert!(
		read_eq(&sc.local, "locked/b.txt", b"written while blocked"),
		"a file under rules that could not be read was rewritten: {blocked:?}"
	);
	assert!(
		!sc.local.join(".filen-sync-trash").exists(),
		"nothing under rules that could not be read may be quarantined: {blocked:?}"
	);
	assert!(
		blocked.ignored.is_empty(),
		"rules that could not be read hide nothing: {blocked:?}"
	);
	assert!(
		blocked.guard.is_none(),
		"the scan completed, so the deletion outside is not held: {blocked:?}"
	);
	assert_eq!(blocked.remotely_trashed, 1, "{blocked:?}");
	assert_eq!(
		remote_paths(&sc.cache.client, &sc.resources.dir).await,
		paths(&["locked", "locked/.filenignore", "locked/a.txt"])
	);

	let recovered = sc.sync().await;
	clean(&recovered);
	assert_eq!(
		recovered.uploaded, 1,
		"b.txt uploads once the rules read: {recovered:?}"
	);
	assert_eq!(
		recovered.remotely_trashed, 1,
		"and the local deletion under them propagates: {recovered:?}"
	);
	assert_eq!(
		remote_paths(&sc.cache.client, &sc.resources.dir).await,
		paths(&["locked", "locked/.filenignore", "locked/b.txt"])
	);

	sc.cleanup();
}

/// A remote `.filenignore` the engine refuses — one over the size cap, one that is not UTF-8 —
/// blocks its directory the same way: nothing under it is downloaded, the local copy it might have
/// covered is neither deleted nor quarantined, and each is reported. Replacing the body with a
/// usable one lets the next pass through.
#[shared_test_runtime]
async fn filter_unusable_remote_filenignore_blocks_its_directory() {
	// One byte past the engine's 1 MiB cap, which is refused without being downloaded at all.
	const OVER_CAP: usize = 1024 * 1024 + 1;

	let sc = single_client(SyncMode::RemoteToLocal).await;
	upload_remote(&sc.cache, &sc.resources.dir, "keep.txt", b"pulled").await;
	let huge = create_remote_dir(&sc.cache, &sc.resources.dir, "huge").await;
	upload_remote(&sc.cache, &huge, ".filenignore", &vec![b'#'; OVER_CAP]).await;
	upload_remote(&sc.cache, &huge, "a.txt", b"under unusable rules").await;
	let binary = create_remote_dir(&sc.cache, &sc.resources.dir, "binary").await;
	upload_remote(&sc.cache, &binary, ".filenignore", &[0xff]).await;
	upload_remote(&sc.cache, &binary, "b.txt", b"under unusable rules").await;
	// A local file under one of them: a pulling mirror would delete it, and must not while the rules
	// that might cover it cannot be read.
	write_file(&sc.local, "huge/mine.txt", b"only here");

	let blocked = sc.sync().await;
	assert!(blocked.refused.is_none(), "{blocked:?}");
	assert_eq!(blocked.errors.len(), 2, "{blocked:?}");
	// The whole line, not the prefix: a rule file that merely failed to download reports under the
	// same prefix, so a flaky fetch would otherwise stand in for the bodies this test staged.
	for (dir, reason) in [
		("binary", "not valid UTF-8, not read"),
		("huge", "larger than 1048576 bytes, not read"),
	] {
		let expected = format!("remote {dir}/.filenignore: {reason}");
		assert!(
			blocked.errors.contains(&expected),
			"{expected}: {blocked:?}"
		);
	}
	assert_eq!(
		blocked.downloaded, 1,
		"only keep.txt, which is outside both: {blocked:?}"
	);
	assert_eq!(blocked.locally_deleted, 0, "{blocked:?}");
	assert!(read_eq(&sc.local, "keep.txt", b"pulled"));
	assert!(
		read_eq(&sc.local, "huge/mine.txt", b"only here"),
		"a local file under rules that could not be read was touched"
	);
	assert!(
		!sc.local.join("binary").exists(),
		"a blocked directory is not created locally"
	);

	for dir in [&huge, &binary] {
		upload_remote(&sc.cache, dir, ".filenignore", b"*.tmp\n").await;
	}
	let recovered = sc.sync().await;
	clean(&recovered);
	assert_eq!(
		recovered.downloaded, 4,
		"both rule files and both files under them: {recovered:?}"
	);
	assert_eq!(
		recovered.locally_deleted, 1,
		"the local-only file is mirrored away once the rules read: {recovered:?}"
	);
	assert!(bytes_recoverable_anywhere(&sc.local, b"only here"));
	assert!(read_eq(&sc.local, "huge/a.txt", b"under unusable rules"));
	assert!(read_eq(&sc.local, "binary/b.txt", b"under unusable rules"));

	sc.cleanup();
}

/// A user text with a bad line is refused whole, naming the line: the stored patterns stay exactly
/// as they were and the pair goes on using them, rather than applying the refused text in part.
#[shared_test_runtime]
async fn filter_invalid_user_ignore_keeps_the_stored_patterns() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	sc.engine.set_user_ignore("*.psd").await.unwrap();
	write_file(&sc.local, "a.psd", b"layers");
	write_file(&sc.local, "b.txt", b"text");
	let first = sc.sync().await;
	clean(&first);
	assert_eq!(first.uploaded, 1, "{first:?}");

	let error = sc
		.engine
		.set_user_ignore("!keep.psd\n[z-a]\n")
		.await
		.expect_err("a text with a bad line must be refused")
		.to_string();
	assert!(
		error.contains("user ignore patterns:2: "),
		"the refusal names the first bad line: {error}"
	);
	assert_eq!(
		sc.engine.user_ignore().await.unwrap(),
		"*.psd",
		"a refused text must not be stored"
	);

	// `keep.psd` is what the refused text's first line would have re-included.
	write_file(&sc.local, "c.psd", b"more layers");
	write_file(&sc.local, "keep.psd", b"re-included by the refused text");
	let second = sc.sync().await;
	clean(&second);
	assert_eq!(
		second.uploaded, 0,
		"the previously stored patterns still apply: {second:?}"
	);
	assert_eq!(
		remote_paths(&sc.cache.client, &sc.resources.dir).await,
		paths(&["b.txt"])
	);

	sc.cleanup();
}
