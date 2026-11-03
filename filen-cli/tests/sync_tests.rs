//! `sync`, run as a user runs it, against the test account.

use std::{
	path::Path,
	process::{Command, Output, Stdio},
	sync::Arc,
	time::{Duration, Instant},
};

use filen_cli::uuid_v5;
use filen_macros::shared_test_runtime;
use filen_sdk_rs::{
	auth::Client,
	fs::{HasName, categories::NonRootFileType},
	io::client_impl::IoSharedClientExt as _,
	sync_engine::SyncEngine,
};
use filen_types::fs::Uuid;
use tokio::sync::RwLock;

#[shared_test_runtime]
async fn sync_once_brings_both_sides_across_and_exits_0() {
	// Not `get_resources_with_lock`: the CLI's sync engine takes the drive lock for every pass, and
	// would wait on this test holding it.
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir_name = resources.dir.name().unwrap();

	// one file on each side, which a two-way sync must bring across to the other
	let file = client
		.make_file_builder("from_remote.txt", resources.dir.uuid)
		.unwrap();
	client
		.upload_file(file, b"from the remote side")
		.await
		.unwrap();
	let local = assert_fs::TempDir::new().unwrap();
	std::fs::write(local.path().join("from_local.txt"), "from the local side").unwrap();
	// its own config dir, so the engine's database starts empty and is deleted afterwards
	let config_dir = assert_fs::TempDir::new().unwrap();
	let pair = format!("{}:{test_dir_name}", local.path().to_str().unwrap());

	let output = sync_once(config_dir.path(), &pair, &[]).await;

	assert_success(&output);
	assert_eq!(
		std::fs::read_to_string(local.path().join("from_remote.txt")).unwrap(),
		"from the remote side"
	);
	assert_eq!(
		remote_content(client, &format!("{test_dir_name}/from_local.txt")).await,
		b"from the local side"
	);
}

/// With `--json`, every line is a JSON object, the upload's among them, and the pass's summary
/// counts the bytes it moved.
#[shared_test_runtime]
async fn sync_once_with_json_prints_an_object_for_each_event() {
	// Not `get_resources_with_lock`, for the reason the first test gives.
	let resources = test_utils::RESOURCES.get_resources().await;
	let test_dir_name = resources.dir.name().unwrap();
	let local = assert_fs::TempDir::new().unwrap();
	std::fs::write(local.path().join("from_local.txt"), "from the local side").unwrap();
	let config_dir = assert_fs::TempDir::new().unwrap();
	let pair = format!("{}:{test_dir_name}", local.path().to_str().unwrap());

	let output = sync_once(config_dir.path(), &pair, &["--json"]).await;

	assert_success(&output);
	let objects = String::from_utf8_lossy(&output.stdout)
		.lines()
		.map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
		.collect::<Vec<_>>();
	let named = |event: &str| {
		objects
			.iter()
			.filter(|object| object["event"] == event)
			.collect::<Vec<_>>()
	};
	assert_eq!(
		named("uploaded")
			.iter()
			.map(|object| &object["path"])
			.collect::<Vec<_>>(),
		["from_local.txt"],
		"{}",
		describe(&output)
	);
	let completed = named("pass_completed");
	assert_eq!(completed.len(), 1, "{}", describe(&output));
	assert_eq!(completed[0]["uploaded"], 1);
	assert_eq!(completed[0]["uploaded_bytes"], "from the local side".len());
}

/// With `--quiet`, a pass that leaves nothing to attend to prints only its summary.
#[shared_test_runtime]
async fn sync_once_with_quiet_prints_only_the_pass_summary() {
	// Not `get_resources_with_lock`, for the reason the first test gives.
	let resources = test_utils::RESOURCES.get_resources().await;
	let test_dir_name = resources.dir.name().unwrap();
	let local = assert_fs::TempDir::new().unwrap();
	std::fs::write(local.path().join("from_local.txt"), "from the local side").unwrap();
	let config_dir = assert_fs::TempDir::new().unwrap();
	let pair = format!("{}:{test_dir_name}", local.path().to_str().unwrap());

	let output = sync_once(config_dir.path(), &pair, &["--quiet"]).await;

	assert_success(&output);
	let stdout = String::from_utf8_lossy(&output.stdout);
	let lines = stdout.lines().collect::<Vec<_>>();
	assert_eq!(lines.len(), 1, "{}", describe(&output));
	assert!(
		lines[0].contains("Sync pass completed: 1 uploaded"),
		"{}",
		describe(&output)
	);
}

