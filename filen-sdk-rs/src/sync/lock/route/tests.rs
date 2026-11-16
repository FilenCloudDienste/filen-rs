use std::{borrow::Cow, cell::Cell, time::Duration};

use filen_types::{api::v3::user::lock::LockType, fs::Uuid};
use tokio::sync::{mpsc, oneshot};

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
use tokio::time::Instant;
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
use wasmtimer::std::Instant;

use super::*;
use crate::{
	ErrorKind,
	socket::{
		ack_args,
		lock_transport_test_support::{LockCommand, scripted_commands},
	},
	sync::lock::lock_payload,
};

/// The operation every test routes, and the JSON its `lock` event carries.
fn refresh() -> LockPayload {
	lock_payload(LockType::Refresh, Uuid::from_u128(1), "r").unwrap()
}
const REFRESH_JSON: &str =
	r#"{"uuid":"00000000-0000-0000-0000-000000000001","type":"refresh","resource":"r"}"#;

/// The answer the HTTP round trip of a test gets, told apart from the socket's by its resource.
fn answered_over_http() -> Response<'static> {
	Response {
		acquired: false,
		released: false,
		refreshed: true,
		resource: Cow::Borrowed("over-http"),
		status: None,
	}
}

/// A socket answer: the operation refreshed, told apart from HTTP's by its resource.
const REFRESHED: &str = r#"[{"status":true,"message":"Lock refreshed.","code":"lock_refreshed","data":{"acquired":false,"released":false,"refreshed":true,"resource":"over-socket","status":"locked"}}]"#;

