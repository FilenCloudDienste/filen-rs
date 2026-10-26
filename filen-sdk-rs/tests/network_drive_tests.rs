//! A network drive mounted by the rclone daemon from the managed rclone, against the test
//! account.
//!
//! Not on macOS: GitHub's macOS runners have no macFUSE (actions/runner-images#598, #5583), and
//! the daemon's `nfsmount` is not tried there; `rclone_daemon_tests` mount through it locally.

#![cfg(not(target_os = "macos"))]

use std::{path::PathBuf, time::Duration};

use filen_sdk_rs::{
	fs::categories::NonRootFileType,
	io::client_impl::IoSharedClientExt,
	rclone::{
		MountOptions, RcloneBinary, RcloneDaemon, RcloneDaemonConfig, StopOptions, VfsTuning,
	},
};
use tokio::fs;
use tracing::info;

const TEST_ROOT_DIR_PREFIX: &str = "filen-rs-filen-network-drive-tests";
const TEST_FILE_CONTENT: &str = "This is a test file for filen-network-drive tests.";

/// Where the drive is mounted: an empty directory in `dir`, or on Windows a free drive letter.
#[cfg(not(windows))]
async fn mount_point(dir: &std::path::Path) -> String {
	let mount_point = dir.join("drive");
	fs::create_dir(&mount_point).await.unwrap();
	mount_point.into_os_string().into_string().unwrap()
}

#[cfg(windows)]
async fn mount_point(_dir: &std::path::Path) -> String {
	filen_sdk_rs::rclone::available_drive_letters()
		.await
		.pop()
		.expect("no free drive letter")
}

#[filen_macros::shared_test_runtime]
async fn start_rclone_mount() {
	let test_root_dir = format!("{}-{}", TEST_ROOT_DIR_PREFIX, uuid::Uuid::new_v4());
	let client = test_utils::RESOURCES.client().await;
	let config_dir = dirs::config_dir().unwrap().join(TEST_ROOT_DIR_PREFIX);
	let dir = tempfile::tempdir().unwrap();

	let daemon = RcloneDaemon::start(
		&client,
		RcloneDaemonConfig {
			// Kept across runs, so the release is downloaded once.
			binary: RcloneBinary::Managed {
				dir: config_dir.join("bin"),
			},
			state_dir: dir.path().join("state"),
			cache_dir: dir.path().join("cache"),
			log_file: None,
			user_agent: "filen-rs-network-drive-tests".to_owned(),
			vfs: VfsTuning::default(),
		},
	)
	.await
	.unwrap();

	#[cfg(windows)]
	assert!(
		daemon
			.mount(MountOptions {
				mount_point: "C".to_owned(),
				read_only: false,
			})
			.await
			.is_err(),
		"mounting on a drive letter in use should fail"
	);

	let mount = daemon
		.mount(MountOptions {
			mount_point: mount_point(dir.path()).await,
			read_only: false,
		})
		.await
		.unwrap();
	info!("Network drive mounted at: {}", mount.mount_point);
	// A drive letter's root is `X:\`; `X:` alone is the drive's current directory.
	let root = if cfg!(windows) {
		PathBuf::from(format!(r"{}\", mount.mount_point))
	} else {
		PathBuf::from(&mount.mount_point)
	};

	let remote_test_root = client
		.create_dir(&client.root().into(), &test_root_dir)
		.await
		.unwrap();

	// A directory created in the mount is created remotely.
	let created_dir_path = format!("{test_root_dir}/created_dir");
	fs::create_dir_all(root.join(&created_dir_path))
		.await
		.unwrap();
	assert!(
		matches!(
			client.find_item_at_path(&created_dir_path).await.unwrap(),
			Some(NonRootFileType::Dir(_))
		),
		"the directory was not created remotely"
	);

	// A file written to the mount is uploaded once the VFS cache's write-back delay has passed.
	let uploaded_file_path = format!("{test_root_dir}/uploaded_file.txt");
	fs::write(root.join(&uploaded_file_path), TEST_FILE_CONTENT)
		.await
		.unwrap();
	let mut uploaded = None;
	for _ in 0..600 {
		let stats = daemon.stats().await.unwrap();
		if stats.uploads_in_progress + stats.uploads_queued == 0
			&& let Some(NonRootFileType::File(file)) =
				client.find_item_at_path(&uploaded_file_path).await.unwrap()
		{
			uploaded = Some(file);
			break;
		}
		tokio::time::sleep(Duration::from_millis(100)).await;
	}
	let file = uploaded.expect("the file was not uploaded within 60 s");
	let content = client.download_file(&*file).await.unwrap();
	assert_eq!(String::from_utf8_lossy(&content), TEST_FILE_CONTENT);

	let report = daemon
		.stop(StopOptions {
			drain_timeout_ms: Some(30_000),
			grace_ms: 10_000,
		})
		.await
		.unwrap();
	assert_eq!(report.pending_uploads, 0);

	client
		.delete_dir_permanently(remote_test_root)
		.await
		.unwrap();
}