#[shared_test_runtime]
async fn sync_once_with_a_conflict_left_open_exits_1() {
	// Not `get_resources_with_lock`, for the reason the test above gives.
	let resources = test_utils::RESOURCES.get_resources().await;
	let (local, config_dir, pair) = diverged_pair(&resources).await;

	let output = sync_once(config_dir.path(), &pair, &[]).await;

	assert_eq!(output.status.code(), Some(1), "{}", describe(&output));
	let stderr = String::from_utf8_lossy(&output.stderr);
	assert!(
		stderr.contains("Sync finished with problems in") && stderr.contains("(1 conflict(s))"),
		"{}",
		describe(&output)
	);
	// a conflict is left alone
	assert_eq!(
		std::fs::read_to_string(local.path().join("fight.txt")).unwrap(),
		"the local edit"
	);
}

/// The resolution a flag gives is applied by one more pass before the command exits.
#[shared_test_runtime]
async fn sync_once_applies_a_conflict_resolution_before_it_exits() {
	// Not `get_resources_with_lock`, for the reason the first test gives.
	let resources = test_utils::RESOURCES.get_resources().await;
	let test_dir_name = resources.dir.name().unwrap();
	let (_local, config_dir, pair) = diverged_pair(&resources).await;

	let output = sync_once(config_dir.path(), &pair, &["--keep-local", "fight.txt"]).await;

	assert_success(&output);
	assert_eq!(
		remote_content(&resources.client, &format!("{test_dir_name}/fight.txt")).await,
		b"the local edit"
	);
}

/// The resolution a flag gives is applied while the watch runs, by a pass it runs for it, rather
/// than by the periodic full pass five minutes later.
#[cfg(unix)]
#[shared_test_runtime]
async fn sync_applies_a_conflict_resolution_while_it_watches() {
	// Not `get_resources_with_lock`, for the reason the first test gives.
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let remote_path = format!("{}/fight.txt", resources.dir.name().unwrap());
	let (_local, config_dir, pair) = diverged_pair(&resources).await;

	let output = sync_until(
		config_dir.path(),
		&["--pair", &pair, "--keep-local", "fight.txt"],
		Duration::from_secs(120),
		async || remote_content(client, &remote_path).await == b"the local edit",
	)
	.await;

	let stdout = String::from_utf8_lossy(&output.stdout);
	assert!(
		stdout.contains("Conflict resolved for fight.txt, kept local version"),
		"{}",
		describe(&output)
	);
}

/// A run that cannot set up its second pair, which syncs the same remote directory as its first,
/// leaves neither registered.
#[shared_test_runtime]
async fn sync_with_a_pair_that_cannot_be_set_up_registers_none() {
	// Not `get_resources_with_lock`, for the reason the first test gives.
	let resources = test_utils::RESOURCES.get_resources().await;
	let test_dir_name = resources.dir.name().unwrap();
	let first_local = assert_fs::TempDir::new().unwrap();
	let second_local = assert_fs::TempDir::new().unwrap();
	let config_dir = assert_fs::TempDir::new().unwrap();
	let first = format!("{}:{test_dir_name}", first_local.path().to_str().unwrap());
	let second = format!("{}:{test_dir_name}", second_local.path().to_str().unwrap());

	let output = sync_once(config_dir.path(), &first, &["--pair", &second]).await;

	assert_eq!(output.status.code(), Some(1), "{}", describe(&output));
	assert!(
		String::from_utf8_lossy(&output.stderr)
			.contains("its remote folder is already synced by the sync pair"),
		"{}",
		describe(&output)
	);
	let engine = SyncEngine::open(
		Arc::clone(&resources.client),
		config_dir.path().join("sync-engine.db"),
	)
	.await
	.unwrap();
	assert_eq!(engine.list_pairs().await.unwrap(), []);
}

