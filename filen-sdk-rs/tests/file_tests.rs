use std::{borrow::Cow, sync::Arc};

use chrono::{SubsecRound, Utc};
use filen_sdk_rs::{
	auth::Client,
	crypto::shared::generate_random_base64_values,
	fs::{
		FSObject, HasName, HasRemoteInfo, HasUUID, NonRootFSObject,
		dir::RemoteDirectory,
		file::traits::{HasFileInfo, HasFileMeta},
	},
};
use filen_sdk_rs_macros::shared_test_runtime;
use rand::TryRngCore;

async fn assert_file_upload_download_equal(name: &str, contents_len: usize) {
	let mut contents = vec![0u8; contents_len];
	rand::rng().try_fill_bytes(&mut contents).unwrap();

	let contents = contents.as_ref();
	let (resources, _lock) = test_utils::RESOURCES.get_resources_with_lock().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let file = client.make_file_builder(name, test_dir).build();
	let file = client.upload_file(file.into(), contents).await.unwrap();

	let found_file = match client
		.find_item_at_path(format!("{}/{}", test_dir.name(), name))
		.await
		.unwrap()
	{
		Some(FSObject::File(file)) => file.into_owned(),
		_ => panic!("Expected a file"),
	};
	assert_eq!(
		file, found_file,
		"Downloaded file didn't match uploaded file for {name}"
	);

	let buf = client.download_file(&file).await.unwrap();

	assert_eq!(buf.len(), contents.len(), "File size mismatch for {name}");
	assert_eq!(&buf, contents, "File contents mismatch for {name}");

	let got_file = client.get_file(file.uuid()).await.unwrap();
	assert_eq!(file, got_file, "File metadata mismatch for {name}");
}

#[shared_test_runtime]
async fn file_upload_download() {
	assert_file_upload_download_equal("small.txt", 10).await;
	assert_file_upload_download_equal("big_chunk_aligned_equal_to_threads.exe", 1024 * 1024 * 8)
		.await;
	assert_file_upload_download_equal("big_chunk_aligned_less_than_threads.exe", 1024 * 1024 * 7)
		.await;
	assert_file_upload_download_equal("big_chunk_aligned_more_than_threads.exe", 1024 * 1024 * 9)
		.await;
	assert_file_upload_download_equal("big_not_chunk_aligned_over.exe", 1024 * 1024 * 8 + 1).await;
	assert_file_upload_download_equal("big_not_chunk_aligned_under.exe", 1024 * 1024 * 8 - 1).await;
	assert_file_upload_download_equal("empty.json", 0).await;
	assert_file_upload_download_equal("one_chunk", 1024 * 1024).await;
}

#[shared_test_runtime]
async fn file_search() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let second_dir = client
		.create_dir(test_dir, "second_dir".to_string())
		.await
		.unwrap();

	let file_random_part_long = generate_random_base64_values(16);
	let file_random_part_short = generate_random_base64_values(2);

	let file_name = format!("{file_random_part_long}{file_random_part_short}.txt");

	let file = client.make_file_builder(&file_name, &second_dir).build();
	let file = client.upload_file(file.into(), &[]).await.unwrap();

	let found_items = client
		.find_item_matches_for_name(file_random_part_long)
		.await
		.unwrap();

	assert_eq!(
		found_items,
		vec![(
			NonRootFSObject::File(Cow::Owned(file.clone())),
			format!("/{}/{}", test_dir.name(), second_dir.name())
		)]
	);

	let found_items = client
		.find_item_matches_for_name(file_random_part_short)
		.await
		.unwrap();

	assert!(found_items.iter().any(|(item, _)| {
		if let NonRootFSObject::File(found_file) = item {
			*found_file.clone() == file
		} else {
			false
		}
	}));
}

