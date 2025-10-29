use std::path::PathBuf;

use filen_macros::shared_test_runtime;
use filen_network_drive::mount_network_drive;
use filen_sdk_rs::fs::FSObject;
use log::{debug, info};
use tokio::fs;

const TEST_DIR: &str = "filen-rs-filen-network-drive-tests";

#[shared_test_runtime]
#[ignore = "would fial in CI, there are still some manual setup steps required"]
async fn start_rclone_mount() {
	let client = test_utils::RESOURCES.client().await;
	let config_dir = dirs::config_dir().unwrap().join(TEST_DIR);

	// mount network drive (is killed on drop)
	let network_drive = mount_network_drive(&client, &config_dir, None, false)
		.await
		.unwrap();
	info!("Network drive mounted at: {}", network_drive.mount_point);

	let created_dir_path = format!("{}/created_dir", TEST_DIR);

	// create remote test root dir if it doesn't exist
	if client.find_item_at_path(TEST_DIR).await.unwrap().is_none() {
		client
			.create_dir(client.root(), TEST_DIR.to_string())
			.await
			.unwrap();
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

	// create local dir inside mount
	debug!(
		"Trying to create local dir at: {}",
		PathBuf::from(network_drive.mount_point.clone())
			.join(&created_dir_path)
			.display()
	);
	fs::create_dir(PathBuf::from(network_drive.mount_point).join(&created_dir_path))
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
}