/// A paused pair is not synced, which `sync` says; with no other pair to watch, it fails rather
/// than waiting for Ctrl-C with nothing to sync.
#[shared_test_runtime]
async fn sync_with_only_a_paused_pair_says_so_and_exits_1() {
	// Not `get_resources_with_lock`, for the reason the first test gives.
	let resources = test_utils::RESOURCES.get_resources().await;
	let test_dir_name = resources.dir.name().unwrap();
	let local = assert_fs::TempDir::new().unwrap();
	let config_dir = assert_fs::TempDir::new().unwrap();
	let pair = format!("{}:{test_dir_name}", local.path().to_str().unwrap());
	assert_success(&sync_once(config_dir.path(), &pair, &[]).await);
	let engine = SyncEngine::open(
		Arc::clone(&resources.client),
		config_dir.path().join("sync-engine.db"),
	)
	.await
	.unwrap();
	let pairs = engine.list_pairs().await.unwrap();
	let [registered] = pairs.as_slice() else {
		panic!("expected the one pair the CLI registered, got {pairs:?}");
	};
	engine.pause_pair(registered.id).await.unwrap();
	// releases the database's lock for the CLI
	drop(engine);

	let output = sync(config_dir.path(), &["--pair", &pair]).await;

	assert_eq!(output.status.code(), Some(1), "{}", describe(&output));
	let stderr = String::from_utf8_lossy(&output.stderr);
	assert!(
		stderr.contains("is paused, so it is not synced")
			&& stderr.contains("None of the sync pairs could be synced"),
		"{}",
		describe(&output)
	);
}

/// A pair of a sync pairs file with `excludeDotFiles` keeps its dot files local, through the
/// `.filenignore` the CLI writes into its local root, which a two-way pair syncs.
#[shared_test_runtime]
async fn sync_once_with_exclude_dot_files_keeps_dot_files_local() {
	// Not `get_resources_with_lock`, for the reason the first test gives.
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir_name = resources.dir.name().unwrap();
	let local = assert_fs::TempDir::new().unwrap();
	std::fs::write(local.path().join(".secret"), "stays here").unwrap();
	std::fs::write(local.path().join("visible.txt"), "goes up").unwrap();
	let config_dir = assert_fs::TempDir::new().unwrap();
	let pairs_file = config_dir.path().join("pairs.json");
	std::fs::write(
		&pairs_file,
		serde_json::json!([{
			"local": local.path(),
			"remote": test_dir_name,
			"syncMode": "twoWay",
			"excludeDotFiles": true,
		}])
		.to_string(),
	)
	.unwrap();

	let output = sync(config_dir.path(), &["--once", pairs_file.to_str().unwrap()]).await;

	assert_success(&output);
	assert_eq!(
		std::fs::read_to_string(local.path().join(".filenignore")).unwrap(),
		".*\n!/.filenignore\n"
	);
	assert_eq!(
		remote_content(client, &format!("{test_dir_name}/visible.txt")).await,
		b"goes up"
	);
	assert_eq!(
		remote_content(client, &format!("{test_dir_name}/.filenignore")).await,
		b".*\n!/.filenignore\n"
	);
	assert!(
		client
			.find_item_at_path(&format!("{test_dir_name}/.secret"))
			.await
			.unwrap()
			.is_none(),
		"{}",
		describe(&output)
	);
}

/// A remote directory that does not exist is created, with the missing one above it, when `--yes`
/// answers the question whether to create it.
#[shared_test_runtime]
async fn sync_once_with_yes_creates_a_missing_remote_directory() {
	// Not `get_resources_with_lock`, for the reason the first test gives.
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir_name = resources.dir.name().unwrap();
	let local = assert_fs::TempDir::new().unwrap();
	std::fs::write(local.path().join("up.txt"), "goes up").unwrap();
	let config_dir = assert_fs::TempDir::new().unwrap();
	let remote = format!("{test_dir_name}/missing/nested");
	let pair = format!("{}:{remote}", local.path().to_str().unwrap());

	let output = sync_once(config_dir.path(), &pair, &["--yes"]).await;

	assert_success(&output);
	assert!(
		String::from_utf8_lossy(&output.stdout).contains("Created the remote directory"),
		"{}",
		describe(&output)
	);
	assert_eq!(
		remote_content(client, &format!("{remote}/up.txt")).await,
		b"goes up"
	);
}

