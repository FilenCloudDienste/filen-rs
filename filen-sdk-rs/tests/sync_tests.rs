#[cfg(feature = "test-seams")]
use std::borrow::Cow;
#[cfg(feature = "test-seams")]
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::{Engine, prelude::BASE64_URL_SAFE_NO_PAD};
use filen_sdk_rs::auth::Client;
#[cfg(feature = "test-seams")]
use filen_sdk_rs::auth::{http::ClientConfig, unauth::UnauthClient};
#[cfg(feature = "test-seams")]
use filen_sdk_rs::socket::DecryptedSocketEvent;
#[cfg(feature = "test-seams")]
use filen_sdk_rs::sync::lock::{LockEvent, LockEventKind};

/// Waits for a dropped lock's release task to free `resource`: acquires it once it is free and
/// releases it again, and fails the test if it is still held after `within`.
async fn wait_until_free(client: &Client, resource: &str, within: Duration) {
	let deadline = Instant::now() + within;
	loop {
		match client
			.acquire_lock(resource, Duration::from_secs(1), 1)
			.await
		{
			Ok(lock) => {
				drop(lock);
				return;
			}
			Err(_) if Instant::now() < deadline => {
				tokio::time::sleep(Duration::from_millis(250)).await;
			}
			Err(e) => panic!("{resource} still held {within:?} after its lock was dropped: {e}"),
		}
	}
}

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
	wait_until_free(client, &resource, Duration::from_secs(30)).await;
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
		// Free again long before a lease left behind would expire (the server's is 60 s).
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

	// Longer than the server's 60 s lease, so only a refreshed lease still blocks the acquire.
	tokio::time::sleep(std::time::Duration::from_secs(75)).await;

	client
		.acquire_lock(&resource, std::time::Duration::from_secs(1), 1)
		.await
		.unwrap_err();
	std::mem::drop(lock);
	wait_until_free(client, &resource, Duration::from_secs(30)).await;
}

/// A lock wait on a connection that delivers the server's lock announcements polls as soon as the
/// holder's release is announced, and otherwise only once a lease (60 s) after its last scheduled
/// poll. The release comes 5 s into the wait, so only the announcement can get it the lock within
/// seconds of the release.
#[cfg(feature = "test-seams")]
#[filen_macros::shared_test_runtime]
async fn a_release_wakes_a_waiter() {
	let client = test_utils::RESOURCES.client().await;
	// The announcements arrive over the socket, which a listener keeps open.
	let _listener = client
		.add_event_listener(
			Box::new(|_: &DecryptedSocketEvent<'_>| {}),
			Some(vec![Cow::Borrowed("authSuccess")]),
		)
		.await
		.unwrap();
	let version = tokio::time::timeout(
		Duration::from_secs(20),
		client.lock_protocol_version_for_test(),
	)
	.await
	.expect("the lockHello probe settles");
	assert_eq!(version, Some(2), "the server does not announce locks");

	let resource = format!(
		"rs-{}",
		BASE64_URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>())
	);
	let holder = client
		.acquire_lock(&resource, Duration::from_secs(1), 1)
		.await
		.unwrap();
	let (waiter, released_at) = tokio::join!(
		client.acquire_lock(&resource, Duration::from_secs(30), 5),
		async {
			// The waiter's first poll is refused, and its next scheduled one is a lease away.
			tokio::time::sleep(Duration::from_secs(5)).await;
			drop(holder);
			Instant::now()
		}
	);
	let waited = released_at.elapsed();
	let waiter = waiter.expect("the waiter gets the lock");
	assert!(
		waited < Duration::from_secs(3),
		"the waiter got the lock {waited:?} after its release"
	);
	println!("the waiter got the lock {waited:?} after its release");
	drop(waiter);
	wait_until_free(&client, &resource, Duration::from_secs(30)).await;
}

