use std::{borrow::Cow, sync::Arc};

use filen_sdk_rs::{
	fs::{FSObjectType, NonRootFSObject, file::FileBuilder},
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
