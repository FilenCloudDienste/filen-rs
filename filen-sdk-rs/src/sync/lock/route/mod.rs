//! Which transport carries a lock operation: the client's socket while its connection speaks
//! socket locks, HTTP otherwise, and HTTP again, with the same uuid, whenever the socket did not
//! do the operation or cannot say whether it did. The socket's own failures never reach the
//! caller, so a lock operation fails with the errors it fails with over HTTP.
//!
//! A release goes over the same connection as the operations of its lease before it whenever
//! that connection still carries lock operations: a client has one connection at a time, and a
//! connection that stops carrying them never carries them again. The server runs a socket's
//! operations on one resource in the order they arrive, so such a release cannot overtake an
//! acquire whose ack timed out on that connection. After a reconnect nothing orders them, and the
//! lock board's zombie check releases a lease such an acquire took late.
//!
//! A caller can stop waiting on the socket before the connection gives up on the ack (see
//! [`route_lock_op`]'s `socket_wait`), so that HTTP still answers in time: a release a shutdown
//! waits for, a refresh the lease's confirmation depends on. The event stays with the
//! connection, which sends it in its turn, so the order above holds, and the operation runs over
//! HTTP as well: a second Release frees only the lease of its uuid, and a second Refresh grants
//! none.
//!
//! A build without a socket (`service-worker`) sends every lock operation over HTTP.

use std::{future::Future, time::Duration};

use bytes::Bytes;
use filen_types::api::v3::user::lock::{Request, Response};
#[cfg(any(
	not(all(target_family = "wasm", target_os = "unknown")),
	feature = "wasm-full"
))]
use filen_types::{api::response::FilenResponse, error::ResponseError};

#[cfg(any(
	not(all(target_family = "wasm", target_os = "unknown")),
	feature = "wasm-full"
))]
use crate::socket::{LockCommands, LockReply, LockTransport};
use crate::{auth::http::AuthClient, error::Error};

/// A lock request serialized once, as the JSON object both transports carry: the body of
/// `POST /v3/user/lock`, and the argument of a `lock` event (the server reads them alike).
#[derive(Debug, Clone)]
pub(crate) struct LockPayload(Bytes);

impl LockPayload {
	pub(crate) fn new(request: &Request<'_>) -> Result<Self, serde_json::Error> {
		serde_json::to_vec(request).map(|json| Self(Bytes::from_owner(json)))
	}

	/// The request as the body of `POST /v3/user/lock`.
	pub(crate) fn body(&self) -> Bytes {
		// refcount bump
		self.0.clone()
	}

	/// The request as JSON text.
	#[cfg(any(
		not(all(target_family = "wasm", target_os = "unknown")),
		feature = "wasm-full"
	))]
	pub(crate) fn json(&self) -> &str {
		str::from_utf8(&self.0).expect("serde_json writes UTF-8 (should be impossible)")
	}
}

/// One round trip of the lock operation `payload` for `client`: over its socket while a
/// connection of it speaks socket locks, with `http` otherwise (see [`send_lock_op`]). The socket
/// is waited on for at most `socket_wait`, or as long as the connection waits for the ack with
/// `None`.
#[cfg(any(
	not(all(target_family = "wasm", target_os = "unknown")),
	feature = "wasm-full"
))]
pub(super) async fn route_lock_op<Fut>(
	client: &AuthClient,
	payload: &LockPayload,
	socket_wait: Option<Duration>,
	http: impl FnOnce() -> Fut,
	on_unknown: impl FnOnce(),
) -> Result<Response<'static>, Error>
where
	Fut: Future<Output = Result<Response<'static>, Error>>,
{
	send_lock_op(
		&client.lock_transport.current(),
		payload,
		socket_wait,
		http,
		on_unknown,
	)
	.await
}

/// One round trip of the lock operation `payload` for `client`, with `http`: this build has no
/// socket.
#[cfg(all(
	all(target_family = "wasm", target_os = "unknown"),
	not(feature = "wasm-full")
))]
pub(super) async fn route_lock_op<Fut>(
	_client: &AuthClient,
	_payload: &LockPayload,
	_socket_wait: Option<Duration>,
	http: impl FnOnce() -> Fut,
	_on_unknown: impl FnOnce(),
) -> Result<Response<'static>, Error>
where
	Fut: Future<Output = Result<Response<'static>, Error>>,
{
	http().await
}

