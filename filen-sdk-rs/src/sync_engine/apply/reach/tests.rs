use std::sync::{Arc, atomic::Ordering};

use tokio::{io::AsyncWriteExt, net::TcpListener};

use super::*;
use crate::{
	auth::{
		http::{
			ClientConfig,
			test_support::{client, read_head},
		},
		unauth,
	},
	sync_engine::apply::test_support::{
		refuse_every_request, refused_error, session_refused, status_error,
	},
};

/// The host of the local server [`status_error`] and [`refused_error`] answer from.
const LOCAL: &str = "127.0.0.1";

#[tokio::test]
async fn a_transient_failure_its_server_answered_around_is_its_paths_own() {
	let client = unauth::test_support::offline_client();
	let reach = Reach::new(&client);
	let since = reach.mark();
	client.unauthed().state().answers().note_answer_from(LOCAL);
	for error in [
		crate::Error::from(status_error(503).await),
		crate::Error::from(status_error(429).await),
		crate::Error::from(refused_error().await),
	] {
		assert_eq!(reach.halt(&error, since).await, None, "{error}");
	}
}

#[tokio::test]
async fn an_answer_from_another_server_or_from_before_the_action_shows_nothing() {
	let client = unauth::test_support::offline_client();
	let answers = client.unauthed().state().answers();
	answers.note_answer_from(LOCAL);
	let reach = Reach::new(&client);
	let since = reach.mark();
	answers.note_answer_from("egest.filen.io");
	let error = crate::Error::from(status_error(503).await);
	assert_eq!(
		reach.halt(&error, since).await,
		Some(HaltReason::RemoteUnavailable)
	);
}

/// A session the server refuses halts the pass however much the server is answering: no request
/// can get past it.
#[tokio::test]
async fn a_refused_session_halts_the_pass_whoever_answers() {
	let client = unauth::test_support::offline_client();
	let reach = Reach::new(&client);
	let since = reach.mark();
	client.unauthed().state().answers().note_answer_from(LOCAL);
	for error in [
		session_refused(),
		crate::Error::from(status_error(401).await),
	] {
		assert_eq!(
			reach.halt(&error, since).await,
			Some(HaltReason::Unauthenticated),
			"{error}"
		);
	}
}

/// What is no remote condition, and a full account, are left as they are.
#[tokio::test]
async fn a_failure_that_is_no_outage_is_left_as_it_is() {
	let client = unauth::test_support::offline_client();
	let reach = Reach::new(&client);
	let since = reach.mark();
	assert_eq!(
		reach
			.halt(&crate::Error::from(status_error(404).await), since)
			.await,
		None
	);
	let full = crate::Error::from(filen_types::error::ResponseError::ApiError {
		message: None,
		code: Some("max_storage_reached".to_string()),
	});
	assert_eq!(
		reach.halt(&full, since).await,
		Some(HaltReason::RemoteStorageFull)
	);
}

/// A client whose every request goes through a proxy that refuses it, noting an answer from its
/// server first when `answering` (see [`refuse_every_request`]), with the proxy's count of the
/// requests that reached it.
async fn refused_client(answering: bool) -> (Client, Arc<std::sync::atomic::AtomicUsize>) {
	let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
	let client = unauth::test_support::client_via_proxy(
		ClientConfig::default(),
		listener.local_addr().unwrap(),
	);
	let answers = Arc::clone(client.unauthed().state().answers());
	let (_, requests) = refuse_every_request(listener, answering.then_some(answers));
	(client, requests)
}

/// A request to the gateway that failed transiently.
async fn gateway_failure(client: &Client) -> crate::Error {
	health::post(client.client()).await.unwrap_err()
}

/// A gateway failure no other answer came in for is settled by asking the gateway for its health:
/// a gateway that answers that is up, and the failure is its path's own.
///
/// On the real clock: a paused one would run out the probe's time limit while the proxy, on the
/// same runtime, is still answering it.
#[tokio::test]
async fn a_gateway_failure_nothing_else_answered_is_settled_by_the_gateways_health() {
	let (client, requests) = refused_client(true).await;
	let error = gateway_failure(&client).await;
	let reach = Reach::new(&client);
	let since = reach.mark();
	let before = requests.load(Ordering::SeqCst);
	assert_eq!(reach.halt(&error, since).await, None, "{error}");
	assert!(
		requests.load(Ordering::SeqCst) > before,
		"the gateway was asked for its health"
	);
}

