//! `sync --once`, run as a user runs it, against the test account.

use std::{
	path::Path,
	process::{Command, Output},
};

use filen_macros::shared_test_runtime;
use filen_sdk_rs::{
	auth::Client,
	fs::{HasName, categories::NonRootFileType},
	io::client_impl::IoSharedClientExt as _,
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
	let (_auth_config_dir, auth_config) = test_utils::cli::prepare_cli_auth_config().await;
	Command::new(env!("CARGO_BIN_EXE_filen-cli"))
		.args([
			"--auth-config-path",
			auth_config.to_str().unwrap(),
			"--config-dir",
			config_dir.to_str().unwrap(),
			"--skip-update",
			"sync",
			"--once",
			"--pair",
			pair,
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