/// Sends the lock operation `payload` over `transport` when it is a socket, and with `http`
/// (one round trip of the same operation over HTTP) when it is not, or when the socket did not
/// do it or cannot say whether it did. `on_unknown` is called before that HTTP round trip when
/// the socket may have done it after all: it may still run, later than the HTTP one. Its answer
/// is waited for at most `socket_wait` (see [`LockCommands::send`]); one that does not come by
/// then is unknown.
///
/// Over the socket, the server's answer is returned as HTTP would return it: a refusal of an
/// invalid request as the same error. Every other refusal falls back to HTTP:
///
/// | Socket outcome | Then |
/// |---|---|
/// | an answer (`status: true`) | returned |
/// | `invalid_params` | returned as the error HTTP returns for it |
/// | `not_authed`, `too_many_requests`, not sent (busy, the connection ended) | HTTP: nothing was done |
/// | `api_key_not_found` | HTTP, which sends the client's current key, and the connection reauthenticates |
/// | `internal_error`, another code, a malformed or missing ack | `on_unknown`, then HTTP |
#[cfg(any(
	not(all(target_family = "wasm", target_os = "unknown")),
	feature = "wasm-full"
))]
async fn send_lock_op<Fut>(
	transport: &LockTransport,
	payload: &LockPayload,
	socket_wait: Option<Duration>,
	http: impl FnOnce() -> Fut,
	on_unknown: impl FnOnce(),
) -> Result<Response<'static>, Error>
where
	Fut: Future<Output = Result<Response<'static>, Error>>,
{
	match transport {
		LockTransport::Socket { commands, .. } => {
			match over_socket(commands, payload, socket_wait).await {
				SocketOutcome::Answered(answer) => return answer,
				SocketOutcome::NotDone => {}
				SocketOutcome::Unknown => on_unknown(),
			}
		}
		LockTransport::None
		| LockTransport::Connecting
		| LockTransport::Probing { .. }
		| LockTransport::Http { .. } => {}
	}
	http().await
}

/// What a lock operation sent over the socket came to.
#[cfg(any(
	not(all(target_family = "wasm", target_os = "unknown")),
	feature = "wasm-full"
))]
enum SocketOutcome {
	/// The server's answer, as HTTP would have returned it.
	Answered(Result<Response<'static>, Error>),
	/// The server did not do it.
	NotDone,
	/// The server may have done it, or may still.
	Unknown,
}

#[cfg(any(
	not(all(target_family = "wasm", target_os = "unknown")),
	feature = "wasm-full"
))]
async fn over_socket(
	commands: &LockCommands,
	payload: &LockPayload,
	wait: Option<Duration>,
) -> SocketOutcome {
	// refcount bump
	let args = match commands.send(payload.clone(), wait).await {
		LockReply::Answered(args) => args,
		LockReply::Unknown => {
			tracing::debug!(
				"A lock operation over the socket went unanswered, sending it over HTTP"
			);
			return SocketOutcome::Unknown;
		}
		LockReply::Busy | LockReply::NotSent => return SocketOutcome::NotDone,
	};
	let envelope = match args.single_arg::<FilenResponse<'static, Response<'static>>>() {
		Ok(envelope) => envelope,
		Err(e) => {
			tracing::warn!(
				"Malformed answer to a lock operation over the socket ({e}), sending it over HTTP"
			);
			return SocketOutcome::Unknown;
		}
	};
	let error = match envelope.into_data() {
		Ok(response) => return SocketOutcome::Answered(Ok(response)),
		Err(error) => error,
	};
	let ResponseError::ApiError { code, .. } = &error;
	match code.as_deref() {
		Some("invalid_params") => SocketOutcome::Answered(Err(error.into())),
		Some("not_authed" | "too_many_requests") => {
			tracing::debug!("The socket refused a lock operation ({error}), sending it over HTTP");
			SocketOutcome::NotDone
		}
		Some("api_key_not_found") => {
			tracing::warn!(
				"The socket refused a lock operation for its API key, sending it over HTTP and reconnecting"
			);
			commands.reauthenticate();
			SocketOutcome::NotDone
		}
		// `internal_error` may have been applied, and so may a refusal this client does not know.
		_ => {
			tracing::warn!(
				"A lock operation over the socket failed ({error}), sending it over HTTP"
			);
			SocketOutcome::Unknown
		}
	}
}

#[cfg(all(
	test,
	any(
		not(all(target_family = "wasm", target_os = "unknown")),
		feature = "wasm-full"
	)
))]
mod tests;
