use std::{borrow::Cow, time::Duration};

use filen_macros::shared_test_runtime;
use filen_sdk_rs::{
	ErrorKind,
	auth::Client,
	fs::{HasParent, HasUUID},
};
use filen_types::{api::v3::socket::SocketEvent, fs::ParentUuid, traits::CowHelpers};

async fn await_event<F>(
	receiver: &mut tokio::sync::mpsc::UnboundedReceiver<SocketEvent<'static>>,
	mut filter: F,
	timeout: Duration,
) where
	F: FnMut(&SocketEvent) -> bool,
{
	let sleep_until = tokio::time::Instant::now() + timeout;
	loop {
		tokio::select! {
			_ = tokio::time::sleep_until(sleep_until) => {
				panic!("Timed out waiting for event");
			}
			event = receiver.recv() => {
				let event = event.expect("Expected to receive event");
				if filter(&event) {
					return;
				}
			}
		}
	}
}

async fn await_not_event<F>(
	receiver: &mut tokio::sync::mpsc::UnboundedReceiver<SocketEvent<'static>>,
	mut filter: F,
	timeout: Duration,
) where
	F: FnMut(&SocketEvent) -> bool,
{
	let sleep_until = tokio::time::Instant::now() + timeout;
	loop {
		tokio::select! {
			_ = tokio::time::sleep_until(sleep_until) => {
				return;
			}
			event = receiver.recv() => {
				let event = event.expect("Expected to receive event");
				if filter(&event) {
					panic!("Received unexpected event: {:?}", event);
				}
			}
		}
	}
}

#[shared_test_runtime]
async fn test_websocket_auth() {
	let client = test_utils::RESOURCES.client().await;

	let (events_sender, mut events_receiver) = tokio::sync::mpsc::unbounded_channel();

	let _handle = client
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
	await_event(
		&mut events_receiver,
		|event| *event == SocketEvent::AuthSuccess,
		Duration::from_secs(20),
	)
	.await;
}

#[shared_test_runtime]
async fn test_websocket_event_filtering() {
	let client = test_utils::RESOURCES.client().await;

	let (events_sender, mut events_receiver) = tokio::sync::mpsc::unbounded_channel();

	let handle1_fut = client.add_event_listener(
		Box::new(move |event| {
			events_sender
				.send(event.as_borrowed_cow().into_owned_cow())
				.unwrap();
		}),
		None,
	);

	let (filtered_events_sender, mut filtered_events_receiver) =
		tokio::sync::mpsc::unbounded_channel();

	let handle2_fut = client.add_event_listener(
		Box::new(move |event| {
			filtered_events_sender
				.send(event.as_borrowed_cow().into_owned_cow())
				.unwrap();
		}),
		Some(vec![Cow::Borrowed("authSuccess")]),
	);

	let (_handle1, _handle2) = tokio::try_join!(handle1_fut, handle2_fut).unwrap();

	await_event(
		&mut events_receiver,
		|event| *event == SocketEvent::AuthSuccess,
		Duration::from_secs(20),
	)
	.await;

	await_not_event(
		&mut filtered_events_receiver,
		|event| *event != SocketEvent::AuthSuccess,
		Duration::from_secs(1),
	)
	.await;
}

#[shared_test_runtime]
async fn test_websocket_file_folder_creation() {
	let resouces = test_utils::RESOURCES.get_resources().await;
	let client = &resouces.client;
	let dir = &resouces.dir;

	let (events_sender, mut events_receiver) = tokio::sync::mpsc::unbounded_channel();

	let _handle = client
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

	let dir_a = client.create_dir(dir, "a".to_string()).await.unwrap();

	let file_1 = client.make_file_builder("file1.txt", &dir_a).build();
	let file_1 = client
		.upload_file(file_1.into(), b"file 1 contents")
		.await
		.unwrap();

	await_event(
		&mut events_receiver,
		|event| match event {
			SocketEvent::FolderSubCreated(data) => {
				ParentUuid::Uuid(data.parent) == *dir_a.parent() && data.uuid == *dir_a.uuid()
			}
			_ => false,
		},
		Duration::from_secs(20),
	)
	.await;

	await_event(
		&mut events_receiver,
		|event| match event {
			SocketEvent::FileNew(data) => data.bucket == file_1.bucket && data.uuid == file_1.uuid,
			_ => false,
		},
		Duration::from_secs(20),
	)
	.await;
}

#[shared_test_runtime]
async fn test_websocket_bad_auth() {
	let client = test_utils::RESOURCES.client().await;

	let (events_sender, mut events_receiver) = tokio::sync::mpsc::unbounded_channel();

	let mut stringified = client.to_stringified();
	stringified.api_key = "invalid_api_key".to_string();
	let client = Client::from_stringified(stringified).unwrap();
	let result = client
		.add_event_listener(
			Box::new(move |event| {
				events_sender
					.send(event.as_borrowed_cow().into_owned_cow())
					.unwrap();
			}),
			None,
		)
		.await;

	match result {
		Ok(_) => panic!("Expected error when adding listener with invalid API key"),
		Err(e) if e.kind() == ErrorKind::Unauthenticated => (),
		Err(e) => panic!("Unexpected error kind: {:?}", e),
	}

	await_event(
		&mut events_receiver,
		|event| *event == SocketEvent::AuthFailed,
		Duration::from_secs(5),
	)
	.await;
}
