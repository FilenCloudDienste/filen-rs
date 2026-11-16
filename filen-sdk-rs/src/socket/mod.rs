use crate::{Error, auth::Client};

use std::borrow::Cow;

mod acks;
mod consts;
mod events;
#[cfg(any(feature = "wasm-full", feature = "uniffi"))]
mod js;
mod listener_manager;
mod lock_transport;
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
pub(crate) mod native;
mod thread_handling;
mod traits;
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
mod wasm;
mod writer;

use traits::EventListenerCallback;

#[cfg(feature = "test-seams")]
use lock_transport::LockTransport;

pub(crate) use {lock_transport::LockTransportSlot, thread_handling::WebSocketHandle};

pub use {events::*, thread_handling::ListenerHandle};

/// Whether a lock wait of a client that finds its lock taken opens the socket, for the server's
/// lock announcements to wake it (see [`Client::listen_for_lock_announcements`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum LockWaitSocket {
	/// It does: a client with a server behind it.
	#[default]
	Opens,
	/// It never does: a client with no server behind it, as the unit tests and the sync engine's
	/// benchmark harness build, which must not connect to the real socket.
	// Only the offline clients of `auth::test_support` are built with it, and a build without
	// that module has none: the tradeoff is a variant a shipping build never constructs, for a
	// seam that keeps those clients off the network without a test-only branch.
	#[cfg_attr(
		not(any(test, all(feature = "sync-engine", feature = "bench-internals"))),
		expect(
			dead_code,
			reason = "only the offline clients of auth::test_support never open the socket"
		)
	)]
	Never,
}

/// The announcements that wake a lock wait: a lease ended.
const LOCK_WAKE_EVENT_TYPES: [&str; 2] = ["lockReleased", "lockTimeout"];

impl Client {
	/// Opens the socket for a lock wait that found its lock taken, when no listener has, so the
	/// server's lock announcements reach the client's lock board and can wake the wait: a
	/// listener that does nothing with what it gets, which lets the socket close once dropped
	/// with the last one. Resolves once it is registered, without waiting for the socket to
	/// connect.
	///
	/// `None` when another listener keeps the socket open already, or for a client that opens no
	/// socket for its lock waits ([`LockWaitSocket::Never`]). A live socket is left alone because
	/// it may be partway through a connection attempt, which any listener request restarts: a
	/// listener added and removed for every contended wait could keep a slow handshake from ever
	/// finishing. Should that listener be dropped before the wait ends, the socket closes and the
	/// wait polls on its schedule.
	pub(crate) async fn listen_for_lock_announcements(
		&self,
	) -> Result<Option<ListenerHandle>, Error> {
		match self.lock_wait_socket {
			LockWaitSocket::Never => Ok(None),
			LockWaitSocket::Opens => {
				let request_sender = self
					.socket_handle
					.lock()
					.unwrap_or_else(|e| e.into_inner())
					.request_sender_unless_live(self.arc_client_ref(), self);
				match request_sender {
					None => Ok(None),
					Some(request_sender) => request_sender
						.add_event_listener_sync(
							// The board reads every announcement ahead of the listeners.
							Box::new(|_: &DecryptedSocketEvent<'_>| {}),
							Some(LOCK_WAKE_EVENT_TYPES.map(Cow::Borrowed).to_vec()),
						)
						.await
						.map(Some),
				}
			}
		}
	}

	pub async fn add_event_listener(
		&self,
		callback: EventListenerCallback,
		event_types: Option<Vec<Cow<'static, str>>>,
	) -> Result<ListenerHandle, Error> {
		let request_sender = {
			let mut socket_handle = self.socket_handle.lock().unwrap_or_else(|e| e.into_inner());
			socket_handle.get_request_sender(self.arc_client_ref(), self)
		};
		request_sender
			.add_event_listener(callback, event_types)
			.await
	}

	/// Register `callback` and return its handle WITHOUT waiting for the socket to connect.
	/// Unlike [`add_event_listener`](Self::add_event_listener) — which resolves only once the
	/// socket is connected (or auth fails) — this inserts the callback into the routing table
	/// synchronously, even while disconnected, so the caller never wedges on a connect ack.
	/// Registration failure still surfaces synchronously, and once the socket connects every event
	/// is delivered to the callback like any other listener.
	pub async fn add_event_listener_sync(
		&self,
		callback: EventListenerCallback,
		event_types: Option<Vec<Cow<'static, str>>>,
	) -> Result<ListenerHandle, Error> {
		let request_sender = {
			let mut socket_handle = self.socket_handle.lock().unwrap_or_else(|e| e.into_inner());
			socket_handle.get_request_sender(self.arc_client_ref(), self)
		};
		request_sender
			.add_event_listener_sync(callback, event_types)
			.await
	}

	/// Asks the socket to reconnect now if it is waiting out a backoff pause after failed
	/// attempts, with the backoff reset: for an app to call when the network changes or it comes
	/// to the foreground. Does nothing while the socket is connected or connecting, or when no
	/// listener ever opened it. Cheap and idempotent: a lock and at most one non-blocking send,
	/// and a burst of calls costs at most one connection attempt.
	pub fn reconnect_socket(&self) {
		self.socket_handle
			.lock()
			.unwrap_or_else(|e| e.into_inner())
			.reconnect();
	}

	pub fn is_socket_connected(&self) -> bool {
		self.socket_handle
			.lock()
			.unwrap_or_else(|e| e.into_inner())
			.is_connected()
	}

	// Test seam: the live socket tests check what the `lockHello` probe found. Gated behind the
	// `test-seams` feature, which production builds never enable.
	/// Waits until the current connection's `lockHello` probe is settled and returns the socket
	/// lock protocol version its server speaks, or `None` when the connection uses HTTP for locks
	/// (or the websocket task ended). Test code only.
	#[cfg(feature = "test-seams")]
	pub async fn lock_protocol_version_for_test(&self) -> Option<u64> {
		let mut transport = self.client().lock_transport.subscribe();
		let settled = transport
			.wait_for(|transport| {
				matches!(
					transport,
					LockTransport::None | LockTransport::Socket { .. } | LockTransport::Http { .. }
				)
			})
			.await
			.ok()?;
		match *settled {
			LockTransport::Socket { version, .. } => Some(version),
			LockTransport::None
			| LockTransport::Connecting
			| LockTransport::Probing { .. }
			| LockTransport::Http { .. } => None,
		}
	}

	pub async fn get_last_event_ids(
		&self,
	) -> Result<filen_types::api::v3::message_ids::Response, Error> {
		crate::api::v3::message_ids::get(self.client()).await
	}
}