/// A gateway that leaves its health unanswered is down: the failure halts the pass, and the
/// failures that come in after it are settled by that probe without another, until the gateway
/// answers anything.
#[tokio::test]
async fn a_gateway_that_leaves_its_health_unanswered_is_down_and_asked_once() {
	let (client, requests) = refused_client(false).await;
	let error = gateway_failure(&client).await;
	let reach = Reach::new(&client);
	let since = reach.mark();
	assert_eq!(
		reach.halt(&error, since).await,
		Some(HaltReason::RemoteUnavailable)
	);
	let probed = requests.load(Ordering::SeqCst);
	assert_eq!(
		reach.halt(&error, since).await,
		Some(HaltReason::RemoteUnavailable)
	);
	assert_eq!(requests.load(Ordering::SeqCst), probed, "no second probe");

	let answers = client.unauthed().state().answers();
	answers.note_answer_from("ingest.filen.io");
	assert_eq!(
		reach.halt(&error, since).await,
		Some(HaltReason::RemoteUnavailable),
		"another server's answer settles nothing"
	);
	assert_eq!(requests.load(Ordering::SeqCst), probed);
}

/// The downloads and uploads servers are not probed: a failure on one that no other answer came
/// in for is taken for its outage.
#[tokio::test(start_paused = true)]
async fn a_transfer_server_no_answer_came_in_from_is_not_probed() {
	let (client, requests) = refused_client(true).await;
	let error = crate::Error::from(status_error(503).await);
	let reach = Reach::new(&client);
	let since = reach.mark();
	assert_eq!(
		reach.halt(&error, since).await,
		Some(HaltReason::RemoteUnavailable)
	);
	assert_eq!(requests.load(Ordering::SeqCst), 0);
}

/// The failure of a chunk download from a server that answers each attempt with a 200 head and
/// then closes the connection three bytes into the hundred it announced.
async fn body_cut_short() -> crate::Error {
	let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
	let addr = listener.local_addr().unwrap();
	let server = tokio::spawn(async move {
		while let Ok((mut socket, _)) = listener.accept().await {
			read_head(&mut socket).await;
			socket
				.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\nabc")
				.await
				.ok();
		}
	});
	let error = client(ClientConfig::default())
		.unauthed
		.get_raw_bytes_with_callback(
			&format!("http://{addr}/chunk"),
			"chunk".into(),
			None,
			None::<&fn(u64, Option<u64>)>,
		)
		.await
		.unwrap_err();
	server.abort();
	error
}

/// A body cut short after its head names the server it came from, as a failure before the head
/// does: without it, every such failure would be taken for an outage, whoever else answered.
#[tokio::test]
async fn a_body_cut_short_names_its_server() {
	let error = body_cut_short().await;
	assert_eq!(
		pass_halt(&error),
		Some(HaltReason::RemoteUnavailable),
		"{error}"
	);
	assert_eq!(transient_server(&error), Some(LOCAL), "{error}");
}

/// A download whose body is cut short while the same server answers a neighbour is the path's
/// own failure, and halts nothing.
#[tokio::test]
async fn a_body_cut_short_while_its_server_answers_a_neighbour_is_its_paths_own() {
	let error = body_cut_short().await;
	let client = unauth::test_support::offline_client();
	let reach = Reach::new(&client);
	let since = reach.mark();
	client.unauthed().state().answers().note_answer_from(LOCAL);
	assert_eq!(reach.halt(&error, since).await, None, "{error}");
}

/// A download whose body is cut short while its server answers nothing else is taken for that
/// server's outage, and halts the pass.
#[tokio::test]
async fn a_body_cut_short_with_no_answer_from_its_server_halts_the_pass() {
	let error = body_cut_short().await;
	let client = unauth::test_support::offline_client();
	let reach = Reach::new(&client);
	let since = reach.mark();
	assert_eq!(
		reach.halt(&error, since).await,
		Some(HaltReason::RemoteUnavailable),
		"{error}"
	);
}