/// A socket refusal with `code`, as the server words it.
fn refused(code: &str, message: &str) -> String {
	format!(r#"[{{"status":false,"code":"{code}","message":"{message}"}}]"#)
}

/// A connection that speaks socket locks, with the end its commands arrive at.
fn socket() -> (LockTransport, mpsc::Receiver<LockCommand>) {
	let (commands, receiver) = scripted_commands();
	let transport = LockTransport::Socket {
		generation: 1,
		version: 2,
		ttl: Duration::from_secs(60),
		commands,
	};
	(transport, receiver)
}

/// What a routed operation came to.
#[derive(Debug, PartialEq, Eq)]
struct Routed {
	/// The resource of the answer it returned (`over-socket` or `over-http`), or the kind and
	/// server code of its error.
	answer: Result<String, (ErrorKind, Option<String>)>,
	/// Whether the HTTP round trip ran.
	http: bool,
	/// Whether the socket left its outcome unknown.
	unknown: bool,
}

/// Routes [`refresh`] over `transport`, with an HTTP round trip answered by
/// [`answered_over_http`].
async fn route(transport: &LockTransport) -> Routed {
	route_within(transport, None).await
}

/// [`route`], waiting on the socket for at most `socket_wait`.
async fn route_within(transport: &LockTransport, socket_wait: Option<Duration>) -> Routed {
	let http = Cell::new(false);
	let unknown = Cell::new(false);
	let answer = send_lock_op(
		transport,
		&refresh(),
		socket_wait,
		|| {
			http.set(true);
			async { Ok(answered_over_http()) }
		},
		|| unknown.set(true),
	)
	.await;
	Routed {
		answer: answer
			.map(|response| response.resource.into_owned())
			.map_err(|error| (error.kind(), error.server_code())),
		http: http.get(),
		unknown: unknown.get(),
	}
}

/// Plays the connection for one operation: takes its command, checks its payload, and tells it
/// `reply`, or nothing, keeping its end open, when `reply` is `None`.
async fn connection(
	commands: &mut mpsc::Receiver<LockCommand>,
	reply: Option<LockReply>,
) -> Option<oneshot::Sender<LockReply>> {
	let Some(LockCommand::Send { payload, reply: to }) = commands.recv().await else {
		panic!("the operation is handed to the connection");
	};
	assert_eq!(payload.json(), REFRESH_JSON);
	match reply {
		Some(reply) => {
			to.send(reply).unwrap();
			None
		}
		None => Some(to),
	}
}

/// Routes [`refresh`] over a socket whose connection replies `reply`.
async fn route_answered(reply: LockReply) -> (Routed, mpsc::Receiver<LockCommand>) {
	let (transport, mut commands) = socket();
	let (routed, _) = tokio::join!(route(&transport), connection(&mut commands, Some(reply)));
	(routed, commands)
}

fn over_socket() -> Routed {
	Routed {
		answer: Ok("over-socket".to_owned()),
		http: false,
		unknown: false,
	}
}

fn over_http(unknown: bool) -> Routed {
	Routed {
		answer: Ok("over-http".to_owned()),
		http: true,
		unknown,
	}
}

#[tokio::test(start_paused = true)]
async fn a_socket_answer_is_returned_without_http() {
	let (routed, _) = route_answered(LockReply::Answered(ack_args(REFRESHED))).await;
	assert_eq!(routed, over_socket());
}

/// The same answer as HTTP's, `refreshed: false` included: an authoritative loss, not an error.
#[tokio::test(start_paused = true)]
async fn a_refused_refresh_over_the_socket_is_an_answer() {
	let not_refreshed = r#"[{"status":true,"message":"Lock not refreshed.","code":"lock_not_refreshed","data":{"acquired":false,"released":false,"refreshed":false,"resource":"over-socket","status":null}}]"#;
	let (routed, _) = route_answered(LockReply::Answered(ack_args(not_refreshed))).await;
	assert_eq!(routed, over_socket());
}

#[tokio::test(start_paused = true)]
async fn an_operation_the_socket_did_not_authenticate_goes_over_http() {
	let answer = refused("not_authed", "Not authed.");
	let (routed, _) = route_answered(LockReply::Answered(ack_args(&answer))).await;
	assert_eq!(routed, over_http(false));
}

#[tokio::test(start_paused = true)]
async fn an_operation_the_server_had_too_many_of_goes_over_http() {
	let answer = refused("too_many_requests", "Too many requests.");
	let (routed, _) = route_answered(LockReply::Answered(ack_args(&answer))).await;
	assert_eq!(routed, over_http(false));
}

#[tokio::test(start_paused = true)]
async fn an_operation_a_busy_connection_did_not_send_goes_over_http() {
	let (routed, _) = route_answered(LockReply::Busy).await;
	assert_eq!(routed, over_http(false));
}

#[tokio::test(start_paused = true)]
async fn an_operation_a_connection_did_not_send_goes_over_http() {
	let (routed, _) = route_answered(LockReply::NotSent).await;
	assert_eq!(routed, over_http(false));
}

/// HTTP sends the client's current key, and the connection is asked to authenticate with it.
#[tokio::test(start_paused = true)]
async fn an_operation_refused_for_the_sockets_key_goes_over_http_and_reauthenticates() {
	let answer = refused("api_key_not_found", "API key not found.");
	let (routed, mut commands) = route_answered(LockReply::Answered(ack_args(&answer))).await;
	assert_eq!(routed, over_http(false));
	assert!(matches!(
		commands.try_recv(),
		Ok(LockCommand::Reauthenticate)
	));
}

/// `internal_error` may have been applied: HTTP is asked with the same uuid, and the caller hears
/// the socket may still have run it.
#[tokio::test(start_paused = true)]
async fn an_internal_error_over_the_socket_is_an_unknown_outcome() {
	let answer = refused("internal_error", "Internal error");
	let (routed, _) = route_answered(LockReply::Answered(ack_args(&answer))).await;
	assert_eq!(routed, over_http(true));
}

#[tokio::test(start_paused = true)]
async fn a_refusal_this_client_does_not_know_is_an_unknown_outcome() {
	let answer = refused("some_new_code", "Something new.");
	let (routed, _) = route_answered(LockReply::Answered(ack_args(&answer))).await;
	assert_eq!(routed, over_http(true));
}

#[tokio::test(start_paused = true)]
async fn a_malformed_socket_answer_is_an_unknown_outcome() {
	for args in [
		"[]",
		r#"[{"status":true}]"#,
		"[1]",
		r#"[{"status":true},2]"#,
	] {
		let (routed, _) = route_answered(LockReply::Answered(ack_args(args))).await;
		assert_eq!(routed, over_http(true), "{args}");
	}
}

#[tokio::test(start_paused = true)]
async fn an_operation_whose_ack_timed_out_is_an_unknown_outcome() {
	let (routed, _) = route_answered(LockReply::Unknown).await;
	assert_eq!(routed, over_http(true));
}

/// A connection that never replies (its task stuck as its runtime shuts down) holds the
/// operation up for the reply timeout, not forever.
#[tokio::test(start_paused = true)]
async fn an_operation_its_connection_never_replies_to_is_an_unknown_outcome_after_eleven_seconds() {
	let begin = Instant::now();
	let (transport, mut commands) = socket();
	let (routed, _kept_open) = tokio::join!(route(&transport), connection(&mut commands, None));
	assert_eq!(routed, over_http(true));
	assert_eq!(begin.elapsed(), Duration::from_secs(11));
}

/// A caller that waits less than the connection does goes over HTTP once its wait is over, the
/// event left to the connection.
#[tokio::test(start_paused = true)]
async fn an_operation_the_socket_does_not_answer_within_its_wait_goes_over_http_then() {
	let begin = Instant::now();
	let (transport, mut commands) = socket();
	let (routed, kept_open) = tokio::join!(
		route_within(&transport, Some(Duration::from_secs(2))),
		connection(&mut commands, None)
	);
	assert_eq!(routed, over_http(true));
	assert_eq!(begin.elapsed(), Duration::from_secs(2));
	assert!(kept_open.is_some_and(|reply| reply.is_closed()));
}

/// The connection's task ended with the operation handed over: it may have been sent.
#[tokio::test(start_paused = true)]
async fn an_operation_its_connection_dropped_unanswered_is_an_unknown_outcome() {
	let (transport, mut commands) = socket();
	let (routed, ()) = tokio::join!(route(&transport), async {
		drop(connection(&mut commands, None).await);
	});
	assert_eq!(routed, over_http(true));
}

/// A client bug the server refuses over HTTP alike: returned as HTTP returns it, not retried.
#[tokio::test(start_paused = true)]
async fn an_invalid_operation_is_returned_as_the_http_error() {
	let answer = refused("invalid_params", "Invalid UUID.");
	let (routed, _) = route_answered(LockReply::Answered(ack_args(&answer))).await;
	assert_eq!(
		routed,
		Routed {
			answer: Err((ErrorKind::Server, Some("invalid_params".to_owned()))),
			http: false,
			unknown: false,
		}
	);
}

/// A connection whose websocket task ended, its channel closed: nothing was sent.
#[tokio::test(start_paused = true)]
async fn an_operation_for_a_connection_that_ended_goes_over_http() {
	let (transport, commands) = socket();
	drop(commands);
	assert_eq!(route(&transport).await, over_http(false));
}

/// Only a connection that answered its probe takes lock operations: nothing is handed to the
/// socket before, and nothing after it stopped carrying them.
#[tokio::test(start_paused = true)]
async fn an_operation_without_a_probed_socket_goes_over_http() {
	for transport in [
		LockTransport::None,
		LockTransport::Connecting,
		LockTransport::Probing { generation: 1 },
		LockTransport::Http { generation: 1 },
	] {
		assert_eq!(route(&transport).await, over_http(false), "{transport:?}");
	}
}
