use std::{
	path::{Path, PathBuf},
	process::Stdio,
	time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use filen_macros::shared_test_runtime;
use filen_sdk_rs::{
	auth::Client,
	connect::DirPublicLinkRW,
	fs::{
		HasName, HasUUID as _,
		categories::{DirType, NonRootFileType},
		dir::RemoteDirectory,
		file::{FileBuilderOptionalName, traits::HasFileInfo as _},
	},
	io::client_impl::IoSharedClientExt as _,
};
use filen_types::fs::Uuid;
use test_utils::{
	authenticated_cli_in_config_dir, authenticated_cli_with_args,
	cli::{exists, file_uuid, is_file, logged_out_command},
	upload_journal::{NotifyingJournalStore, resumable_upload_contents},
};

#[shared_test_runtime]
async fn uploading_a_directory_uploads_its_files() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir_name = resources.dir.name().unwrap();

	let local = tempfile::tempdir().unwrap();
	let source = local.path().join("uploaded");
	std::fs::create_dir_all(source.join("nested")).unwrap();
	std::fs::write(source.join("nested").join("a.txt"), b"contents").unwrap();

	authenticated_cli_with_args!("upload", source.to_str().unwrap(), test_dir_name).success();

	assert!(is_file(client, &format!("{test_dir_name}/uploaded/nested/a.txt")).await);
}

#[cfg(unix)]
#[shared_test_runtime]
async fn uploading_a_symlinked_file_keeps_the_links_name() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir_name = resources.dir.name().unwrap();

	let local = tempfile::tempdir().unwrap();
	std::fs::write(local.path().join("target.txt"), b"contents").unwrap();
	std::os::unix::fs::symlink(
		local.path().join("target.txt"),
		local.path().join("link.txt"),
	)
	.unwrap();

	authenticated_cli_with_args!(
		"upload",
		local.path().join("link.txt").to_str().unwrap(),
		test_dir_name
	)
	.success();

	assert!(is_file(client, &format!("{test_dir_name}/link.txt")).await);
	assert!(!exists(client, &format!("{test_dir_name}/target.txt")).await);
}

#[shared_test_runtime]
async fn uploading_to_a_new_file_path_renames_the_file() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir_name = resources.dir.name().unwrap();

	let local = tempfile::tempdir().unwrap();
	std::fs::create_dir(local.path().join("a")).unwrap();
	std::fs::write(local.path().join("a").join("b.txt"), b"contents").unwrap();
	authenticated_cli_with_args!("mkdir", &format!("{test_dir_name}/dir")).success();

	authenticated_cli_with_args!(
		"upload",
		local.path().join("a").join("b.txt").to_str().unwrap(),
		&format!("{test_dir_name}/dir/new.txt")
	)
	.success();

	assert!(is_file(client, &format!("{test_dir_name}/dir/new.txt")).await);
	assert!(!exists(client, &format!("{test_dir_name}/dir/b.txt")).await);
}

#[shared_test_runtime]
async fn uploading_into_a_missing_directory_creates_it() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir_name = resources.dir.name().unwrap();

	let local = tempfile::tempdir().unwrap();
	std::fs::write(local.path().join("a.txt"), b"contents").unwrap();

	authenticated_cli_with_args!(
		"upload",
		local.path().join("a.txt").to_str().unwrap(),
		&format!("{test_dir_name}/x/y/z/")
	)
	.success();

	assert!(is_file(client, &format!("{test_dir_name}/x/y/z/a.txt")).await);
}

#[shared_test_runtime]
async fn downloading_to_a_new_path_in_a_missing_directory_renames_the_file() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let test_dir_name = resources.dir.name().unwrap();

	let local = tempfile::tempdir().unwrap();
	std::fs::write(local.path().join("a.txt"), b"contents").unwrap();
	authenticated_cli_with_args!(
		"upload",
		local.path().join("a.txt").to_str().unwrap(),
		test_dir_name
	)
	.success();

	let target = local.path().join("missing").join("renamed.txt");
	authenticated_cli_with_args!(
		"download",
		&format!("{test_dir_name}/a.txt"),
		target.to_str().unwrap()
	)
	.success();

	assert_eq!(std::fs::read(&target).unwrap(), b"contents");
}

#[shared_test_runtime]
async fn uploading_with_json_prints_the_result_as_json() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let test_dir_name = resources.dir.name().unwrap();

	let local = tempfile::tempdir().unwrap();
	std::fs::write(local.path().join("a.txt"), b"contents").unwrap();

	let output = authenticated_cli_with_args!(
		"upload",
		"--json",
		local.path().join("a.txt").to_str().unwrap(),
		test_dir_name
	)
	.success();

	let result: serde_json::Value = serde_json::from_slice(&output.get_output().stdout).unwrap();
	assert_eq!(
		result,
		serde_json::json!({
			"destination": format!("/{test_dir_name}/a.txt"),
			"files": 1,
			"bytes": 8,
			"resumed_bytes": 0,
			"skipped": 0,
			"skipped_symlinks": [],
			"failed": [],
		})
	);
}

