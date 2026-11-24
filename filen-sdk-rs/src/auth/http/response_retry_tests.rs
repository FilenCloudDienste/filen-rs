//! Which response bodies the stack sends a request again for: a body cut short and the server's
//! `internal_error` answer are retried, a complete body of the wrong shape and any other error
//! answer are not. Driven through `post_raw_bytes_auth` against a scripted local server for the
//! JSON stacks, and through the client's retry layer for both formats.

use std::sync::{
	Arc,
	atomic::{AtomicUsize, Ordering},
};

use bytes::Bytes;
use serde::{Deserialize, de::DeserializeOwned};
use tokio::{
	io::{AsyncReadExt, AsyncWriteExt},
	net::{TcpListener, TcpStream},
	task::JoinHandle,
};
use tower::{ServiceBuilder, ServiceExt};

use super::{
	ClientConfig, Request, RequestMethod, ResponseType,
	deserialize::{DeserializeLayer, Json, Msgpack, ResponseSlot, WireFormat},
	retry::RetryError,
	test_support::client,
};
use crate::{Error, ErrorKind};

/// The `data` every answer here carries.
#[derive(Deserialize, Debug, PartialEq)]
struct Answer {
	n: u32,
}

const GOOD: &str = r#"{"status":true,"message":"ok","code":"ok","data":{"n":7}}"#;
const TRUNCATED: &str = r#"{"status":true,"message":"ok","code":"ok","data":{"n":"#;
const INTERNAL_ERROR: &str =
	r#"{"status":false,"message":"Internal server error.","code":"internal_error"}"#;
const FOLDER_NOT_FOUND: &str =
	r#"{"status":false,"message":"Folder not found.","code":"folder_not_found"}"#;
const WRONG_TYPE: &str = r#"{"status":true,"message":"ok","code":"ok","data":{"n":"seven"}}"#;

/// Reads one request head, or `None` once the client closed the connection.
async fn next_head(socket: &mut TcpStream) -> Option<()> {
	let mut head = Vec::new();
	let mut byte = [0u8; 1];
	while !head.ends_with(b"\r\n\r\n") {
		match socket.read(&mut byte).await {
			Ok(1) => head.push(byte[0]),
			_ => return None,
		}
	}
	Some(())
}

/// Answers each request, on whichever connection it arrives, with the next of `answers`, and
/// counts them. Every request here has an empty body, so a head is a whole request.
async fn serve(answers: &'static [&'static str]) -> (String, Arc<AtomicUsize>, JoinHandle<()>) {
	let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
	let addr = listener.local_addr().unwrap();
	let served = Arc::new(AtomicUsize::new(0));
	let server = tokio::spawn({
		let served = Arc::clone(&served);
		async move {
			loop {
				let (mut socket, _) = listener.accept().await.unwrap();
				while next_head(&mut socket).await.is_some() {
					let answer = answers[served.fetch_add(1, Ordering::SeqCst)];
					let response = format!(
						"HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{answer}",
						answer.len()
					);
					socket.write_all(response.as_bytes()).await.unwrap();
				}
			}
		}
	});
	(format!("http://{addr}/v3/test"), served, server)
}

/// Sends one `post_raw_bytes_auth` to a server answering with `answers` in turn: its result and
/// how many attempts reached the server.
async fn post_answered(answers: &'static [&'static str]) -> (Result<Answer, Error>, usize) {
	let (url, served, server) = serve(answers).await;
	let client = client(ClientConfig::default());
	let result = client
		.post_raw_bytes_auth::<Answer>(Bytes::new(), &url, "v3/test".into())
		.await;
	server.abort();
	(result, served.load(Ordering::SeqCst))
}

#[tokio::test]
async fn a_truncated_json_body_is_sent_again() {
	let (result, attempts) = post_answered(&[TRUNCATED, GOOD]).await;
	assert_eq!(result.unwrap(), Answer { n: 7 });
	assert_eq!(attempts, 2);
}

#[tokio::test]
async fn an_internal_error_answer_is_sent_again() {
	let (result, attempts) = post_answered(&[INTERNAL_ERROR, GOOD]).await;
	assert_eq!(result.unwrap(), Answer { n: 7 });
	assert_eq!(attempts, 2);
}

