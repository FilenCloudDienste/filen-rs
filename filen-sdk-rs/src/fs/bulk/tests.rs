use std::{collections::VecDeque, sync::Mutex};

use bytes::Bytes;
use chrono::{DateTime, Utc};
use filen_types::{
	api::v3::bulk::MAX_WRITE_ITEMS, crypto::EncryptedString, error::ResponseError, fs::Uuid,
};

use super::{
	Backend,
	create_dirs::{CreateDirsError, CreatedDir, NewDir, NewDirParent, create_dirs},
};
use crate::{
	api::v3::{
		bulk::{Body, Sent, dir::create, send},
		dir::create::Response,
	},
	auth::http::{
		AuthClient, ClientConfig,
		test_support::{client, serve_once},
	},
	error::{Error, ErrorKind},
	fs::dir::client_impl::NewDirRequest,
};

/// A backend whose bulk requests go through the client's whole stack to a scripted server, and
/// whose single calls are answered here.
struct Scripted {
	client: AuthClient,
	/// The answers to the bulk requests, in the order they are sent.
	answers: Mutex<VecDeque<String>>,
	/// Each bulk request's directories, as (uuid, parent).
	requests: Mutex<Vec<Vec<(Uuid, Uuid)>>>,
	/// Each single create, as (uuid, parent).
	singles: Mutex<Vec<(Uuid, Uuid)>>,
	/// Single creates answered with another uuid, as (requested, answered).
	merges: Vec<(Uuid, Uuid)>,
	/// Single creates that fail with `cannot_create_in_this_folder`.
	failing: Vec<Uuid>,
}

impl Scripted {
	fn new(answers: impl IntoIterator<Item = String>) -> Self {
		Self {
			client: client(ClientConfig::default()),
			answers: Mutex::new(answers.into_iter().collect()),
			requests: Mutex::default(),
			singles: Mutex::default(),
			merges: Vec::new(),
			failing: Vec::new(),
		}
	}

	fn requests(&self) -> Vec<Vec<(Uuid, Uuid)>> {
		self.requests.lock().unwrap().clone()
	}

	fn singles(&self) -> Vec<(Uuid, Uuid)> {
		self.singles.lock().unwrap().clone()
	}
}

impl Backend for Scripted {
	/// Mints the directory named `"n"` as [`uuid`]`(n)`.
	async fn mint_dir(&self, name: &str, _: DateTime<Utc>) -> Result<NewDirRequest, Error> {
		Ok(NewDirRequest {
			uuid: uuid(name.parse().unwrap()),
			meta: EncryptedString(format!("encrypted {name}").into()),
			name_hashed: format!("hashed {name}"),
		})
	}

	async fn bulk_create_dirs<'a>(
		&self,
		items: &'a [create::Item<'a>],
	) -> Result<Sent<'a, create::Item<'a>, Response>, Error> {
		self.requests
			.lock()
			.unwrap()
			.push(items.iter().map(|item| (item.uuid, item.parent)).collect());
		let answer = self.answers.lock().unwrap().pop_front();
		let (url, server) = serve_once(answer.expect("an answer for each request")).await;
		let sent = send(
			items,
			MAX_WRITE_ITEMS,
			Body::ItemsOnly,
			self.client
				.post_raw_bytes_auth(Bytes::new(), &url, create::ENDPOINT.into()),
		)
		.await;
		drop(server.await.unwrap());
		sent
	}

	async fn create_one_dir(&self, item: &create::Item<'_>) -> Result<Response, Error> {
		self.singles.lock().unwrap().push((item.uuid, item.parent));
		if self.failing.contains(&item.uuid) {
			return Err(ResponseError::ApiError {
				message: Some("Cannot create in this folder.".to_owned()),
				code: Some("cannot_create_in_this_folder".to_owned()),
			}
			.into());
		}
		let uuid = self
			.merges
			.iter()
			.find(|(requested, _)| *requested == item.uuid)
			.map_or(item.uuid, |(_, answered)| *answered);
		Ok(Response {
			uuid,
			timestamp: DateTime::UNIX_EPOCH,
		})
	}
}

fn uuid(n: u128) -> Uuid {
	Uuid::from_u128(n)
}

const ROOT: u128 = 1000;

/// Directories named `"1"`, `"2"`, ... in these parents.
fn dirs<'a>(names: &'a [String], parents: &[NewDirParent]) -> Vec<NewDir<'a>> {
	names
		.iter()
		.zip(parents)
		.map(|(name, parent)| NewDir {
			name,
			created: DateTime::UNIX_EPOCH,
			parent: *parent,
		})
		.collect()
}

fn names(count: u128) -> Vec<String> {
	(1..=count).map(|n| n.to_string()).collect()
}

fn in_root() -> NewDirParent {
	NewDirParent::Existing(uuid(ROOT))
}

fn bulk(results: &[String]) -> String {
	format!(
		r#"{{"status":true,"message":"Bulk processed.","code":"bulk_processed","data":{{"results":[{}]}}}}"#,
		results.join(",")
	)
}

