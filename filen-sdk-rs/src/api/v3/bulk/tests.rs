use std::future::{self, Ready};

use bytes::Bytes;
use filen_types::api::v3::bulk::Response;
use serde::Deserialize;
use serde_json::value::RawValue;
use tokio::{
	net::{TcpListener, TcpStream},
	task::JoinHandle,
};

use super::{Body, BulkError, Sent, run_each, send};
use crate::{
	auth::http::{
		ClientConfig,
		test_support::{client, read_head, respond_with},
	},
	error::{Error, ErrorKind},
};

/// An item's `data` on an endpoint that answers some.
#[derive(Deserialize, Debug, PartialEq)]
struct Data {
	n: u32,
}

/// Serves one request with `answer` and returns the URL to send it to. The task holds the
/// connection until it is joined: closing it with the request body unread resets it.
async fn serve_once(answer: &'static str) -> (String, JoinHandle<TcpStream>) {
	let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
	let addr = listener.local_addr().unwrap();
	let server = tokio::spawn(async move {
		let (mut socket, _) = listener.accept().await.unwrap();
		read_head(&mut socket).await;
		respond_with(&mut socket, answer).await;
		socket
	});
	(format!("http://{addr}/v3/bulk/trash"), server)
}

/// Sends `items` as one chunk, through the client's whole stack, to a server that answers it with
/// `answer`. The limit is the item count: the largest chunk [`send`] takes.
async fn send_answered<'a, T>(
	items: &'a [u32],
	body: Body,
	answer: &'static str,
) -> Result<Sent<'a, u32, T>, Error>
where
	T: serde::de::DeserializeOwned + std::fmt::Debug,
{
	let (url, server) = serve_once(answer).await;
	let client = client(ClientConfig::default());
	let sent = send(
		items,
		items.len(),
		body,
		client.post_raw_bytes_auth(Bytes::new(), &url, "v3/bulk/trash".into()),
	)
	.await;
	drop(server.await.unwrap());
	sent
}

/// A post that fails the test if it is ever polled.
fn never_sent() -> impl Future<Output = Result<Response<'static, Box<RawValue>>, Error>> {
	future::poll_fn(|_| panic!("this chunk must not be sent"))
}

/// A single call that records the item it ran for and answers `Data { n: 100 + item }`.
fn single(calls: &mut Vec<u32>) -> impl FnMut(&u32) -> Ready<Result<Data, Error>> + '_ {
	|item| {
		calls.push(*item);
		future::ready(Ok(Data { n: 100 + item }))
	}
}

/// Each result's data, or its error's kind and server code.
fn outcomes<T>(results: Vec<Result<T, Error>>) -> Vec<Result<T, (ErrorKind, Option<String>)>> {
	results
		.into_iter()
		.map(|r| r.map_err(|e| (e.kind(), e.server_code())))
		.collect()
}

fn server_error(kind: ErrorKind, code: &str) -> (ErrorKind, Option<String>) {
	(kind, Some(code.to_owned()))
}

#[tokio::test]
async fn answers_without_data_map_like_their_single_calls() {
	let items = [1, 2, 3, 4];
	let sent = send_answered::<()>(
		&items,
		Body::ItemsOnly,
		r#"{"status":true,"message":"Bulk processed.","code":"bulk_processed","data":{"results":[
			{"status":true,"message":"File moved to trash.","code":"file_trashed"},
			{"status":true,"message":"File not found.","code":"file_not_found"},
			{"status":false,"message":"Folder not found.","code":"folder_not_found"},
			{"status":false,"message":"Cannot move this file.","code":"cannot_move_this_file"}
		]}}"#,
	)
	.await
	.unwrap();

	let mut calls = Vec::new();
	let results = run_each(sent, |item| {
		calls.push(*item);
		future::ready(Ok(()))
	})
	.await;

	assert_eq!(
		outcomes(results),
		[
			Ok(()),
			// `metadata` and `rename` answer a missing file with `status: true`; the single call
			// takes that as done
			Ok(()),
			Err(server_error(ErrorKind::FolderNotFound, "folder_not_found")),
			Err(server_error(ErrorKind::Server, "cannot_move_this_file")),
		]
	);
	assert_eq!(calls, [] as [u32; 0]);
}

#[tokio::test]
async fn answers_with_data_map_like_their_single_calls() {
	let items = [1, 2, 3];
	let sent = send_answered::<Data>(
		&items,
		Body::ItemsOnly,
		r#"{"status":true,"message":"Bulk processed.","code":"bulk_processed","data":{"results":[
			{"status":true,"message":"Folder created.","code":"folder_created","data":{"n":7}},
			{"status":true,"message":"File not found.","code":"file_not_found"},
			{"status":false,"message":"A file with the same name already exists in this directory.","code":"file_with_same_name_exists_in_parent"}
		]}}"#,
	)
	.await
	.unwrap();

	let mut calls = Vec::new();
	let results = run_each(sent, single(&mut calls)).await;

	assert_eq!(
		outcomes(results),
		[
			Ok(Data { n: 7 }),
			// an endpoint that answers data fails a `status: true` answer without any
			Err(server_error(ErrorKind::FileNotFound, "file_not_found")),
			Err(server_error(
				ErrorKind::Server,
				"file_with_same_name_exists_in_parent"
			)),
		]
	);
	assert_eq!(calls, [] as [u32; 0]);
}