#[tokio::test]
async fn a_json_body_of_the_wrong_shape_is_not_sent_again() {
	let (result, attempts) = post_answered(&[WRONG_TYPE, GOOD]).await;
	let e = result.unwrap_err();
	assert_eq!(attempts, 1);
	assert_eq!(e.kind(), ErrorKind::Response);
	assert_eq!(e.server_code(), None);
	assert_eq!(
		e.to_string(),
		format!(
			"Error of kind Response: context: Failed to deserialize json response ({} bytes): \
			 v3/test, error: invalid type: string \"seven\", expected u32 at line 1 column 12",
			WRONG_TYPE.len()
		)
	);
}

#[tokio::test]
async fn an_error_answer_is_not_sent_again() {
	let (result, attempts) = post_answered(&[FOLDER_NOT_FOUND, GOOD]).await;
	let e = result.unwrap_err();
	assert_eq!(attempts, 1);
	assert_eq!(e.kind(), ErrorKind::FolderNotFound);
	assert_eq!(e.server_code().as_deref(), Some("folder_not_found"));
	assert_eq!(
		e.to_string(),
		"Error of kind FolderNotFound: context: v3/test, error: API Error, message: \
		 `Some(\"Folder not found.\")`, code: `Some(\"folder_not_found\")`"
	);
}

/// Sends one request through the client's retry layer and the response layer of format `W`, the
/// attempts answered with `bodies` in turn, then takes the answer out as the entry points do: its
/// result and how many attempts were made.
async fn through_retry<W, Res>(bodies: &[Vec<u8>]) -> (Result<Res, Error>, usize)
where
	W: WireFormat,
	Res: DeserializeOwned + Send,
{
	let client = client(ClientConfig::default());
	let attempts = AtomicUsize::new(0);
	let slot = ResponseSlot::new();
	let accepted = ServiceBuilder::new()
		.layer(client.unauthed.state.retry.clone())
		.layer(DeserializeLayer::<W>::new(&slot))
		.service_fn(|req: Request<(), ()>| {
			assert_eq!(req.response_type, W::RESPONSE_TYPE);
			let body = bodies[attempts.fetch_add(1, Ordering::SeqCst)].clone();
			std::future::ready(Ok::<_, RetryError<Error>>(body))
		})
		.oneshot(Request {
			method: RequestMethod::Get,
			response_type: ResponseType::Standard,
			url: (),
			client: reqwest::Client::new(),
		})
		.await;
	let result = accepted.and_then(|accepted| slot.into_response(accepted));
	(result, attempts.load(Ordering::SeqCst))
}

fn msgpack(json: &str) -> Vec<u8> {
	rmp_serde::to_vec(&serde_json::from_str::<serde_json::Value>(json).unwrap()).unwrap()
}

#[tokio::test]
async fn a_truncated_msgpack_body_is_sent_again() {
	let good = msgpack(GOOD);
	let truncated = good[..good.len() - 2].to_vec();
	let (result, attempts) = through_retry::<Msgpack, Answer>(&[truncated, good]).await;
	assert_eq!(result.unwrap(), Answer { n: 7 });
	assert_eq!(attempts, 2);
}

#[tokio::test]
async fn an_internal_error_msgpack_answer_is_sent_again() {
	let (result, attempts) =
		through_retry::<Msgpack, Answer>(&[msgpack(INTERNAL_ERROR), msgpack(GOOD)]).await;
	assert_eq!(result.unwrap(), Answer { n: 7 });
	assert_eq!(attempts, 2);
}

#[tokio::test]
async fn a_msgpack_body_of_the_wrong_shape_is_not_sent_again() {
	let wrong = msgpack(WRONG_TYPE);
	let len = wrong.len();
	let (result, attempts) = through_retry::<Msgpack, Answer>(&[wrong, msgpack(GOOD)]).await;
	let e = result.unwrap_err();
	assert_eq!(attempts, 1);
	assert_eq!(e.kind(), ErrorKind::Response);
	assert_eq!(
		e.to_string(),
		format!(
			"Error of kind Response: context: Failed to deserialize msgpack response ({len} \
			 bytes), error: wrong msgpack marker FixStr(5)"
		)
	);
}