#[shared_test_runtime]
async fn file_trash() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let file_name = "file.txt";
	let file = client.make_file_builder(file_name, test_dir).build();
	let mut file = client
		.upload_file(file.into(), b"Hello World from Rust!")
		.await
		.unwrap();

	assert_eq!(
		client
			.find_item_at_path(format!("{}/{}", test_dir.name(), file_name))
			.await
			.unwrap(),
		Some(FSObject::File(Cow::Borrowed(&file)))
	);

	let _lock = client
		.acquire_lock("test:rs:trash", std::time::Duration::from_secs(1), 600)
		.await
		.unwrap();
	client.trash_file(&mut file).await.unwrap();

	assert!(
		client
			.find_item_at_path(format!("{}/{}", test_dir.name(), file_name))
			.await
			.unwrap()
			.is_none()
	);

	client.restore_file(&mut file).await.unwrap();
	assert_eq!(
		client
			.find_item_at_path(format!("{}/{}", test_dir.name(), file_name))
			.await
			.unwrap(),
		Some(FSObject::File(Cow::Borrowed(&file)))
	);
}

#[shared_test_runtime]
async fn file_delete_permanently() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let file_name = "file.txt";
	let file = client.make_file_builder(file_name, test_dir).build();
	let mut file = client
		.upload_file(file.into(), b"Hello World from Rust!")
		.await
		.unwrap();

	assert_eq!(
		client
			.find_item_at_path(format!("{}/{}", test_dir.name(), file_name))
			.await
			.unwrap(),
		Some(FSObject::File(Cow::Borrowed(&file)))
	);

	client.delete_file_permanently(file.clone()).await.unwrap();

	assert!(
		client
			.find_item_at_path(format!("{}/{}", test_dir.name(), file_name))
			.await
			.unwrap()
			.is_none()
	);

	assert!(client.restore_file(&mut file).await.is_err());

	assert!(client.get_file(file.uuid()).await.is_err());

	// Uncomment this when the API immediately permanently deletes the file
	// let mut reader = file.into_reader(client.clone());
	// let mut buf = Vec::new();
	// assert!(reader.read_to_end(&mut buf).await.is_err());
}

#[shared_test_runtime]
async fn file_move() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let file_name = "file.txt";
	let file = client.make_file_builder(file_name, test_dir).build();
	let mut file = client
		.upload_file(file.into(), b"Hello World from Rust!")
		.await
		.unwrap();

	assert_eq!(
		client
			.find_item_at_path(format!("{}/{}", test_dir.name(), file_name))
			.await
			.unwrap(),
		Some(FSObject::File(Cow::Borrowed(&file)))
	);

	let second_dir = client
		.create_dir(test_dir, "second_dir".to_string())
		.await
		.unwrap();
	client.move_file(&mut file, &second_dir).await.unwrap();

	assert!(
		client
			.find_item_at_path(format!("{}/{}", test_dir.name(), file_name))
			.await
			.unwrap()
			.is_none(),
	);

	assert_eq!(
		client
			.find_item_at_path(format!(
				"{}/{}/{}",
				test_dir.name(),
				second_dir.name(),
				file_name
			))
			.await
			.unwrap(),
		Some(FSObject::File(Cow::Borrowed(&file)))
	);
}

#[shared_test_runtime]
async fn file_update_meta() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let file_name = "file.txt";
	let file = client.make_file_builder(file_name, test_dir).build();
	let mut file = client
		.upload_file(file.into(), b"Hello World from Rust!")
		.await
		.unwrap();

	assert_eq!(
		client
			.find_item_at_path(format!("{}/{}", test_dir.name(), file_name))
			.await
			.unwrap(),
		Some(FSObject::File(Cow::Borrowed(&file)))
	);

	let mut meta = file.get_meta();
	meta.set_name("new_name.json").unwrap();

	client.update_file_metadata(&mut file, meta).await.unwrap();

	assert_eq!(file.name(), "new_name.json");
	assert_eq!(
		client
			.find_item_at_path(format!("{}/{}", test_dir.name(), file.name()))
			.await
			.unwrap(),
		Some(FSObject::File(Cow::Borrowed(&file)))
	);

	let mut meta = file.get_meta();
	let created = Utc::now() - chrono::Duration::days(1);
	let modified = Utc::now();
	let new_mime = "application/json";
	meta.set_mime(new_mime);
	meta.set_last_modified(modified);
	meta.set_created(created);

	client.update_file_metadata(&mut file, meta).await.unwrap();
	assert_eq!(file.mime(), new_mime);
	assert_eq!(file.created(), created.round_subsecs(3));
	assert_eq!(file.last_modified(), modified.round_subsecs(3));

	let found_file = client.get_file(file.uuid()).await.unwrap();
	assert_eq!(found_file.mime(), new_mime);
	assert_eq!(found_file.created(), created.round_subsecs(3));
	assert_eq!(found_file.last_modified(), modified.round_subsecs(3));
	assert_eq!(found_file, file);
}