#[shared_test_runtime]
async fn uploading_a_directory_verbosely_lists_its_files() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let test_dir_name = resources.dir.name().unwrap();

	let local = tempfile::tempdir().unwrap();
	let source = local.path().join("listed");
	std::fs::create_dir_all(source.join("nested")).unwrap();
	std::fs::write(source.join("nested").join("a.txt"), b"contents").unwrap();

	// The test harness always passes -v.
	let output =
		authenticated_cli_with_args!("upload", source.to_str().unwrap(), test_dir_name).success();

	let stdout = String::from_utf8_lossy(&output.get_output().stdout);
	assert!(
		stdout
			.lines()
			.any(|line| line == format!("/{test_dir_name}/listed/nested/a.txt")),
		"{stdout}"
	);
	assert!(stdout.contains("1 file, 8 B in "), "{stdout}");
}

/// `len` bytes of a fixed pattern that does not repeat at chunk boundaries.
fn pattern(len: usize) -> Vec<u8> {
	(0..len)
		.map(|i| u8::try_from(i % 251).expect("below 251"))
		.collect()
}

fn set_modified(path: &Path, modified: SystemTime) {
	std::fs::File::options()
		.write(true)
		.open(path)
		.unwrap()
		.set_modified(modified)
		.unwrap();
}

#[shared_test_runtime]
async fn a_file_comes_back_byte_for_byte_after_an_upload_and_a_download() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let test_dir_name = resources.dir.name().unwrap();

	let local = tempfile::tempdir().unwrap();
	// Past the 1 MiB chunk size, so that the file spans two chunks.
	let contents = pattern(1024 * 1024 + 512 * 1024 + 7);
	std::fs::write(local.path().join("data.bin"), &contents).unwrap();

	authenticated_cli_with_args!(
		"upload",
		local.path().join("data.bin").to_str().unwrap(),
		test_dir_name
	)
	.success();
	let back = local.path().join("back.bin");
	authenticated_cli_with_args!(
		"download",
		&format!("{test_dir_name}/data.bin"),
		back.to_str().unwrap()
	)
	.success();

	assert!(std::fs::read(&back).unwrap() == contents);
}

#[shared_test_runtime]
async fn uploading_a_changed_file_again_replaces_it_under_the_same_name() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir_name = resources.dir.name().unwrap();

	let local = tempfile::tempdir().unwrap();
	let source = local.path().join("a.txt");
	std::fs::write(&source, b"first").unwrap();
	authenticated_cli_with_args!("upload", source.to_str().unwrap(), test_dir_name).success();
	let first = file_uuid(client, &format!("{test_dir_name}/a.txt")).await;

	std::fs::write(&source, b"second version").unwrap();
	authenticated_cli_with_args!("upload", source.to_str().unwrap(), test_dir_name).success();
	let second = file_uuid(client, &format!("{test_dir_name}/a.txt")).await;
	assert_ne!(first, second);

	let back = local.path().join("back.txt");
	authenticated_cli_with_args!(
		"download",
		&format!("{test_dir_name}/a.txt"),
		back.to_str().unwrap()
	)
	.success();
	assert_eq!(std::fs::read(&back).unwrap(), b"second version");
}

#[shared_test_runtime]
async fn downloading_onto_a_differing_local_file_replaces_it() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let test_dir_name = resources.dir.name().unwrap();

	let local = tempfile::tempdir().unwrap();
	std::fs::write(local.path().join("a.txt"), b"remote contents").unwrap();
	authenticated_cli_with_args!(
		"upload",
		local.path().join("a.txt").to_str().unwrap(),
		test_dir_name
	)
	.success();

	let target = local.path().join("target.txt");
	std::fs::write(&target, b"local").unwrap();
	authenticated_cli_with_args!(
		"download",
		&format!("{test_dir_name}/a.txt"),
		target.to_str().unwrap()
	)
	.success();

	assert_eq!(std::fs::read(&target).unwrap(), b"remote contents");
}

#[shared_test_runtime]
async fn downloading_onto_an_identical_local_file_leaves_it_alone() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let test_dir_name = resources.dir.name().unwrap();

	let local = tempfile::tempdir().unwrap();
	std::fs::write(local.path().join("a.txt"), b"same contents").unwrap();
	authenticated_cli_with_args!(
		"upload",
		local.path().join("a.txt").to_str().unwrap(),
		test_dir_name
	)
	.success();

	// A replaced file would take the uploaded file's modification time, which is now.
	let target = local.path().join("target.txt");
	std::fs::write(&target, b"same contents").unwrap();
	let old = UNIX_EPOCH + Duration::from_secs(1_000_000_000);
	set_modified(&target, old);
	authenticated_cli_with_args!(
		"download",
		&format!("{test_dir_name}/a.txt"),
		target.to_str().unwrap()
	)
	.success();

	assert_eq!(std::fs::metadata(&target).unwrap().modified().unwrap(), old);
	assert_eq!(std::fs::read(&target).unwrap(), b"same contents");
}

#[shared_test_runtime]
async fn a_files_modification_time_survives_an_upload_and_a_download() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let test_dir_name = resources.dir.name().unwrap();

	let local = tempfile::tempdir().unwrap();
	let source = local.path().join("a.txt");
	std::fs::write(&source, b"contents").unwrap();
	// The drive keeps times to the millisecond.
	let modified = UNIX_EPOCH + Duration::from_millis(1_600_000_000_123);
	set_modified(&source, modified);

	authenticated_cli_with_args!("upload", source.to_str().unwrap(), test_dir_name).success();
	let back = local.path().join("back.txt");
	authenticated_cli_with_args!(
		"download",
		&format!("{test_dir_name}/a.txt"),
		back.to_str().unwrap()
	)
	.success();

	assert_eq!(
		std::fs::metadata(&back).unwrap().modified().unwrap(),
		modified
	);
}