#[tokio::test]
async fn an_item_answered_internal_error_runs_as_its_single_call() {
	let items = [1, 2, 3];
	let sent = send_answered::<Data>(
		&items,
		Body::ItemsOnly,
		r#"{"status":true,"message":"Bulk processed.","code":"bulk_processed","data":{"results":[
			{"status":true,"message":"Folder created.","code":"folder_created","data":{"n":1}},
			{"status":false,"message":"Internal error","code":"internal_error"},
			{"status":true,"message":"Folder created.","code":"folder_created","data":{"n":3}}
		]}}"#,
	)
	.await
	.unwrap();

	let mut calls = Vec::new();
	let results = run_each(sent, single(&mut calls)).await;

	assert_eq!(
		outcomes(results),
		[Ok(Data { n: 1 }), Ok(Data { n: 102 }), Ok(Data { n: 3 })]
	);
	assert_eq!(calls, [2]);
}

#[tokio::test]
async fn an_item_whose_data_does_not_decode_fails_alone() {
	let items = [1, 2, 3];
	let sent = send_answered::<Data>(
		&items,
		Body::ItemsOnly,
		r#"{"status":true,"message":"Bulk processed.","code":"bulk_processed","data":{"results":[
			{"status":true,"message":"Folder created.","code":"folder_created","data":{"n":1}},
			{"status":true,"message":"Folder created.","code":"folder_created","data":{"n":"two"}},
			{"status":true,"message":"Folder created.","code":"folder_created","data":{"n":3}}
		]}}"#,
	)
	.await
	.unwrap();

	let mut calls = Vec::new();
	let results = run_each(sent, single(&mut calls)).await;

	assert_eq!(
		outcomes(results),
		[
			Ok(Data { n: 1 }),
			// the kind a single call gets for a response it cannot decode
			Err((ErrorKind::Response, None)),
			Ok(Data { n: 3 })
		]
	);
	assert_eq!(calls, [] as [u32; 0]);
}

#[tokio::test]
async fn a_server_without_the_endpoint_runs_every_item_as_its_single_call_in_order() {
	let items = [3, 1, 2];
	// even beside top-level fields: no item ran, and the single route exists
	let sent = send_answered::<Data>(
		&items,
		Body::WithTopLevelFields,
		r#"{"status":false,"message":"Invalid endpoint.","code":"invalid_endpoint"}"#,
	)
	.await
	.unwrap();

	let mut calls = Vec::new();
	let results = run_each(sent, single(&mut calls)).await;

	assert_eq!(
		outcomes(results),
		[
			Ok(Data { n: 103 }),
			Ok(Data { n: 101 }),
			Ok(Data { n: 102 })
		]
	);
	assert_eq!(calls, [3, 1, 2]);
}

#[tokio::test]
async fn invalid_params_on_an_items_only_endpoint_runs_the_single_calls() {
	let items = [1, 2];
	let sent = send_answered::<Data>(
		&items,
		Body::ItemsOnly,
		r#"{"status":false,"message":"Invalid UUID.","code":"invalid_params"}"#,
	)
	.await
	.unwrap();

	let mut calls = Vec::new();
	let results = run_each(sent, single(&mut calls)).await;

	assert_eq!(
		outcomes(results),
		[Ok(Data { n: 101 }), Ok(Data { n: 102 })]
	);
	assert_eq!(calls, [1, 2]);
}

#[tokio::test]
async fn invalid_params_beside_top_level_fields_fails_the_chunk() {
	let items = [1, 2];
	let Err(error) = send_answered::<()>(
		&items,
		Body::WithTopLevelFields,
		r#"{"status":false,"message":"Invalid email.","code":"invalid_params"}"#,
	)
	.await
	else {
		panic!("an invalid_params beside top-level fields must fail the chunk");
	};

	assert_eq!(error.kind(), ErrorKind::Server);
	assert_eq!(error.server_code().as_deref(), Some("invalid_params"));
}

#[tokio::test]
async fn a_result_count_other_than_the_item_count_fails_the_chunk() {
	let items = [1, 2];
	let Err(error) = send_answered::<()>(
		&items,
		Body::ItemsOnly,
		r#"{"status":true,"message":"Bulk processed.","code":"bulk_processed","data":{"results":[
			{"status":true,"message":"File moved to trash.","code":"file_trashed"}
		]}}"#,
	)
	.await
	else {
		panic!("one result for two items must fail the chunk");
	};

	assert_eq!(error.kind(), ErrorKind::Response);
	assert_eq!(
		error.downcast_ref::<BulkError>(),
		Some(&BulkError::ResultCount {
			items: 2,
			results: 1
		})
	);
}

#[tokio::test]
async fn a_chunk_over_the_limit_fails_without_a_request() {
	let items = [1, 2, 3];
	let Err(error) = send::<_, ()>(&items, 2, Body::ItemsOnly, never_sent()).await else {
		panic!("three items over a limit of two must fail");
	};

	assert_eq!(error.kind(), ErrorKind::Internal);
	assert_eq!(
		error.downcast_ref::<BulkError>(),
		Some(&BulkError::TooManyItems { items: 3, cap: 2 })
	);
}

#[tokio::test]
async fn an_empty_chunk_answers_without_a_request() {
	let sent = send::<u32, ()>(&[], 2, Body::WithTopLevelFields, never_sent())
		.await
		.unwrap();

	assert!(matches!(&sent, Sent::Answers(answers) if answers.is_empty()));
	assert!(run_each(sent, |_| future::ready(Ok(()))).await.is_empty());
}