#[tokio::test]
async fn an_error_msgpack_answer_is_not_sent_again() {
	let (result, attempts) =
		through_retry::<Msgpack, Answer>(&[msgpack(FOLDER_NOT_FOUND), msgpack(GOOD)]).await;
	let e = result.unwrap_err();
	assert_eq!(attempts, 1);
	assert_eq!(e.kind(), ErrorKind::FolderNotFound);
	assert_eq!(e.server_code().as_deref(), Some("folder_not_found"));
}

#[tokio::test]
async fn a_truncated_json_body_is_sent_again_by_the_retry_layer() {
	let (result, attempts) = through_retry::<Json, Answer>(&[TRUNCATED.into(), GOOD.into()]).await;
	assert_eq!(result.unwrap(), Answer { n: 7 });
	assert_eq!(attempts, 2);
}

/// A unit answer needs no `data`: `status: true` alone succeeds.
#[tokio::test]
async fn a_unit_answer_without_data_succeeds() {
	let (result, attempts) =
		through_retry::<Json, ()>(&[br#"{"status":true,"message":"ok","code":"ok"}"#.to_vec()])
			.await;
	result.unwrap();
	assert_eq!(attempts, 1);
}

/// A non-unit answer without `data` fails with the server's message, once.
#[tokio::test]
async fn an_answer_without_data_is_not_sent_again() {
	let (result, attempts) = through_retry::<Json, Answer>(&[
		br#"{"status":true,"message":"Done.","code":"done"}"#.to_vec(),
		GOOD.into(),
	])
	.await;
	let e = result.unwrap_err();
	assert_eq!(attempts, 1);
	assert_eq!(e.server_code().as_deref(), Some("done"));
}

const INTERNAL_ERROR_WITHOUT_DATA: &str =
	r#"{"status":true,"message":"Internal server error.","code":"internal_error"}"#;

/// A successful status without the `data` a non-unit answer needs is judged by its code, so an
/// `internal_error` is sent again.
#[tokio::test]
async fn an_internal_error_answer_without_data_is_sent_again() {
	let (result, attempts) = post_answered(&[INTERNAL_ERROR_WITHOUT_DATA, GOOD]).await;
	assert_eq!(result.unwrap(), Answer { n: 7 });
	assert_eq!(attempts, 2);
}

/// A unit answer needs no `data`, so the same body is a success, whatever its code.
#[tokio::test]
async fn a_unit_internal_error_answer_without_data_succeeds() {
	let (result, attempts) =
		through_retry::<Json, ()>(&[INTERNAL_ERROR_WITHOUT_DATA.into(), GOOD.into()]).await;
	result.unwrap();
	assert_eq!(attempts, 1);
}

const FOLDER_NOT_FOUND_WRONG_TYPE: &str = r#"{"status":false,"message":"Folder not found.","code":"folder_not_found","data":{"n":"seven"}}"#;

/// `data` is decoded into the response type before the status is judged, so an error answer whose
/// `data` has the wrong shape fails on the shape, once. The position counts from the start of
/// `data`.
#[tokio::test]
async fn an_error_answer_with_data_of_the_wrong_shape_fails_on_the_shape() {
	let (result, attempts) = post_answered(&[FOLDER_NOT_FOUND_WRONG_TYPE, GOOD]).await;
	let e = result.unwrap_err();
	assert_eq!(attempts, 1);
	assert_eq!(e.kind(), ErrorKind::Response);
	assert_eq!(e.server_code(), None);
	assert_eq!(
		e.to_string(),
		format!(
			"Error of kind Response: context: Failed to deserialize json response ({} bytes): \
			 v3/test, error: invalid type: string \"seven\", expected u32 at line 1 column 12",
			FOLDER_NOT_FOUND_WRONG_TYPE.len()
		)
	);
}

const TRUNCATED_WRONG_TYPE: &str =
	r#"{"status":true,"message":"ok","code":"ok","data":{"n":"seven""#;

/// A body cut short whose `data` would have had the wrong shape is sent again: the envelope is
/// decoded whole before `data` is, so the decode runs out of bytes before it sees the shape.
#[tokio::test]
async fn a_truncated_body_of_the_wrong_shape_is_sent_again() {
	let (result, attempts) = post_answered(&[TRUNCATED_WRONG_TYPE, GOOD]).await;
	assert_eq!(result.unwrap(), Answer { n: 7 });
	assert_eq!(attempts, 2);
}
