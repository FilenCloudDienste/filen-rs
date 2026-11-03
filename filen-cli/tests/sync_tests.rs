//! `sync --once`, run as a user runs it, against the test account.

use std::{
	path::Path,
	process::{Command, Output},
	sync::Arc,
};

use filen_macros::shared_test_runtime;
use filen_sdk_rs::{
	auth::Client,
	fs::{HasName, categories::NonRootFileType},
	io::client_impl::IoSharedClientExt as _,
	sync_engine::SyncEngine,
};

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
	let (_auth_config_dir, auth_config) = test_utils::cli::prepare_cli_auth_config().await;
	Command::new(env!("CARGO_BIN_EXE_filen-cli"))
		.args([
			"--auth-config-path",
			auth_config.to_str().unwrap(),
			"--config-dir",
			config_dir.to_str().unwrap(),
			"--skip-update",
			"sync",
		])
		.args(args)
		.output()
		.unwrap()
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
