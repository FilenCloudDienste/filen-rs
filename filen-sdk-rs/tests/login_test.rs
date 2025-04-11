use core::panic;
use std::env;

use base64::{Engine, prelude::BASE64_URL_SAFE_NO_PAD};
use filen_sdk_rs::{
	auth::Client,
	fs::{FSObjectType, HasMeta, dir::Directory},
	prelude::*,
};

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
		FSObjectType::Dir(std::borrow::Cow::Borrowed(&dir_c))
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
		Into::<FSObjectType<'_>>::into(nested_dir),
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
