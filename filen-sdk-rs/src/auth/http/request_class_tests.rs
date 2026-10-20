//! Control requests sent through the client's real entry points while chunk transfers hold every
//! transfer place: each must reach the network without waiting for a chunk to finish.

use std::{sync::Arc, time::Duration};

use tokio::net::{TcpListener, TcpStream};

use super::{
	AuthClient, ClientConfig,
	class_limit::CONTROL_REQUEST_HEADROOM,
	test_support::{GATEWAY_CONNECT, Stall, client_via_proxy, read_head},
};

/// How long a request that must reach the network gets to do so. It only bounds the failure
/// path: a request that passes reaches the proxy in milliseconds.
const REACH_BOUND: Duration = Duration::from_secs(10);

/// Accepts the next request at the scripted proxy and returns its connection and head, failing
/// with `why` if none arrives within [`REACH_BOUND`].
async fn next_request(listener: &TcpListener, why: &str) -> (TcpStream, String) {
	let (mut socket, _) = tokio::time::timeout(REACH_BOUND, listener.accept())
		.await
		.expect(why)
		.expect("the scripted proxy accepts");
	let head = read_head(&mut socket).await;
	(socket, head)
}

/// A `with_concurrency(1)` client sending through the scripted proxy at `listener`, so a single
/// chunk transfer takes every transfer place.
fn one_transfer_client(listener: &TcpListener) -> Arc<AuthClient> {
	let proxy = listener
		.local_addr()
		.expect("a bound listener has an address");
	Arc::new(client_via_proxy(
		ClientConfig::default().with_concurrency(1),
		proxy,
	))
}

/// Sends `send` while a chunk transfer of kind `stall` holds the only transfer place of a
/// `with_concurrency(1)` client, and asserts it reaches the network: the proxy reads its
/// `CONNECT gateway.filen.io:443` within the bound.
async fn control_request_beside_a_stalled_transfer<F, Fut>(stall: Stall, send: F)
where
	F: FnOnce(Arc<AuthClient>) -> Fut,
	Fut: Future<Output = ()> + Send + 'static,
{
	let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
	let client = one_transfer_client(&listener);
	let transfer = tokio::spawn({
		let client = Arc::clone(&client);
		async move { stall.send(&client).await }
	});
	let (stalled, head) =
		next_request(&listener, "the chunk transfer must reach the network").await;
	assert!(head.starts_with(stall.connect_line()), "{head}");

	let control = tokio::spawn(send(client));
	let (reached, head) = next_request(
		&listener,
		"the control request must not wait for a chunk transfer",
	)
	.await;
	assert!(head.starts_with(GATEWAY_CONNECT), "{head}");

	transfer.abort();
	control.abort();
	// held until both requests are gone: a connection closed under a live request fails it, and
	// a retry would reach the proxy again
	drop((stalled, reached));
}

#[tokio::test]
async fn a_listing_reaches_the_network_beside_a_stalled_chunk_upload() {
	control_request_beside_a_stalled_transfer(Stall::Upload, |client| async move {
		let _ = client
			.post_large_response_auth::<(), (), fn(u64, Option<u64>)>(
				"v3/dir/content".into(),
				&(),
				None,
			)
			.await;
	})
	.await;
}

#[tokio::test]
async fn an_authenticated_post_reaches_the_network_beside_a_stalled_chunk_download() {
	control_request_beside_a_stalled_transfer(Stall::Download, |client| async move {
		let _ = client
			.post_auth::<(), ()>("v3/dir/rename".into(), &())
			.await;
	})
	.await;
}

#[tokio::test]
async fn an_authenticated_get_reaches_the_network_beside_a_stalled_chunk_upload() {
	control_request_beside_a_stalled_transfer(Stall::Upload, |client| async move {
		let _ = client.get_auth::<()>("v3/user/info".into()).await;
	})
	.await;
}

#[tokio::test]
async fn an_unauthenticated_post_reaches_the_network_beside_a_stalled_chunk_download() {
	control_request_beside_a_stalled_transfer(Stall::Download, |client| async move {
		let _ = client
			.unauthed
			.post::<(), ()>("v3/auth/info".into(), &())
			.await;
	})
	.await;
}

#[tokio::test]
async fn a_public_link_listing_reaches_the_network_beside_a_stalled_chunk_upload() {
	control_request_beside_a_stalled_transfer(Stall::Upload, |client| async move {
		let _ = client
			.unauthed
			.post_large_response::<(), (), fn(u64, Option<u64>)>(
				"v3/dir/link/content".into(),
				&(),
				None,
			)
			.await;
	})
	.await;
}

#[tokio::test]
async fn queued_chunk_transfers_do_not_take_the_control_headroom() {
	let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
	let client = one_transfer_client(&listener);
	let transfer = tokio::spawn({
		let client = Arc::clone(&client);
		async move { Stall::Upload.send(&client).await }
	});
	let (stalled, head) = next_request(&listener, "the chunk upload must reach the network").await;
	assert!(head.starts_with(Stall::Upload.connect_line()), "{head}");

	// polled once each, so every one is queued for a transfer place before the renames are sent
	let mut queued = (0..CONTROL_REQUEST_HEADROOM)
		.map(|_| {
			let client = Arc::clone(&client);
			Box::pin(async move { Stall::Upload.send(&client).await })
		})
		.collect::<Vec<_>>();
	for upload in &mut queued {
		assert!(futures::poll!(upload.as_mut()).is_pending());
	}

	let renames = (0..CONTROL_REQUEST_HEADROOM)
		.map(|_| {
			let client = Arc::clone(&client);
			tokio::spawn(async move {
				let _ = client
					.post_auth::<(), ()>("v3/dir/rename".into(), &())
					.await;
			})
		})
		.collect::<Vec<_>>();
	let mut reached = Vec::new();
	for _ in 0..CONTROL_REQUEST_HEADROOM {
		let (socket, head) = next_request(
			&listener,
			"queued chunk transfers must not take the places left to other requests",
		)
		.await;
		assert!(head.starts_with(GATEWAY_CONNECT), "{head}");
		reached.push(socket);
	}

	transfer.abort();
	for rename in renames {
		rename.abort();
	}
	drop(queued);
	drop((stalled, reached));
}