#[shared_test_runtime]
async fn downloading_the_whole_drive_starts_writing_into_the_destination() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let test_dir_name = resources.dir.name().unwrap();

	// So the drive holds at least this to download, whatever else is in it.
	let local = tempfile::tempdir().unwrap();
	std::fs::write(local.path().join("a.txt"), b"contents").unwrap();
	authenticated_cli_with_args!(
		"upload",
		local.path().join("a.txt").to_str().unwrap(),
		test_dir_name
	)
	.success();

	let destination = tempfile::tempdir().unwrap();
	let (_auth_dir, auth_config) = test_utils::cli::prepare_cli_auth_config().await;
	let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_filen-cli"))
		.arg("--auth-config-path")
		.arg(auth_config.path())
		.args(["download", "/"])
		.arg(destination.path())
		.stdin(Stdio::null())
		.stdout(Stdio::null())
		.stderr(Stdio::null())
		.spawn()
		.unwrap();

	// The account's whole drive may be large: stop the download once it has written something.
	let deadline = Instant::now() + Duration::from_secs(240);
	let wrote = loop {
		let wrote = std::fs::read_dir(destination.path())
			.unwrap()
			.next()
			.is_some();
		if wrote || Instant::now() > deadline || child.try_wait().unwrap().is_some() {
			break wrote;
		}
		tokio::time::sleep(Duration::from_millis(200)).await;
	};
	let _ = child.kill();
	child.wait().unwrap();

	assert!(wrote, "downloading / wrote nothing into the destination");
}

#[cfg(unix)]
#[shared_test_runtime]
async fn an_unreadable_file_fails_only_itself_and_the_upload_exits_nonzero() {
	use std::os::unix::fs::PermissionsExt as _;

	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir_name = resources.dir.name().unwrap();

	let local = tempfile::tempdir().unwrap();
	let source = local.path().join("partial");
	std::fs::create_dir(&source).unwrap();
	std::fs::write(source.join("ok.txt"), b"contents").unwrap();
	let locked = source.join("locked.txt");
	std::fs::write(&locked, b"secret").unwrap();
	std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
	assert!(
		std::fs::File::open(&locked).is_err(),
		"this test needs a user that cannot read a file of mode 000 (not root)"
	);

	let output =
		authenticated_cli_with_args!("upload", source.to_str().unwrap(), test_dir_name).failure();

	let output = output.get_output();
	let printed = format!(
		"{}{}",
		String::from_utf8_lossy(&output.stdout),
		String::from_utf8_lossy(&output.stderr)
	);
	assert!(printed.contains("locked.txt"), "{printed}");
	assert!(is_file(client, &format!("{test_dir_name}/partial/ok.txt")).await);
	assert!(!exists(client, &format!("{test_dir_name}/partial/locked.txt")).await);
}

#[shared_test_runtime]
async fn paths_with_spaces_round_trip() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir_name = resources.dir.name().unwrap();

	let local = tempfile::tempdir().unwrap();
	let source = local.path().join("my dir");
	std::fs::create_dir(&source).unwrap();
	std::fs::write(source.join("a file.txt"), b"spaced contents").unwrap();

	authenticated_cli_with_args!(
		"upload",
		source.to_str().unwrap(),
		&format!("{test_dir_name}/remote dir/")
	)
	.success();
	assert!(
		is_file(
			client,
			&format!("{test_dir_name}/remote dir/my dir/a file.txt")
		)
		.await
	);

	let back = local.path().join("out dir").join("copy dir");
	authenticated_cli_with_args!(
		"download",
		&format!("{test_dir_name}/remote dir/my dir"),
		back.to_str().unwrap()
	)
	.success();
	assert_eq!(
		std::fs::read(back.join("a file.txt")).unwrap(),
		b"spaced contents"
	);
}

#[cfg(unix)]
#[shared_test_runtime]
async fn uploading_without_following_symlinks_skips_and_lists_them() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir_name = resources.dir.name().unwrap();

	let local = tempfile::tempdir().unwrap();
	let outside = local.path().join("outside");
	std::fs::create_dir(&outside).unwrap();
	std::fs::write(outside.join("x.txt"), b"outside").unwrap();
	let source = local.path().join("with-links");
	std::fs::create_dir(&source).unwrap();
	std::fs::write(source.join("real.txt"), b"real").unwrap();
	std::os::unix::fs::symlink(source.join("real.txt"), source.join("link.txt")).unwrap();
	std::os::unix::fs::symlink(&outside, source.join("link-dir")).unwrap();

	// The test harness always passes -v.
	let output = authenticated_cli_with_args!(
		"upload",
		"--no-follow-symlinks",
		source.to_str().unwrap(),
		test_dir_name
	)
	.success();

	let stdout = String::from_utf8_lossy(&output.get_output().stdout);
	for link in ["link.txt", "link-dir"] {
		let line = format!("Skipped symlink {}", source.join(link).display());
		assert!(stdout.lines().any(|l| l == line), "{stdout}");
		assert!(!exists(client, &format!("{test_dir_name}/with-links/{link}")).await);
	}
	assert!(is_file(client, &format!("{test_dir_name}/with-links/real.txt")).await);
}

