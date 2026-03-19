use std::{borrow::Cow, sync::Arc};

use chrono::{SubsecRound, Utc};
use filen_macros::shared_test_runtime;

use filen_sdk_rs::{
	auth::Client,
	crypto::shared::generate_random_base64_values,
	fs::{
		HasName, HasRemoteInfo, HasUUID,
		categories::{NonRootFileType, NonRootItemType},
		dir::RemoteDirectory,
		file::{
			client_impl::FileReaderSharedClientExt,
			meta::{FileMeta, FileMetaChanges},
			traits::{HasFileInfo, HasFileMeta},
		},
		name::EntryNameError,
	},
	io::client_impl::IoSharedClientExt,
	util::MaybeSendCallback,
};
use futures::AsyncReadExt;
use rand::TryRngCore;

async fn assert_file_upload_download_equal(name: &str, contents_len: usize) {
	let mut contents = vec![0u8; contents_len];
	rand::rng().try_fill_bytes(&mut contents).unwrap();

	let contents = contents.as_ref();
	let (resources, _lock) = test_utils::RESOURCES.get_resources_with_lock().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let file = client
		.make_file_builder(name, *test_dir.uuid())
		.unwrap()
		.build();
	let file = client.upload_file(file.into(), contents).await.unwrap();

	let found_file = match client
		.find_item_at_path(&format!("{}/{}", test_dir.name().unwrap(), name))
		.await
		.unwrap()
	{
		Some(NonRootFileType::File(file)) => file.into_owned(),
		_ => panic!("Expected a file"),
	};
	assert_eq!(
		file, found_file,
		"Downloaded file didn't match uploaded file for {name}"
	);

	let buf = client.download_file(&file).await.unwrap();

	assert_eq!(buf.len(), contents.len(), "File size mismatch for {name}");
	assert_eq!(&buf, contents, "File contents mismatch for {name}");

	let got_file = client.get_file(*file.uuid()).await.unwrap();
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
		.create_dir(&test_dir.into(), "second_dir")
		.await
		.unwrap();

	let rng = &mut rand::rng();
	let file_random_part_long = generate_random_base64_values(8, rng);
	let file_random_part_short = generate_random_base64_values(2, rng);

	let file_name = format!("{file_random_part_long}{file_random_part_short}.txt");

	let file = client
		.make_file_builder(&file_name, *second_dir.uuid())
		.unwrap()
		.build();
	let file = client.upload_file(file.into(), &[]).await.unwrap();

	let found_items = client
		.find_item_matches_for_name(&file_random_part_long)
		.await
		.unwrap();

	assert_eq!(
		found_items,
		vec![(
			NonRootItemType::File(Cow::Owned(file.clone())),
			format!(
				"/{}/{}",
				test_dir.name().unwrap(),
				second_dir.name().unwrap()
			)
		)]
	);

	let found_items = client
		.find_item_matches_for_name(&file_random_part_short)
		.await
		.unwrap();

	assert!(found_items.iter().any(|(item, _)| {
		if let NonRootItemType::File(found_file) = item {
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
	let file = client
		.make_file_builder(file_name, *test_dir.uuid())
		.unwrap()
		.build();
	let mut file = client
		.upload_file(file.into(), b"Hello World from Rust!")
		.await
		.unwrap();

	assert_eq!(
		client
			.find_item_at_path(&format!("{}/{}", test_dir.name().unwrap(), file_name))
			.await
			.unwrap(),
		Some(NonRootFileType::File(Cow::Borrowed(&file)))
	);

	let _lock = client
		.acquire_lock("test:rs:trash", std::time::Duration::from_secs(1), 600)
		.await
		.unwrap();
	client.trash_file(&mut file).await.unwrap();

	assert!(
		client
			.find_item_at_path(&format!("{}/{}", test_dir.name().unwrap(), file_name))
			.await
			.unwrap()
			.is_none()
	);

	client.restore_file(&mut file).await.unwrap();
	assert_eq!(
		client
			.find_item_at_path(&format!("{}/{}", test_dir.name().unwrap(), file_name))
			.await
			.unwrap(),
		Some(NonRootFileType::File(Cow::Borrowed(&file)))
	);
}

#[shared_test_runtime]
async fn file_delete_permanently() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let file_name = "file.txt";
	let file = client
		.make_file_builder(file_name, *test_dir.uuid())
		.unwrap()
		.build();
	let mut file = client
		.upload_file(file.into(), b"Hello World from Rust!")
		.await
		.unwrap();

	assert_eq!(
		client
			.find_item_at_path(&format!("{}/{}", test_dir.name().unwrap(), file_name))
			.await
			.unwrap(),
		Some(NonRootFileType::File(Cow::Borrowed(&file)))
	);

	client.delete_file_permanently(file.clone()).await.unwrap();

	assert!(
		client
			.find_item_at_path(&format!("{}/{}", test_dir.name().unwrap(), file_name))
			.await
			.unwrap()
			.is_none()
	);

	assert!(client.restore_file(&mut file).await.is_err());

	assert!(client.get_file(*file.uuid()).await.is_err());

	// Uncomment this when the API immediately permanently deletes the file
	// let mut reader = file.into_reader(client.clone());
	// let mut buf = Vec::new();
	// assert!(reader.read_to_end(&mut buf).await.is_err());
}

#[shared_test_runtime]
async fn file_link() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let file_name = "file.txt";
	let file = client
		.make_file_builder(file_name, *test_dir.uuid())
		.unwrap()
		.build();
	let file = client
		.upload_file(file.into(), b"Hello World from Rust!")
		.await
		.unwrap();

	let link = client.public_link_file(&file).await.unwrap();
	let got_link = client.get_file_link_status(&file).await.unwrap();
	assert_eq!(Some(&link), got_link.as_ref());
	client.remove_file_link(&file, link).await.unwrap();
	let got_link = client.get_file_link_status(&file).await.unwrap();
	assert_eq!(None, got_link);

	let mut link = client.public_link_file(&file).await.unwrap();
	let password = "test";
	link.set_password(password.to_owned());
	client.update_file_link(&file, &link).await.unwrap();
	let got_link = client.get_file_link_status(&file).await.unwrap();
	assert_eq!(Some(&link), got_link.as_ref());
	client.remove_file_link(&file, link).await.unwrap();
	let got_link = client.get_file_link_status(&file).await.unwrap();
	assert_eq!(None, got_link);
}

#[shared_test_runtime]
async fn file_move() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let file_name = "file.txt";
	let file = client
		.make_file_builder(file_name, *test_dir.uuid())
		.unwrap()
		.build();
	let mut file = client
		.upload_file(file.into(), b"Hello World from Rust!")
		.await
		.unwrap();

	assert_eq!(
		client
			.find_item_at_path(&format!("{}/{}", test_dir.name().unwrap(), file_name))
			.await
			.unwrap(),
		Some(NonRootFileType::File(Cow::Borrowed(&file)))
	);

	let second_dir = client
		.create_dir(&test_dir.into(), "second_dir")
		.await
		.unwrap();
	client
		.move_file(&mut file, &(&second_dir).into())
		.await
		.unwrap();

	assert!(
		client
			.find_item_at_path(&format!("{}/{}", test_dir.name().unwrap(), file_name))
			.await
			.unwrap()
			.is_none(),
	);

	assert_eq!(
		client
			.find_item_at_path(&format!(
				"{}/{}/{}",
				test_dir.name().unwrap(),
				second_dir.name().unwrap(),
				file_name
			))
			.await
			.unwrap(),
		Some(NonRootFileType::File(Cow::Borrowed(&file)))
	);
}

#[shared_test_runtime]
async fn file_update_meta() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let file_name = "file.txt";
	let file = client
		.make_file_builder(file_name, *test_dir.uuid())
		.unwrap()
		.build();
	let mut file = client
		.upload_file(file.into(), b"Hello World from Rust!")
		.await
		.unwrap();

	assert_eq!(
		client
			.find_item_at_path(&format!("{}/{}", test_dir.name().unwrap(), file_name))
			.await
			.unwrap(),
		Some(NonRootFileType::File(Cow::Borrowed(&file)))
	);

	client
		.update_file_metadata(
			&mut file,
			FileMetaChanges::default().name("new_name.json").unwrap(),
		)
		.await
		.unwrap();

	assert_eq!(file.name().unwrap(), "new_name.json");
	assert_eq!(
		client
			.find_item_at_path(&format!(
				"{}/{}",
				test_dir.name().unwrap(),
				file.name().unwrap()
			))
			.await
			.unwrap(),
		Some(NonRootFileType::File(Cow::Borrowed(&file)))
	);

	let created = Utc::now() - chrono::Duration::days(1);
	let modified = Utc::now();
	let new_mime = "application/json";

	client
		.update_file_metadata(
			&mut file,
			FileMetaChanges::default()
				.mime(new_mime.to_string())
				.last_modified(modified)
				.created(Some(created)),
		)
		.await
		.unwrap();
	assert_eq!(file.mime().unwrap(), new_mime);
	assert_eq!(file.created().unwrap(), created.round_subsecs(3));
	assert_eq!(file.last_modified().unwrap(), modified.round_subsecs(3));

	let found_file = client.get_file(*file.uuid()).await.unwrap();
	assert_eq!(found_file.mime().unwrap(), new_mime);
	assert_eq!(found_file.created().unwrap(), created.round_subsecs(3));
	assert_eq!(
		found_file.last_modified().unwrap(),
		modified.round_subsecs(3)
	);
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
			.file_exists(file_name, &test_dir.into())
			.await
			.unwrap()
			.is_none()
	);

	let file = client
		.make_file_builder(file_name, *test_dir.uuid())
		.unwrap()
		.build();
	let mut file = client
		.upload_file(file.into(), b"Hello World from Rust!")
		.await
		.unwrap();

	assert_eq!(
		client
			.file_exists(file.name().unwrap(), &test_dir.into())
			.await
			.unwrap(),
		Some(*file.uuid())
	);

	let new_name = "new_name.json";
	client
		.update_file_metadata(
			&mut file,
			FileMetaChanges::default().name(new_name).unwrap(),
		)
		.await
		.unwrap();

	assert_eq!(
		client
			.file_exists(new_name, &test_dir.into())
			.await
			.unwrap(),
		Some(*file.uuid())
	);

	assert!(
		client
			.file_exists(file_name, &test_dir.into())
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
	let file = client
		.make_file_builder(file_name, *test_dir.uuid())
		.unwrap()
		.build();
	let mut file = client
		.upload_file(file.into(), b"Hello World from Rust!")
		.await
		.unwrap();

	assert_eq!(
		client
			.find_item_at_path(&format!("{}/{}", test_dir.name().unwrap(), file_name))
			.await
			.unwrap(),
		Some(NonRootFileType::File(Cow::Borrowed(&file)))
	);
	let _lock = client
		.acquire_lock("test:rs:trash", std::time::Duration::from_secs(1), 600)
		.await
		.unwrap();
	client.trash_file(&mut file).await.unwrap();
	assert!(
		client
			.find_item_at_path(&format!("{}/{}", test_dir.name().unwrap(), file_name))
			.await
			.unwrap()
			.is_none()
	);

	assert_eq!(&client.get_file(*file.uuid()).await.unwrap(), &file);
	client.empty_trash().await.unwrap();
	// emptying trash is asynchronous, so we need to wait a bit
	tokio::time::sleep(std::time::Duration::from_secs(300)).await;
	assert!(client.get_file(*file.uuid()).await.is_err());
}

