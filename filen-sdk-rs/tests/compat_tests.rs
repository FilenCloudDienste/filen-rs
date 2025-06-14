use core::panic;
use std::{fmt::Write, str::FromStr};

use chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, Utc};
use filen_sdk_rs::{
	auth::Client,
	crypto::file::FileKey,
	fs::{
		FSObject, HasUUID,
		dir::{HasContents, RemoteDirectory},
		file::FileBuilder,
	},
};
use filen_types::auth::FileEncryptionVersion;
use futures::{AsyncReadExt, AsyncWriteExt};

use rand::TryRngCore;

fn get_compat_test_file(client: &Client, parent: &impl HasContents) -> (FileBuilder, String) {
	let file_key_str = match client.file_encryption_version() {
		FileEncryptionVersion::V1 => "0123456789abcdefghijklmnopqrstuv",
		FileEncryptionVersion::V2 => "0123456789abcdefghijklmnopqrstuv",
		FileEncryptionVersion::V3 => {
			&faster_hex::hex_string("0123456789abcdefghijklmnopqrstuv".as_bytes())
		}
	};
	let file = client
		.make_file_builder("large_sample-20mb.txt", parent)
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

#[derive(serde::Serialize, serde::Deserialize, Debug, PartialEq, Eq)]
struct NameSplitterFile {
	name1: String,
	split1: Vec<String>,
	name2: String,
	split2: Vec<String>,
	name3: String,
	split3: Vec<String>,
	name4: String,
	split4: Vec<String>,
}

fn get_name_splitter_test_value() -> NameSplitterFile {
	NameSplitterFile {
		name1: "General_Invitation_-_the_ECSO_Award_Finals_2024.docx".to_string(),
		split1: filen_sdk_rs::search::split_name(
			"General_Invitation_-_the_ECSO_Award_Finals_2024.docx",
			2,
			16,
		)
		.iter()
		.map(|s| s.to_string())
		.collect(),
		name2: "Screenshot 2023-05-16 201840.png".to_string(),
		split2: filen_sdk_rs::search::split_name("Screenshot 2023-05-16 201840.png", 2, 16)
			.iter()
			.map(|s| s.to_string())
			.collect(),
		name3: "!service-invoice-657c56116e4f6947a80001cc.pdf".to_string(),
		split3: filen_sdk_rs::search::split_name(
			"!service-invoice-657c56116e4f6947a80001cc.pdf",
			2,
			16,
		)
		.iter()
		.map(|s| s.to_string())
		.collect(),
		name4: "файл.txt".to_string(),
		split4: filen_sdk_rs::search::split_name("файл.txt", 2, 16)
			.iter()
			.map(|s| s.to_string())
			.collect(),
	}
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn make_rs_compat_dir() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;

	let _lock = client
		.acquire_lock("test:rs", std::time::Duration::from_secs(1), 600)
		.await
		.unwrap();

	if let Some(FSObject::Dir(dir)) = client.find_item_at_path("compat-rs").await.unwrap() {
		client.trash_dir(&dir).await.unwrap();
	}

	let compat_dir = client
		.create_dir(client.root(), "compat-rs".to_string())
		.await
		.unwrap();

	client
		.create_dir(&compat_dir, "dir".to_string())
		.await
		.unwrap();

	let empty_file = client.make_file_builder("empty.txt", &compat_dir).build();
	let mut writer = client.get_file_writer(empty_file).unwrap();
	writer.write_all(b"").await.unwrap();
	writer.close().await.unwrap();

	let small_file = client.make_file_builder("small.txt", &compat_dir).build();
	let mut writer = client.get_file_writer(small_file).unwrap();
	writer.write_all(b"Hello World from Rust!").await.unwrap();
	writer.close().await.unwrap();
	writer.into_remote_file().unwrap();

	let mut big_random_bytes = vec![0u8; 1024 * 1024 * 4];
	// fill with random bytes
	rand::rng().try_fill_bytes(&mut big_random_bytes).unwrap();
	let big_file = client.make_file_builder("big.txt", &compat_dir).build();
	let mut writer = client.get_file_writer(big_file).unwrap();
	writer
		.write_all(faster_hex::hex_string(&big_random_bytes).as_bytes())
		.await
		.unwrap();
	writer.close().await.unwrap();

	let (file, test_str) = get_compat_test_file(client, &compat_dir);
	let file = file.build();
	let mut writer = client.get_file_writer(file).unwrap();
	writer.write_all(test_str.as_bytes()).await.unwrap();
	writer.close().await.unwrap();

	let file = client
		.make_file_builder("nameSplitter.json", &compat_dir)
		.build();
	let mut writer = client.get_file_writer(file).unwrap();
	writer
		.write_all(
			serde_json::to_string(&get_name_splitter_test_value())
				.unwrap()
				.as_bytes(),
		)
		.await
		.unwrap();
	writer.close().await.unwrap();
}

async fn run_compat_tests(client: &Client, compat_dir: RemoteDirectory, language: &str) {
	match client.find_item_in_dir(&compat_dir, "dir").await.unwrap() {
		Some(FSObject::Dir(_)) => {}
		_ => panic!("dir not found in compat-go directory"),
	}
	match client
		.find_item_in_dir(&compat_dir, "empty.txt")
		.await
		.unwrap()
	{
		Some(FSObject::File(file)) => {
			let mut reader = client.get_file_reader(file.as_ref());
			let mut buf = Vec::new();
			reader.read_to_end(&mut buf).await.unwrap();
			assert_eq!(buf.len(), 0, "empty.txt should be empty");
		}
		_ => panic!("empty.txt not found in compat-go directory"),
	}
	match client
		.find_item_in_dir(&compat_dir, "small.txt")
		.await
		.unwrap()
	{
		Some(FSObject::File(file)) => {
			let mut reader = client.get_file_reader(file.as_ref());
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
	match client
		.find_item_in_dir(&compat_dir, "big.txt")
		.await
		.unwrap()
	{
		Some(FSObject::File(file)) => {
			let mut reader = client.get_file_reader(file.as_ref());
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

	let (compat_test_file, test_str) = get_compat_test_file(client, &compat_dir);

	match client
		.find_item_in_dir(&compat_dir, "large_sample-20mb.txt")
		.await
		.unwrap()
	{
		Some(FSObject::File(file)) => {
			let compat_test_file = compat_test_file.uuid(file.uuid()).build();
			assert_eq!(
				*file.inner_file(),
				compat_test_file,
				"file inner_file mismatch"
			);

			let mut reader = client.get_file_reader(file.as_ref());
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

	match client
		.find_item_in_dir(&compat_dir, "nameSplitter.json")
		.await
		.unwrap()
	{
		Some(FSObject::File(file)) => {
			let mut reader = client.get_file_reader(file.as_ref());
			let mut buf = Vec::new();
			reader.read_to_end(&mut buf).await.unwrap();
			let mut name_splitter = serde_json::from_slice::<NameSplitterFile>(&buf).unwrap();
			name_splitter.split1.sort_unstable();
			name_splitter.split2.sort_unstable();
			name_splitter.split3.sort_unstable();
			name_splitter.split4.sort_unstable();
			assert_eq!(
				name_splitter,
				get_name_splitter_test_value(),
				"nameSplitter.json contents mismatch"
			);
		}
		_ => panic!("nameSplitter.json not found in compat-go directory"),
	};
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn check_go_compat_dir() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;

	let _lock = client
		.acquire_lock("test:go", std::time::Duration::from_secs(1), 600)
		.await
		.unwrap();

	let compat_dir = match client.find_item_at_path("compat-go").await.unwrap() {
		Some(FSObject::Dir(dir)) => dir.into_owned(),
		_ => panic!("compat-go directory not found"),
	};

	run_compat_tests(client, compat_dir, "Go").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn check_ts_compat_dir() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;

	let _lock = client
		.acquire_lock("test:ts", std::time::Duration::from_secs(1), 600)
		.await
		.unwrap();

	let compat_dir = match client.find_item_at_path("compat-ts").await.unwrap() {
		Some(FSObject::Dir(dir)) => dir.into_owned(),
		_ => panic!("compat-go directory not found"),
	};

	run_compat_tests(client, compat_dir, "TypeScript").await;
}