/// A lock wait of a client whose socket no listener opened opens it once its lock is found
/// taken, so the announcement of the holder's release gets it the lock within seconds, where its
/// next scheduled poll is a lease (60 s) away; and lets it close once it has the lock.
#[cfg(feature = "test-seams")]
#[filen_macros::shared_test_runtime]
async fn a_contended_wait_opens_the_socket_for_the_release() {
	let holding = test_utils::RESOURCES.client().await;
	// A client of its own, signed in as the shared one is, whose socket nothing opened.
	let waiting = UnauthClient::from_config(ClientConfig::default())
		.expect("building an unauthenticated client")
		.from_stringified(holding.to_stringified())
		.expect("restoring the signed-in client");
	assert!(!waiting.is_socket_connected());

	let resource = format!(
		"rs-{}",
		BASE64_URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>())
	);
	let holder = holding
		.acquire_lock(&resource, Duration::from_secs(1), 1)
		.await
		.unwrap();
	let (waiter, released_at) = tokio::join!(
		waiting.acquire_lock(&resource, Duration::from_secs(30), 20),
		async {
			// The waiter's first poll is refused, which opens its socket.
			let opened = Instant::now();
			while !waiting.is_socket_connected() {
				assert!(
					opened.elapsed() < Duration::from_secs(10),
					"the contended wait opened no socket"
				);
				tokio::time::sleep(Duration::from_millis(50)).await;
			}
			let version = tokio::time::timeout(
				Duration::from_secs(20),
				waiting.lock_protocol_version_for_test(),
			)
			.await
			.expect("the lockHello probe settles");
			assert_eq!(version, Some(2), "the server does not announce locks");
			drop(holder);
			Instant::now()
		}
	);
	let waited = released_at.elapsed();
	let waiter = waiter.expect("the waiter gets the lock");
	assert!(
		waited < Duration::from_secs(10),
		"the waiter got the lock {waited:?} after its release"
	);
	println!("the waiter got the lock {waited:?} after its release");

	// The wait's listener was the socket's only one.
	let ended = Instant::now();
	while waiting.is_socket_connected() {
		assert!(
			ended.elapsed() < Duration::from_secs(5),
			"the socket outlived the wait"
		);
		tokio::time::sleep(Duration::from_millis(50)).await;
	}
	drop(waiter);
	wait_until_free(&holding, &resource, Duration::from_secs(30)).await;
}

/// A held lock whose lease another client releases (the server releases a lease for its uuid,
/// whoever asks) is lost as soon as the release is announced: the announcement makes the holder
/// refresh at once, and the server refuses that refresh. Its keep-alive alone would find out at
/// its next refresh, 15 s after the acquire.
#[cfg(feature = "test-seams")]
#[filen_macros::shared_test_runtime]
async fn a_lock_released_elsewhere_is_lost_at_once() {
	let releasing = test_utils::RESOURCES.client().await;
	// A client of its own, signed in as the shared one is, so its lock listener and its socket
	// are the test's.
	let holding = UnauthClient::from_config(ClientConfig::default())
		.expect("building an unauthenticated client")
		.from_stringified(releasing.to_stringified())
		.expect("restoring the signed-in client");
	let owed = Arc::new(Mutex::new(Vec::new()));
	holding.set_lock_listener({
		// a second handle, for the listener
		let owed = Arc::clone(&owed);
		move |event: LockEvent| {
			if matches!(event.kind, LockEventKind::Owed) {
				owed.lock().unwrap().push(event.lock);
			}
		}
	});
	// The announcements arrive over the socket, which a listener keeps open.
	let _listener = holding
		.add_event_listener(
			Box::new(|_: &DecryptedSocketEvent<'_>| {}),
			Some(vec![Cow::Borrowed("authSuccess")]),
		)
		.await
		.unwrap();
	let version = tokio::time::timeout(
		Duration::from_secs(20),
		holding.lock_protocol_version_for_test(),
	)
	.await
	.expect("the lockHello probe settles");
	assert_eq!(version, Some(2), "the server does not announce locks");

	let resource = format!(
		"rs-{}",
		BASE64_URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>())
	);
	let lock = holding
		.acquire_lock(&resource, Duration::from_secs(1), 1)
		.await
		.unwrap();
	assert!(lock.is_valid());
	let id = {
		let mut owed = owed.lock().unwrap();
		assert_eq!(owed.len(), 1, "one lock is owed");
		owed.pop().unwrap()
	};
	assert_eq!(id.resource, resource);

	let released_at = Instant::now();
	releasing.release_locks([id]).await;
	tokio::time::timeout(Duration::from_secs(10), lock.wait_for_loss())
		.await
		.expect("the lock is lost within 10 s of its release");
	let lost_after = released_at.elapsed();
	println!("the lock was lost {lost_after:?} after its release was sent");
	assert!(
		lost_after < Duration::from_secs(5),
		"the lock was lost {lost_after:?} after its release was sent"
	);
	assert!(!lock.is_valid());
	drop(lock);
	wait_until_free(&releasing, &resource, Duration::from_secs(30)).await;
}