async fn test_callback_sums(client: &Client, test_dir: &RemoteDirectory, contents_len: usize) {
	let mut contents = vec![0u8; contents_len];
	rand::rng().try_fill_bytes(&mut contents).unwrap();
	let file_name = format!("file_{contents_len}.txt");
	let file = client
		.make_file_builder(&file_name, *test_dir.uuid())
		.unwrap()
		.build();
	let (sender, receiver) = std::sync::mpsc::channel::<u64>();
	client
		.upload_file_from_reader(
			file.into(),
			&mut &contents[..],
			Some(Arc::new(|bytes_read: u64| {
				sender.send(bytes_read).unwrap();
			}) as MaybeSendCallback<u64>),
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

	let file = client
		.make_file_builder("test", *test_dir.uuid())
		.unwrap()
		.build();
	let mut file = client.upload_file(file.into(), b"").await.unwrap();

	assert!(!file.favorited());

	client.set_file_favorite(&mut file, true).await.unwrap();
	assert!(file.favorited());

	client.set_file_favorite(&mut file, false).await.unwrap();
	assert!(!file.favorited());
}

#[shared_test_runtime]
async fn file_read_range() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let file = client
		.make_file_builder("test", *test_dir.uuid())
		.unwrap()
		.build();
	let file = client
		.upload_file(file.into(), b"Hello, Filen!")
		.await
		.unwrap();

	let mut reader = client.get_file_reader_for_range(&file, 6, 1000000);
	let mut buf = Vec::new();
	reader.read_to_end(&mut buf).await.unwrap();
	assert_eq!(str::from_utf8(&buf).unwrap(), " Filen!");
	buf.clear();
	let mut reader = client.get_file_reader_for_range(&file, 0, 5);
	reader.read_to_end(&mut buf).await.unwrap();
	assert_eq!(str::from_utf8(&buf).unwrap(), "Hello");

	let file = client
		.make_file_builder("test2", *test_dir.uuid())
		.unwrap()
		.build();

	let border_contents = b"Hello, Filen";
	let mut big_contents = vec![0u8; 1024 * 1024 * 3 + border_contents.len() / 2];
	big_contents
		[1024 * 1024 * 2 - border_contents.len() / 2..1024 * 1024 * 2 + border_contents.len() / 2]
		.copy_from_slice(&border_contents[..]);

	let file = client
		.upload_file(file.into(), &big_contents)
		.await
		.unwrap();

	let mut reader = client.get_file_reader_for_range(
		&file,
		(1024 * 1024 * 2 - border_contents.len() / 2) as u64,
		(1024 * 1024 * 2 + border_contents.len() / 2) as u64,
	);
	buf.clear();
	reader.read_to_end(&mut buf).await.unwrap();
	assert_eq!(str::from_utf8(&buf).unwrap(), "Hello, Filen");
}