#[cfg(unix)]
#[shared_test_runtime]
async fn uploading_without_following_symlinks_lists_them_in_the_json_result() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let test_dir_name = resources.dir.name().unwrap();

	let local = tempfile::tempdir().unwrap();
	let source = local.path().join("json-links");
	std::fs::create_dir(&source).unwrap();
	std::fs::write(source.join("real.txt"), b"real").unwrap();
	std::os::unix::fs::symlink(source.join("real.txt"), source.join("link.txt")).unwrap();

	let output = authenticated_cli_with_args!(
		"upload",
		"--json",
		"--no-follow-symlinks",
		source.to_str().unwrap(),
		test_dir_name
	)
	.success();

	let result: serde_json::Value = serde_json::from_slice(&output.get_output().stdout).unwrap();
	assert_eq!(
		result["skipped_symlinks"],
		serde_json::json!([source.join("link.txt").display().to_string()])
	);
	assert_eq!(result["files"], 1);
}

/// The uuid of the file at `rel_path` below the test directory.
async fn file_uuid_at(client: &Client, test_dir_name: &str, rel_path: &str) -> Uuid {
	file_uuid(client, &format!("{test_dir_name}/{rel_path}")).await
}

#[shared_test_runtime]
async fn uploading_with_skip_existing_keeps_same_named_files_whatever_their_case() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir_name = resources.dir.name().unwrap();

	let first = tempfile::tempdir().unwrap();
	std::fs::create_dir(first.path().join("up")).unwrap();
	std::fs::write(first.path().join("up").join("Notes.TXT"), b"first").unwrap();
	authenticated_cli_with_args!(
		"upload",
		first.path().join("up").to_str().unwrap(),
		test_dir_name
	)
	.success();
	let kept = file_uuid_at(client, test_dir_name, "up/Notes.TXT").await;

	let second = tempfile::tempdir().unwrap();
	std::fs::create_dir(second.path().join("up")).unwrap();
	std::fs::write(
		second.path().join("up").join("notes.txt"),
		b"second version",
	)
	.unwrap();
	std::fs::write(second.path().join("up").join("new.txt"), b"new").unwrap();
	let output = authenticated_cli_with_args!(
		"upload",
		"--skip-existing",
		second.path().join("up").to_str().unwrap(),
		test_dir_name
	)
	.success();

	assert_eq!(
		file_uuid_at(client, test_dir_name, "up/Notes.TXT").await,
		kept
	);
	assert!(is_file(client, &format!("{test_dir_name}/up/new.txt")).await);
	let stdout = String::from_utf8_lossy(&output.get_output().stdout);
	assert!(stdout.contains("1 file, 3 B in "), "{stdout}");
	assert!(stdout.contains(", 1 skipped"), "{stdout}");
}

#[shared_test_runtime]
async fn uploading_with_update_replaces_only_the_changed_files() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir_name = resources.dir.name().unwrap();

	let local = tempfile::tempdir().unwrap();
	let source = local.path().join("up");
	std::fs::create_dir(&source).unwrap();
	std::fs::write(source.join("same.txt"), b"same").unwrap();
	std::fs::write(source.join("changed.txt"), b"before").unwrap();
	authenticated_cli_with_args!("upload", source.to_str().unwrap(), test_dir_name).success();
	let same = file_uuid_at(client, test_dir_name, "up/same.txt").await;
	let changed = file_uuid_at(client, test_dir_name, "up/changed.txt").await;

	std::fs::write(source.join("changed.txt"), b"after, longer").unwrap();
	authenticated_cli_with_args!(
		"upload",
		"--update",
		source.to_str().unwrap(),
		test_dir_name
	)
	.success();

	assert_eq!(
		file_uuid_at(client, test_dir_name, "up/same.txt").await,
		same
	);
	assert_ne!(
		file_uuid_at(client, test_dir_name, "up/changed.txt").await,
		changed
	);
}

#[shared_test_runtime]
async fn downloading_with_no_clobber_keeps_every_local_file() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let test_dir_name = resources.dir.name().unwrap();

	let local = tempfile::tempdir().unwrap();
	let source = local.path().join("down");
	std::fs::create_dir(&source).unwrap();
	std::fs::write(source.join("a.txt"), b"remote a").unwrap();
	std::fs::write(source.join("b.txt"), b"remote b").unwrap();
	authenticated_cli_with_args!("upload", source.to_str().unwrap(), test_dir_name).success();

	let target = tempfile::tempdir().unwrap();
	std::fs::create_dir(target.path().join("down")).unwrap();
	std::fs::write(target.path().join("down").join("a.txt"), b"local").unwrap();
	authenticated_cli_with_args!(
		"download",
		"--no-clobber",
		&format!("{test_dir_name}/down"),
		target.path().to_str().unwrap()
	)
	.success();

	assert_eq!(
		std::fs::read(target.path().join("down").join("a.txt")).unwrap(),
		b"local"
	);
	assert_eq!(
		std::fs::read(target.path().join("down").join("b.txt")).unwrap(),
		b"remote b"
	);
}