#[shared_test_runtime]
async fn file_exists() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let file_name = "file.txt";

	assert!(
		client
			.file_exists(file_name, test_dir)
			.await
			.unwrap()
			.is_none()
	);

	let file = client.make_file_builder(file_name, test_dir).build();
	let mut file = client
		.upload_file(file.into(), b"Hello World from Rust!")
		.await
		.unwrap();

	assert_eq!(
		client.file_exists(file.name(), test_dir).await.unwrap(),
		Some(file.uuid())
	);

	let mut meta = file.get_meta();
	let new_name = "new_name.json";
	meta.set_name(new_name).unwrap();
	client.update_file_metadata(&mut file, meta).await.unwrap();

	assert_eq!(
		client.file_exists(new_name, test_dir).await.unwrap(),
		Some(file.uuid())
	);

	assert!(
		client
			.file_exists(file_name, test_dir)
			.await
			.unwrap()
			.is_none(),
	);
}

#[shared_test_runtime]
async fn file_trash_empty() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let file_name = "file.txt";
	let file = client.make_file_builder(file_name, test_dir).build();
	let mut file = client
		.upload_file(file.into(), b"Hello World from Rust!")
		.await
		.unwrap();

	assert_eq!(
		client
			.find_item_at_path(format!("{}/{}", test_dir.name(), file_name))
			.await
			.unwrap(),
		Some(FSObject::File(Cow::Borrowed(&file)))
	);
	let _lock = client
		.acquire_lock("test:rs:trash", std::time::Duration::from_secs(1), 600)
		.await
		.unwrap();
	client.trash_file(&mut file).await.unwrap();
	assert!(
		client
			.find_item_at_path(format!("{}/{}", test_dir.name(), file_name))
			.await
			.unwrap()
			.is_none()
	);

	assert_eq!(&client.get_file(file.uuid()).await.unwrap(), &file);
	client.empty_trash().await.unwrap();
	// emptying trash is asynchronous, so we need to wait a bit
	tokio::time::sleep(std::time::Duration::from_secs(30)).await;
	assert!(client.get_file(file.uuid()).await.is_err());
}

async fn test_callback_sums(client: &Client, test_dir: &RemoteDirectory, contents_len: usize) {
	let mut contents = vec![0u8; contents_len];
	rand::rng().try_fill_bytes(&mut contents).unwrap();
	let file_name = format!("file_{contents_len}.txt");
	let file = client.make_file_builder(file_name, test_dir).build();
	let (sender, receiver) = std::sync::mpsc::channel::<u64>();
	client
		.upload_file_from_reader(
			file.into(),
			&mut &contents[..],
			Some(Arc::new(|bytes_read: u64| {
				sender.send(bytes_read).unwrap();
			})),
			None,
		)
		.await
		.unwrap();
	std::mem::drop(sender); // Close the sender to stop the loop
	let mut total_bytes = 0;
	while let Ok(bytes_read) = receiver.recv() {
		total_bytes += bytes_read;
	}
	assert_eq!(total_bytes, contents.len() as u64);
}

#[shared_test_runtime]
async fn file_callbacks() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	test_callback_sums(client, test_dir, 10).await;
	test_callback_sums(client, test_dir, 1024 * 1024).await;
	test_callback_sums(client, test_dir, 1024 * 1024 * 8).await;
	test_callback_sums(client, test_dir, 1024 * 1024 * 8 + 1).await;
	test_callback_sums(client, test_dir, 1024 * 1024 * 8 - 1).await;
	test_callback_sums(client, test_dir, 0).await;
}

#[shared_test_runtime]
async fn file_favorite() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let file = client.make_file_builder("test", test_dir).build();
	let mut file = client.upload_file(file.into(), b"").await.unwrap();

	assert!(!file.favorited());

	client.set_favorite(&mut file, true).await.unwrap();
	assert!(file.favorited());

	client.set_favorite(&mut file, false).await.unwrap();
	assert!(!file.favorited());
}