#[shared_test_runtime]
async fn file_versions() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let mut versions = Vec::new();
	// TODO: when backend supports size in version info, use different lengths for these strings
	for content in ["Version 1", "Version a 2", "Version as 3", "Version asd 4"] {
		let base_file = client
			.make_file_builder("test", *test_dir.uuid())
			.unwrap()
			.build();
		let file = client
			.upload_file(base_file.clone().into(), content.as_bytes())
			.await
			.unwrap();
		// we do this because timestamps have a resolution of 1 second on the backend
		tokio::time::sleep(std::time::Duration::from_secs(2)).await;
		versions.push((file, content));
	}
	let mut current = versions.pop().unwrap().0;

	let mut listed_versions = client.list_file_versions(&current).await.unwrap();

	let current_version = listed_versions.remove(0);

	assert_eq!(current_version.metadata(), current.get_meta());
	assert_eq!(current_version.timestamp(), current.timestamp);

	assert_eq!(listed_versions.len(), versions.len());
	for (listed, (expected, expected_content)) in
		listed_versions.into_iter().zip(versions.iter_mut().rev())
	{
		assert_eq!(listed.metadata(), expected.get_meta());
		assert_eq!(listed.size(), expected.size());
		assert_eq!(listed.timestamp(), expected.timestamp());
		client
			.restore_file_version(&mut current, listed)
			.await
			.unwrap();
		let downloaded = client.download_file(&current).await.unwrap();
		assert_eq!(&downloaded, expected_content.as_bytes());
		let mut old_last_modified = None;
		if let (FileMeta::Decoded(expected_meta), FileMeta::Decoded(meta)) =
			(&mut expected.meta, &current.meta)
		{
			// restore file version updates the last modified time to fix a bug in the old sync engine
			// so we need to adjust that here before we assert_eq
			old_last_modified = Some(expected_meta.last_modified);
			expected_meta.last_modified = meta.last_modified;
		}
		assert_eq!(&current, expected);

		if let Some(old_last_modified) = old_last_modified {
			if let FileMeta::Decoded(expected) = &mut expected.meta {
				// undo the previous change for the next iteration
				expected.last_modified = old_last_modified;
			} else {
				unreachable!();
			}
		}
	}
}