#[shared_test_runtime]
async fn uploading_with_include_and_exclude_takes_only_the_matching_files() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir_name = resources.dir.name().unwrap();

	let local = tempfile::tempdir().unwrap();
	let source = local.path().join("filtered");
	std::fs::create_dir_all(source.join("keep")).unwrap();
	std::fs::create_dir_all(source.join("skip")).unwrap();
	std::fs::create_dir_all(source.join("empty")).unwrap();
	std::fs::write(source.join("keep").join("a.JPG"), b"a").unwrap();
	std::fs::write(source.join("keep").join("b.txt"), b"b").unwrap();
	std::fs::write(source.join("skip").join("c.jpg"), b"c").unwrap();
	std::fs::write(source.join("top.txt"), b"top").unwrap();

	authenticated_cli_with_args!(
		"upload",
		"--include",
		"*.jpg",
		"--exclude",
		"skip/",
		source.to_str().unwrap(),
		test_dir_name
	)
	.success();

	let found = |rel_path: &'static str| async move {
		exists(client, &format!("{test_dir_name}/filtered/{rel_path}")).await
	};
	assert!(found("keep/a.JPG").await);
	for left_out in ["keep/b.txt", "skip", "empty", "top.txt"] {
		assert!(!found(left_out).await, "{left_out} was uploaded");
	}
}

#[shared_test_runtime]
async fn uploading_several_items_goes_on_past_one_that_fails() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir_name = resources.dir.name().unwrap();

	let local = tempfile::tempdir().unwrap();
	std::fs::write(local.path().join("a.txt"), b"a").unwrap();
	std::fs::write(local.path().join("b.txt"), b"b").unwrap();

	let output = authenticated_cli_with_args!(
		"upload",
		local.path().join("a.txt").to_str().unwrap(),
		local.path().join("missing.txt").to_str().unwrap(),
		local.path().join("b.txt").to_str().unwrap(),
		&format!("{test_dir_name}/several")
	)
	.failure();

	for name in ["a.txt", "b.txt"] {
		assert!(is_file(client, &format!("{test_dir_name}/several/{name}")).await);
	}
	let stderr = String::from_utf8_lossy(&output.get_output().stderr);
	assert!(stderr.contains("missing.txt"), "{stderr}");
	assert!(stderr.contains("1 item(s) failed to upload"), "{stderr}");
}

#[shared_test_runtime]
async fn downloading_several_items_puts_them_into_the_destination_directory() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let test_dir_name = resources.dir.name().unwrap();

	let local = tempfile::tempdir().unwrap();
	std::fs::write(local.path().join("a.txt"), b"a").unwrap();
	std::fs::write(local.path().join("b.txt"), b"b").unwrap();
	authenticated_cli_with_args!(
		"upload",
		local.path().join("a.txt").to_str().unwrap(),
		local.path().join("b.txt").to_str().unwrap(),
		test_dir_name
	)
	.success();

	let target = local.path().join("into").join("here");
	authenticated_cli_with_args!(
		"download",
		&format!("{test_dir_name}/a.txt"),
		&format!("{test_dir_name}/b.txt"),
		target.to_str().unwrap()
	)
	.success();

	assert_eq!(std::fs::read(target.join("a.txt")).unwrap(), b"a");
	assert_eq!(std::fs::read(target.join("b.txt")).unwrap(), b"b");
}

#[shared_test_runtime]
async fn a_file_piped_in_through_stdin_comes_back_out_through_stdout() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let test_dir_name = resources.dir.name().unwrap();
	// Past a chunk, so the pipe is read and written in several parts.
	let contents = (0..1024 * 1024 + 512 * 1024 + 7)
		.map(|i: u32| u8::try_from(i % 251).unwrap())
		.collect::<Vec<_>>();
	let remote = format!("{test_dir_name}/piped/in.bin");

	let (_auth_dir, auth_config) = test_utils::cli::prepare_cli_auth_config().await;
	assert_cmd::Command::new(assert_cmd::cargo::cargo_bin!("filen-cli"))
		.arg("--auth-config-path")
		.arg(auth_config.path())
		.args(["upload", "-", &remote])
		.write_stdin(contents.clone())
		.assert()
		.success();

	let output = assert_cmd::Command::new(assert_cmd::cargo::cargo_bin!("filen-cli"))
		.arg("--auth-config-path")
		.arg(auth_config.path())
		.args(["download", &remote, "-"])
		.assert()
		.success();
	let output = output.get_output();
	assert!(
		output.stdout == contents,
		"stdout differs from what was piped in"
	);
	let stderr = String::from_utf8_lossy(&output.stderr);
	assert!(stderr.contains("to stdout"), "{stderr}");
}

#[test]
fn uploading_piped_stdin_without_a_login_fails_instead_of_prompting() {
	// The login prompt cannot read stdin, which carries the file.
	let config_dir = tempfile::tempdir().unwrap();
	let output = assert_cmd::Command::from_std(logged_out_command(
		Path::new(env!("CARGO_BIN_EXE_filen-cli")),
		config_dir.path(),
	))
	.args(["--skip-update", "upload", "-", "/today.txt"])
	.write_stdin("the file's contents")
	.output()
	.unwrap();

	assert_eq!(output.status.code(), Some(2));
	let stderr = String::from_utf8_lossy(&output.stderr);
	assert!(
		stderr.contains("Cannot ask \"Email:\": stdin is not a terminal."),
		"{stderr}"
	);
}

