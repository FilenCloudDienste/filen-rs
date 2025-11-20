use std::{borrow::Cow, time::Duration};

use filen_macros::shared_test_runtime;
use filen_types::{api::v3::socket::SocketEvent, traits::CowHelpers};
// separate file because it needs to avoid interference with other tests

async fn await_event<F>(
	receiver: &mut tokio::sync::mpsc::UnboundedReceiver<SocketEvent<'static>>,
	mut filter: F,
	timeout: Duration,
) -> Result<SocketEvent<'static>, Cow<'static, str>>
where
	F: FnMut(&SocketEvent) -> bool,
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
				events_sender
					.send(event.as_borrowed_cow().into_owned_cow())
					.unwrap();
			}),
			None,
		)
		.await
		.unwrap();
	assert!(client.is_socket_connected());

	await_event(
		&mut events_receiver,
		|event| matches!(event, SocketEvent::AuthSuccess),
		Duration::from_secs(20),
	)
	.await
	.unwrap();

	std::mem::drop(handle);
	assert!(!client.is_socket_connected());
	await_event(
		&mut events_receiver,
		|event| matches!(event, SocketEvent::Unsubscribed),
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
				events_sender
					.send(event.as_borrowed_cow().into_owned_cow())
					.unwrap();
			}),
			None,
		)
		.await
		.unwrap();
	assert!(client.is_socket_connected());

	await_event(
		&mut events_receiver,
		|event| matches!(event, SocketEvent::AuthSuccess),
		Duration::from_secs(20),
	)
	.await
	.unwrap();

	std::mem::drop(handle);
	assert!(!client.is_socket_connected());
	await_event(
		&mut events_receiver,
		|event| matches!(event, SocketEvent::Unsubscribed),
		Duration::from_secs(20),
	)
	.await
	.unwrap();
}