/// HTTP provider tests.
///
/// Run with: `cargo test -p filen-sdk-rs --features http-provider,uniffi --test file_tests`
#[cfg(feature = "http-provider")]
mod http_provider_tests {
	use filen_macros::shared_test_runtime;
	use filen_sdk_rs::{fs::HasUUID, http_provider::client_impl::HttpProviderSharedClientExt};

	// ─── helpers ─────────────────────────────────────────────────────────────

	async fn upload_test_file(
		client: &filen_sdk_rs::auth::Client,
		test_dir: &filen_sdk_rs::fs::dir::RemoteDirectory,
		name: &str,
		contents: &[u8],
	) -> filen_sdk_rs::fs::file::RemoteFile {
		let file = client
			.make_file_builder(name, *test_dir.uuid())
			.unwrap()
			.build();
		client.upload_file(file.into(), contents).await.unwrap()
	}

	/// Parses a `multipart/byteranges` response body into `(headers, body)` pairs.
	async fn parse_multipart_body(
		body: bytes::Bytes,
		content_type: &str,
	) -> Vec<(http::HeaderMap, bytes::Bytes)> {
		let boundary = content_type
			.split(';')
			.map(str::trim)
			.find_map(|s| s.strip_prefix("boundary="))
			.unwrap_or_else(|| panic!("no boundary in content-type: {content_type}"))
			.to_string();
		let stream = futures::stream::once(async move { Ok::<_, std::convert::Infallible>(body) });
		let mut multipart = multer::Multipart::new(stream, boundary);
		let mut parts = Vec::new();
		while let Some(field) = multipart.next_field().await.unwrap() {
			let headers = field.headers().clone();
			let body = field.bytes().await.unwrap();
			parts.push((headers, body));
		}
		parts
	}

	// ─── basic serving ────────────────────────────────────────────────────────

	/// The provider serves the full file when no Range header is sent:
	/// 200 OK, correct Content-Length, Accept-Ranges: bytes, and correct body.
	#[shared_test_runtime]
	async fn http_provider_full_file_download() {
		let resources = test_utils::RESOURCES.get_resources().await;
		let client = &resources.client;
		let test_dir = &resources.dir;

		let content = b"Hello, HTTP Provider!";
		let file = upload_test_file(client, test_dir, "http_provider_full.txt", content).await;

		let handle = client.start_http_provider(None).await.unwrap();
		let url = handle.get_file_url(&(&file).into());

		let response = reqwest::get(&url).await.unwrap();
		assert_eq!(response.status(), 200);

		let content_length: u64 = response
			.headers()
			.get("content-length")
			.and_then(|v| v.to_str().ok())
			.and_then(|v| v.parse().ok())
			.expect("server must set Content-Length");
		assert_eq!(content_length, content.len() as u64);

		assert_eq!(
			response
				.headers()
				.get("accept-ranges")
				.and_then(|v| v.to_str().ok()),
			Some("bytes"),
			"server must advertise Accept-Ranges: bytes"
		);

		let body = response.bytes().await.unwrap();
		assert_eq!(&body[..], content);
	}

	/// The provider correctly starts and the port is non-zero.
	#[shared_test_runtime]
	async fn http_provider_starts_and_returns_port() {
		let resources = test_utils::RESOURCES.get_resources().await;
		let client = &resources.client;

		let handle = client.start_http_provider(None).await.unwrap();
		let port = handle.port();
		assert_ne!(port, 0, "provider should bind to an ephemeral port");
	}

	/// Calling `start_http_provider` twice returns a handle to the same server instance
	/// (same port number).
	#[shared_test_runtime]
	async fn http_provider_reuses_existing_instance() {
		let resources = test_utils::RESOURCES.get_resources().await;
		let client = &resources.client;

		let handle1 = client.start_http_provider(None).await.unwrap();
		let port1 = handle1.port();

		let handle2 = client.start_http_provider(None).await.unwrap();
		let port2 = handle2.port();

		assert_eq!(port1, port2, "both calls should return the same provider");
	}

	/// Dropping all handles eventually stops the provider.
	/// After dropping, the port should refuse new connections.
	#[shared_test_runtime]
	async fn http_provider_stops_on_drop() {
		let resources = test_utils::RESOURCES.get_resources().await;
		let client = &resources.client;

		let handle = client.start_http_provider(None).await.unwrap();
		let port = handle.port();
		drop(handle);

		// Give the graceful-shutdown a moment to complete.
		tokio::time::sleep(std::time::Duration::from_millis(500)).await;

		let result = reqwest::get(format!("http://127.0.0.1:{port}/file?file=x")).await;
		assert!(
			result.is_err(),
			"connection to stopped provider should fail, got: {:?}",
			result
		);
	}

	// ─── single-range requests ────────────────────────────────────────────────

	/// A partial range request (bytes=7-12) returns 206 with a correct Content-Range
	/// header, correct Content-Length, and the expected byte slice.
	#[shared_test_runtime]
	async fn http_provider_range_request() {
		let resources = test_utils::RESOURCES.get_resources().await;
		let client = &resources.client;
		let test_dir = &resources.dir;

		// "Hello, Filen!" — bytes 7-12 inclusive are "Filen!"
		let content = b"Hello, Filen!";
		let file = upload_test_file(client, test_dir, "http_provider_range.txt", content).await;

		let handle = client.start_http_provider(None).await.unwrap();
		let url = handle.get_file_url(&(&file).into());

		let response = reqwest::Client::new()
			.get(&url)
			.header("Range", "bytes=7-12")
			.send()
			.await
			.unwrap();

		assert_eq!(response.status(), 206, "partial request must return 206");

		// RFC 7233 §4.1 requires a Content-Range header in 206 responses.
		assert_eq!(
			response
				.headers()
				.get("content-range")
				.and_then(|v| v.to_str().ok()),
			Some(format!("bytes 7-12/{}", content.len()).as_str()),
			"206 response must include Content-Range"
		);

		let content_length: u64 = response
			.headers()
			.get("content-length")
			.and_then(|v| v.to_str().ok())
			.and_then(|v| v.parse().ok())
			.expect("server must set Content-Length");
		assert_eq!(content_length, 6, "bytes 7-12 is 6 bytes");

		let body = response.bytes().await.unwrap();
		assert_eq!(&body[..], b"Filen!");
	}

