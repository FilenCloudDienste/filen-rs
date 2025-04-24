use std::time;

use base64::{Engine, prelude::BASE64_URL_SAFE_NO_PAD};

mod test_utils;

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_acquire_lock() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let resource = format!(
		"rs-{}",
		BASE64_URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>())
	);
	{
		let lock = client
			.acquire_lock(&resource, time::Duration::from_secs(1), 5)
			.await
			.unwrap();
		assert_eq!(lock.resource(), resource);

		assert!(
			client
				.acquire_lock(&resource, time::Duration::from_secs(1), 1)
				.await
				.is_err()
		);
	}
	assert_eq!(
		client
			.acquire_lock(&resource, time::Duration::from_secs(1), 1)
			.await
			.unwrap()
			.resource(),
		resource
	);
}
