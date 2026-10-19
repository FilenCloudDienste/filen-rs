use super::*;

// Regression test for the "websocket thread has been closed" error.
//
// `WebSocketHandle` caches a `WeakSender` to the websocket task and decides whether
// the task is still alive via `WeakSender::upgrade`. But `upgrade` only checks the
// strong-sender count — not whether the receiver (the task) is still alive — so it
// succeeds as long as any strong `Sender` (a live `ListenerHandle`) remains. When
// the task exits for a non-channel reason (auth failure, critical receive error,
// panic) while a `ListenerHandle` is still held, the cached weak still upgrades and
// the next `add_event_listener` sends into a dropped receiver, failing with
// `InvalidState: websocket thread has been closed`.
//
// `live_sender` must therefore reject a channel whose receiver has been dropped, so
// `get_request_sender` re-spawns a fresh thread instead of handing out a dead one.
#[test]
fn live_sender_rejects_channel_whose_receiver_was_dropped() {
	let (sender, receiver) = tokio::sync::mpsc::channel::<SocketRequest>(16);
	// A still-held `ListenerHandle` keeps the strong-sender count above zero.
	let _zombie_handle = sender.clone();
	let handle = WebSocketHandle {
		request_sender: Some(sender.downgrade()),
		start: WorkerStart::default(),
	};

	// While the websocket task (the receiver) is alive, the cached sender is usable.
	assert!(handle.live_sender().is_some());
	assert!(handle.is_connected());

	// The websocket task dies for a non-channel reason: its receiver is dropped
	// while a strong sender (the `ListenerHandle`) is still alive.
	drop(receiver);

	assert!(
		handle.live_sender().is_none(),
		"a cached sender to a dropped-receiver channel must be treated as dead"
	);
	assert!(
		!handle.is_connected(),
		"a websocket thread whose receiver was dropped must not report connected"
	);
}

// A websocket worker that never starts is given up on: everything waiting on it fails with
// why, and the handle stops treating it as live so the next caller spawns a fresh one.
#[test]
fn requests_to_a_worker_given_up_on_fail_with_the_startup_error() {
	use futures::FutureExt;

	use crate::{error::WorkerStartupError, runtime::WorkerInbox};

	fn assert_startup_error(result: Option<Result<ListenerHandle, Error>>) {
		let Some(Err(error)) = result else {
			panic!("the request must fail at once");
		};
		assert_eq!(
			error.downcast_ref::<WorkerStartupError>(),
			Some(&WorkerStartupError::LoadUnreported)
		);
	}

	let (sender, receiver) = tokio::sync::mpsc::channel::<SocketRequest>(16);
	let inbox = WorkerInbox::new(receiver);
	let handle = WebSocketHandle {
		request_sender: Some(sender.downgrade()),
		start: inbox.start().clone(),
	};
	let request_sender = || RequestSender {
		sender: sender.clone(),
		start: inbox.start().clone(),
	};

	let mut queued = Box::pin(request_sender().add_event_listener(Box::new(|_| {}), None));
	assert!(queued.as_mut().now_or_never().is_none());

	assert!(inbox.abandon(WorkerStartupError::LoadUnreported));

	assert_startup_error(queued.now_or_never());
	assert_startup_error(
		request_sender()
			.add_event_listener_sync(Box::new(|_| {}), None)
			.now_or_never(),
	);
	assert!(!handle.is_connected());
}

#[test]
fn ping_interval_from_millis_floors_zero() {
	assert_eq!(ping_interval_from_millis(0), Duration::from_secs(1));
}

/// Hermetic tests of the full websocket pipeline (connect → handshake → event
/// loop → reconnect). Everything under [`run_async_websocket_task`] is generic
/// over the transport traits, so a scripted in-memory fake exercises the REAL
/// production control flow — reconnect classification, retry policy, listener
/// broadcasts — with no network and a paused clock.
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
mod pipeline {
	use std::{
		cell::RefCell,
		collections::VecDeque,
		sync::{Arc, Mutex, OnceLock, RwLock},
	};

	use filen_types::{auth::APIKey, crypto::EncryptedString};

	use super::*;
	use crate::{
		auth::{http::ClientConfig, unauth::UnauthClient},
		crypto::error::ConversionError,
	};

	// ----- scripted fake transport -----

