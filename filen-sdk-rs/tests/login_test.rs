use core::panic;
use std::{env, sync::Arc};

use base64::{Engine, prelude::BASE64_URL_SAFE_NO_PAD};
use filen_sdk_rs::{
	auth::Client,
	fs::{FSObjectType, HasMeta, HasUUID, dir::Directory, file::FileBuilder, move_dir},
	prelude::*,
};

use futures::{AsyncReadExt, AsyncWriteExt};
use rand::TryRngCore;
use tokio::sync::OnceCell;

struct Resources {
	client: OnceCell<Client>,
}

struct TestResources {
	client: Client,
	dir: Directory,
}

impl Default for TestResources {
	fn default() -> Self {
		Self {
			client: RESOURCES.client.get().unwrap().clone(),
			dir: Directory::default(),
		}
	}
}

impl Drop for TestResources {
	fn drop(&mut self) {
		futures::executor::block_on(async move {
			match trash_dir(&self.client, self.dir.clone()).await {
				Ok(_) => {}
				Err(e) => eprintln!("Failed to clean up test directory: {}", e),
			}
		})
	}
}

impl Resources {
	async fn client(&self) -> &Client {
		self.client
			.get_or_init(|| async {
				dotenv::dotenv().ok();
				login(
					env::var("TEST_EMAIL").unwrap(),
					&env::var("TEST_PASSWORD").unwrap(),
					&env::var("TEST_2FA_CODE").unwrap_or("XXXXXX".to_string()),
				)
				.await
				.unwrap()
			})
			.await
	}

	async fn get_resources(&self) -> TestResources {
		let name = format!(
			"rs-{}",
			BASE64_URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>())
		);
		let client = self.client().await.clone();
		let test_dir = create_dir(&client, client.root(), name).await.unwrap();
		TestResources {
			client,
			dir: test_dir,
		}
	}
}

static RESOURCES: Resources = Resources {
	client: OnceCell::const_new(),
};

// all tests must be multi_threaded, otherwise drop will deadlock for TestResources
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_login() {
	RESOURCES.client().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_dir_actions() {
	let resources = RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let dir = create_dir(client, test_dir, "test_dir".to_string())
		.await
		.unwrap();
	assert_eq!(dir.name(), "test_dir");

	let (dirs, _) = list_dir(client, test_dir).await.unwrap();

	trash_dir(client, dir.clone()).await.unwrap();

	if !dirs.contains(&dir) {
		panic!("Directory not found in root directory");
	}
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_find_path() {
	let resources = RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let dir_a = create_dir(client, test_dir, "a".to_string()).await.unwrap();
	let dir_b = create_dir(client, &dir_a, "b".to_string()).await.unwrap();
	let dir_c = create_dir(client, &dir_b, "c".to_string()).await.unwrap();

	assert_eq!(
		find_item_at_path(client, format!("{}/a/b/c", test_dir.name()))
			.await
			.unwrap(),
		Some(FSObjectType::Dir(std::borrow::Cow::Borrowed(&dir_c)))
	);

	assert_eq!(
		find_item_at_path(client, format!("{}/a/bc", test_dir.name()))
			.await
			.unwrap(),
		None
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_find_create_dir() {
	let resources = RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;
	let path = format!("{}/a/b/c", test_dir.name());
	let nested_dir = find_or_create_dir(client, &path).await.unwrap();

	assert_eq!(
		Some(Into::<FSObjectType<'_>>::into(nested_dir)),
		find_item_at_path(client, &path).await.unwrap()
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_list_dir_recursive() {
	let resources = RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let dir_a = create_dir(client, test_dir, "a".to_string()).await.unwrap();
	let dir_b = create_dir(client, &dir_a, "b".to_string()).await.unwrap();
	let dir_c = create_dir(client, &dir_b, "c".to_string()).await.unwrap();

	let (dirs, _) = list_dir_recursive(client, test_dir).await.unwrap();

	assert!(dirs.contains(&dir_a));
	assert!(dirs.contains(&dir_b));
	assert!(dirs.contains(&dir_c));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_dir_exists() {
	let resources = RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	assert!(dir_exists(client, test_dir, "a").await.unwrap().is_none());

	let dir_a = create_dir(client, test_dir, "a".to_string()).await.unwrap();

	assert_eq!(
		Some(dir_a.uuid()),
		dir_exists(client, test_dir, "a").await.unwrap()
	);

	trash_dir(client, dir_a).await.unwrap();
	assert!(dir_exists(client, test_dir, "a").await.unwrap().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_dir_move() {
	let resources = RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let mut dir_a = create_dir(client, test_dir, "a".to_string()).await.unwrap();
	let dir_b = create_dir(client, test_dir, "b".to_string()).await.unwrap();

	assert!(list_dir(client, &dir_b).await.unwrap().0.is_empty());

	move_dir(client, &mut dir_a, &dir_b).await.unwrap();
	assert!(list_dir(client, &dir_b).await.unwrap().0.contains(&dir_a));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_dir_size() {
	let resources = RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	assert_eq!(
		get_dir_size(client, test_dir, false).await.unwrap(),
		filen_types::api::v3::dir::size::Response {
			size: 0,
			files: 0,
			dirs: 0
		}
	);

	create_dir(client, test_dir, "a".to_string()).await.unwrap();
	assert_eq!(
		get_dir_size(client, test_dir, false).await.unwrap(),
		filen_types::api::v3::dir::size::Response {
			size: 0,
			files: 0,
			dirs: 1
		}
	);

	create_dir(client, test_dir, "b".to_string()).await.unwrap();
	assert_eq!(
		get_dir_size(client, test_dir, false).await.unwrap(),
		filen_types::api::v3::dir::size::Response {
			size: 0,
			files: 0,
			dirs: 2
		}
	);
}

async fn test_file_action(name: &str, contents_len: usize) {
	let mut contents = vec![0u8; contents_len];
	rand::rng().try_fill_bytes(&mut contents).unwrap();

	let contents = contents.as_ref();
	let resources = RESOURCES.get_resources().await;
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
async fn test_file_actions() {
	test_file_action("small.txt", 10).await;
	test_file_action("big_chunk_aligned_equal_to_threads.exe", 1024 * 1024 * 8).await;
	test_file_action("big_chunk_aligned_less_than_threads.exe", 1024 * 1024 * 7).await;
	test_file_action("big_chunk_aligned_more_than_threads.exe", 1024 * 1024 * 9).await;
	test_file_action("big_not_chunk_aligned_over.exe", 1024 * 1024 * 8 + 1).await;
	test_file_action("big_not_chunk_aligned_under.exe", 1024 * 1024 * 8 - 1).await;
	test_file_action("empty.json", 0).await;
	test_file_action("one_chunk", 1024 * 1024).await;
}