	/// A range that spans the entire file returns 200 OK (not 206 Partial Content),
	/// per RFC 7233 §4.1.
	#[shared_test_runtime]
	async fn http_provider_range_full_file() {
		let resources = test_utils::RESOURCES.get_resources().await;
		let client = &resources.client;
		let test_dir = &resources.dir;

		let content = b"Full range test content";
		let file =
			upload_test_file(client, test_dir, "http_provider_range_full.txt", content).await;

		let handle = client.start_http_provider(None).await.unwrap();
		let url = handle.get_file_url(&(&file).into());

		let len = content.len() as u64;
		let response = reqwest::Client::new()
			.get(&url)
			.header("Range", format!("bytes=0-{}", len - 1))
			.send()
			.await
			.unwrap();

		assert_eq!(response.status(), 200);

		let body = response.bytes().await.unwrap();
		assert_eq!(&body[..], content);
	}

	/// `bytes=0-N` (start=0, explicit end) returns 206 with the correct slice.
	#[shared_test_runtime]
	async fn http_provider_range_from_start() {
		let resources = test_utils::RESOURCES.get_resources().await;
		let client = &resources.client;
		let test_dir = &resources.dir;

		let content = b"Hello, Filen!";
		let file =
			upload_test_file(client, test_dir, "http_provider_range_start.txt", content).await;

		let handle = client.start_http_provider(None).await.unwrap();
		let url = handle.get_file_url(&(&file).into());

		// bytes 0-4 inclusive → "Hello"
		let response = reqwest::Client::new()
			.get(&url)
			.header("Range", "bytes=0-4")
			.send()
			.await
			.unwrap();

		assert_eq!(response.status(), 206);
		let body = response.bytes().await.unwrap();
		assert_eq!(&body[..], b"Hello");
	}

	/// `bytes=N-` (open-ended range from offset N) returns 206 with every byte from N
	/// to the end of the file.
	#[shared_test_runtime]
	async fn http_provider_open_ended_range() {
		let resources = test_utils::RESOURCES.get_resources().await;
		let client = &resources.client;
		let test_dir = &resources.dir;

		let content = b"Hello, Filen!";
		let file =
			upload_test_file(client, test_dir, "http_provider_open_ended.txt", content).await;

		let handle = client.start_http_provider(None).await.unwrap();
		let url = handle.get_file_url(&(&file).into());

		let response = reqwest::Client::new()
			.get(&url)
			.header("Range", "bytes=7-")
			.send()
			.await
			.unwrap();

		assert_eq!(response.status(), 206);

		assert_eq!(
			response
				.headers()
				.get("content-range")
				.and_then(|v| v.to_str().ok()),
			Some(format!("bytes 7-12/{}", content.len()).as_str()),
		);

		let body = response.bytes().await.unwrap();
		assert_eq!(&body[..], b"Filen!");
	}

	/// `bytes=-N` (suffix range: last N bytes) returns 206 with the last N bytes.
	#[shared_test_runtime]
	async fn http_provider_suffix_range() {
		let resources = test_utils::RESOURCES.get_resources().await;
		let client = &resources.client;
		let test_dir = &resources.dir;

		let content = b"Hello, Filen!";
		let file = upload_test_file(client, test_dir, "http_provider_suffix.txt", content).await;

		let handle = client.start_http_provider(None).await.unwrap();
		let url = handle.get_file_url(&(&file).into());

		let response = reqwest::Client::new()
			.get(&url)
			.header("Range", "bytes=-5")
			.send()
			.await
			.unwrap();

		assert_eq!(response.status(), 206);

		let body = response.bytes().await.unwrap();
		assert_eq!(&body[..], b"ilen!");
	}

	/// A large file (spanning multiple encrypted chunks) is served correctly without a
	/// Range header.
	#[shared_test_runtime]
	async fn http_provider_large_file_download() {
		let resources = test_utils::RESOURCES.get_resources().await;
		let client = &resources.client;
		let test_dir = &resources.dir;

		let content: Vec<u8> = (0u8..=255).cycle().take(1024 * 1024 * 3).collect();
		let file = upload_test_file(client, test_dir, "http_provider_large.bin", &content).await;

		let handle = client.start_http_provider(None).await.unwrap();
		let url = handle.get_file_url(&(&file).into());

		let response = reqwest::get(&url).await.unwrap();
		assert_eq!(response.status(), 200);
		let body = response.bytes().await.unwrap();
		assert_eq!(&body[..], &content[..]);
	}

	/// An empty file (size 0) is served as 200 OK with an empty body and Content-Length: 0.
	#[shared_test_runtime]
	async fn http_provider_empty_file() {
		let resources = test_utils::RESOURCES.get_resources().await;
		let client = &resources.client;
		let test_dir = &resources.dir;

		let file = upload_test_file(client, test_dir, "http_provider_empty.txt", b"").await;

		let handle = client.start_http_provider(None).await.unwrap();
		let url = handle.get_file_url(&(&file).into());

		let response = reqwest::get(&url).await.unwrap();
		assert_eq!(response.status(), 200);

		let content_length: u64 = response
			.headers()
			.get("content-length")
			.and_then(|v| v.to_str().ok())
			.and_then(|v| v.parse().ok())
			.unwrap_or(u64::MAX);
		assert_eq!(content_length, 0);

		let body = response.bytes().await.unwrap();
		assert_eq!(&body[..], b"");
	}

