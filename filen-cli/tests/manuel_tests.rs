// Linux only: the recordings print the default config directory, which is `~/.config` on Linux
// and `~/Library/Application Support` on macOS. Windows has no bash for manuel to drive.
#![cfg(target_os = "linux")]

use std::io::Write as _;

use chrono::{TimeDelta, Utc};
use filen_sdk_rs::{
	auth::Client,
	fs::{
		HasName as _,
		categories::{DirType, Normal},
		dir::RemoteDirectory,
	},
};

/// The remote directory the recordings work in.
const REMOTE_TEST_ROOT: &str = "/filen-cli-testing";

/// How every recording's remote directory under [`REMOTE_TEST_ROOT`] starts (`set-remote-cwd` in
/// `manuel_recordings/.bashrc`), followed by its run's id.
const REMOTE_DIR_PREFIX: &str = "manuel-";

/// How old a leftover recording directory has to be before a run deletes it. A younger one may
/// belong to a run still going, from a CI leg or a developer on the same account, or to a failed
/// run someone is debugging.
const STALE_AFTER: TimeDelta = TimeDelta::hours(24);

#[filen_macros::shared_test_runtime]
// std::env::set_var is unsafe since edition 2024.
#[allow(unsafe_code)]
async fn run_manuel_tests() {
	// export auth config to tmp file
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let auth_config =
		filen_cli::serialize_auth_config(client).expect("Failed to serialize auth config");
	let mut temp_file = tempfile::NamedTempFile::new().expect("Failed to create temp file");
	let auth_config_path = temp_file
		.path()
		.to_str()
		.expect("Failed to convert temp file path to str")
		.to_string();
	write!(temp_file, "{}", auth_config).expect("Failed to write auth config to temp file");
	dotenv::dotenv().ok(); // loads TEST_EMAIL and TEST_PASSWORD from .env file, just like test_utils does
	// Every recording of this run names its remote directory `manuel-<run id>-<random>`, so the
	// run can find and delete its own directories afterwards.
	let run_id = format!("{:016x}", rand::random::<u64>());
	unsafe {
		std::env::set_var("OVERRIDE_TEST_AUTH_CONFIG_PATH", auth_config_path);
		std::env::set_var("MANUEL_RUN_ID", &run_id);
	}

	// create remote test root /filen-cli-testing
	let remote_root = client
		.find_or_create_dir(REMOTE_TEST_ROOT)
		.await
		.expect("Failed to find or create remote test root");

	// Runs that failed, or were killed, leave their directories behind.
	let stale_before = Utc::now() - STALE_AFTER;
	delete_recording_dirs(client, &remote_root, |dir| dir.timestamp < stale_before).await;

	let write_diffs_to_file = {
		let term_program = std::env::var("TERM_PROGRAM").unwrap_or_default();
		term_program == "vscode" || term_program == "tmux" // non-exhaustive
	};

	manuel::run_manuel_tests_in_dir(
		"tests/manuel_recordings",
		true,
		std::time::Duration::from_secs(300),
		!write_diffs_to_file,
		false,
	);

	// Only reached when every recording passed: a failed run keeps its directories for
	// debugging, and a later run deletes them once they are stale.
	let own_prefix = format!("{REMOTE_DIR_PREFIX}{run_id}-");
	delete_recording_dirs(client, &remote_root, |dir| {
		dir.name().is_some_and(|name| name.starts_with(&own_prefix))
	})
	.await;
}

/// Permanently deletes the recording directories directly under `remote_root` that `select`
/// picks. A failure is printed, not raised: a directory left behind is deleted by a later run,
/// once it is stale.
async fn delete_recording_dirs(
	client: &Client,
	remote_root: &DirType<'_, Normal>,
	select: impl Fn(&RemoteDirectory) -> bool,
) {
	let dirs = match client
		.list_dir::<_, Normal>(remote_root, None::<&fn(u64, Option<u64>)>)
		.await
	{
		Ok((dirs, _)) => dirs,
		Err(e) => {
			eprintln!("Failed to list {REMOTE_TEST_ROOT} to clean up recording directories: {e}");
			return;
		}
	};
	for dir in dirs.into_iter().filter(|dir| {
		dir.name()
			.is_some_and(|name| name.starts_with(REMOTE_DIR_PREFIX))
			&& select(dir)
	}) {
		let uuid = dir.uuid;
		if let Err(e) = client.delete_dir_permanently(dir).await {
			eprintln!("Failed to delete recording directory {uuid} under {REMOTE_TEST_ROOT}: {e}");
		}
	}
}