#[test]
fn uploading_stdin_among_commands_piped_into_the_cli_is_refused() {
	// Needs no account: the refusal comes before any login.
	let config_dir = tempfile::tempdir().unwrap();
	let output = assert_cmd::Command::from_std(logged_out_command(
		Path::new(env!("CARGO_BIN_EXE_filen-cli")),
		config_dir.path(),
	))
	.arg("--skip-update")
	.write_stdin("upload - /today.txt\nthe next command, not the file\n")
	.output()
	.unwrap();

	assert_eq!(output.status.code(), Some(2));
	let stderr = String::from_utf8_lossy(&output.stderr);
	assert!(
		stderr.contains("\"-\" (stdin or stdout) only works in a single command"),
		"{stderr}"
	);
}

#[shared_test_runtime]
async fn uploading_empty_stdin_creates_an_empty_file() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir_name = resources.dir.name().unwrap();
	let remote = format!("{test_dir_name}/empty-stdin.txt");

	let (_auth_dir, auth_config) = test_utils::cli::prepare_cli_auth_config().await;
	assert_cmd::Command::new(assert_cmd::cargo::cargo_bin!("filen-cli"))
		.arg("--auth-config-path")
		.arg(auth_config.path())
		.args(["upload", "-", &remote])
		.write_stdin(Vec::new())
		.assert()
		.success();

	let Some(NonRootFileType::File(file)) = client.find_item_at_path(&remote).await.unwrap() else {
		panic!("no file at {remote}");
	};
	assert_eq!(file.size(), 0);
}

#[shared_test_runtime]
async fn uploading_stdin_with_skip_existing_keeps_the_existing_file() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir_name = resources.dir.name().unwrap();
	let builder = client
		.make_file_builder("kept-from-stdin.txt", resources.dir.uuid())
		.unwrap();
	let existing = client.upload_file(builder, b"old").await.unwrap();
	let remote = format!("{test_dir_name}/kept-from-stdin.txt");

	let (_auth_dir, auth_config) = test_utils::cli::prepare_cli_auth_config().await;
	let output = assert_cmd::Command::new(assert_cmd::cargo::cargo_bin!("filen-cli"))
		.arg("--auth-config-path")
		.arg(auth_config.path())
		.args(["upload", "--skip-existing", "-", &remote])
		.write_stdin("new")
		.assert()
		.success();

	let stdout = String::from_utf8_lossy(&output.get_output().stdout);
	assert!(stdout.contains("1 skipped"), "{stdout}");
	assert_eq!(file_uuid(client, &remote).await, existing.uuid());
}

#[shared_test_runtime]
async fn a_download_to_stdout_fails_once_stdout_is_closed() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir_name = resources.dir.name().unwrap();
	// Far more than a pipe holds, so the download is still writing when the reader goes.
	let builder = client
		.make_file_builder("closed.bin", resources.dir.uuid())
		.unwrap();
	client
		.upload_file(builder, &vec![7u8; 4 * 1024 * 1024])
		.await
		.unwrap();

	let (_auth_dir, auth_config) = test_utils::cli::prepare_cli_auth_config().await;
	let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_filen-cli"))
		.arg("--auth-config-path")
		.arg(auth_config.path())
		.args(["download", &format!("{test_dir_name}/closed.bin"), "-"])
		.stdin(Stdio::null())
		.stdout(Stdio::piped())
		.stderr(Stdio::piped())
		.spawn()
		.unwrap();
	let mut stdout = child.stdout.take().unwrap();
	let mut first = [0u8; 10];
	std::io::Read::read_exact(&mut stdout, &mut first).unwrap();
	drop(stdout);
	let output = child.wait_with_output().unwrap();

	assert_eq!(first, [7u8; 10]);
	let stderr = String::from_utf8_lossy(&output.stderr);
	assert_eq!(output.status.code(), Some(1), "{stderr}");
	assert!(
		console::strip_ansi_codes(&stderr)
			.ends_with("✘ Stdout was closed before the whole file was written to it\n"),
		"{stderr}"
	);
}

#[shared_test_runtime]
async fn downloading_with_resume_continues_from_what_an_earlier_download_left() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir_name = resources.dir.name().unwrap();
	let contents = (0..1024 * 1024 + 512 * 1024 + 7)
		.map(|i: u32| u8::try_from(i % 251).unwrap())
		.collect::<Vec<_>>();
	let builder = client
		.make_file_builder("resumed.bin", resources.dir.uuid())
		.unwrap();
	let file = client.upload_file(builder, &contents).await.unwrap();

	let local = tempfile::tempdir().unwrap();
	// What a download that did not finish keeps with --resume.
	let partial = local.path().join(format!("{}.filendl", file.uuid()));
	// Past the first chunk, which the download then does not download again.
	std::fs::write(&partial, &contents[..1124 * 1024]).unwrap();
	let target = local.path().join("resumed.bin");

	let output = authenticated_cli_with_args!(
		"download",
		"--resume",
		&format!("{test_dir_name}/resumed.bin"),
		target.to_str().unwrap()
	)
	.success();

	let stdout = String::from_utf8_lossy(&output.get_output().stdout);
	assert!(stdout.contains("(1 MiB resumed)"), "{stdout}");

	assert!(
		std::fs::read(&target).unwrap() == contents,
		"the resumed file differs"
	);
	assert!(!partial.exists(), "the partial file was left behind");
}

