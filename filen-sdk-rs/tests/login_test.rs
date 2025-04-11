use core::panic;
use std::{env, sync::Arc};

use base64::{Engine, prelude::BASE64_URL_SAFE_NO_PAD};
use filen_sdk_rs::{
	auth::Client,
	fs::{HasMeta, dir::Directory},
	prelude::*,
};

use tokio::sync::OnceCell;

struct Resources {
	client: OnceCell<Arc<Client>>,
}

struct TestResources {
	client: Arc<Client>,
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
	async fn client(&self) -> Arc<Client> {
		self.client
			.get_or_init(|| async {
				dotenv::dotenv().ok();
				let client = login(
					env::var("TEST_EMAIL").unwrap(),
					&env::var("TEST_PASSWORD").unwrap(),
					&env::var("TEST_2FA_CODE").unwrap_or("XXXXXX".to_string()),
				)
				.await
				.unwrap();
				Arc::new(client)
			})
			.await
			.clone()
	}

	async fn get_resources(&self) -> TestResources {
		let name = format!(
			"rs-{}",
			BASE64_URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>())
		);
		let client = self.client().await;
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
