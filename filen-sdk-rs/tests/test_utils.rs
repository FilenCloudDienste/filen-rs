use std::env;

use base64::{Engine, prelude::BASE64_URL_SAFE_NO_PAD};
use filen_sdk_rs::{auth::Client, fs::dir::Directory};

use tokio::sync::OnceCell;

pub struct Resources {
	client: OnceCell<Client>,
}

pub struct TestResources {
	pub client: Client,
	pub dir: Directory,
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
			match self.client.trash_dir(&self.dir).await {
				Ok(_) => {}
				Err(e) => eprintln!("Failed to clean up test directory: {}", e),
			}
		})
	}
}

impl Resources {
	pub async fn client(&self) -> &Client {
		self.client
			.get_or_init(|| async {
				dotenv::dotenv().ok();
				Client::login(
					env::var("TEST_EMAIL").unwrap(),
					&env::var("TEST_PASSWORD").unwrap(),
					&env::var("TEST_2FA_CODE").unwrap_or("XXXXXX".to_string()),
				)
				.await
				.unwrap()
			})
			.await
	}

	pub async fn get_resources(&self) -> TestResources {
		let name = format!(
			"rs-{}",
			BASE64_URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>())
		);
		let client = self.client().await.clone();
		let test_dir = client.create_dir(client.root(), name).await.unwrap();
		TestResources {
			client,
			dir: test_dir,
		}
	}
}

pub static RESOURCES: Resources = Resources {
	client: OnceCell::const_new(),
};