/// A local directory that does not exist is created, with the missing one above it, when `--yes`
/// answers the question whether to create it, and a pair that only downloads fills it.
#[shared_test_runtime]
async fn sync_once_with_yes_creates_a_missing_local_directory() {
	// Not `get_resources_with_lock`, for the reason the first test gives.
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir_name = resources.dir.name().unwrap();
	let file = client
		.make_file_builder("down.txt", resources.dir.uuid)
		.unwrap();
	client.upload_file(file, b"comes down").await.unwrap();
	let parent = assert_fs::TempDir::new().unwrap();
	let local = parent.path().join("missing").join("nested");
	let config_dir = assert_fs::TempDir::new().unwrap();
	let pair = format!("{}:remoteToLocal:{test_dir_name}", local.to_str().unwrap());

	let output = sync_once(config_dir.path(), &pair, &["--yes"]).await;

	assert_success(&output);
	assert!(
		String::from_utf8_lossy(&output.stdout).contains("Created the local directory"),
		"{}",
		describe(&output)
	);
	assert_eq!(
		std::fs::read_to_string(local.join("down.txt")).unwrap(),
		"comes down"
	);
}

/// Without a terminal to ask at and without `--yes`, a missing remote directory is not created,
/// and the command says to pass `--yes`.
#[shared_test_runtime]
async fn sync_once_without_yes_or_a_terminal_leaves_a_missing_remote_directory_alone() {
	// Not `get_resources_with_lock`, for the reason the first test gives.
	let resources = test_utils::RESOURCES.get_resources().await;
	let test_dir_name = resources.dir.name().unwrap();
	let local = assert_fs::TempDir::new().unwrap();
	let config_dir = assert_fs::TempDir::new().unwrap();
	let remote = format!("{test_dir_name}/missing");
	let pair = format!("{}:{remote}", local.path().to_str().unwrap());

	let output = sync_once(config_dir.path(), &pair, &[]).await;

	assert_eq!(output.status.code(), Some(2), "{}", describe(&output));
	assert!(
		String::from_utf8_lossy(&output.stderr).contains("Pass --yes to answer yes."),
		"{}",
		describe(&output)
	);
	assert!(
		resources
			.client
			.find_item_at_path(&remote)
			.await
			.unwrap()
			.is_none()
	);
}

/// The state the TypeScript CLI kept for a pair, under the uuid of the pair as it was given, is
/// imported when `sync` sets the pair up.
#[shared_test_runtime]
async fn sync_once_imports_the_typescript_clis_state_of_a_new_pair() {
	// Not `get_resources_with_lock`, for the reason the first test gives.
	let resources = test_utils::RESOURCES.get_resources().await;
	let test_dir_name = resources.dir.name().unwrap();
	let local = assert_fs::TempDir::new().unwrap();
	let config_dir = assert_fs::TempDir::new().unwrap();
	let pair = format!("{}:{test_dir_name}", local.path().to_str().unwrap());
	// one file both trees list at the same size, as filen-sync wrote them
	let state_dir = config_dir
		.path()
		.join("sync/state/v2")
		.join(uuid_v5(Uuid::nil(), &pair).to_string());
	std::fs::create_dir_all(&state_dir).unwrap();
	std::fs::write(
		state_dir.join("previousLocalTree"),
		r#"{"prop":"/a.txt","data":{"lastModified":1700000000123,"type":"file","path":"/a.txt","size":5,"creation":1600000000000,"inode":42}}"#,
	)
	.unwrap();
	std::fs::write(
		state_dir.join("previousRemoteTree"),
		format!(
			r#"{{"prop":"/a.txt","data":{{"type":"file","uuid":"{}","name":"a.txt","size":5,"path":"/a.txt"}}}}"#,
			Uuid::from_u128(1)
		),
	)
	.unwrap();

	let output = sync_once(config_dir.path(), &pair, &[]).await;

	assert_success(&output);
	assert!(
		String::from_utf8_lossy(&output.stdout).contains(&format!(
			"Imported the TypeScript CLI's sync state of the sync pair {} <> /{test_dir_name}: 1 \
			 synced file(s), 0 unreadable line(s) skipped",
			local.path().display()
		)),
		"{}",
		describe(&output)
	);
}