	pub(super) enum ServerAction {
		/// deliver this frame to the client
		Msg(String),
		/// deliver this frame after the delay elapses (the paused clock
		/// auto-advances to it); NOTE: the item is consumed when `receive`
		/// is first polled, so a script must not rely on redelivery if
		/// another select arm wins during the delay
		Delayed(Duration, String),
		/// surface a transport-level receive error
		Err(&'static str),
		/// clean close (receive yields None)
		Close,
		/// never respond (black-holed connection)
		Hang,
	}

	pub(super) struct FakeConnection {
		script: VecDeque<ServerAction>,
		/// runs when the connection is established — e.g. rotate the api key
		/// or drop the request sender to end the task
		on_connect: Option<Box<dyn FnOnce()>>,
	}

	impl FakeConnection {
		fn new(script: Vec<ServerAction>) -> Self {
			Self {
				script: script.into(),
				on_connect: None,
			}
		}

		fn with_on_connect(mut self, f: impl FnOnce() + 'static) -> Self {
			self.on_connect = Some(Box::new(f));
			self
		}
	}

	#[derive(Default)]
	struct FakeState {
		connections: VecDeque<FakeConnection>,
		connect_count: usize,
		sent: Vec<String>,
	}

	thread_local! {
		static STATE: RefCell<FakeState> = RefCell::default();
	}

	fn prime(connections: Vec<FakeConnection>) {
		STATE.with(|s| {
			*s.borrow_mut() = FakeState {
				connections: connections.into(),
				..Default::default()
			}
		});
	}

	fn connect_count() -> usize {
		STATE.with(|s| s.borrow().connect_count)
	}

	fn sent_messages() -> Vec<String> {
		STATE.with(|s| s.borrow().sent.clone())
	}

	pub(super) struct FakeMsg(String);

	impl AsRef<str> for FakeMsg {
		fn as_ref(&self) -> &str {
			&self.0
		}
	}

	impl IntoStableDeref for FakeMsg {
		type Output = String;

		fn into_stable_deref(self) -> String {
			self.0
		}
	}

	pub(super) struct FakeSender;

	impl Sender for FakeSender {
		async fn send(&mut self, msg: Cow<'_, str>) -> Result<Option<()>, Error> {
			STATE.with(|s| s.borrow_mut().sent.push(msg.into_owned()));
			Ok(Some(()))
		}

		async fn send_multiple(
			&mut self,
			msgs: impl IntoIterator<Item = Cow<'_, str>>,
		) -> Result<Option<()>, Error> {
			for msg in msgs {
				self.send(msg).await?;
			}
			Ok(Some(()))
		}
	}

	impl UnauthedSender for FakeSender {
		type AuthedType = FakeSender;
	}

	pub(super) struct FakeReceiver {
		script: VecDeque<ServerAction>,
	}

	impl Receiver<FakeMsg> for FakeReceiver {
		async fn receive(&mut self) -> Option<Result<FakeMsg, Error>> {
			match self.script.pop_front() {
				Some(ServerAction::Msg(s)) => Some(Ok(FakeMsg(s))),
				Some(ServerAction::Delayed(delay, s)) => {
					tokio::time::sleep(delay).await;
					Some(Ok(FakeMsg(s)))
				}
				Some(ServerAction::Err(s)) => Some(Err(Error::custom(ErrorKind::Server, s))),
				Some(ServerAction::Close) => None,
				// an exhausted script behaves like a black-holed connection
				Some(ServerAction::Hang) | None => std::future::pending().await,
			}
		}
	}

	impl UnauthedReceiver<FakeMsg> for FakeReceiver {
		type AuthedType = FakeReceiver;
	}

	pub(super) struct FakeUnauthedSocket {
		script: VecDeque<ServerAction>,
	}

	impl UnauthedSocket<FakeSender, FakeReceiver, FakeMsg> for FakeUnauthedSocket {
		fn split(self) -> (FakeSender, FakeReceiver) {
			(
				FakeSender,
				FakeReceiver {
					script: self.script,
				},
			)
		}
	}

	pub(super) struct FakeNoopPingTask;

	impl PingTask<FakeSender> for FakeNoopPingTask {
		fn new(_: FakeSender, _: Duration) -> Self {
			Self
		}

		fn abort(self) {}
	}

	pub(super) struct FakeSocket {
		script: VecDeque<ServerAction>,
	}

