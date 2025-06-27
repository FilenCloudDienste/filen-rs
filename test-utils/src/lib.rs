use std::{
	env,
	sync::{Arc, OnceLock},
};

use base64::{Engine, prelude::BASE64_URL_SAFE_NO_PAD};
use filen_sdk_rs::{auth::Client, fs::dir::RemoteDirectory, sync::lock::ResourceLock};

use tokio::sync::OnceCell;

pub struct Resources {
	client: OnceCell<Arc<Client>>,
	account_prefix: &'static str,
}

pub struct TestResources {
	pub client: Arc<Client>,
	pub dir: RemoteDirectory,
}

impl Default for TestResources {
	fn default() -> Self {
		Self {
			client: RESOURCES.client.get().unwrap().clone(),
			dir: RemoteDirectory::default(),
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
	pub async fn client(&self) -> Arc<Client> {
		self.client
			.get_or_init(|| async {
				dotenv::dotenv().ok();
				let client = Client::login(
					env::var(format!("{}_EMAIL", self.account_prefix)).unwrap(),
					&env::var(format!("{}_PASSWORD", self.account_prefix)).unwrap(),
					&env::var(format!("{}_2FA_CODE", self.account_prefix))
						.unwrap_or("XXXXXX".to_string()),
				)
				.await
				.inspect_err(|e| {
					println!("Failed to login: {}, error: {}", self.account_prefix, e);
				})
				.unwrap();
				Arc::new(client)
			})
			.await
			.clone()
	}

	pub async fn get_resources(&self) -> TestResources {
		let name = format!(
			"rs-{}",
			BASE64_URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>())
		);
		let client = self.client().await;
		let test_dir = client.create_dir(client.root(), name).await.unwrap();
		TestResources {
			client,
			dir: test_dir,
		}
	}

	pub async fn get_resources_with_lock(&self) -> (TestResources, Arc<ResourceLock>) {
		let name = format!(
			"rs-{}",
			BASE64_URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>())
		);
		let client = self.client().await;
		let lock = client.lock_drive().await.unwrap();
		let test_dir = client.create_dir(client.root(), name).await.unwrap();
		(
			TestResources {
				client,
				dir: test_dir,
			},
			lock,
		)
	}
}

static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();

pub fn rt() -> &'static tokio::runtime::Runtime {
	RUNTIME.get_or_init(|| {
		let _ = env_logger::try_init();
		tokio::runtime::Builder::new_multi_thread()
			.enable_all()
			.build()
			.expect("Failed to create Tokio runtime")
	})
}

pub static RESOURCES: Resources = Resources {
	client: OnceCell::const_new(),
	account_prefix: "TEST",
};

pub static SHARE_RESOURCES: Resources = Resources {
	client: OnceCell::const_new(),
	account_prefix: "TEST_SHARE",
};
