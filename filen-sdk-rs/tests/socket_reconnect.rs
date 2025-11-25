use std::{borrow::Cow, time::Duration};

use filen_macros::shared_test_runtime;
use filen_sdk_rs::socket::DecryptedSocketEvent;
use filen_types::traits::CowHelpersExt;
// separate file because it needs to avoid interference with other tests

async fn await_event<F, T>(
	receiver: &mut tokio::sync::mpsc::UnboundedReceiver<T>,
	mut filter: F,
	timeout: Duration,
) -> Result<T, Cow<'static, str>>
where
	F: FnMut(&T) -> bool,
{
	let sleep_until = tokio::time::Instant::now() + timeout;
	loop {
		tokio::select! {
			_ = tokio::time::sleep_until(sleep_until) => {
				return Err("Timed out waiting for event".into());
			}
			event = receiver.recv() => {
				let event = event.ok_or(Cow::Borrowed("Expected to receive event"))?;
				if filter(&event) {
					return Ok(event);
				}
			}
		}
	}
}

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
	)
	.await
	.unwrap();

	std::mem::drop(handle);
	assert!(!client.is_socket_connected());
	await_event(
		&mut events_receiver,
		|event| matches!(event, DecryptedSocketEvent::Unsubscribed),
		Duration::from_secs(20),
	)
	.await
	.unwrap();

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
	)
	.await
	.unwrap();

	std::mem::drop(handle);
	assert!(!client.is_socket_connected());
	await_event(
		&mut events_receiver,
		|event| matches!(event, DecryptedSocketEvent::Unsubscribed),
		Duration::from_secs(20),
	)
	.await
	.unwrap();
}
