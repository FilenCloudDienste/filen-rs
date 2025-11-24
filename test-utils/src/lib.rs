use std::{
	env,
	sync::{Arc, OnceLock},
};

use base64::{Engine, prelude::BASE64_URL_SAFE_NO_PAD};
use filen_sdk_rs::{
	auth::Client,
	fs::{HasName, HasUUID, dir::RemoteDirectory},
	sync::lock::ResourceLock,
};

use futures::{StreamExt, stream::FuturesUnordered};
use tokio::sync::OnceCell;

pub struct Resources {
	client: OnceCell<Arc<Client>>,
	account_prefix: &'static str,
}

pub struct TestResources {
	pub client: Arc<Client>,
	pub dir: RemoteDirectory,
}

impl Drop for TestResources {
	fn drop(&mut self) {
		match tokio::runtime::Handle::try_current() {
			Ok(handle) => {
				handle.spawn(Self::cleanup(self.client.clone(), self.dir.clone()));
			}
			Err(_) => {
				let rt = rt();
				rt.block_on(Self::cleanup(self.client.clone(), self.dir.clone()));
			}
		}
	}
}

impl TestResources {
	async fn cleanup(client: Arc<Client>, dir: RemoteDirectory) {
		match client.delete_dir_permanently(dir).await {
			Ok(_) => {}
			Err(e) => eprintln!("Failed to clean up test directory: {e}"),
		}
	}
}

impl Resources {
	pub async fn client(&self) -> Arc<Client> {
		self.client
			.get_or_init(|| async {
				dotenv::dotenv().ok();
				let client = Client::login(
					env::var(format!("{}_EMAIL", self.account_prefix)).unwrap_or_else(|_| {
						panic!(
							"Failed to get Filen testing account email from environment variable {}_EMAIL",
							self.account_prefix
						)
					}),
					&env::var(format!("{}_PASSWORD", self.account_prefix)).unwrap_or_else(|_| {
						panic!(
							"Failed to get Filen testing account password from environment variable {}_PASSWORD",
							self.account_prefix
						)
					}),
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

pub async fn set_up_contact_no_add<'a>(
	client: &'a Client,
	share_client: &'a Client,
) -> (Arc<ResourceLock>, Arc<ResourceLock>, usize, usize) {
	let lock1 = client
		.acquire_lock_with_default("test:contact")
		.await
		.unwrap();
	let lock2 = share_client
		.acquire_lock_with_default("test:contact")
		.await
		.unwrap();

	let _ = futures::join!(
		async {
			for contact in client.get_contacts().await.unwrap() {
				let _ = client.delete_contact(contact.uuid).await;
			}
		},
		async {
			for contact in share_client.get_contacts().await.unwrap() {
				let _ = share_client.delete_contact(contact.uuid).await;
			}
		},
		async {
			for contact in client.list_outgoing_contact_requests().await.unwrap() {
				let _ = client.cancel_contact_request(contact.uuid).await;
			}
		},
		async {
			for contact in share_client.list_incoming_contact_requests().await.unwrap() {
				let _ = share_client.deny_contact_request(contact.uuid).await;
			}
		},
		async {
			let (out_dirs, out_files) = client.list_out_shared(None).await.unwrap();
			let mut out_futures = out_dirs
				.into_iter()
				.filter_map(|d| {
					if d.get_dir().name().unwrap().starts_with("compat-") {
						None
					} else {
						Some((*d.get_dir().uuid(), d.get_source_id()))
					}
				})
				.chain(
					out_files
						.into_iter()
						.map(|f| (*f.get_file().uuid(), f.get_source_id())),
				)
				.map(|(uuid, source_id)| async move {
					let _ = client.remove_shared_link_out(uuid, source_id).await;
				})
				.collect::<FuturesUnordered<_>>();
			while (out_futures.next().await).is_some() {}
		},
		async {
			let (in_dirs, in_files) = share_client.list_in_shared().await.unwrap();

			let mut in_futures = in_dirs
				.into_iter()
				.filter_map(|d| {
					if d.get_dir().name().unwrap().starts_with("compat-") {
						None
					} else {
						Some(*d.get_dir().uuid())
					}
				})
				.chain(in_files.into_iter().map(|f| *f.get_file().uuid()))
				.map(|uuid| async move {
					let _ = share_client.remove_shared_link_in(uuid).await;
				})
				.collect::<FuturesUnordered<_>>();
			while (in_futures.next().await).is_some() {}
		},
		async {
			let blocked_contacts = client.get_blocked_contacts().await.unwrap();
			let mut futures = blocked_contacts
				.into_iter()
				.map(|c| async move {
					let _ = client.unblock_contact(c.uuid).await;
				})
				.collect::<FuturesUnordered<_>>();
			while (futures.next().await).is_some() {}
		},
		async {
			let blocked_contacts = share_client.get_blocked_contacts().await.unwrap();
			let mut futures = blocked_contacts
				.into_iter()
				.map(|c| async move {
					let _ = share_client.unblock_contact(c.uuid).await;
				})
				.collect::<FuturesUnordered<_>>();
			while (futures.next().await).is_some() {}
		}
	);
	// tokio::time::sleep(std::time::Duration::from_secs(300)).await;
	let (out_dirs, _) = client.list_out_shared(None).await.unwrap();
	let (in_dirs, _) = share_client.list_in_shared().await.unwrap();
	(lock1, lock2, out_dirs.len(), in_dirs.len())
}

pub async fn set_up_contact<'a>(
	client: &'a Client,
	share_client: &'a Client,
) -> (Arc<ResourceLock>, Arc<ResourceLock>, usize, usize) {
	let (lock1, lock2, num_shared_out, num_shared_in) =
		set_up_contact_no_add(client, share_client).await;

	let request_uuid = client
		.send_contact_request(share_client.email())
		.await
		.unwrap();

	share_client
		.accept_contact_request(request_uuid)
		.await
		.unwrap();

	(lock1, lock2, num_shared_out, num_shared_in)
}