	impl
		Socket<
			(),
			FakeUnauthedSocket,
			FakeSender,
			FakeReceiver,
			FakeMsg,
			FakeSender,
			FakeReceiver,
			FakeNoopPingTask,
		> for FakeSocket
	{
		async fn build_request() -> Result<(), Error> {
			Ok(())
		}

		async fn connect(_request: ()) -> Result<FakeUnauthedSocket, Error> {
			let conn = STATE.with(|s| {
				let mut s = s.borrow_mut();
				s.connect_count += 1;
				s.connections.pop_front()
			});
			match conn {
				Some(conn) => {
					if let Some(f) = conn.on_connect {
						f();
					}
					Ok(FakeUnauthedSocket {
						script: conn.script,
					})
				}
				None => Err(Error::custom(
					ErrorKind::Server,
					"no more scripted connections",
				)),
			}
		}

		fn from_unauthed_parts(_: FakeSender, receiver: FakeReceiver) -> Self {
			Self {
				script: receiver.script,
			}
		}

		fn split_into_receiver_and_ping_task(
			self,
			_ping_interval: Duration,
		) -> (FakeReceiver, FakeNoopPingTask) {
			(
				FakeReceiver {
					script: self.script,
				},
				FakeNoopPingTask,
			)
		}
	}

	// ----- helpers -----

	struct NoopCrypter;

	impl MetaCrypter for NoopCrypter {
		fn blocking_encrypt_meta_into(
			&self,
			_meta: &str,
			_out: String,
		) -> EncryptedString<'static> {
			unreachable!("pipeline tests never encrypt metadata")
		}

		fn blocking_decrypt_meta_into(
			&self,
			_meta: &EncryptedString,
			_out: Vec<u8>,
		) -> Result<String, (ConversionError, Vec<u8>)> {
			unreachable!("pipeline tests never decrypt metadata")
		}
	}

	fn test_private_key() -> Arc<RsaPrivateKey> {
		static KEY: OnceLock<Arc<RsaPrivateKey>> = OnceLock::new();
		Arc::clone(KEY.get_or_init(|| {
			// tiny key: never used for real crypto, only to satisfy the signature
			Arc::new(RsaPrivateKey::new(&mut old_rng::thread_rng(), 512).unwrap())
		}))
	}

	fn test_config() -> WebSocketConfig {
		let unauthed = UnauthClient::from_config(ClientConfig::default()).unwrap();
		WebSocketConfig {
			client: Arc::new(AuthClient::from_unauthed(
				unauthed,
				Arc::new(RwLock::new(APIKey(Cow::Borrowed("test-api-key")))),
			)),
			reconnect_delay: RECONNECT_DELAY,
			ping_interval: PING_INTERVAL,
			user_id: 0,
		}
	}

