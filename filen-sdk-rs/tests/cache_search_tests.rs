use std::{
	sync::{Arc, Mutex},
	time::Duration,
};

use filen_macros::shared_test_runtime;
use filen_sdk_rs::{
	auth::Client,
	cache::{CacheError, SearchConfig, SearchItemType, SearchSnapshot, SearchWindowCallback},
	fs::{HasUUID, file::meta::FileMetaChanges},
	io::RemoteDirectory,
};
use uuid::Uuid;

mod helpers;
use helpers::*;

type SnapshotLog = Arc<Mutex<Vec<SearchSnapshot>>>;

fn snapshot_log() -> (SnapshotLog, SearchWindowCallback) {
	let log: SnapshotLog = Arc::new(Mutex::new(Vec::new()));
	let sink = log.clone();
	let callback: SearchWindowCallback = Box::new(move |snapshot| {
		sink.lock().unwrap().push(snapshot);
	});
	(log, callback)
}

fn last_snapshot(log: &SnapshotLog) -> Option<SearchSnapshot> {
	log.lock().unwrap().last().cloned()
}

fn names(snapshot: &SearchSnapshot) -> Vec<String> {
	snapshot
		.results
		.iter()
		.map(|result| result.name().to_string())
		.collect()
}

/// A derived client (its own cache slot) configured onto a fresh temp DB, with NO sync roots —
/// the search under test owns its registrations.
async fn search_client() -> Arc<Client> {
	let base = test_utils::RESOURCES.client().await;
	let client = derive_client(base.as_ref());
	client
		.configure_cache(temp_cache_path(), |_| {})
		.await
		.unwrap();
	client
}

/// A unique scratch dir under `resources.dir`. The caller MUST keep the `TestResources` alive
/// for the whole test: its `Drop` permanently deletes `resources.dir`, which cascades to the
/// scratch dir (and conveniently cleans it up at test end).
async fn scratch_dir(
	client: &Arc<Client>,
	resources: &test_utils::TestResources,
	tag: &str,
) -> RemoteDirectory {
	client
		.create_dir(
			&(&resources.dir).into(),
			&format!("search_{tag}_{}", Uuid::new_v4()),
		)
		.await
		.unwrap()
}

/// Upload into a possibly JUST-CREATED dir with retries: the backend is eventually-consistent
/// about fresh dirs, and the ingest node can reject with "Cannot upload into this folder" for a
/// few seconds after creation.
async fn upload_text(
	client: &Arc<Client>,
	dir: &RemoteDirectory,
	name: &str,
) -> filen_sdk_rs::io::RemoteFile {
	let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
	loop {
		let builder = client.make_file_builder(name, *dir.uuid()).unwrap();
		match client.upload_file(builder, b"x").await {
			Ok(file) => return file,
			Err(e) if tokio::time::Instant::now() < deadline => {
				eprintln!("upload of {name} not accepted yet ({e}); retrying");
				tokio::time::sleep(Duration::from_millis(1000)).await;
			}
			Err(e) => panic!("upload of {name} kept failing: {e:?}"),
		}
	}
}

/// `create_dir` into a possibly just-created parent, with the same propagation retries.
async fn create_dir_retry(
	client: &Arc<Client>,
	parent: &RemoteDirectory,
	name: &str,
) -> RemoteDirectory {
	let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
	loop {
		match client.create_dir(&parent.into(), name).await {
			Ok(dir) => return dir,
			Err(e) if tokio::time::Instant::now() < deadline => {
				eprintln!("create_dir {name} not accepted yet ({e}); retrying");
				tokio::time::sleep(Duration::from_millis(1000)).await;
			}
			Err(e) => panic!("create_dir {name} kept failing: {e:?}"),
		}
	}
}

