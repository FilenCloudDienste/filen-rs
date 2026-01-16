use std::{path::PathBuf, time::Duration};

use filen_macros::shared_test_runtime;
use filen_sdk_rs::fs::FSObject;
use log::{debug, info, trace};
use tokio::fs;

const TEST_ROOT_DIR_PREFIX: &str = "filen-rs-filen-network-drive-tests";
const TEST_FILE_CONTENT: &str = "This is a test file for filen-network-drive tests.";

// according to https://github.com/actions/runner-images/issues/598
// and https://github.com/actions/runner-images/issues/5583#issuecomment-1196763627,
// macFUSE does not work on GitHub runners, therefore we can't test it in ci
// for local tests, run with `cargo test -- --ignored`
#[cfg(not(target_os = "macos"))]
#[shared_test_runtime]
async fn start_rclone_mount() {
	use filen_rclone_wrapper::network_drive::NetworkDrive;

	let test_root_dir = format!("{}-{}", TEST_ROOT_DIR_PREFIX, uuid::Uuid::new_v4());

	let client = test_utils::RESOURCES.client().await;
	let config_dir = dirs::config_dir().unwrap().join(TEST_ROOT_DIR_PREFIX);

	#[cfg(windows)]
	{
		// try to mount on invalid drive letter
		assert!(
			mount_network_drive(&client, &config_dir, Some("C:\\"), false)
				.await
				.is_err(),
			"Mounting on used drive letter should fail"
		);
		info!("Tested mounting on used drive letter fails as expected");
	}

	// mount network drive (is killed on drop)
	let mut network_drive = NetworkDrive::mount(&client, &config_dir, None, false)
		.await
		.unwrap();
	info!("Network drive mounted at: {}", network_drive.mount_point);

	let created_dir_path = format!("{}/created_dir", test_root_dir);

	network_drive.wait_until_active().await.unwrap();

	// get stats
	let stats = network_drive.get_stats().await.unwrap();
	debug!("Stats: {:?}", stats);

	// create remote test root dir if it doesn't exist
	if client
		.find_item_at_path(&test_root_dir)
		.await
		.unwrap()
		.is_none()
	{
		client
			.create_dir(client.root(), test_root_dir.clone())
			.await
			.unwrap();
	};
	let remote_test_root = {
		match client.find_item_at_path(&test_root_dir).await.unwrap() {
			Some(item) => match item {
				FSObject::Dir(dir) => dir,
				_ => panic!("Remote root dir is not a directory"),
			},
			None => panic!("Failed to find remote root dir"),
		}
	};

	// check that dir doesn't exist before creation
	if client
		.find_item_at_path(&created_dir_path)
		.await
		.unwrap()
		.is_some()
	{
		panic!("Directory already exists remotely before creation");
	}

	// list mount directory
	let mut entries = fs::read_dir(&network_drive.mount_point).await.unwrap();
	while let Some(entry) = entries.next_entry().await.unwrap() {
		info!("Mount entry: {}", entry.path().display());
	}

	// create local dir inside mount
	debug!(
		"Trying to create local dir at: {}",
		PathBuf::from(network_drive.mount_point.clone())
			.join(&created_dir_path)
			.display()
	);
	fs::create_dir_all(PathBuf::from(network_drive.mount_point.clone()).join(&created_dir_path))
		.await
		.unwrap();

	// check that dir exists remotely and clean it up
	let remote_created_dir = client.find_item_at_path(&created_dir_path).await.unwrap();
	if remote_created_dir.is_none() {
		panic!("Directory was not created remotely");
	} else {
		info!("Directory was created remotely");
	}
	match remote_created_dir.unwrap() {
		FSObject::Dir(dir) => {
			client
				.delete_dir_permanently(dir.into_owned())
				.await
				.unwrap();
			debug!("Cleaned up remote directory");
		}
		_ => panic!("Created item is not a directory"),
	}

	// todo: upload file, check stats

	let uploaded_file_path = format!("{}/uploaded_file.txt", test_root_dir);

	// check that file doesn't exist before upload
	if client
		.find_item_at_path(&uploaded_file_path)
		.await
		.unwrap()
		.is_some()
	{
		panic!("File already exists remotely before upload");
	}

	// create local file inside mount
	debug!(
		"Trying to create local file at: {}",
		PathBuf::from(network_drive.mount_point.clone())
			.join(&uploaded_file_path)
			.display()
	);
	fs::write(
		PathBuf::from(network_drive.mount_point.clone()).join(&uploaded_file_path),
		TEST_FILE_CONTENT,
	)
	.await
	.unwrap();

	// check that upload stats work
	let mut has_found_transfer = false;
	let mut transfer_i = 0;
	loop {
		transfer_i += 1;
		if transfer_i > 300 {
			panic!("Upload transfer did not complete in time (30s)");
		}
		let stats = network_drive.get_stats().await.unwrap();
		let transfers = stats.transfers.len();
		if transfers == 0 {
			trace!("Still no transfers");
			if has_found_transfer {
				info!("Upload transfer completed");
				break;
			}
		} else {
			if has_found_transfer {
				trace!("Transfer still found");
			} else {
				info!("Transfer found");
			}
			has_found_transfer = true;
		}
		tokio::time::sleep(Duration::from_millis(100)).await;
	}

	// check that file exists remotely with right content and clean it up
	tokio::time::sleep(Duration::from_secs(2)).await; // wait a bit for rclone to sync
	let remote_uploaded_file = client.find_item_at_path(&uploaded_file_path).await.unwrap();
	if remote_uploaded_file.is_none() {
		panic!("File was not uploaded remotely");
	} else {
		info!("File was uploaded remotely");
	}
	match remote_uploaded_file.unwrap() {
		FSObject::File(file) => {
			let content = client.download_file(&*file).await.unwrap();
			let content = String::from_utf8_lossy(&content);
			assert_eq!(content, TEST_FILE_CONTENT);
			info!("Uploaded file content is correct");
			client
				.delete_file_permanently(file.into_owned())
				.await
				.unwrap();
			debug!("Cleaned up remote file");
		}
		_ => panic!("Uploaded item is not a file"),
	}

	drop(network_drive.process);
	info!("Network drive unmounted");

	// cleanup test root dir
	client
		.delete_dir_permanently(remote_test_root.into_owned())
		.await
		.unwrap();
}