/// The directory in the config directory `config_dir` where `upload --resume` keeps its journals.
fn upload_journal_dir(config_dir: &Path) -> PathBuf {
	config_dir.join("uploads")
}

/// The journal files in `config_dir`'s `uploads/`, none when it is missing.
fn upload_journal_files(config_dir: &Path) -> Vec<PathBuf> {
	match std::fs::read_dir(upload_journal_dir(config_dir)) {
		Ok(entries) => entries
			.map(|entry| entry.unwrap().path())
			.filter(|path| path.extension().is_some_and(|ext| ext == "filenup"))
			.collect(),
		Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
		Err(e) => panic!("reading the upload journals: {e}"),
	}
}

/// Starts uploading the file at `source` into `parent` as the CLI with the config directory
/// `config_dir` would with `--resume`, and drops the upload once its journal was first saved, as
/// an upload interrupted there leaves it.
async fn plant_upload_journal(client: &Client, config_dir: &Path, source: &Path, parent: Uuid) {
	let store = NotifyingJournalStore::open(&upload_journal_dir(config_dir), false).await;
	tokio::select! {
		// The save is checked first, so that the upload is dropped before it sends anything more.
		biased;
		() = store.saved.notified() => {}
		result = client.upload_file_from_path_resuming(
			FileBuilderOptionalName::new(parent),
			source.to_owned(),
			&store,
			None,
			None,
		) => panic!(
			"the upload ended before its journal was saved: {:?}",
			result.map(|(file, _)| file.uuid())
		),
	}
	assert_eq!(upload_journal_files(config_dir).len(), 1);
}

/// Asserts that the `--json` result of an upload in `stdout` names some, but not all, of the
/// `len` bytes of the one file a planted journal was left for as resumed.
fn assert_partly_resumed(stdout: &[u8], len: usize) {
	let result: serde_json::Value = serde_json::from_slice(stdout).unwrap();
	let resumed = result["resumed_bytes"].as_u64().unwrap();
	assert!(
		0 < resumed && resumed < u64::try_from(len).unwrap(),
		"{resumed} of {len} bytes resumed"
	);
}

/// The contents of the file at `path` in the drive.
async fn remote_contents(client: &Client, path: &str) -> Vec<u8> {
	let Some(NonRootFileType::File(file)) = client.find_item_at_path(path).await.unwrap() else {
		panic!("no file at {path}");
	};
	client.download_file(file.as_ref()).await.unwrap()
}

#[shared_test_runtime]
async fn uploading_with_resume_continues_what_an_earlier_upload_left() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir_name = resources.dir.name().unwrap();
	let config_dir = tempfile::tempdir().unwrap();
	let local = tempfile::tempdir().unwrap();
	let source = local.path().join("resumed-upload.bin");
	let contents = resumable_upload_contents(1);
	std::fs::write(&source, &contents).unwrap();
	plant_upload_journal(client, config_dir.path(), &source, resources.dir.uuid()).await;

	let output = authenticated_cli_in_config_dir!(
		config_dir.path();
		"upload",
		"--resume",
		"--json",
		source.to_str().unwrap(),
		test_dir_name
	)
	.success();

	assert_partly_resumed(&output.get_output().stdout, contents.len());
	assert!(
		remote_contents(client, &format!("{test_dir_name}/resumed-upload.bin")).await == contents,
		"the resumed file differs"
	);
	assert_eq!(
		upload_journal_files(config_dir.path()),
		Vec::<PathBuf>::new()
	);
}

#[shared_test_runtime]
async fn uploading_a_directory_with_resume_continues_its_unfinished_file() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir_name = resources.dir.name().unwrap();
	let config_dir = tempfile::tempdir().unwrap();
	let local = tempfile::tempdir().unwrap();
	let source = local.path().join("resumed-dir");
	std::fs::create_dir(&source).unwrap();
	let big = resumable_upload_contents(2);
	std::fs::write(source.join("big.bin"), &big).unwrap();
	std::fs::write(source.join("small.txt"), b"small contents").unwrap();
	// The remote directory the upload creates, which the journal names as the big file's parent.
	let DirType::Dir(remote_dir) = client
		.find_or_create_dir_starting_at(resources.dir.clone().into(), "resumed-dir")
		.await
		.unwrap()
	else {
		panic!("the remote directory is the drive's root");
	};
	plant_upload_journal(
		client,
		config_dir.path(),
		&source.join("big.bin"),
		remote_dir.uuid(),
	)
	.await;

	let output = authenticated_cli_in_config_dir!(
		config_dir.path();
		"upload",
		"--resume",
		"--json",
		source.to_str().unwrap(),
		test_dir_name
	)
	.success();

	assert_partly_resumed(&output.get_output().stdout, big.len());
	assert!(
		remote_contents(client, &format!("{test_dir_name}/resumed-dir/big.bin")).await == big,
		"the resumed file differs"
	);
	assert_eq!(
		remote_contents(client, &format!("{test_dir_name}/resumed-dir/small.txt")).await,
		b"small contents"
	);
	assert_eq!(
		upload_journal_files(config_dir.path()),
		Vec::<PathBuf>::new()
	);
}