#[shared_test_runtime]
async fn test_search_initial_results_sorted_dirs_first() {
	let client = search_client().await;
	let resources = test_utils::RESOURCES.get_resources().await;
	let scratch = scratch_dir(&client, &resources, "initial").await;

	client
		.create_dir(&(&scratch).into(), "zzz dir")
		.await
		.unwrap();
	for name in ["beta.txt", "alpha.txt"] {
		upload_text(&client, &scratch, name).await;
	}

	let search = client
		.clone()
		.create_search(scratch.uuid().into(), SearchConfig::new())
		.await
		.unwrap();
	assert!(
		poll_until(Duration::from_secs(60), || search.total() == 3).await,
		"the convergence resync populates the search"
	);

	let (log, callback) = snapshot_log();
	let (snapshot, _window) = search.get_range(0..10, callback).await.unwrap();
	assert!(snapshot.live);
	assert_eq!(snapshot.total, 3);
	assert_eq!(
		names(&snapshot),
		vec!["zzz dir", "alpha.txt", "beta.txt"],
		"dirs first, then names ascending"
	);
	assert!(
		log.lock().unwrap().is_empty(),
		"the initial snapshot is returned, never delivered via the callback"
	);
}

#[shared_test_runtime]
async fn test_search_live_upload_fires_window_snapshot() {
	let client = search_client().await;
	let resources = test_utils::RESOURCES.get_resources().await;
	let scratch = scratch_dir(&client, &resources, "live").await;
	let search = client
		.clone()
		.create_search(scratch.uuid().into(), SearchConfig::new())
		.await
		.unwrap();
	let (log, callback) = snapshot_log();
	let (snapshot, _window) = search.get_range(0..10, callback).await.unwrap();
	assert_eq!(snapshot.total, 0);

	upload_text(&client, &scratch, "fresh.txt").await;

	assert!(
		poll_until(Duration::from_secs(60), || {
			last_snapshot(&log).is_some_and(|snapshot| names(&snapshot) == vec!["fresh.txt"])
		})
		.await,
		"the upload's socket event refreshes the window"
	);
}

#[shared_test_runtime]
async fn test_search_rename_reorders_results() {
	let client = search_client().await;
	let resources = test_utils::RESOURCES.get_resources().await;
	let scratch = scratch_dir(&client, &resources, "rename").await;
	let mut alpha = upload_text(&client, &scratch, "alpha.txt").await;
	upload_text(&client, &scratch, "beta.txt").await;

	let search = client
		.clone()
		.create_search(scratch.uuid().into(), SearchConfig::new())
		.await
		.unwrap();
	assert!(poll_until(Duration::from_secs(60), || search.total() == 2).await);
	let (log, callback) = snapshot_log();
	let (snapshot, _window) = search.get_range(0..10, callback).await.unwrap();
	assert_eq!(names(&snapshot), vec!["alpha.txt", "beta.txt"]);

	let changes = FileMetaChanges::default().name("zeta.txt").unwrap();
	client
		.update_file_metadata(&mut alpha, changes)
		.await
		.unwrap();

	assert!(
		poll_until(Duration::from_secs(60), || {
			last_snapshot(&log)
				.is_some_and(|snapshot| names(&snapshot) == vec!["beta.txt", "zeta.txt"])
		})
		.await,
		"the rename re-sorts the window"
	);
}

#[shared_test_runtime]
async fn test_search_favorite_toggle_fires_content_refresh() {
	let client = search_client().await;
	let resources = test_utils::RESOURCES.get_resources().await;
	let scratch = scratch_dir(&client, &resources, "favorite").await;
	let mut file = upload_text(&client, &scratch, "fav.txt").await;

	let search = client
		.clone()
		.create_search(scratch.uuid().into(), SearchConfig::new())
		.await
		.unwrap();
	assert!(poll_until(Duration::from_secs(60), || search.total() == 1).await);
	let (log, callback) = snapshot_log();
	let (_snapshot, _window) = search.get_range(0..10, callback).await.unwrap();

	client.set_file_favorite(&mut file, true).await.unwrap();

	assert!(
		poll_until(Duration::from_secs(60), || {
			last_snapshot(&log).is_some_and(|snapshot| {
				snapshot.results.first().is_some_and(|result| match result {
					filen_sdk_rs::cache::SearchResult::File(file) => file.favorited,
					_ => false,
				})
			})
		})
		.await,
		"a favorite toggle (name unchanged) still refreshes the window"
	);
}

