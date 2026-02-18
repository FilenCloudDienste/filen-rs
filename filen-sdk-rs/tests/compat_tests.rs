use core::panic;
use std::{borrow::Cow, fmt::Write, sync::Arc};

use chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, Utc};
use filen_macros::shared_test_runtime;
use filen_sdk_rs::{
	auth::Client,
	connect::fs::SharedDirectory,
	crypto::file::FileKey,
	fs::{
		HasName, HasUUID, NonRootFSObject, UnsharedFSObject,
		dir::{HasUUIDContents, RemoteDirectory},
		file::FileBuilder,
	},
	io::client_impl::IoSharedClientExt,
	sync::lock::ResourceLock,
};
use filen_types::auth::{AuthVersion, FileEncryptionVersion};

use rand::TryRngCore;

fn get_compat_test_file(client: &Client, parent: &impl HasUUIDContents) -> (FileBuilder, String) {
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
		.key(
			FileKey::from_string_with_version(
				Cow::Borrowed(file_key_str),
				client.file_encryption_version(),
			)
			.unwrap(),
		);

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

async fn get_contact(
	client: &Client,
) -> (
	filen_types::api::v3::contacts::Contact<'static>,
	Arc<ResourceLock>,
	Arc<ResourceLock>,
) {
	let share_client = test_utils::SHARE_RESOURCES.client().await;
	let lock1 = client
		.acquire_lock_with_default("test:contact")
		.await
		.unwrap();
	let lock2 = share_client
		.acquire_lock_with_default("test:contact")
		.await
		.unwrap();

	let contacts = client.get_contacts().await.unwrap();
	for contact in contacts {
		if contact.email == share_client.email() {
			return (contact, lock1, lock2);
		}
	}
	let contact_uuid = client
		.send_contact_request(share_client.email())
		.await
		.unwrap();
	share_client
		.accept_contact_request(contact_uuid)
		.await
		.unwrap();

	let contacts = client.get_contacts().await.unwrap();
	for contact in contacts {
		if contact.email == share_client.email() {
			return (contact, lock1, lock2);
		}
	}
	panic!("Contact not found after sending request");
}

#[shared_test_runtime]
async fn make_rs_compat_dir() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;

	let _lock = client.acquire_lock_with_default("test:rs").await.unwrap();

	if let Some(UnsharedFSObject::Dir(dir)) = client.find_item_at_path("compat-rs").await.unwrap() {
		client.trash_dir(&mut dir.into_owned()).await.unwrap();
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
	client.upload_file(empty_file.into(), b"").await.unwrap();

	let (contact, _lock1, _lock2) = get_contact(client).await;
	client
		.share_dir(&compat_dir, &contact, &|downloaded, total| {
			log::trace!(
				"Shared compat-rs dir: downloaded {} / {:?}",
				downloaded,
				total
			);
		})
		.await
		.unwrap();

	let small_file = client.make_file_builder("small.txt", &compat_dir).build();
	client
		.upload_file(small_file.into(), b"Hello World from Rust!")
		.await
		.unwrap();

	if client.auth_version() == AuthVersion::V1 {
		// we aren't able to upload files larger than 1MiB to the V1 account
		return;
	}

	let mut big_random_bytes = vec![0u8; 1024 * 1024 * 4];
	// fill with random bytes
	rand::rng().try_fill_bytes(&mut big_random_bytes).unwrap();
	let big_file = client.make_file_builder("big.txt", &compat_dir).build();
	client
		.upload_file(
			big_file.into(),
			faster_hex::hex_string(&big_random_bytes).as_bytes(),
		)
		.await
		.unwrap();

	let (file, test_str) = get_compat_test_file(client, &compat_dir);
	let file = file.build();
	client
		.upload_file(file.into(), test_str.as_bytes())
		.await
		.unwrap();

	let file = client
		.make_file_builder("nameSplitter.json", &compat_dir)
		.build();
	client
		.upload_file(
			file.into(),
			serde_json::to_string(&get_name_splitter_test_value())
				.unwrap()
				.as_bytes(),
		)
		.await
		.unwrap();
}

async fn prep_shared_compat_tests(client: &Client, language: &str, shortened: &str) {
	let share_client = &test_utils::SHARE_RESOURCES.client().await;

	let _lock1 = client
		.acquire_lock_with_default("test:contact")
		.await
		.unwrap();
	let _lock2 = share_client
		.acquire_lock_with_default("test:contact")
		.await
		.unwrap();

	let share_dirs = share_client
		.list_in_shared()
		.await
		.unwrap()
		.0
		.into_iter()
		.filter(|d| d.get_dir().name() == Some(&format!("compat-{shortened}")))
		.collect::<Vec<_>>();

	assert_eq!(
		share_dirs.len(),
		3,
		"Expected 3 compat-{shortened} shares (one for each auth version)"
	);

	for dir in share_dirs {
		run_shared_compat_tests(share_client, dir, language).await;
	}
}

async fn run_shared_compat_tests(
	share_client: &Client,
	compat_dir: SharedDirectory,
	language: &str,
) {
	let (dirs, files) = share_client
		.list_in_shared_dir(compat_dir.get_dir())
		.await
		.unwrap();
	assert!(dirs.iter().any(|d| d.get_dir().name() == Some("dir")));

	let Some(empty_file) = files
		.iter()
		.find(|f| f.get_file().name() == Some("empty.txt"))
	else {
		panic!("empty.txt not found in shared compat dir for {language}");
	};

	assert_eq!(
		share_client
			.download_file(empty_file.get_file())
			.await
			.unwrap()
			.len(),
		0,
		"empty.txt should be empty"
	);

	let Some(small_file) = files
		.iter()
		.find(|f| f.get_file().name() == Some("small.txt"))
	else {
		panic!("small.txt not found in shared compat dir for {language}");
	};

	assert_eq!(
		share_client
			.download_file(small_file.get_file())
			.await
			.unwrap(),
		format!("Hello World from {language}!").as_bytes(),
		"small.txt contents mismatch"
	);
}

async fn run_compat_tests(
	client: &Client,
	compat_dir: RemoteDirectory,
	language: &str,
	shortened: &str,
) {
	match client.find_item_in_dir(&compat_dir, "dir").await.unwrap() {
		Some(NonRootFSObject::Dir(_)) => {}
		_ => panic!("dir not found in compat-{shortened} directory"),
	}
	match client
		.find_item_in_dir(&compat_dir, "empty.txt")
		.await
		.unwrap()
	{
		Some(NonRootFSObject::File(file)) => {
			assert_eq!(
				client.download_file(file.as_ref()).await.unwrap().len(),
				0,
				"empty.txt should be empty"
			);
		}
		_ => panic!("empty.txt not found in compat-{shortened} directory"),
	}
	match client
		.find_item_in_dir(&compat_dir, "small.txt")
		.await
		.unwrap()
	{
		Some(NonRootFSObject::File(file)) => {
			assert_eq!(
				client.download_file(file.as_ref()).await.unwrap(),
				format!("Hello World from {language}!").as_bytes(),
				"small.txt contents mismatch"
			);
		}
		_ => panic!("small.txt not found in compat-{shortened} directory"),
	}

	match client
		.find_item_in_dir(&compat_dir, "nameSplitter.json")
		.await
		.unwrap()
	{
		Some(NonRootFSObject::File(file)) => {
			let buf = client.download_file(file.as_ref()).await.unwrap();
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

	if client.auth_version() == AuthVersion::V1 {
		// we weren't able to upload files larger than 1MiB to the V1 account
		return;
	}
	match client
		.find_item_in_dir(&compat_dir, "big.txt")
		.await
		.unwrap()
	{
		Some(NonRootFSObject::File(file)) => {
			assert_eq!(
				client.download_file(file.as_ref()).await.unwrap().len(),
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
		Some(NonRootFSObject::File(file)) => {
			let compat_test_file = compat_test_file.uuid(*file.uuid()).build();
			assert_eq!(*file, compat_test_file, "file inner_file mismatch");

			let buf = client.download_file(file.as_ref()).await.unwrap();
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

#[shared_test_runtime]
async fn check_go_compat_dir() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;

	let _lock = client.acquire_lock_with_default("test:go").await.unwrap();

	let compat_dir = match client.find_item_at_path("compat-go").await.unwrap() {
		Some(UnsharedFSObject::Dir(dir)) => dir.into_owned(),
		_ => panic!("compat-go directory not found"),
	};

	run_compat_tests(client, compat_dir, "Go", "go").await;
	prep_shared_compat_tests(client, "Go", "go").await;
}

#[shared_test_runtime]
async fn check_ts_compat_dir() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;

	let _lock = client.acquire_lock_with_default("test:ts").await.unwrap();

	let compat_dir = match client.find_item_at_path("compat-ts").await.unwrap() {
		Some(UnsharedFSObject::Dir(dir)) => dir.into_owned(),
		_ => panic!("compat-ts directory not found"),
	};

	run_compat_tests(client, compat_dir, "TypeScript", "ts").await;
	// todo uncomment when TS compat dirs are shared
	// prep_shared_compat_tests(client, "TypeScript", "ts").await;
}