fn folder_created(n: u128) -> String {
	format!(
		r#"{{"status":true,"message":"Folder created.","code":"folder_created","data":{{"uuid":"{}","timestamp":1700000000}}}}"#,
		uuid(n)
	)
}

fn internal_error() -> String {
	r#"{"status":false,"message":"Internal error","code":"internal_error"}"#.to_owned()
}

fn cannot_create() -> String {
	r#"{"status":false,"message":"Cannot create in this folder.","code":"cannot_create_in_this_folder"}"#
		.to_owned()
}

fn created(n: u128) -> Result<CreatedDir, (ErrorKind, Option<String>)> {
	Ok(CreatedDir {
		uuid: uuid(n),
		merged: false,
	})
}

fn merged(n: u128) -> Result<CreatedDir, (ErrorKind, Option<String>)> {
	Ok(CreatedDir {
		uuid: uuid(n),
		merged: true,
	})
}

/// Each result's directory, or its error's kind and server code.
fn outcomes(
	results: &[Result<CreatedDir, Error>],
) -> Vec<Result<CreatedDir, (ErrorKind, Option<String>)>> {
	results
		.iter()
		.map(|r| r.as_ref().copied().map_err(|e| (e.kind(), e.server_code())))
		.collect()
}

fn parent_not_created(result: &Result<CreatedDir, Error>) -> Option<&CreateDirsError> {
	result.as_ref().err()?.downcast_ref::<CreateDirsError>()
}

#[tokio::test]
async fn a_child_after_its_parent_is_sent_under_the_uuid_the_parent_is_requested_under() {
	let names = names(2);
	let dirs = dirs(&names, &[in_root(), NewDirParent::Earlier(0)]);
	let backend = Scripted::new([bulk(&[folder_created(1), folder_created(2)])]);

	let results = create_dirs(&backend, &dirs).await.unwrap();

	assert_eq!(outcomes(&results), [created(1), created(2)]);
	assert_eq!(
		backend.requests(),
		[[(uuid(1), uuid(ROOT)), (uuid(2), uuid(1))]]
	);
	assert_eq!(backend.singles(), []);
}

#[tokio::test]
async fn a_folder_already_there_is_answered_as_merged_and_its_children_follow_it() {
	let names = names(3);
	let dirs = dirs(
		&names,
		&[
			in_root(),
			NewDirParent::Earlier(0),
			NewDirParent::Earlier(1),
		],
	);
	// the server ran 2 in folder 50, the one it answered for 1
	let backend = Scripted::new([bulk(&[
		folder_created(50),
		folder_created(2),
		folder_created(3),
	])]);

	let results = create_dirs(&backend, &dirs).await.unwrap();

	assert_eq!(outcomes(&results), [merged(50), created(2), created(3)]);
	assert_eq!(backend.singles(), []);
}

#[tokio::test]
async fn a_parent_answered_internal_error_runs_as_its_single_call_and_its_children_follow_it() {
	let names = names(4);
	let dirs = dirs(
		&names,
		&[
			in_root(),
			NewDirParent::Earlier(0),
			NewDirParent::Earlier(1),
			in_root(),
		],
	);
	let mut backend = Scripted::new([bulk(&[
		internal_error(),
		cannot_create(),
		cannot_create(),
		folder_created(4),
	])]);
	// the single call finds 1 already there as 60
	backend.merges.push((uuid(1), uuid(60)));

	let results = create_dirs(&backend, &dirs).await.unwrap();

	assert_eq!(
		outcomes(&results),
		[merged(60), created(2), created(3), created(4)]
	);
	assert_eq!(
		backend.singles(),
		[
			(uuid(1), uuid(ROOT)),
			(uuid(2), uuid(60)),
			(uuid(3), uuid(2))
		]
	);
}

#[tokio::test]
async fn a_parent_that_is_not_created_fails_its_children_without_sending_them_again() {
	let names = names(3);
	let dirs = dirs(
		&names,
		&[
			in_root(),
			NewDirParent::Earlier(0),
			NewDirParent::Earlier(1),
		],
	);
	let mut backend = Scripted::new([bulk(&[internal_error(), cannot_create(), cannot_create()])]);
	backend.failing.push(uuid(1));

	let results = create_dirs(&backend, &dirs).await.unwrap();

	assert_eq!(
		outcomes(&results),
		[
			Err((
				ErrorKind::Server,
				Some("cannot_create_in_this_folder".to_owned())
			)),
			Err((ErrorKind::FolderNotFound, None)),
			Err((ErrorKind::FolderNotFound, None)),
		]
	);
	assert_eq!(
		parent_not_created(&results[1]),
		Some(&CreateDirsError::ParentNotCreated { item: 1, parent: 0 })
	);
	assert_eq!(
		parent_not_created(&results[2]),
		Some(&CreateDirsError::ParentNotCreated { item: 2, parent: 1 })
	);
	assert_eq!(backend.singles(), [(uuid(1), uuid(ROOT))]);
}

