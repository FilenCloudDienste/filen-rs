use std::{
	path::Path,
	process::Stdio,
	time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use filen_macros::shared_test_runtime;
use filen_sdk_rs::{
	auth::Client,
	fs::{HasName, HasUUID as _, categories::NonRootFileType},
};
use filen_types::fs::Uuid;
use test_utils::authenticated_cli_with_args;

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

	assert!(matches!(
		client
			.find_item_at_path(&format!("{test_dir_name}/uploaded/nested/a.txt"))
			.await
			.unwrap(),
		Some(NonRootFileType::File(_))
	));
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

	assert!(matches!(
		client
			.find_item_at_path(&format!("{test_dir_name}/link.txt"))
			.await
			.unwrap(),
		Some(NonRootFileType::File(_))
	));
	assert!(
		client
			.find_item_at_path(&format!("{test_dir_name}/target.txt"))
			.await
			.unwrap()
			.is_none()
	);
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

	assert!(matches!(
		client
			.find_item_at_path(&format!("{test_dir_name}/dir/new.txt"))
			.await
			.unwrap(),
		Some(NonRootFileType::File(_))
	));
	assert!(
		client
			.find_item_at_path(&format!("{test_dir_name}/dir/b.txt"))
			.await
			.unwrap()
			.is_none()
	);
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

	assert!(matches!(
		client
			.find_item_at_path(&format!("{test_dir_name}/x/y/z/a.txt"))
			.await
			.unwrap(),
		Some(NonRootFileType::File(_))
	));
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

/// The uuid of the file at `path` in the drive.
async fn remote_file_uuid(client: &Client, path: &str) -> Uuid {
	match client.find_item_at_path(path).await.unwrap() {
		Some(NonRootFileType::File(file)) => file.uuid(),
		other => panic!("expected a file at {path}, found {other:?}"),
	}
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
	let first = remote_file_uuid(client, &format!("{test_dir_name}/a.txt")).await;

	std::fs::write(&source, b"second version").unwrap();
	authenticated_cli_with_args!("upload", source.to_str().unwrap(), test_dir_name).success();
	let second = remote_file_uuid(client, &format!("{test_dir_name}/a.txt")).await;
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
	assert!(matches!(
		client
			.find_item_at_path(&format!("{test_dir_name}/partial/ok.txt"))
			.await
			.unwrap(),
		Some(NonRootFileType::File(_))
	));
	assert!(
		client
			.find_item_at_path(&format!("{test_dir_name}/partial/locked.txt"))
			.await
			.unwrap()
			.is_none()
	);
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
	assert!(matches!(
		client
			.find_item_at_path(&format!("{test_dir_name}/remote dir/my dir/a file.txt"))
			.await
			.unwrap(),
		Some(NonRootFileType::File(_))
	));

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
		assert!(
			client
				.find_item_at_path(&format!("{test_dir_name}/with-links/{link}"))
				.await
				.unwrap()
				.is_none()
		);
	}
	assert!(matches!(
		client
			.find_item_at_path(&format!("{test_dir_name}/with-links/real.txt"))
			.await
			.unwrap(),
		Some(NonRootFileType::File(_))
	));
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