/// A two-way pair switched to `localBackup` keeps on the remote what is deleted locally, and
/// `list-sync-pairs` shows its new mode.
#[shared_test_runtime]
async fn set_sync_mode_switches_a_registered_pair_from_its_next_pass() {
	// Not `get_resources_with_lock`, for the reason the first test gives.
	let resources = test_utils::RESOURCES.get_resources().await;
	let test_dir_name = resources.dir.name().unwrap();
	let local = assert_fs::TempDir::new().unwrap();
	std::fs::write(local.path().join("kept.txt"), "backed up").unwrap();
	let config_dir = assert_fs::TempDir::new().unwrap();
	let local_root = local.path().to_str().unwrap();
	let pair = format!("{local_root}:{test_dir_name}");
	assert_success(&sync_once(config_dir.path(), &pair, &[]).await);

	let output = filen(config_dir.path(), &["set-sync-mode", local_root, "lb"]).await;

	assert_success(&output);
	let listed = filen(config_dir.path(), &["list-sync-pairs", "--json"]).await;
	assert_success(&listed);
	let listed = serde_json::from_slice::<serde_json::Value>(&listed.stdout).unwrap();
	assert_eq!(listed[0]["mode"], "localBackup", "{listed}");
	assert_eq!(listed[0]["remote"], test_dir_name, "{listed}");
	std::fs::remove_file(local.path().join("kept.txt")).unwrap();
	// in its new mode, which `sync` must be given it in from now on
	let output = sync_once(
		config_dir.path(),
		&format!("{local_root}:lb:{test_dir_name}"),
		&[],
	)
	.await;
	assert_success(&output);
	assert_eq!(
		remote_content(&resources.client, &format!("{test_dir_name}/kept.txt")).await,
		b"backed up"
	);
}

/// A pair given in another mode than the one it is registered in is refused, with the mode it has.
#[shared_test_runtime]
async fn sync_with_a_pair_in_another_mode_than_registered_names_its_mode() {
	// Not `get_resources_with_lock`, for the reason the first test gives.
	let resources = test_utils::RESOURCES.get_resources().await;
	let test_dir_name = resources.dir.name().unwrap();
	let local = assert_fs::TempDir::new().unwrap();
	let config_dir = assert_fs::TempDir::new().unwrap();
	let local_root = local.path().to_str().unwrap();
	assert_success(
		&sync_once(
			config_dir.path(),
			&format!("{local_root}:{test_dir_name}"),
			&[],
		)
		.await,
	);

	let output = sync_once(
		config_dir.path(),
		&format!("{local_root}:lb:{test_dir_name}"),
		&[],
	)
	.await;

	assert_eq!(output.status.code(), Some(1), "{}", describe(&output));
	assert!(
		String::from_utf8_lossy(&output.stderr)
			.contains("as localBackup: it is registered as twoWay. Give its mode as twoWay"),
		"{}",
		describe(&output)
	);
}

/// A conflict a pass left is listed by `list-sync-conflicts`, and the resolution
/// `resolve-sync-conflict` gives is applied by the next `sync`.
#[shared_test_runtime]
async fn a_listed_conflict_is_resolved_by_resolve_sync_conflict() {
	// Not `get_resources_with_lock`, for the reason the first test gives.
	let resources = test_utils::RESOURCES.get_resources().await;
	let test_dir_name = resources.dir.name().unwrap();
	let (local, config_dir, pair) = diverged_pair(&resources).await;
	let local_root = local.path().to_str().unwrap();
	assert_eq!(
		sync_once(config_dir.path(), &pair, &[]).await.status.code(),
		Some(1)
	);

	let listed = filen(
		config_dir.path(),
		&["list-sync-conflicts", local_root, "--json"],
	)
	.await;

	assert_success(&listed);
	assert_eq!(
		serde_json::from_slice::<serde_json::Value>(&listed.stdout).unwrap(),
		serde_json::json!([{"path": "fight.txt", "local": "file", "remote": "file"}])
	);
	let resolved = filen(
		config_dir.path(),
		&[
			"resolve-sync-conflict",
			local_root,
			"fight.txt",
			"--keep-local",
		],
	)
	.await;
	assert_success(&resolved);
	assert_success(&sync_once(config_dir.path(), &pair, &[]).await);
	assert_eq!(
		remote_content(&resources.client, &format!("{test_dir_name}/fight.txt")).await,
		b"the local edit"
	);
}