#[tokio::test]
async fn a_server_without_the_endpoint_creates_each_directory_in_the_folder_its_parent_answered() {
	let names = names(2);
	let dirs = dirs(&names, &[in_root(), NewDirParent::Earlier(0)]);
	let mut backend = Scripted::new([
		r#"{"status":false,"message":"Invalid endpoint.","code":"invalid_endpoint"}"#.to_owned(),
	]);
	backend.merges.push((uuid(1), uuid(60)));

	let results = create_dirs(&backend, &dirs).await.unwrap();

	assert_eq!(outcomes(&results), [merged(60), created(2)]);
	assert_eq!(
		backend.singles(),
		[(uuid(1), uuid(ROOT)), (uuid(2), uuid(60))]
	);
}

#[tokio::test]
async fn a_parent_that_does_not_come_before_its_child_is_rejected_before_anything_is_sent() {
	let names = names(2);
	let dirs = dirs(&names, &[in_root(), NewDirParent::Earlier(1)]);
	let backend = Scripted::new([]);

	let Err(error) = create_dirs(&backend, &dirs).await else {
		panic!("a directory that is its own parent must be rejected");
	};

	assert_eq!(error.kind(), ErrorKind::Internal);
	assert_eq!(
		error.downcast_ref::<CreateDirsError>(),
		Some(&CreateDirsError::ParentNotEarlier { item: 1, parent: 1 })
	);
	assert_eq!(backend.requests(), [] as [Vec<(Uuid, Uuid)>; 0]);
}

#[tokio::test]
async fn more_directories_than_one_request_takes_go_out_in_requests_in_list_order() {
	let count = u128::try_from(MAX_WRITE_ITEMS).unwrap() + 1;
	let names = names(count);
	let mut parents = vec![in_root(); MAX_WRITE_ITEMS];
	// the last directory goes in the first, which the first request answers as merged into 999
	parents.push(NewDirParent::Earlier(0));
	let dirs = dirs(&names, &parents);
	let first = std::iter::once(folder_created(999))
		.chain((2..count).map(folder_created))
		.collect::<Vec<_>>();
	let backend = Scripted::new([bulk(&first), bulk(&[folder_created(count)])]);

	let results = create_dirs(&backend, &dirs).await.unwrap();

	let mut expected = vec![merged(999)];
	expected.extend((2..=count).map(created));
	assert_eq!(outcomes(&results), expected);
	let requests = backend.requests();
	assert_eq!(
		requests[0],
		(1..count)
			.map(|n| (uuid(n), uuid(ROOT)))
			.collect::<Vec<_>>()
	);
	// a later request names the uuid the parent was answered: the server remaps only within one
	assert_eq!(requests[1], [(uuid(count), uuid(999))]);
	assert_eq!(requests.len(), 2);
}

#[tokio::test]
async fn a_child_refused_under_a_parent_the_same_request_created_keeps_its_error() {
	let names = names(2);
	let dirs = dirs(&names, &[in_root(), NewDirParent::Earlier(0)]);
	let backend = Scripted::new([bulk(&[folder_created(1), cannot_create()])]);

	let results = create_dirs(&backend, &dirs).await.unwrap();

	assert_eq!(
		outcomes(&results),
		[
			created(1),
			Err((
				ErrorKind::Server,
				Some("cannot_create_in_this_folder".to_owned())
			)),
		]
	);
	assert_eq!(backend.singles(), []);
}

#[tokio::test]
async fn a_directory_refused_under_an_existing_parent_keeps_its_error() {
	let names = names(1);
	let dirs = dirs(&names, &[in_root()]);
	let backend = Scripted::new([bulk(&[cannot_create()])]);

	let results = create_dirs(&backend, &dirs).await.unwrap();

	assert_eq!(
		outcomes(&results),
		[Err((
			ErrorKind::Server,
			Some("cannot_create_in_this_folder".to_owned())
		))]
	);
	assert_eq!(backend.singles(), []);
}

#[tokio::test]
async fn a_request_that_fails_as_a_whole_fails_its_directories_and_their_children_only() {
	let count = u128::try_from(MAX_WRITE_ITEMS).unwrap() + 2;
	let names = names(count);
	let mut parents = vec![in_root(); MAX_WRITE_ITEMS];
	// the second request holds a child of the first directory and an independent directory
	parents.extend([NewDirParent::Earlier(0), in_root()]);
	let dirs = dirs(&names, &parents);
	// one result for a whole request of directories
	let backend = Scripted::new([bulk(&[folder_created(1)]), bulk(&[folder_created(count)])]);

	let results = create_dirs(&backend, &dirs).await.unwrap();

	let mut expected = vec![Err((ErrorKind::Response, None)); MAX_WRITE_ITEMS];
	expected.extend([Err((ErrorKind::FolderNotFound, None)), created(count)]);
	assert_eq!(outcomes(&results), expected);
	assert_eq!(
		parent_not_created(&results[MAX_WRITE_ITEMS]),
		Some(&CreateDirsError::ParentNotCreated {
			item: MAX_WRITE_ITEMS,
			parent: 0
		})
	);
	let requests = backend.requests();
	assert_eq!(requests.len(), 2);
	assert_eq!(requests[1], [(uuid(count), uuid(ROOT))]);
	assert_eq!(backend.singles(), []);
}
