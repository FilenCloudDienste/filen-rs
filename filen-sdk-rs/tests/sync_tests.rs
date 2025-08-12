use base64::{Engine, prelude::BASE64_URL_SAFE_NO_PAD};

#[cfg(not(feature = "tokio"))]
// in the tokio runtime, the lock might not be released immediately
// because we spawn a task to release it
#[filen_macros::shared_test_runtime]
async fn test_acquire_lock() {
	use std::time;

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

#[cfg(feature = "tokio")]
#[filen_macros::shared_test_runtime]
async fn test_refresh_lock() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let resource = format!(
		"rs-{}",
		BASE64_URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>())
	);
	let lock = client
		.acquire_lock(&resource, std::time::Duration::from_secs(1), 1)
		.await
		.unwrap();

	client
		.acquire_lock(&resource, std::time::Duration::from_secs(1), 1)
		.await
		.unwrap_err();

	tokio::time::sleep(std::time::Duration::from_secs(30)).await;

	client
		.acquire_lock(&resource, std::time::Duration::from_secs(1), 1)
		.await
		.unwrap_err();
	std::mem::drop(lock);
	// wait for the tokio task to release
	tokio::time::sleep(std::time::Duration::from_secs(10)).await;
	client
		.acquire_lock(&resource, std::time::Duration::from_secs(1), 1)
		.await
		.unwrap();
}