/// A pair `pause-sync-pair` paused is not synced, which `sync` says with how to resume it, and
/// `resume-sync-pair` lets `sync` sync it again.
#[shared_test_runtime]
async fn a_paused_pair_is_left_alone_until_it_is_resumed() {
	// Not `get_resources_with_lock`, for the reason the first test gives.
	let resources = test_utils::RESOURCES.get_resources().await;
	let test_dir_name = resources.dir.name().unwrap();
	let local = assert_fs::TempDir::new().unwrap();
	let config_dir = assert_fs::TempDir::new().unwrap();
	let local_root = local.path().to_str().unwrap();
	let pair = format!("{local_root}:{test_dir_name}");
	assert_success(&sync_once(config_dir.path(), &pair, &[]).await);

	assert_success(&filen(config_dir.path(), &["pause-sync-pair", local_root]).await);
	std::fs::write(local.path().join("later.txt"), "after the pause").unwrap();
	let paused = sync_once(config_dir.path(), &pair, &[]).await;

	assert_eq!(paused.status.code(), Some(1), "{}", describe(&paused));
	assert!(
		String::from_utf8_lossy(&paused.stderr)
			.contains("is paused, so it is not synced. Resume it with `filen resume-sync-pair 1`."),
		"{}",
		describe(&paused)
	);
	assert!(
		resources
			.client
			.find_item_at_path(&format!("{test_dir_name}/later.txt"))
			.await
			.unwrap()
			.is_none()
	);
	// nor does a dry run plan a pass that `sync` would not run
	let dry_run = sync(config_dir.path(), &["--dry-run", &pair]).await;
	assert_success(&dry_run);
	assert!(
		String::from_utf8_lossy(&dry_run.stderr).contains("is paused, so it is not synced"),
		"{}",
		describe(&dry_run)
	);
	assert!(
		!String::from_utf8_lossy(&dry_run.stdout).contains("would:"),
		"{}",
		describe(&dry_run)
	);
	assert_success(&filen(config_dir.path(), &["resume-sync-pair", "1"]).await);
	assert_success(&sync_once(config_dir.path(), &pair, &[]).await);
	assert_eq!(
		remote_content(&resources.client, &format!("{test_dir_name}/later.txt")).await,
		b"after the pause"
	);
}

/// `sync --dry-run` prints the upload a pass would make, and makes none.
#[shared_test_runtime]
async fn sync_dry_run_prints_the_planned_upload_and_transfers_nothing() {
	// Not `get_resources_with_lock`, for the reason the first test gives.
	let resources = test_utils::RESOURCES.get_resources().await;
	let test_dir_name = resources.dir.name().unwrap();
	let local = assert_fs::TempDir::new().unwrap();
	std::fs::write(local.path().join("planned.txt"), "not yet").unwrap();
	let config_dir = assert_fs::TempDir::new().unwrap();
	let pair = format!("{}:{test_dir_name}", local.path().to_str().unwrap());

	let output = sync(config_dir.path(), &["--dry-run", &pair]).await;

	assert_success(&output);
	assert!(
		String::from_utf8_lossy(&output.stdout).contains("  upload planned.txt (7 B)"),
		"{}",
		describe(&output)
	);
	assert!(
		resources
			.client
			.find_item_at_path(&format!("{test_dir_name}/planned.txt"))
			.await
			.unwrap()
			.is_none(),
		"{}",
		describe(&output)
	);
}

/// A pair whose first `sync --once` agreed on `fight.txt`, which has since been changed on both
/// sides, with the local root and the config dir that holds its database.
async fn diverged_pair(
	resources: &test_utils::TestResources,
) -> (assert_fs::TempDir, assert_fs::TempDir, String) {
	let client = &resources.client;
	let test_dir_name = resources.dir.name().unwrap();
	let local = assert_fs::TempDir::new().unwrap();
	let config_dir = assert_fs::TempDir::new().unwrap();
	let pair = format!("{}:{test_dir_name}", local.path().to_str().unwrap());

	std::fs::write(local.path().join("fight.txt"), "the agreed content").unwrap();
	assert_success(&sync_once(config_dir.path(), &pair, &[]).await);

	std::fs::write(local.path().join("fight.txt"), "the local edit").unwrap();
	let file = client
		.make_file_builder("fight.txt", resources.dir.uuid)
		.unwrap();
	client
		.upload_file(file, b"the remote edit, which is longer")
		.await
		.unwrap();
	(local, config_dir, pair)
}