	/// The scripted handshake exchange, followed by `then` as the in-session script.
	fn handshake_then(then: Vec<ServerAction>) -> Vec<ServerAction> {
		let mut script = vec![
			ServerAction::Msg(format!(
				"0{}",
				r#"{"sid":"fake","upgrades":[],"pingInterval":25000,"pingTimeout":20000}"#
			)),
			ServerAction::Msg(MESSAGE_CONNECT_PAYLOAD.to_string()),
			ServerAction::Msg(r#"42["authed",false]"#.to_string()),
			ServerAction::Msg(r#"42["authSuccess"]"#.to_string()),
		];
		script.extend(then);
		script
	}

	fn recording_listener() -> (EventListenerCallback, Arc<Mutex<Vec<String>>>) {
		let events = Arc::new(Mutex::new(Vec::new()));
		let events_clone = Arc::clone(&events);
		let callback: EventListenerCallback = Box::new(move |event| {
			events_clone
				.lock()
				.unwrap()
				.push(event.event_type().to_string());
		});
		(callback, events)
	}

	/// Registers a global recording listener and returns the recorded event
	/// names. Keep the id receiver alive — dropping it removes the listener;
	/// the canceller removes it only via an explicit send(()).
	async fn add_recording_listener(
		request_sender: &tokio::sync::mpsc::Sender<SocketRequest>,
	) -> (
		Arc<Mutex<Vec<String>>>,
		tokio::sync::oneshot::Receiver<Result<u64, Error>>,
		tokio::sync::oneshot::Sender<()>,
	) {
		let (callback, events) = recording_listener();
		let (id_sender, id_receiver) = tokio::sync::oneshot::channel();
		let (canceller, cancel_receiver) = tokio::sync::oneshot::channel();
		request_sender
			.send(SocketRequest::AddListener {
				id_sender,
				cancel_receiver,
				callback,
				event_types: None,
			})
			.await
			.unwrap();
		(events, id_receiver, canceller)
	}

	// ----- tests -----

	#[tokio::test(start_paused = true)]
	async fn transport_error_triggers_reconnect_with_broadcasts() {
		let (request_sender, request_receiver) = tokio::sync::mpsc::channel(16);

		// conn1 dies with a transport error mid-session (WiFi drop), conn2
		// closes cleanly, conn3 stays healthy and shuts the task down by
		// dropping the final request sender
		let final_sender = request_sender.clone();
		prime(vec![
			FakeConnection::new(handshake_then(vec![ServerAction::Err(
				"connection reset without closing handshake",
			)])),
			FakeConnection::new(handshake_then(vec![ServerAction::Close])),
			FakeConnection::new(handshake_then(vec![ServerAction::Hang]))
				.with_on_connect(move || drop(final_sender)),
		]);

		let (events, _id_receiver, _canceller) = add_recording_listener(&request_sender).await;
		// the clones inside the script now hold the only remaining senders
		drop(request_sender);

		run_async_websocket_task::<FakeSocket, _, _, _, _, _, _, _, _>(
			test_config(),
			request_receiver,
			Arc::new(NoopCrypter),
			test_private_key(),
		)
		.await;

		// a transport error must demote (broadcasting `reconnecting`) and
		// reconnect — not kill the task
		assert_eq!(connect_count(), 3);
		assert_eq!(
			events.lock().unwrap().as_slice(),
			[
				"authSuccess",
				"reconnecting",
				"authSuccess",
				"reconnecting",
				"authSuccess"
			]
		);
	}

	#[tokio::test(start_paused = true)]
	async fn events_received_before_a_transport_error_are_not_lost() {
		let (request_sender, request_receiver) = tokio::sync::mpsc::channel(16);

		// a decrypt-free drive event arrives, then the transport dies: the
		// event may still be sitting in the decryption queue at break time
		// and must be broadcast before the reconnect demotes the listeners
		let final_sender = request_sender.clone();
		prime(vec![
			FakeConnection::new(handshake_then(vec![
				ServerAction::Msg(
					r#"42["file-trash",{"uuid":"00000000-0000-0000-0000-000000000000","stableUUID":"00000000-0000-0000-0000-000000000000","driveMessageId":1}]"#
						.to_string(),
				),
				ServerAction::Err("connection reset"),
			])),
			FakeConnection::new(handshake_then(vec![ServerAction::Hang]))
				.with_on_connect(move || drop(final_sender)),
		]);

		let (events, _id_receiver, _canceller) = add_recording_listener(&request_sender).await;
		drop(request_sender);

		run_async_websocket_task::<FakeSocket, _, _, _, _, _, _, _, _>(
			test_config(),
			request_receiver,
			Arc::new(NoopCrypter),
			test_private_key(),
		)
		.await;

		assert_eq!(
			events.lock().unwrap().as_slice(),
			["authSuccess", "fileTrash", "reconnecting", "authSuccess"]
		);
	}

	#[tokio::test(start_paused = true)]
	async fn api_key_is_reread_on_each_connect_attempt() {
		let mut config = test_config();
		let client = Arc::clone(&config.client);

		// conn1 dies during the handshake (retryable, before auth is sent);
		// the key rotates while the retry loop runs, so attempt 2 must
		// authenticate with the NEW key, not a pre-loop snapshot
		prime(vec![
			FakeConnection::new(vec![ServerAction::Close]).with_on_connect(move || {
				*client
					.api_key()
					.write()
					.unwrap_or_else(|poisoned| poisoned.into_inner()) = APIKey(Cow::Borrowed("rotated-key"));
			}),
			FakeConnection::new(handshake_then(vec![])),
		]);

		let (_request_sender, mut request_receiver) = tokio::sync::mpsc::channel(16);
		let mut listeners = DisconnectedListenerManager::new();
		let socket = initialize_websocket::<FakeSocket, _, _, _, _, _, _, _, _>(
			&mut config,
			&mut request_receiver,
			&mut listeners,
		)
		.await;
		assert!(socket.is_some(), "second attempt should connect");

		let auth_messages: Vec<String> = sent_messages()
			.into_iter()
			.filter(|m| m.contains("apiKey"))
			.collect();
		assert_eq!(auth_messages, [r#"42["auth",{"apiKey":"rotated-key"}]"#]);
	}

	#[tokio::test(start_paused = true)]
	async fn black_holed_connection_triggers_reconnect_via_liveness_deadline() {
		let begin = tokio::time::Instant::now();
		let config = test_config();

		// a NAT rebind / network black hole: the connection stops delivering
		// anything, without an error or a close frame
		let socket = FakeSocket::from_unauthed_parts(
			FakeSender,
			FakeReceiver {
				script: vec![ServerAction::Hang].into(),
			},
		);

		let (_request_sender, mut request_receiver) = tokio::sync::mpsc::channel(16);
		let mut listeners = DisconnectedListenerManager::new().into_connected();

		let should_retry = tokio::time::timeout(
			Duration::from_secs(3600),
			handle_initialized_websocket(
				&config,
				socket,
				&mut request_receiver,
				&mut listeners,
				&NoopCrypter,
				&test_private_key(),
			),
		)
		.await
		.expect("a silent connection must be torn down by the liveness deadline");

		assert!(
			should_retry,
			"a dead connection reconnects, it does not kill the task"
		);
		assert_eq!(begin.elapsed(), liveness_timeout_from(config.ping_interval));
	}

	#[tokio::test(start_paused = true)]
	async fn inbound_traffic_resets_the_liveness_deadline() {
		let begin = tokio::time::Instant::now();
		let config = test_config();
		let liveness = liveness_timeout_from(config.ping_interval);

		// one (ignored, non-event) frame arrives shortly before the deadline
		// would fire; the deadline must restart from that frame
		let socket = FakeSocket::from_unauthed_parts(
			FakeSender,
			FakeReceiver {
				script: vec![
					ServerAction::Delayed(liveness - Duration::from_secs(1), "3".to_string()),
					ServerAction::Hang,
				]
				.into(),
			},
		);

		let (_request_sender, mut request_receiver) = tokio::sync::mpsc::channel(16);
		let mut listeners = DisconnectedListenerManager::new().into_connected();

		let should_retry = tokio::time::timeout(
			Duration::from_secs(3600),
			handle_initialized_websocket(
				&config,
				socket,
				&mut request_receiver,
				&mut listeners,
				&NoopCrypter,
				&test_private_key(),
			),
		)
		.await
		.expect("a silent connection must be torn down by the liveness deadline");

		assert!(should_retry);
		assert_eq!(
			begin.elapsed(),
			(liveness - Duration::from_secs(1)) + liveness
		);
	}

	#[tokio::test(start_paused = true)]
	async fn hung_handshake_times_out_and_stays_retryable() {
		let begin = tokio::time::Instant::now();

		// server accepts the connection but never sends the handshake packet
		prime(vec![FakeConnection::new(vec![ServerAction::Hang])]);

		let mut listeners = DisconnectedListenerManager::new();
		let result = tokio::time::timeout(
			Duration::from_secs(3600),
			connect_and_build_socket::<
				FakeUnauthedSocket,
				FakeSender,
				FakeReceiver,
				FakeMsg,
				FakeSocket,
				FakeSender,
				FakeReceiver,
				(),
				FakeNoopPingTask,
			>("key", &mut listeners),
		)
		.await
		.expect("a hung handshake must time out on its own");

		let err = match result {
			Ok(_) => panic!("the hung handshake must fail"),
			Err(e) => e,
		};
		assert!(
			!matches!(err.kind(), ErrorKind::Unauthenticated),
			"a handshake timeout must stay retryable, got: {err}"
		);
		assert_eq!(begin.elapsed(), CONNECT_TIMEOUT);
	}
}

#[test]
fn ping_interval_from_millis_floors_tiny_values() {
	assert_eq!(ping_interval_from_millis(5), Duration::from_secs(1));
	assert_eq!(ping_interval_from_millis(999), Duration::from_secs(1));
}

#[test]
fn ping_interval_from_millis_passes_through_sane_values() {
	assert_eq!(ping_interval_from_millis(1000), Duration::from_secs(1));
	assert_eq!(
		ping_interval_from_millis(25_000),
		Duration::from_millis(25_000)
	);
}

// A huge server-sent pingInterval would stop our pings and inflate the
// liveness deadline toward infinity — the black-hole wedge by other means.
#[test]
fn ping_interval_from_millis_caps_huge_values() {
	assert_eq!(
		ping_interval_from_millis(25_000_000_000),
		Duration::from_secs(60)
	);
	assert_eq!(ping_interval_from_millis(u64::MAX), Duration::from_secs(60));
}
