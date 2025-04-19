use core::panic;
use std::{fmt::Write, str::FromStr, sync::Arc};

use chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, Utc};
use filen_sdk_rs::{
	auth::Client,
	fs::{
		FSObjectType, HasContents,
		dir::Directory,
		file::{FileBuilder, FileKey},
	},
	prelude::*,
};
use filen_types::auth::FileEncryptionVersion;
use futures::{AsyncReadExt, AsyncWriteExt};
use rand::TryRngCore;

mod test_utils;

fn get_compat_test_file(client: &Client, parent: impl HasContents) -> (FileBuilder, String) {
	let file_key_str = match client.file_encryption_version() {
		FileEncryptionVersion::V1 => "0123456789abcdefghijklmnopqrstuv",
		FileEncryptionVersion::V2 => "0123456789abcdefghijklmnopqrstuv",
		FileEncryptionVersion::V3 => {
			&faster_hex::hex_string("0123456789abcdefghijklmnopqrstuv".as_bytes())
		}
	};
	let file = FileBuilder::new("large_sample-20mb.txt", parent, client)
		.created(DateTime::<Utc>::from_naive_utc_and_offset(
			NaiveDateTime::new(
				NaiveDate::from_ymd_opt(2025, 1, 11).unwrap(),
				NaiveTime::from_hms_milli_opt(12, 13, 14, 15).unwrap(),
			),
			Utc,
		))
		.modified(DateTime::<Utc>::from_naive_utc_and_offset(
			NaiveDateTime::new(
				NaiveDate::from_ymd_opt(2025, 1, 11).unwrap(),
				NaiveTime::from_hms_milli_opt(12, 13, 14, 16).unwrap(),
			),
			Utc,
		))
		.key(FileKey::from_str(file_key_str).unwrap());

	let mut test_str = String::new();
	for i in 0..2_700_000 {
		test_str.write_str(i.to_string().as_str()).unwrap();
		test_str.write_char('\n').unwrap();
	}

	(file, test_str)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn make_rs_compat_dir() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = Arc::new(resources.client.clone());

	if let Some(FSObjectType::Dir(dir)) = find_item_at_path(&client, "compat-rs").await.unwrap() {
		trash_dir(&client, dir.into_owned()).await.unwrap();
	}

	let compat_dir = create_dir(&client, client.root(), "compat-rs")
		.await
		.unwrap();

	create_dir(&client, &compat_dir, "dir").await.unwrap();

	let empty_file = FileBuilder::new("empty.txt", &compat_dir, &client).build();
	let mut writer = empty_file.into_writer(client.clone());
	writer.write_all(b"").await.unwrap();
	writer.close().await.unwrap();

	let small_file = FileBuilder::new("small.txt", &compat_dir, &client).build();
	let mut writer = small_file.into_writer(client.clone());
	writer.write_all(b"Hello World from Rust!").await.unwrap();
	writer.close().await.unwrap();
	writer.into_remote_file().unwrap();

	let mut big_random_bytes = vec![0u8; 1024 * 1024 * 4];
	// fill with random bytes
	rand::rng().try_fill_bytes(&mut big_random_bytes).unwrap();
	let big_file = FileBuilder::new("big.txt", &compat_dir, &client).build();
	let mut writer = big_file.into_writer(client.clone());
	writer
		.write_all(faster_hex::hex_string(&big_random_bytes).as_bytes())
		.await
		.unwrap();
	writer.close().await.unwrap();

	let (file, test_str) = get_compat_test_file(&client, &compat_dir);
	let file = file.build();
	let mut writer = file.into_writer(client.clone());
	writer.write_all(test_str.as_bytes()).await.unwrap();
	writer.close().await.unwrap();
}

async fn run_compat_tests(client: Arc<Client>, compat_dir: Directory, language: &str) {
	match find_item_in_dir(&client, &compat_dir, "dir").await.unwrap() {
		Some(FSObjectType::Dir(_)) => {}
		_ => panic!("dir not found in compat-go directory"),
	}
	match find_item_in_dir(&client, &compat_dir, "empty.txt")
		.await
		.unwrap()
	{
		Some(FSObjectType::File(file)) => {
			let mut reader = file.into_owned().into_reader(client.clone());
			let mut buf = Vec::new();
			reader.read_to_end(&mut buf).await.unwrap();
			assert_eq!(buf.len(), 0, "empty.txt should be empty");
		}
		_ => panic!("empty.txt not found in compat-go directory"),
	}
	match find_item_in_dir(&client, &compat_dir, "small.txt")
		.await
		.unwrap()
	{
		Some(FSObjectType::File(file)) => {
			let mut reader = file.into_owned().into_reader(client.clone());
			let mut buf = Vec::new();
			reader.read_to_end(&mut buf).await.unwrap();
			assert_eq!(
				buf,
				format!("Hello World from {}!", language).as_bytes(),
				"small.txt contents mismatch"
			);
		}
		_ => panic!("small.txt not found in compat-go directory"),
	}
	match find_item_in_dir(&client, &compat_dir, "big.txt")
		.await
		.unwrap()
	{
		Some(FSObjectType::File(file)) => {
			let mut reader = file.into_owned().into_reader(client.clone());
			let mut buf = Vec::with_capacity(1024 * 1024 * 4 * 2);
			reader.read_to_end(&mut buf).await.unwrap();
			assert_eq!(
				buf.len(),
				1024 * 1024 * 4 * 2,
				"big.txt should be 8MiB of random bytes"
			);
		}
		_ => panic!("big.txt not found in compat-go directory"),
	}

	let (compat_test_file, test_str) = get_compat_test_file(&client, &compat_dir);

	match find_item_in_dir(&client, &compat_dir, "large_sample-20mb.txt")
		.await
		.unwrap()
	{
		Some(FSObjectType::File(file)) => {
			let compat_test_file = compat_test_file.uuid(file.uuid()).build();
			assert_eq!(
				*file.inner_file(),
				compat_test_file,
				"file inner_file mismatch"
			);

			let mut reader = file.into_owned().into_reader(client.clone());
			let mut buf = Vec::with_capacity(test_str.len());
			reader.read_to_end(&mut buf).await.unwrap();
			assert_eq!(test_str.len(), buf.len(), "file size mismatch");
			assert_eq!(
				test_str,
				String::from_utf8_lossy(&buf),
				"file contents mismatch"
			);
		}
		_ => panic!("large_sample-20mb.txt not found in compat-go directory"),
	}
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn check_go_compat_dir() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = Arc::new(resources.client.clone());

	let compat_dir = match find_item_at_path(&client, "compat-go").await.unwrap() {
		Some(FSObjectType::Dir(dir)) => dir.into_owned(),
		_ => panic!("compat-go directory not found"),
	};

	run_compat_tests(client, compat_dir, "Go").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn check_ts_compat_dir() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = Arc::new(resources.client.clone());

	let compat_dir = match find_item_at_path(&client, "compat-ts").await.unwrap() {
		Some(FSObjectType::Dir(dir)) => dir.into_owned(),
		_ => panic!("compat-go directory not found"),
	};

	run_compat_tests(client, compat_dir, "TypeScript").await;
}