	/// A Range header whose bounds lie entirely beyond EOF returns 416 Range Not
	/// Satisfiable with a `Content-Range: bytes */size` header (RFC 7233 §4.4).
	#[shared_test_runtime]
	async fn http_provider_unsatisfiable_range() {
		let resources = test_utils::RESOURCES.get_resources().await;
		let client = &resources.client;
		let test_dir = &resources.dir;

		let content = b"Short file";
		let file = upload_test_file(client, test_dir, "http_provider_unsat.txt", content).await;

		let handle = client.start_http_provider(None).await.unwrap();
		let url = handle.get_file_url(&(&file).into());

		let response = reqwest::Client::new()
			.get(&url)
			.header("Range", "bytes=9999-99999")
			.send()
			.await
			.unwrap();

		assert_eq!(
			response.status(),
			416,
			"unsatisfiable range must return 416"
		);
		assert_eq!(
			response
				.headers()
				.get("content-range")
				.and_then(|v| v.to_str().ok()),
			Some(format!("bytes */{}", content.len()).as_str()),
			"416 response must include Content-Range: bytes */size"
		);
	}

	// ─── multi-range (multipart/byteranges) requests ──────────────────────────

	/// A multi-range request (`Range: bytes=0-4, 7-12`) returns a proper
	/// `multipart/byteranges` response (RFC 7233 §4.1) with the correct bytes in each part.
	#[shared_test_runtime]
	async fn http_provider_multipart_range() {
		let resources = test_utils::RESOURCES.get_resources().await;
		let client = &resources.client;
		let test_dir = &resources.dir;

		// "Hello, Filen!" (13 bytes)
		// bytes 0-4  → "Hello"
		// bytes 7-12 → "Filen!"
		let content = b"Hello, Filen!";
		let file = upload_test_file(client, test_dir, "http_provider_multipart.txt", content).await;

		let handle = client.start_http_provider(None).await.unwrap();
		let url = handle.get_file_url(&(&file).into());

		let response = reqwest::Client::new()
			.get(&url)
			.header("Range", "bytes=0-4, 7-12")
			.send()
			.await
			.unwrap();

		assert_eq!(response.status(), 206);

		let ct = response
			.headers()
			.get("content-type")
			.and_then(|v| v.to_str().ok())
			.unwrap_or("")
			.to_string();
		assert!(
			ct.starts_with("multipart/byteranges"),
			"multi-range response must use multipart/byteranges, got: {ct}"
		);

		let raw_body = response.bytes().await.unwrap();
		let parts = parse_multipart_body(raw_body, &ct).await;

		assert_eq!(parts.len(), 2, "expected exactly 2 parts");

		let size = content.len();

		// Each part must carry both Content-Type and Content-Range headers.
		for (i, (headers, _)) in parts.iter().enumerate() {
			assert!(
				headers.contains_key(http::header::CONTENT_TYPE),
				"part {i} is missing Content-Type header"
			);
			assert!(
				headers.contains_key(http::header::CONTENT_RANGE),
				"part {i} is missing Content-Range header"
			);
		}

		assert_eq!(
			parts[0]
				.0
				.get(http::header::CONTENT_RANGE)
				.and_then(|v| v.to_str().ok()),
			Some(format!("bytes 0-4/{size}").as_str()),
			"part 0 Content-Range mismatch"
		);
		assert_eq!(&parts[0].1[..], b"Hello", "part 0 body mismatch");

		assert_eq!(
			parts[1]
				.0
				.get(http::header::CONTENT_RANGE)
				.and_then(|v| v.to_str().ok()),
			Some(format!("bytes 7-12/{size}").as_str()),
			"part 1 Content-Range mismatch"
		);
		assert_eq!(&parts[1].1[..], b"Filen!", "part 1 body mismatch");
	}

	/// Adjacent ranges in a multi-range request are each returned as a separate part
	/// with correct headers and body bytes, not merged.
	#[shared_test_runtime]
	async fn http_provider_multipart_adjacent_ranges() {
		let resources = test_utils::RESOURCES.get_resources().await;
		let client = &resources.client;
		let test_dir = &resources.dir;

		// "ABCDEFGHIJKLMNOPQRSTUVWXYZ" (26 bytes)
		let content: Vec<u8> = (b'A'..=b'Z').collect();
		let file = upload_test_file(
			client,
			test_dir,
			"http_provider_multipart_adj.txt",
			&content,
		)
		.await;

		let handle = client.start_http_provider(None).await.unwrap();
		let url = handle.get_file_url(&(&file).into());

		let response = reqwest::Client::new()
			.get(&url)
			.header("Range", "bytes=0-9, 10-19, 20-25")
			.send()
			.await
			.unwrap();

		assert_eq!(response.status(), 206);

		let ct = response
			.headers()
			.get("content-type")
			.and_then(|v| v.to_str().ok())
			.unwrap_or("")
			.to_string();
		assert!(
			ct.starts_with("multipart/byteranges"),
			"expected multipart/byteranges, got: {ct}"
		);

		let raw_body = response.bytes().await.unwrap();
		let parts = parse_multipart_body(raw_body, &ct).await;

		assert_eq!(parts.len(), 3, "expected 3 parts for 3 ranges");

		let size = content.len();

		assert_eq!(
			parts[0]
				.0
				.get(http::header::CONTENT_RANGE)
				.and_then(|v| v.to_str().ok()),
			Some(format!("bytes 0-9/{size}").as_str()),
		);
		assert_eq!(&parts[0].1[..], b"ABCDEFGHIJ");

		assert_eq!(
			parts[1]
				.0
				.get(http::header::CONTENT_RANGE)
				.and_then(|v| v.to_str().ok()),
			Some(format!("bytes 10-19/{size}").as_str()),
		);
		assert_eq!(&parts[1].1[..], b"KLMNOPQRST");

		assert_eq!(
			parts[2]
				.0
				.get(http::header::CONTENT_RANGE)
				.and_then(|v| v.to_str().ok()),
			Some(format!("bytes 20-25/{size}").as_str()),
		);
		assert_eq!(&parts[2].1[..], b"UVWXYZ");
	}

