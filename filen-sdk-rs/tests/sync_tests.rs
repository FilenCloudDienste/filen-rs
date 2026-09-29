use std::time::{Duration, Instant};

use base64::{Engine, prelude::BASE64_URL_SAFE_NO_PAD};

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
	tokio::time::sleep(time::Duration::from_secs(10)).await;
	assert_eq!(
		client
			.acquire_lock(&resource, time::Duration::from_secs(1), 1)
			.await
			.unwrap()
			.resource(),
		resource
	);
}

/// An acquisition dropped while its request is in flight may already have been granted by the
/// server, and nobody else knows its uuid: unless the acquisition releases what it may hold, every
/// client waits out the server lease on a resource nobody holds. The cuts are spread across one
/// round trip so at least one lands between the server granting the lock and the client reading
/// the answer.
#[filen_macros::shared_test_runtime]
async fn test_cancelled_acquire_leaves_the_resource_free() {
	const CUTS: u32 = 8;
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let fresh_resource = || {
		format!(
			"rs-{}",
			BASE64_URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>())
		)
	};

	let started = Instant::now();
	drop(
		client
			.acquire_lock(&fresh_resource(), Duration::from_secs(1), 1)
			.await
			.unwrap(),
	);
	let round_trip = started.elapsed();

	for cut in 1..=CUTS {
		let resource = fresh_resource();
		let cut_at = round_trip * cut / CUTS;
		// Granted in time or not, whatever the attempt held is dropped with it here.
		let _ = tokio::time::timeout(
			cut_at,
			client.acquire_lock(&resource, Duration::from_secs(1), 1),
		)
		.await;
		// Free again long before a lease left behind would expire (the server's is 30 s).
		let deadline = Instant::now() + Duration::from_secs(10);
		let freed = loop {
			match client
				.acquire_lock(&resource, Duration::from_secs(1), 1)
				.await
			{
				Ok(lock) => break Some(lock),
				Err(_) if Instant::now() < deadline => {
					tokio::time::sleep(Duration::from_millis(250)).await;
				}
				Err(_) => break None,
			}
		};
		assert!(
			freed.is_some(),
			"an acquisition cut {cut_at:?} into a {round_trip:?} round trip left {resource} held"
		);
	}
}

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
