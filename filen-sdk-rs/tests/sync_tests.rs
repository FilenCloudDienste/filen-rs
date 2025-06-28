#[cfg(not(feature = "tokio"))]
// in the tokio runtime, the lock might not be released immediately
// because we spawn a task to release it
#[filen_sdk_rs_macros::shared_test_runtime]
async fn test_acquire_lock() {
	use std::time;

	use base64::{Engine, prelude::BASE64_URL_SAFE_NO_PAD};

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