	/// When a multi-range request mixes satisfiable and unsatisfiable sub-ranges, only
	/// the satisfiable ones are served as a `multipart/byteranges` response.
	/// RFC 7233 §4.4: 416 is returned only when ALL sub-ranges are unsatisfiable.
	#[shared_test_runtime]
	async fn http_provider_multipart_partial_satisfiable() {
		let resources = test_utils::RESOURCES.get_resources().await;
		let client = &resources.client;
		let test_dir = &resources.dir;

		// "Short" (5 bytes): bytes=0-2 and bytes=3-4 are satisfiable;
		// bytes=9999-99999 is not and gets filtered out before reaching the handler.
		let content = b"Short";
		let file = upload_test_file(
			client,
			test_dir,
			"http_provider_multipart_partial.txt",
			content,
		)
		.await;

		let handle = client.start_http_provider(None).await.unwrap();
		let url = handle.get_file_url(&(&file).into());

		let response = reqwest::Client::new()
			.get(&url)
			.header("Range", "bytes=0-2, 3-4, 9999-99999")
			.send()
			.await
			.unwrap();

		assert_eq!(response.status(), 206);

		let ct = response
			.headers()
			.get("content-type")
			.and_then(|v| v.to_str().ok())
			.unwrap_or("")
			.to_string();
		assert!(
			ct.starts_with("multipart/byteranges"),
			"two satisfiable ranges should produce multipart/byteranges, got: {ct}"
		);

		let raw_body = response.bytes().await.unwrap();
		let parts = parse_multipart_body(raw_body, &ct).await;

		// Only the two satisfiable sub-ranges are served.
		assert_eq!(
			parts.len(),
			2,
			"expected 2 parts (unsatisfiable range filtered out)"
		);

		let size = content.len();

		assert_eq!(
			parts[0]
				.0
				.get(http::header::CONTENT_RANGE)
				.and_then(|v| v.to_str().ok()),
			Some(format!("bytes 0-2/{size}").as_str()),
		);
		assert_eq!(&parts[0].1[..], b"Sho");

		assert_eq!(
			parts[1]
				.0
				.get(http::header::CONTENT_RANGE)
				.and_then(|v| v.to_str().ok()),
			Some(format!("bytes 3-4/{size}").as_str()),
		);
		assert_eq!(&parts[1].1[..], b"rt");
	}
}

#[cfg(feature = "malformed")]
#[shared_test_runtime]
async fn file_malformed_meta() {
	use filen_sdk_rs::fs::file::{meta::FileMeta, traits::HasFileMeta};

	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let uuid = client
		.create_malformed_file(
			&test_dir.into(),
			"malformed_meta",
			"malformed_meta",
			"asdfsadfasfd",
			"asdfsaf",
		)
		.await
		.unwrap();

	let file = client.get_file(uuid).await.unwrap();
	assert!(matches!(file.get_meta(), FileMeta::Encrypted(_)));

	let files = client
		.list_dir(&test_dir.into(), None::<&fn(u64, Option<u64>)>)
		.await
		.unwrap()
		.1;
	assert!(files.iter().any(|f| *f.uuid() == uuid));
	assert_eq!(files.len(), 1);
}

// ── Name validation: FileMetaChanges ────────────────────────────────

