use std::{borrow::Cow, sync::Arc};

use chrono::{SubsecRound, Utc};
use filen_sdk_rs::{
	fs::{
		FSObjectType, NonRootFSObject,
		file::{FileBuilder, update_file_metadata},
	},
	prelude::*,
};
use futures::{AsyncReadExt, AsyncWriteExt};
use rand::TryRngCore;

mod test_utils;

async fn assert_file_upload_download_equal(name: &str, contents_len: usize) {
	let mut contents = vec![0u8; contents_len];
	rand::rng().try_fill_bytes(&mut contents).unwrap();

	let contents = contents.as_ref();
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = Arc::new(resources.client.clone());
	let test_dir = &resources.dir;

	let file = FileBuilder::new(name, test_dir, &client).build();
	let mut writer = file.into_writer(client.clone());
	writer.write_all(contents).await.unwrap();
	writer.close().await.unwrap();
	let file = writer.into_remote_file().unwrap();

	let found_file = match find_item_at_path(&client, format!("{}/{}", test_dir.name(), name))
		.await
		.unwrap()
	{
		Some(FSObjectType::File(file)) => file.into_owned(),
		_ => panic!("Expected a file"),
	};
	assert_eq!(
		file, found_file,
		"Downloaded file didn't match uploaded file for {}",
		name
	);

	let mut reader = found_file.into_reader(client.clone());
	let mut buf = Vec::with_capacity(contents.len());
	reader.read_to_end(&mut buf).await.unwrap();

	assert_eq!(buf.len(), contents.len(), "File size mismatch for {}", name);
	assert_eq!(&buf, contents, "File contents mismatch for {}", name);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
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

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn file_search() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = Arc::new(resources.client.clone());
	let test_dir = &resources.dir;

	let second_dir = create_dir(&client, test_dir, "second_dir").await.unwrap();

	let file_random_part_long = generate_random_base64_values(16);
	let file_random_part_short = generate_random_base64_values(2);

	let file_name = format!("{}{}.txt", file_random_part_long, file_random_part_short);

	let file = FileBuilder::new(&file_name, &second_dir, &client).build();
	let mut writer = file.into_writer(client.clone());
	writer.close().await.unwrap();
	let file = writer.into_remote_file().unwrap();

	let found_items = find_item_matches_for_name(&client, file_random_part_long)
		.await
		.unwrap();

	assert_eq!(
		found_items,
		vec![(
			NonRootFSObject::File(Cow::Owned(file.clone())),
			format!("/{}/{}", test_dir.name(), second_dir.name())
		)]
	);

	let found_items = find_item_matches_for_name(&client, file_random_part_short)
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

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn file_trash() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = Arc::new(resources.client.clone());
	let test_dir = &resources.dir;

	let file_name = "file.txt";
	let file = FileBuilder::new(file_name, test_dir, &client).build();
	let mut writer = file.into_writer(client.clone());
	writer.write_all(b"Hello World from Rust!").await.unwrap();
	writer.close().await.unwrap();
	let file = writer.into_remote_file().unwrap();

	assert_eq!(
		find_item_at_path(&client, format!("{}/{}", test_dir.name(), file_name))
			.await
			.unwrap(),
		Some(FSObjectType::File(Cow::Borrowed(&file)))
	);

	let _lock = acquire_lock(
		client.clone(),
		"test:rs:trash",
		std::time::Duration::from_secs(1),
		600,
	)
	.await
	.unwrap();
	trash_file(&client, &file).await.unwrap();

	assert!(
		find_item_at_path(&client, format!("{}/{}", test_dir.name(), file_name))
			.await
			.unwrap()
			.is_none()
	);

	restore_file(&client, &file).await.unwrap();
	assert_eq!(
		find_item_at_path(&client, format!("{}/{}", test_dir.name(), file_name))
			.await
			.unwrap(),
		Some(FSObjectType::File(Cow::Borrowed(&file)))
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn file_delete_permanently() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = Arc::new(resources.client.clone());
	let test_dir = &resources.dir;

	let file_name = "file.txt";
	let file = FileBuilder::new(file_name, test_dir, &client).build();
	let mut writer = file.into_writer(client.clone());
	writer.write_all(b"Hello World from Rust!").await.unwrap();
	writer.close().await.unwrap();
	let file = writer.into_remote_file().unwrap();

	assert_eq!(
		find_item_at_path(&client, format!("{}/{}", test_dir.name(), file_name))
			.await
			.unwrap(),
		Some(FSObjectType::File(Cow::Borrowed(&file)))
	);

	delete_file_permanently(&client, file.clone())
		.await
		.unwrap();

	assert!(
		find_item_at_path(&client, format!("{}/{}", test_dir.name(), file_name))
			.await
			.unwrap()
			.is_none()
	);

	assert!(restore_file(&client, &file).await.is_err());

	assert!(get_file(&client, file.uuid()).await.is_err());

	// Uncomment this when the API immediately permanently deletes the file
	// let mut reader = file.into_reader(client.clone());
	// let mut buf = Vec::new();
	// assert!(reader.read_to_end(&mut buf).await.is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn file_move() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = Arc::new(resources.client.clone());
	let test_dir = &resources.dir;

	let file_name = "file.txt";
	let file = FileBuilder::new(file_name, test_dir, &client).build();
	let mut writer = file.into_writer(client.clone());
	writer.write_all(b"Hello World from Rust!").await.unwrap();
	writer.close().await.unwrap();
	let mut file = writer.into_remote_file().unwrap();

	assert_eq!(
		find_item_at_path(&client, format!("{}/{}", test_dir.name(), file_name))
			.await
			.unwrap(),
		Some(FSObjectType::File(Cow::Borrowed(&file)))
	);

	let second_dir = create_dir(&client, test_dir, "second_dir").await.unwrap();
	move_file(&client, &mut file, &second_dir).await.unwrap();

	assert!(
		find_item_at_path(&client, format!("{}/{}", test_dir.name(), file_name))
			.await
			.unwrap()
			.is_none(),
	);

	assert_eq!(
		find_item_at_path(
			&client,
			format!("{}/{}/{}", test_dir.name(), second_dir.name(), file_name)
		)
		.await
		.unwrap(),
		Some(FSObjectType::File(Cow::Borrowed(&file)))
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn file_update_meta() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = Arc::new(resources.client.clone());
	let test_dir = &resources.dir;

	let file_name = "file.txt";
	let file = FileBuilder::new(file_name, test_dir, &client).build();
	let mut writer = file.into_writer(client.clone());
	writer.write_all(b"Hello World from Rust!").await.unwrap();
	writer.close().await.unwrap();
	let mut file = writer.into_remote_file().unwrap();

	assert_eq!(
		find_item_at_path(&client, format!("{}/{}", test_dir.name(), file_name))
			.await
			.unwrap(),
		Some(FSObjectType::File(Cow::Borrowed(&file)))
	);

	let mut meta = file.get_meta();
	meta.set_name("new_name.json");

	update_file_metadata(&client, &mut file, meta)
		.await
		.unwrap();

	assert_eq!(file.name(), "new_name.json");
	assert_eq!(
		find_item_at_path(&client, format!("{}/{}", test_dir.name(), file.name()))
			.await
			.unwrap(),
		Some(FSObjectType::File(Cow::Borrowed(&file)))
	);

	let mut meta = file.get_meta();
	let created = Utc::now() - chrono::Duration::days(1);
	let modified = Utc::now();
	let new_mime = "application/json";
	meta.set_mime(new_mime);
	meta.set_last_modified(modified);
	meta.set_created(created);

	update_file_metadata(&client, &mut file, meta)
		.await
		.unwrap();
	assert_eq!(file.mime(), new_mime);
	assert_eq!(file.created(), created.round_subsecs(3));
	assert_eq!(file.last_modified(), modified.round_subsecs(3));

	let found_file = get_file(&client, file.uuid()).await.unwrap();
	assert_eq!(found_file.mime(), new_mime);
	assert_eq!(found_file.created(), created.round_subsecs(3));
	assert_eq!(found_file.last_modified(), modified.round_subsecs(3));
	assert_eq!(found_file, file);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn file_exists() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = Arc::new(resources.client.clone());
	let test_dir = &resources.dir;

	let file_name = "file.txt";

	assert!(
		exists_file(&client, file_name, test_dir)
			.await
			.unwrap()
			.is_none()
	);

	let file = FileBuilder::new(file_name, test_dir, &client).build();
	let mut writer = file.into_writer(client.clone());
	writer.write_all(b"Hello World from Rust!").await.unwrap();
	writer.close().await.unwrap();
	let mut file = writer.into_remote_file().unwrap();

	assert_eq!(
		exists_file(&client, file.name(), test_dir).await.unwrap(),
		Some(file.uuid())
	);

	let mut meta = file.get_meta();
	let new_name = "new_name.json";
	meta.set_name(new_name);
	update_file_metadata(&client, &mut file, meta)
		.await
		.unwrap();

	assert_eq!(
		exists_file(&client, new_name, test_dir).await.unwrap(),
		Some(file.uuid())
	);

	assert!(
		exists_file(&client, file_name, test_dir)
			.await
			.unwrap()
			.is_none(),
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn file_trash_empty() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = Arc::new(resources.client.clone());
	let test_dir = &resources.dir;

	let file_name = "file.txt";
	let file = FileBuilder::new(file_name, test_dir, &client).build();
	let mut writer = file.into_writer(client.clone());
	writer.write_all(b"Hello World from Rust!").await.unwrap();
	writer.close().await.unwrap();
	let file = writer.into_remote_file().unwrap();

	assert_eq!(
		find_item_at_path(&client, format!("{}/{}", test_dir.name(), file_name))
			.await
			.unwrap(),
		Some(FSObjectType::File(Cow::Borrowed(&file)))
	);
	let _lock = acquire_lock(
		client.clone(),
		"test:rs:trash",
		std::time::Duration::from_secs(1),
		600,
	)
	.await
	.unwrap();
	trash_file(&client, &file).await.unwrap();
	assert!(
		find_item_at_path(&client, format!("{}/{}", test_dir.name(), file_name))
			.await
			.unwrap()
			.is_none()
	);

	assert_eq!(&get_file(&client, file.uuid()).await.unwrap(), &file);
	empty_trash(&client).await.unwrap();
	// emptying trash is asynchronous, so we need to wait a bit
	tokio::time::sleep(std::time::Duration::from_secs(10)).await;
	assert!(get_file(&client, file.uuid()).await.is_err());
}