#[shared_test_runtime]
async fn test_search_move_out_removes_result() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = search_client().await;
	let scratch = scratch_dir(&client, &resources, "moveout").await;
	let mut file = upload_text(&client, &scratch, "mover.txt").await;

	let search = client
		.clone()
		.create_search(scratch.uuid().into(), SearchConfig::new())
		.await
		.unwrap();
	assert!(poll_until(Duration::from_secs(60), || search.total() == 1).await);

	client
		.move_file(&mut file, &(&resources.dir).into())
		.await
		.unwrap();

	assert!(
		poll_until(Duration::from_secs(60), || search.total() == 0).await,
		"moving the file out of the searched subtree removes it"
	);
}

#[shared_test_runtime]
async fn test_search_move_in_of_populated_dir_indexes_descendants() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = search_client().await;
	let scratch = scratch_dir(&client, &resources, "movein").await;

	// A populated dir OUTSIDE the search root, cached via a whole-account registration.
	let account_root: Uuid = client.root().uuid().into();
	let _account_registration = client
		.clone()
		.add_sync_root(account_root, noop_sync_root_callback())
		.await
		.unwrap();
	let mut populated = client
		.create_dir(
			&(&resources.dir).into(),
			&format!("populated_{}", Uuid::new_v4()),
		)
		.await
		.unwrap();
	upload_text(&client, &populated, "inner.txt").await;

	// COVERED fast path: the scratch dir is cached under the account root, so this registers
	// instantly (no validation round-trip, no resync) — its contents are already queryable.
	let search = client
		.clone()
		.create_search(scratch.uuid().into(), SearchConfig::new())
		.await
		.unwrap();
	assert_eq!(search.total(), 0);

	// Move the populated dir INTO the searched subtree: the single Move event must ingest the
	// already-cached descendants (the account-root registration converges them; if the move's
	// event beats that convergence, the resync synthetics deliver them as ordinary News —
	// either path must end at 2).
	client
		.move_dir(&mut populated, &(&scratch).into())
		.await
		.unwrap();

	assert!(
		poll_until(Duration::from_secs(120), || search.total() == 2).await,
		"the moved-in dir AND its already-cached child are indexed"
	);
}

#[shared_test_runtime]
async fn test_search_name_filter_applies_to_live_events() {
	let client = search_client().await;
	let resources = test_utils::RESOURCES.get_resources().await;
	let scratch = scratch_dir(&client, &resources, "filter").await;
	let search = client
		.clone()
		.create_search(
			scratch.uuid().into(),
			SearchConfig::new().with_name("Report"),
		)
		.await
		.unwrap();

	for name in ["q3-REPORT.txt", "unrelated.txt"] {
		upload_text(&client, &scratch, name).await;
	}

	assert!(
		poll_until(Duration::from_secs(60), || search.total() == 1).await,
		"only the (case-insensitively) matching upload enters the results"
	);
}

#[shared_test_runtime]
async fn test_search_set_config_refilters_without_reregistration() {
	let client = search_client().await;
	let resources = test_utils::RESOURCES.get_resources().await;
	let scratch = scratch_dir(&client, &resources, "setconfig").await;
	create_dir_retry(&client, &scratch, "a dir").await;
	upload_text(&client, &scratch, "a file.txt").await;

	let search = client
		.clone()
		.create_search(scratch.uuid().into(), SearchConfig::new())
		.await
		.unwrap();
	assert!(poll_until(Duration::from_secs(60), || search.total() == 2).await);

	// Engine-local: no re-registration, no resync — answers from the cached subtree.
	search
		.set_config(SearchConfig::new().with_item_type(SearchItemType::Dir))
		.await
		.unwrap();
	assert_eq!(search.total(), 1, "set_config takes effect synchronously");

	let (_log, callback) = snapshot_log();
	let (snapshot, _window) = search.get_range(0..10, callback).await.unwrap();
	assert_eq!(names(&snapshot), vec!["a dir"]);
}