#[shared_test_runtime]
async fn uploading_without_resume_keeps_no_record() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let test_dir_name = resources.dir.name().unwrap();
	let config_dir = tempfile::tempdir().unwrap();
	let local = tempfile::tempdir().unwrap();
	let source = local.path().join("unrecorded.bin");
	std::fs::write(&source, resumable_upload_contents(3)).unwrap();

	authenticated_cli_in_config_dir!(
		config_dir.path();
		"upload",
		source.to_str().unwrap(),
		test_dir_name
	)
	.success();

	assert!(!upload_journal_dir(config_dir.path()).exists());
}

/// The URL the web app gives a public link: its route, uuid and hex-encoded key.
fn public_link_url(route: &str, uuid: impl std::fmt::Display, key: &str) -> String {
	let key = key
		.bytes()
		.map(|byte| format!("{byte:02x}"))
		.collect::<String>();
	format!("https://app.filen.io/#/{route}/{uuid}%23{key}")
}

#[shared_test_runtime]
async fn downloading_a_file_link_downloads_the_file_it_shares() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let builder = client
		.make_file_builder("linked.txt", resources.dir.uuid())
		.unwrap();
	let file = client
		.upload_file(builder, b"linked contents")
		.await
		.unwrap();
	let link = client.public_link_file(&file).await.unwrap();
	let url = public_link_url("d", link.uuid(), file.key().unwrap().to_str().as_ref());

	let local = tempfile::tempdir().unwrap();
	authenticated_cli_with_args!("download", &url, local.path().to_str().unwrap()).success();

	assert_eq!(
		std::fs::read(local.path().join("linked.txt")).unwrap(),
		b"linked contents"
	);
}

/// A directory with a file inside, uploaded into the test directory, and a public link to it.
async fn linked_directory(
	client: &Client,
	test_dir_name: &str,
) -> (RemoteDirectory, DirPublicLinkRW) {
	let local = tempfile::tempdir().unwrap();
	let source = local.path().join("linked");
	std::fs::create_dir_all(source.join("inner")).unwrap();
	std::fs::write(source.join("inner").join("a.txt"), b"inside").unwrap();
	authenticated_cli_with_args!("upload", source.to_str().unwrap(), test_dir_name).success();
	let Some(NonRootFileType::Dir(dir)) = client
		.find_item_at_path(&format!("{test_dir_name}/linked"))
		.await
		.unwrap()
	else {
		panic!("the uploaded directory is missing");
	};
	let dir = dir.into_owned();
	let link = client
		.public_link_dir::<fn(u64, Option<u64>)>(&dir, None)
		.await
		.unwrap();
	(dir, link)
}

#[shared_test_runtime]
async fn downloading_a_directory_link_downloads_everything_inside() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir_name = resources.dir.name().unwrap();
	let (_dir, link) = linked_directory(client, test_dir_name).await;
	let url = public_link_url("f", link.uuid(), &link.key_string().unwrap());

	let local = tempfile::tempdir().unwrap();
	authenticated_cli_with_args!("download", &url, local.path().to_str().unwrap()).success();

	assert_eq!(
		std::fs::read(local.path().join("linked").join("inner").join("a.txt")).unwrap(),
		b"inside"
	);
}

#[shared_test_runtime]
async fn downloading_a_password_protected_directory_link_takes_its_password() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir_name = resources.dir.name().unwrap();
	let (dir, mut link) = linked_directory(client, test_dir_name).await;
	link.set_password("hunter2".to_owned());
	client.update_dir_link(&dir, &link).await.unwrap();
	let url = public_link_url("f", link.uuid(), &link.key_string().unwrap());

	let local = tempfile::tempdir().unwrap();
	authenticated_cli_with_args!(
		"download",
		&url,
		local.path().to_str().unwrap(),
		"--link-password",
		"hunter2"
	)
	.success();

	assert_eq!(
		std::fs::read(local.path().join("linked").join("inner").join("a.txt")).unwrap(),
		b"inside"
	);
}

#[shared_test_runtime]
async fn downloading_a_link_needs_no_login() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let builder = client
		.make_file_builder("linked-anonymously.txt", resources.dir.uuid())
		.unwrap();
	let file = client
		.upload_file(builder, b"linked contents")
		.await
		.unwrap();
	let link = client.public_link_file(&file).await.unwrap();
	let url = public_link_url("d", link.uuid(), file.key().unwrap().to_str().as_ref());

	// No credentials anywhere, and no prompt for them: a login would fail the command.
	let config_dir = tempfile::tempdir().unwrap();
	let local = tempfile::tempdir().unwrap();
	let output = logged_out_command(
		Path::new(env!("CARGO_BIN_EXE_filen-cli")),
		config_dir.path(),
	)
	.args(["--skip-update", "--no-input", "download", &url])
	.arg(local.path())
	.stdin(Stdio::null())
	.output()
	.unwrap();

	assert!(
		output.status.success(),
		"{}",
		String::from_utf8_lossy(&output.stderr)
	);
	assert_eq!(
		std::fs::read(local.path().join("linked-anonymously.txt")).unwrap(),
		b"linked contents"
	);
}