/// Runs `sync --once --pair <pair> <args>` with `config_dir`.
async fn sync_once(config_dir: &Path, pair: &str, args: &[&str]) -> Output {
	sync(config_dir, &[&["--once", "--pair", pair], args].concat()).await
}

/// Runs `sync <args>` with `config_dir`.
async fn sync(config_dir: &Path, args: &[&str]) -> Output {
	filen(config_dir, &[&["sync"], args].concat()).await
}

/// Taken by every CLI run of these tests: shared by a run that ends on its own, held alone by a
/// sync that watches until a deadline ([`sync_until`]).
///
/// Every run needs the account's drive lock for its passes, and the server hands it to whichever
/// poll reaches it first after a release. With the tests running at once, a watching sync's pass
/// polls every 5 s against about ten other runs of these tests, and lost every poll for two
/// minutes, past the deadline its test gives it. Holding this alone, the watching sync competes
/// only with what the test process itself does on the drive.
static CLI_RUNS: RwLock<()> = RwLock::const_new(());

/// Runs the CLI with `args` and `config_dir`.
async fn filen(config_dir: &Path, args: &[&str]) -> Output {
	let _runs = CLI_RUNS.read().await;
	let (_auth_config_dir, auth_config) = test_utils::cli::prepare_cli_auth_config().await;
	Command::new(env!("CARGO_BIN_EXE_filen-cli"))
		.args([
			"--auth-config-path",
			auth_config.to_str().unwrap(),
			"--config-dir",
			config_dir.to_str().unwrap(),
			"--skip-update",
		])
		.args(args)
		.output()
		.unwrap()
}

/// Runs `sync <args>` with `config_dir` until `done` holds, polled every 2 s for at most `within`,
/// then stops it with Ctrl-C, so that it finishes its pass and releases the drive lock instead of
/// leaving it to expire on the shared test account. Fails unless it exits with 0. No other CLI run
/// of these tests runs meanwhile (see [`CLI_RUNS`]), and `within` starts once the others ended.
#[cfg(unix)]
async fn sync_until(
	config_dir: &Path,
	args: &[&str],
	within: Duration,
	mut done: impl AsyncFnMut() -> bool,
) -> Output {
	let _runs = CLI_RUNS.write().await;
	let (_auth_config_dir, auth_config) = test_utils::cli::prepare_cli_auth_config().await;
	let mut cli = Command::new(env!("CARGO_BIN_EXE_filen-cli"))
		.args([
			"--auth-config-path",
			auth_config.to_str().unwrap(),
			"--config-dir",
			config_dir.to_str().unwrap(),
			"--skip-update",
			"sync",
		])
		.args(args)
		.stdout(Stdio::piped())
		.stderr(Stdio::piped())
		.spawn()
		.unwrap();

	let deadline = Instant::now() + within;
	while !done().await {
		if Instant::now() >= deadline || cli.try_wait().unwrap().is_some() {
			let _ = cli.kill();
			panic!(
				"sync did not get there in time\n{}",
				describe(&cli.wait_with_output().unwrap())
			);
		}
		tokio::time::sleep(Duration::from_secs(2)).await;
	}

	let interrupted = Command::new("kill")
		.args(["-INT", &cli.id().to_string()])
		.status()
		.unwrap();
	assert!(interrupted.success());
	let output = cli.wait_with_output().unwrap();
	assert_success(&output);
	output
}

fn assert_success(output: &Output) {
	assert!(output.status.success(), "{}", describe(output));
}

fn describe(output: &Output) -> String {
	format!(
		"exit status: {}\nstdout:\n{}\nstderr:\n{}",
		output.status,
		String::from_utf8_lossy(&output.stdout),
		String::from_utf8_lossy(&output.stderr)
	)
}

async fn remote_content(client: &Client, path: &str) -> Vec<u8> {
	match client.find_item_at_path(path).await.unwrap() {
		Some(NonRootFileType::File(file)) => client.download_file(file.as_ref()).await.unwrap(),
		other => panic!("expected a file at {path}, got {other:?}"),
	}
}