#[shared_test_runtime]
async fn test_search_case_sensitive_toggle() {
	let client = search_client().await;
	let resources = test_utils::RESOURCES.get_resources().await;
	let scratch = scratch_dir(&client, &resources, "case").await;
	upload_text(&client, &scratch, "MiXeD.txt").await;

	let search = client
		.clone()
		.create_search(
			scratch.uuid().into(),
			SearchConfig::new().with_name("mixed"),
		)
		.await
		.unwrap();
	assert!(
		poll_until(Duration::from_secs(60), || search.total() == 1).await,
		"the default mode matches case-insensitively"
	);

	// Engine-local refilter: byte-exact mode no longer matches the lowercased needle...
	search
		.set_config(
			SearchConfig::new()
				.with_name("mixed")
				.with_case_sensitive(true),
		)
		.await
		.unwrap();
	assert_eq!(search.total(), 0, "case-sensitive mode matches raw bytes");

	// ...but does match the exact original casing.
	search
		.set_config(
			SearchConfig::new()
				.with_name("MiXeD")
				.with_case_sensitive(true),
		)
		.await
		.unwrap();
	assert_eq!(search.total(), 1);
}

#[shared_test_runtime]
async fn test_search_out_of_window_insert_updates_delivered_total() {
	let client = search_client().await;
	let resources = test_utils::RESOURCES.get_resources().await;
	let scratch = scratch_dir(&client, &resources, "totals").await;
	upload_text(&client, &scratch, "aaa.txt").await;

	let search = client
		.clone()
		.create_search(scratch.uuid().into(), SearchConfig::new())
		.await
		.unwrap();
	assert!(poll_until(Duration::from_secs(60), || search.total() == 1).await);
	let (log, callback) = snapshot_log();
	// The window exactly covers the lone existing result.
	let (snapshot, _window) = search.get_range(0..1, callback).await.unwrap();
	assert_eq!(snapshot.total, 1);

	// Sorts AFTER the window: the window's contents stay identical, but its delivered total
	// must not go stale.
	upload_text(&client, &scratch, "zzz.txt").await;

	assert!(
		poll_until(Duration::from_secs(60), || {
			last_snapshot(&log)
				.is_some_and(|snapshot| snapshot.total == 2 && names(&snapshot) == vec!["aaa.txt"])
		})
		.await,
		"an out-of-window insert still delivers the fresh total"
	);
}

#[shared_test_runtime]
async fn test_search_root_deleted_goes_terminal_with_frozen_get_range() {
	let client = search_client().await;
	let resources = test_utils::RESOURCES.get_resources().await;
	let mut scratch = scratch_dir(&client, &resources, "rootdel").await;
	upload_text(&client, &scratch, "doomed.txt").await;

	let search = client
		.clone()
		.create_search(scratch.uuid().into(), SearchConfig::new())
		.await
		.unwrap();
	assert!(poll_until(Duration::from_secs(60), || search.total() == 1).await);
	let (log, callback) = snapshot_log();
	let (_snapshot, _window) = search.get_range(0..10, callback).await.unwrap();

	// Deleting the searched dir server-side drops the worker-side registration → the engine's
	// terminal signal.
	client.trash_dir(&mut scratch).await.unwrap();
	client.delete_dir_permanently(scratch).await.unwrap();

	assert!(
		poll_until(Duration::from_secs(60), || !search.is_live()).await,
		"the search learns its root is gone"
	);
	assert!(
		poll_until(Duration::from_secs(60), || {
			last_snapshot(&log).is_some_and(|snapshot| !snapshot.live)
		})
		.await,
		"the window's final fire carries live = false"
	);
	let final_snapshot = last_snapshot(&log).unwrap();
	assert_eq!(
		names(&final_snapshot),
		vec!["doomed.txt"],
		"the final fire re-sends the last-delivered results, not a re-hydration of wiped rows"
	);

	// Frozen but still answering.
	let (_log2, callback2) = snapshot_log();
	let (frozen, _window2) = search.get_range(0..10, callback2).await.unwrap();
	assert!(!frozen.live);
}