#[test]
fn file_meta_changes_rejects_invalid_names() {
	assert_eq!(
		FileMetaChanges::default().name("").unwrap_err(),
		EntryNameError::Empty
	);
	assert_eq!(
		FileMetaChanges::default().name(".").unwrap_err(),
		EntryNameError::DotEntry
	);
	assert_eq!(
		FileMetaChanges::default().name("..").unwrap_err(),
		EntryNameError::DotEntry
	);
	assert_eq!(
		FileMetaChanges::default().name(" leading").unwrap_err(),
		EntryNameError::LeadingSpace
	);
	assert_eq!(
		FileMetaChanges::default().name("trailing.").unwrap_err(),
		EntryNameError::TrailingDotOrSpace
	);
	assert_eq!(
		FileMetaChanges::default().name("trailing ").unwrap_err(),
		EntryNameError::TrailingDotOrSpace
	);
	assert!(matches!(
		FileMetaChanges::default().name("a/b"),
		Err(EntryNameError::ForbiddenChar { ch: '/', .. })
	));
	assert!(matches!(
		FileMetaChanges::default().name("a\\b"),
		Err(EntryNameError::ForbiddenChar { ch: '\\', .. })
	));
	assert!(matches!(
		FileMetaChanges::default().name("a:b"),
		Err(EntryNameError::ForbiddenChar { ch: ':', .. })
	));
	assert!(matches!(
		FileMetaChanges::default().name("a*b"),
		Err(EntryNameError::ForbiddenChar { ch: '*', .. })
	));
	assert!(matches!(
		FileMetaChanges::default().name("a?b"),
		Err(EntryNameError::ForbiddenChar { ch: '?', .. })
	));
	assert!(matches!(
		FileMetaChanges::default().name("a\"b"),
		Err(EntryNameError::ForbiddenChar { ch: '"', .. })
	));
	assert!(matches!(
		FileMetaChanges::default().name("a<b"),
		Err(EntryNameError::ForbiddenChar { ch: '<', .. })
	));
	assert!(matches!(
		FileMetaChanges::default().name("a>b"),
		Err(EntryNameError::ForbiddenChar { ch: '>', .. })
	));
	assert!(matches!(
		FileMetaChanges::default().name("a|b"),
		Err(EntryNameError::ForbiddenChar { ch: '|', .. })
	));
	assert_eq!(
		FileMetaChanges::default().name("CON").unwrap_err(),
		EntryNameError::ReservedName
	);
	assert_eq!(
		FileMetaChanges::default().name("con").unwrap_err(),
		EntryNameError::ReservedName
	);
	assert_eq!(
		FileMetaChanges::default().name("PRN").unwrap_err(),
		EntryNameError::ReservedName
	);
	assert_eq!(
		FileMetaChanges::default().name("AUX").unwrap_err(),
		EntryNameError::ReservedName
	);
	assert_eq!(
		FileMetaChanges::default().name("NUL").unwrap_err(),
		EntryNameError::ReservedName
	);
	assert_eq!(
		FileMetaChanges::default().name("COM1").unwrap_err(),
		EntryNameError::ReservedName
	);
	assert_eq!(
		FileMetaChanges::default().name("LPT9").unwrap_err(),
		EntryNameError::ReservedName
	);
	assert!(matches!(
		FileMetaChanges::default().name(&"x".repeat(256)),
		Err(EntryNameError::TooLong { .. })
	));
}

#[test]
fn file_meta_changes_accepts_valid_names() {
	for name in [
		"hello.txt",
		"file",
		".hidden",
		"CON.txt",
		"NUL.log",
		"COM1.dat",
		"CONSOLE",
		"NULL",
		"日本語.txt",
		"café.doc",
	] {
		assert!(
			FileMetaChanges::default().name(name).is_ok(),
			"expected {name:?} to be accepted"
		);
	}
}

// ── Name validation: make_file_builder ──────────────────────────────

#[shared_test_runtime]
async fn make_file_builder_rejects_invalid_names() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	assert!(matches!(
		client.make_file_builder("", *test_dir.uuid()),
		Err(EntryNameError::Empty)
	));
	assert!(matches!(
		client.make_file_builder("CON", *test_dir.uuid()),
		Err(EntryNameError::ReservedName)
	));
	assert!(matches!(
		client.make_file_builder("foo/bar", *test_dir.uuid()),
		Err(EntryNameError::ForbiddenChar { ch: '/', .. })
	));
	assert!(matches!(
		client.make_file_builder("trail.", *test_dir.uuid()),
		Err(EntryNameError::TrailingDotOrSpace)
	));
}

#[shared_test_runtime]
async fn make_file_builder_normalizes_nfc() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	// NFD: e + combining acute accent
	let nfd_name = "caf\u{0065}\u{0301}.txt";
	let nfc_name = "caf\u{00E9}.txt";

	let builder = client
		.make_file_builder(nfd_name, *test_dir.uuid())
		.unwrap();
	assert_eq!(builder.get_name(), nfc_name);
}

#[shared_test_runtime]
async fn file_upload_normalizes_nfc() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let nfd_name = "caf\u{0065}\u{0301}.txt";
	let nfc_name = "caf\u{00E9}.txt";

	let file = client
		.make_file_builder(nfd_name, *test_dir.uuid())
		.unwrap()
		.build();
	let file = client.upload_file(file.into(), b"nfc test").await.unwrap();
	assert_eq!(file.name().unwrap(), nfc_name);

	// Should be findable by NFC name
	let found = client
		.find_item_at_path(&format!("{}/{}", test_dir.name().unwrap(), nfc_name))
		.await
		.unwrap();
	assert!(matches!(found, Some(NonRootFileType::File(_))));
}

// ── Name validation: update_file_metadata ───────────────────────────

#[shared_test_runtime]
async fn update_file_meta_rejects_invalid_name() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let file = client
		.make_file_builder("valid.txt", *test_dir.uuid())
		.unwrap()
		.build();
	let mut file = client.upload_file(file.into(), b"content").await.unwrap();

	assert!(FileMetaChanges::default().name("").is_err());
	assert!(FileMetaChanges::default().name("CON").is_err());
	assert!(FileMetaChanges::default().name("a*b").is_err());

	// Valid rename should work
	client
		.update_file_metadata(
			&mut file,
			FileMetaChanges::default().name("renamed.txt").unwrap(),
		)
		.await
		.unwrap();
	assert_eq!(file.name().unwrap(), "renamed.txt");
}

#[shared_test_runtime]
async fn update_file_meta_normalizes_nfc() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let file = client
		.make_file_builder("nfc_test.txt", *test_dir.uuid())
		.unwrap()
		.build();
	let mut file = client.upload_file(file.into(), b"content").await.unwrap();

	let nfd_name = "u\u{0308}ber.txt"; // ü as u + combining diaeresis
	let nfc_name = "\u{00FC}ber.txt"; // ü as single codepoint

	client
		.update_file_metadata(
			&mut file,
			FileMetaChanges::default().name(nfd_name).unwrap(),
		)
		.await
		.unwrap();
	assert_eq!(file.name().unwrap(), nfc_name);
}
