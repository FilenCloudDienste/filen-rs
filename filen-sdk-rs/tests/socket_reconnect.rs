use std::time::Duration;

use filen_macros::shared_test_runtime;
use filen_sdk_rs::socket::DecryptedSocketEvent;
use filen_types::traits::CowHelpersExt;
use test_utils::await_event;
// separate file because it needs to avoid interference with other tests

#[shared_test_runtime]
async fn test_websocket_disconnect_reconnect() {
	let client = test_utils::RESOURCES.client().await;

	let (events_sender, mut events_receiver) = tokio::sync::mpsc::unbounded_channel();

	assert!(!client.is_socket_connected());

	let handle = client
		.add_event_listener(
			Box::new(move |event| {
				events_sender.send(event.to_owned_cow()).unwrap();
			}),
			None,
		)
		.await
		.unwrap();
	assert!(client.is_socket_connected());

	await_event(
		&mut events_receiver,
		|event| matches!(event, DecryptedSocketEvent::AuthSuccess),
		Duration::from_secs(20),
		"authSuccess 1",
	)
	.await;

	std::mem::drop(handle);
	assert!(!client.is_socket_connected());
	await_event(
		&mut events_receiver,
		|event| matches!(event, DecryptedSocketEvent::Unsubscribed),
		Duration::from_secs(20),
		"unsubscribed 1",
	)
	.await;

	// making sure it can reconnect

	let (events_sender, mut events_receiver) = tokio::sync::mpsc::unbounded_channel();

	assert!(!client.is_socket_connected());

	let handle = client
		.add_event_listener(
			Box::new(move |event| {
				events_sender.send(event.to_owned_cow()).unwrap();
			}),
			None,
		)
		.await
		.unwrap();
	assert!(client.is_socket_connected());

	await_event(
		&mut events_receiver,
		|event| matches!(event, DecryptedSocketEvent::AuthSuccess),
		Duration::from_secs(20),
		"authSuccess 2",
	)
	.await;

	std::mem::drop(handle);
	assert!(!client.is_socket_connected());
	await_event(
		&mut events_receiver,
		|event| matches!(event, DecryptedSocketEvent::Unsubscribed),
		Duration::from_secs(20),
		"unsubscribed 2",
	)
	.await;
}