#[shared_test_runtime]
async fn test_search_flush_cache_goes_terminal() {
	let client = search_client().await;
	let resources = test_utils::RESOURCES.get_resources().await;
	let scratch = scratch_dir(&client, &resources, "flush").await;
	upload_text(&client, &scratch, "kept.txt").await;

	let search = client
		.clone()
		.create_search(scratch.uuid().into(), SearchConfig::new())
		.await
		.unwrap();
	assert!(poll_until(Duration::from_secs(60), || search.total() == 1).await);

	client.flush_cache().await;

	assert!(
		poll_until(Duration::from_secs(60), || !search.is_live()).await,
		"a stopped worker is a terminal signal for the search"
	);
	// The DB rows survive a flush, so frozen get_range still hydrates fully.
	let (_log, callback) = snapshot_log();
	let (frozen, _window) = search.get_range(0..10, callback).await.unwrap();
	assert!(!frozen.live);
	assert_eq!(names(&frozen), vec!["kept.txt"]);
}

#[shared_test_runtime]
async fn test_search_unknown_uuid_is_rejected() {
	let client = search_client().await;
	// The slot must be live for the validation to run on a worker — register the account root.
	let account_root: Uuid = client.root().uuid().into();
	let _registration = client
		.clone()
		.add_sync_root(account_root, noop_sync_root_callback())
		.await
		.unwrap();

	let err = client
		.clone()
		.create_search(Uuid::new_v4(), SearchConfig::new())
		.await
		.expect_err("a bogus uuid must be rejected");
	assert!(
		matches!(
			err.downcast_ref::<CacheError>(),
			Some(CacheError::InvalidSyncRoot { .. })
		),
		"got {err:?}"
	);
}

#[shared_test_runtime]
async fn test_search_close_returns_with_live_window_handle() {
	let client = search_client().await;
	let resources = test_utils::RESOURCES.get_resources().await;
	let scratch = scratch_dir(&client, &resources, "close").await;
	let search = client
		.clone()
		.create_search(scratch.uuid().into(), SearchConfig::new())
		.await
		.unwrap();
	let (_log, callback) = snapshot_log();
	let (_snapshot, window) = search.get_range(0..10, callback).await.unwrap();

	// The outstanding window handle must not deadlock the close.
	tokio::time::timeout(Duration::from_secs(30), search.close())
		.await
		.expect("close() must not hang while window handles are alive");
	drop(window); // inert now; its Drop is a no-op
}

#[shared_test_runtime]
async fn test_search_drop_releases_the_sync_root() {
	let client = search_client().await;
	let resources = test_utils::RESOURCES.get_resources().await;
	let scratch = scratch_dir(&client, &resources, "droprel").await;
	let search = client
		.clone()
		.create_search(scratch.uuid().into(), SearchConfig::new())
		.await
		.unwrap();
	assert!(poll_until(Duration::from_secs(60), || search.is_live()).await);

	// The search holds the ONLY registration: dropping it lets the cache worker wind down;
	// flush_cache then joins deterministically (and must not hang).
	drop(search);
	tokio::time::timeout(Duration::from_secs(30), client.flush_cache())
		.await
		.expect("worker drains and exits after the last registration drops");
}
