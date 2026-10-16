use std::{
	borrow::Cow,
	fmt::Debug,
	sync::{Arc, Mutex, RwLock},
	time::Duration,
};

use filen_types::{api::v3::bulk::Response, auth::APIKey, error::ResponseError, fs::Uuid};
use futures::future::{self, Ready};
use serde_json::{Value, json, value::RawValue};
use tokio::time::{Instant, sleep, timeout};

use super::*;
use crate::{
	ErrorKind,
	api::v3::item::linked,
	auth::{
		http::{AuthClient, ClientConfig, SharedClientState},
		unauth::test_support::offline_unauthed_with,
	},
};

const WINDOW: Duration = Duration::from_millis(5);

/// A read route: identical items share a slot.
const READ: BulkRoute = BulkRoute {
	endpoint: "v3/bulk/read",
	cap: 1000,
	dedupe: true,
	tag: None,
	top: &[],
};

/// A write route, capped as the batcher caps writes.
const WRITE: BulkRoute = BulkRoute {
	endpoint: "v3/bulk/write",
	cap: 50,
	dedupe: false,
	tag: None,
	top: &[],
};

/// A write route whose bulk body carries `email` once, beside its items.
const WITH_TOP: BulkRoute = BulkRoute {
	endpoint: "v3/bulk/top",
	cap: 50,
	dedupe: false,
	tag: None,
	top: &["email"],
};

type BulkAnswer = Result<Response<'static, Box<RawValue>>, Error>;

/// What the fake server answers for a body's items: the `results` array, or the request's error.
type Answering = fn(&[Value]) -> Result<Value, Error>;

/// Records every bulk request it is sent.
#[derive(Default)]
struct Server {
	posts: Arc<Mutex<Vec<(Instant, Value)>>>,
}

impl Server {
	/// A post that records its body and answers with `answer`.
	fn post(&self, answer: Answering) -> impl FnOnce(Arc<Taken>) -> Ready<BulkAnswer> {
		let posts = Arc::clone(&self.posts);
		move |taken| {
			let body = serde_json::to_value(&*taken).unwrap();
			let items = body["items"].as_array().unwrap().to_owned();
			posts.lock().unwrap().push((Instant::now(), body));
			future::ready(answer(&items).map(|results| {
				serde_json::from_str(&json!({ "results": results }).to_string()).unwrap()
			}))
		}
	}

	/// A post that records its body and never answers.
	fn hang(&self) -> impl FnOnce(Arc<Taken>) -> future::Pending<BulkAnswer> {
		let posts = Arc::clone(&self.posts);
		move |taken| {
			let body = serde_json::to_value(&*taken).unwrap();
			posts.lock().unwrap().push((Instant::now(), body));
			future::pending()
		}
	}

	fn bodies(&self) -> Vec<Value> {
		let posts = self.posts.lock().unwrap();
		posts.iter().map(|(_, body)| body.to_owned()).collect()
	}
}

fn ok(data: &Value) -> Value {
	json!({"status": true, "message": "Done.", "code": "done", "data": data})
}

/// Answers each item with itself as its data.
fn echo(items: &[Value]) -> Result<Value, Error> {
	Ok(items.iter().map(ok).collect())
}

fn api_error(code: &str) -> Error {
	Error::from(ResponseError::ApiError {
		message: Some("Refused.".to_owned()),
		code: Some(code.to_owned()),
	})
}

async fn call_in<Res, P, Fut>(
	batcher: &Batcher,
	route: &BulkRoute,
	lane: Lane,
	request: Value,
	post: P,
) -> Reply<Res>
where
	Res: ItemData,
	P: FnOnce(Arc<Taken>) -> Fut,
	Fut: Future<Output = BulkAnswer>,
{
	let split = route.split(&request).expect("a test request splits");
	batcher.call(route, lane, split, WINDOW, post).await
}

async fn call<Res, P, Fut>(
	batcher: &Batcher,
	route: &BulkRoute,
	request: Value,
	post: P,
) -> Reply<Res>
where
	Res: ItemData,
	P: FnOnce(Arc<Taken>) -> Fut,
	Fut: Future<Output = BulkAnswer>,
{
	call_in(batcher, route, current_lane(), request, post).await
}

fn current_lane() -> Lane {
	lane().expect("tests run on a runtime")
}

fn data<Res: Debug>(reply: Reply<Res>) -> Res {
	match reply {
		Reply::Item(Ok(data)) => data,
		other => panic!("expected the item's data, got {other:?}"),
	}
}

fn item_error<Res: Debug>(reply: Reply<Res>) -> Error {
	match reply {
		Reply::Item(Err(e)) => e,
		other => panic!("expected the item's error, got {other:?}"),
	}
}

fn failure<Res: Debug>(reply: Reply<Res>) -> Error {
	match reply {
		Reply::Failed(e) => e,
		other => panic!("expected the bulk request's failure, got {other:?}"),
	}
}

fn assert_alone<Res: Debug>(reply: &Reply<Res>) {
	assert!(
		matches!(reply, Reply::Alone),
		"expected alone, got {reply:?}"
	);
}

/// A lead and two joiners on `route`, the joiners answered with `answer`.
async fn burst(route: &BulkRoute, answer: Answering) -> (Server, Reply<Value>, Reply<Value>) {
	let server = Server::default();
	let batcher = Batcher::default();
	let (lead, a, b) = futures::join!(
		call::<Value, _, _>(
			&batcher,
			route,
			json!({"email": "a", "n": 0}),
			server.post(echo)
		),
		call::<Value, _, _>(
			&batcher,
			route,
			json!({"email": "a", "n": 1}),
			server.post(answer)
		),
		call::<Value, _, _>(
			&batcher,
			route,
			json!({"email": "a", "n": 2}),
			server.post(answer)
		),
	);
	assert_alone(&lead);
	(server, a, b)
}

#[test]
fn the_default_config_turns_batching_off() {
	let state = SharedClientState::new(ClientConfig::default()).expect("the defaults are valid");
	assert_eq!(state.batch_window(), Duration::ZERO);
}

#[tokio::test(start_paused = true)]
async fn a_lead_goes_alone_and_its_joiners_share_one_bulk_request_at_the_deadline() {
	let server = Server::default();
	let batcher = Batcher::default();
	let start = Instant::now();
	let (lead, a, b) = futures::join!(
		call::<Value, _, _>(&batcher, &READ, json!({"uuid": 0}), server.post(echo)),
		call::<Value, _, _>(&batcher, &READ, json!({"uuid": 1}), server.post(echo)),
		call::<Value, _, _>(&batcher, &READ, json!({"uuid": 2}), server.post(echo)),
	);
	assert_alone(&lead);
	assert_eq!(data(a), json!({"uuid": 1}));
	assert_eq!(data(b), json!({"uuid": 2}));
	assert_eq!(
		*server.posts.lock().unwrap(),
		[(start + WINDOW, json!({"items": [{"uuid": 1}, {"uuid": 2}]}))]
	);
}

#[tokio::test(start_paused = true)]
async fn a_flushed_window_leaves_the_map_before_the_next_call() {
	let server = Server::default();
	let batcher = Batcher::default();
	let (_, a, b) = futures::join!(
		call::<Value, _, _>(&batcher, &READ, json!({"uuid": 0}), server.post(echo)),
		call::<Value, _, _>(&batcher, &READ, json!({"uuid": 1}), server.post(echo)),
		call::<Value, _, _>(&batcher, &READ, json!({"uuid": 2}), server.post(echo)),
	);
	assert_eq!(data(a), json!({"uuid": 1}));
	assert_eq!(data(b), json!({"uuid": 2}));
	assert!(batcher.windows.lock().unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_lone_call_sends_nothing_and_the_next_call_after_the_deadline_leads_again() {
	let server = Server::default();
	let batcher = Batcher::default();
	let lone = call::<Value, _, _>(&batcher, &READ, json!({"uuid": 0}), server.post(echo)).await;
	assert_alone(&lone);
	sleep(WINDOW).await;

	let start = Instant::now();
	let next = call::<Value, _, _>(&batcher, &READ, json!({"uuid": 0}), server.post(echo)).await;
	assert_alone(&next);
	assert_eq!(Instant::now(), start, "a lead does not wait");
	let windows = batcher.windows.lock().unwrap();
	let deadlines = windows.values().map(|w| w.deadline).collect::<Vec<_>>();
	assert_eq!(deadlines, [start + WINDOW], "the expired window is gone");
	assert!(server.bodies().is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_single_joiner_goes_alone_at_the_deadline() {
	let server = Server::default();
	let batcher = Batcher::default();
	let start = Instant::now();
	let (lead, joiner) = futures::join!(
		call::<Value, _, _>(&batcher, &READ, json!({"uuid": 0}), server.post(echo)),
		call::<Value, _, _>(&batcher, &READ, json!({"uuid": 1}), server.post(echo)),
	);
	assert_alone(&lead);
	assert_alone(&joiner);
	assert_eq!(Instant::now(), start + WINDOW);
	assert!(server.bodies().is_empty());
}

#[tokio::test(start_paused = true)]
async fn identical_reads_share_one_item_and_its_answer() {
	let server = Server::default();
	let batcher = Batcher::default();
	let (_, a, b) = futures::join!(
		call::<Value, _, _>(&batcher, &READ, json!({"uuid": 0}), server.post(echo)),
		call::<Value, _, _>(&batcher, &READ, json!({"uuid": 1}), server.post(echo)),
		call::<Value, _, _>(&batcher, &READ, json!({"uuid": 1}), server.post(echo)),
	);
	assert_eq!(data(a), json!({"uuid": 1}));
	assert_eq!(data(b), json!({"uuid": 1}));
	assert_eq!(server.bodies(), [json!({"items": [{"uuid": 1}]})]);
}

#[tokio::test(start_paused = true)]
async fn identical_writes_each_keep_their_own_item() {
	let server = Server::default();
	let batcher = Batcher::default();
	let (_, a, b) = futures::join!(
		call::<Value, _, _>(&batcher, &WRITE, json!({"uuid": 0}), server.post(echo)),
		call::<Value, _, _>(&batcher, &WRITE, json!({"uuid": 1}), server.post(echo)),
		call::<Value, _, _>(&batcher, &WRITE, json!({"uuid": 1}), server.post(echo)),
	);
	assert_eq!(data(a), json!({"uuid": 1}));
	assert_eq!(data(b), json!({"uuid": 1}));
	assert_eq!(
		server.bodies(),
		[json!({"items": [{"uuid": 1}, {"uuid": 1}]})]
	);
}

#[tokio::test(start_paused = true)]
async fn a_joiner_dropped_before_the_deadline_leaves_its_item_out() {
	let server = Server::default();
	let batcher = Batcher::default();
	// the first joiner drives the flush, so the second one takes it over
	let (_, dropped, a, b) = futures::join!(
		call::<Value, _, _>(&batcher, &READ, json!({"uuid": 0}), server.post(echo)),
		timeout(
			Duration::from_millis(1),
			call::<Value, _, _>(&batcher, &READ, json!({"uuid": 1}), server.post(echo)),
		),
		call::<Value, _, _>(&batcher, &READ, json!({"uuid": 2}), server.post(echo)),
		call::<Value, _, _>(&batcher, &READ, json!({"uuid": 3}), server.post(echo)),
	);
	assert!(dropped.is_err());
	assert_eq!(data(a), json!({"uuid": 2}));
	assert_eq!(data(b), json!({"uuid": 3}));
	assert_eq!(
		server.bodies(),
		[json!({"items": [{"uuid": 2}, {"uuid": 3}]})]
	);
}

#[tokio::test(start_paused = true)]
async fn a_window_whose_joiners_all_dropped_sends_nothing_and_is_pruned() {
	let server = Server::default();
	let batcher = Batcher::default();
	let (_, a, b) = futures::join!(
		call::<Value, _, _>(&batcher, &READ, json!({"uuid": 0}), server.post(echo)),
		timeout(
			Duration::from_millis(1),
			call::<Value, _, _>(&batcher, &READ, json!({"uuid": 1}), server.post(echo)),
		),
		timeout(
			Duration::from_millis(1),
			call::<Value, _, _>(&batcher, &READ, json!({"uuid": 2}), server.post(echo)),
		),
	);
	assert!(a.is_err() && b.is_err());
	sleep(WINDOW).await;

	let start = Instant::now();
	let next = call::<Value, _, _>(&batcher, &READ, json!({"uuid": 3}), server.post(echo)).await;
	assert_alone(&next);
	let windows = batcher.windows.lock().unwrap();
	let deadlines = windows.values().map(|w| w.deadline).collect::<Vec<_>>();
	assert_eq!(deadlines, [start + WINDOW]);
	assert!(server.bodies().is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_full_window_sends_its_cap_and_the_next_joiners_open_another() {
	let server = Server::default();
	let batcher = Batcher::default();
	let replies = futures::future::join_all(
		(0..102)
			.map(|n| call::<Value, _, _>(&batcher, &WRITE, json!({"uuid": n}), server.post(echo))),
	)
	.await;
	// the lead, and the one joiner left over after two full windows
	let alone = replies
		.iter()
		.filter(|reply| matches!(reply, Reply::Alone))
		.count();
	let answered = replies
		.iter()
		.filter(|reply| matches!(reply, Reply::Item(Ok(_))))
		.count();
	assert_eq!((alone, answered), (2, 100));
	let sizes = server
		.bodies()
		.iter()
		.map(|body| body["items"].as_array().unwrap().len())
		.collect::<Vec<_>>();
	assert_eq!(sizes, [50, 50]);
}

#[tokio::test(start_paused = true)]
async fn calls_with_different_top_level_fields_go_in_different_requests() {
	let server = Server::default();
	let batcher = Batcher::default();
	let request = |email: &str, n: u32| json!({"email": email, "n": n});
	let _ = futures::join!(
		call::<Value, _, _>(&batcher, &WITH_TOP, request("a", 0), server.post(echo)),
		call::<Value, _, _>(&batcher, &WITH_TOP, request("b", 0), server.post(echo)),
		call::<Value, _, _>(&batcher, &WITH_TOP, request("a", 1), server.post(echo)),
		call::<Value, _, _>(&batcher, &WITH_TOP, request("b", 1), server.post(echo)),
		call::<Value, _, _>(&batcher, &WITH_TOP, request("a", 2), server.post(echo)),
		call::<Value, _, _>(&batcher, &WITH_TOP, request("b", 2), server.post(echo)),
	);
	let mut bodies = server.bodies();
	bodies.sort_by_key(Value::to_string);
	assert_eq!(
		bodies,
		[
			json!({"email": "a", "items": [{"n": 1}, {"n": 2}]}),
			json!({"email": "b", "items": [{"n": 1}, {"n": 2}]}),
		]
	);
}

#[tokio::test(start_paused = true)]
async fn calls_on_different_lanes_go_in_different_requests() {
	let other = tokio::runtime::Builder::new_current_thread()
		.build()
		.unwrap();
	let (here, there) = (current_lane(), other.handle().id());
	let server = Server::default();
	let batcher = Batcher::default();
	let _ = futures::join!(
		call_in::<Value, _, _>(&batcher, &READ, here, json!({"n": 0}), server.post(echo)),
		call_in::<Value, _, _>(&batcher, &READ, there, json!({"n": 0}), server.post(echo)),
		call_in::<Value, _, _>(&batcher, &READ, here, json!({"n": 1}), server.post(echo)),
		call_in::<Value, _, _>(&batcher, &READ, there, json!({"n": 1}), server.post(echo)),
		call_in::<Value, _, _>(&batcher, &READ, here, json!({"n": 2}), server.post(echo)),
		call_in::<Value, _, _>(&batcher, &READ, there, json!({"n": 2}), server.post(echo)),
	);
	assert_eq!(
		server.bodies(),
		[
			json!({"items": [{"n": 1}, {"n": 2}]}),
			json!({"items": [{"n": 1}, {"n": 2}]}),
		]
	);
	other.shutdown_background();
}

#[tokio::test(start_paused = true)]
async fn a_member_taking_over_a_dropped_sender_sends_the_same_body_again() {
	let server = Server::default();
	let batcher = Batcher::default();
	let start = Instant::now();
	let (_, dropped, survivor) = futures::join!(
		call::<Value, _, _>(&batcher, &READ, json!({"uuid": 0}), server.post(echo)),
		timeout(
			Duration::from_millis(10),
			call::<Value, _, _>(&batcher, &READ, json!({"uuid": 1}), server.hang()),
		),
		call::<Value, _, _>(&batcher, &READ, json!({"uuid": 2}), server.post(echo)),
	);
	assert!(dropped.is_err());
	assert_eq!(data(survivor), json!({"uuid": 2}));
	let body = json!({"items": [{"uuid": 1}, {"uuid": 2}]});
	assert_eq!(
		*server.posts.lock().unwrap(),
		[
			(start + WINDOW, body.to_owned()),
			(start + Duration::from_millis(10), body)
		]
	);
}

#[tokio::test(start_paused = true)]
async fn a_bulk_request_refused_as_a_whole_sends_its_calls_alone() {
	for (route, answer) in [
		(
			&READ,
			(|_: &[Value]| Err(api_error("invalid_params"))) as Answering,
		),
		(&WITH_TOP, |_: &[Value]| Err(api_error("invalid_params"))),
		(&READ, |_: &[Value]| Err(api_error("invalid_endpoint"))),
		(&READ, |_: &[Value]| Err(api_error("api_key_not_found"))),
		(&WITH_TOP, |_: &[Value]| Err(api_error("rate_limited"))),
	] {
		let (server, a, b) = burst(route, answer).await;
		assert_alone(&a);
		assert_alone(&b);
		assert_eq!(server.bodies().len(), 1);
	}
}

#[tokio::test(start_paused = true)]
async fn an_answer_with_the_wrong_result_count_sends_the_calls_alone() {
	let (_, a, b) = burst(&READ, |items| Ok(json!([ok(&items[0])]))).await;
	assert_alone(&a);
	assert_alone(&b);
}

#[tokio::test(start_paused = true)]
async fn a_lost_response_fails_every_member_with_its_kind() {
	let (_, a, b) = burst(&READ, |_| {
		Err(Error::custom(ErrorKind::Reqwest, "connection reset"))
	})
	.await;
	for e in [failure(a), failure(b)] {
		assert_eq!(e.kind(), ErrorKind::Reqwest);
		assert!(e.to_string().contains("connection reset"), "{e}");
	}
}

#[tokio::test(start_paused = true)]
async fn a_whole_request_internal_error_fails_every_member() {
	let (_, a, b) = burst(&READ, |_| Err(api_error("internal_error"))).await;
	for e in [failure(a), failure(b)] {
		assert_eq!(e.kind(), ErrorKind::Server);
		assert_eq!(e.server_code().as_deref(), Some("internal_error"));
	}
}

#[tokio::test(start_paused = true)]
async fn an_item_internal_error_sends_that_call_alone() {
	let (_, a, b) = burst(&READ, |items| {
		Ok(json!([
			{"status": false, "message": "Internal error.", "code": "internal_error"},
			ok(&items[1]),
		]))
	})
	.await;
	assert_alone(&a);
	assert_eq!(data(b), json!({"email": "a", "n": 2}));
}

#[tokio::test(start_paused = true)]
async fn an_item_error_reaches_its_caller_as_its_single_call_would_get_it() {
	let (_, a, b) = burst(&READ, |items| {
		Ok(json!([
			{"status": false, "message": "File not found.", "code": "file_not_found"},
			ok(&items[1]),
		]))
	})
	.await;
	let e = item_error(a);
	assert_eq!(e.kind(), ErrorKind::FileNotFound);
	assert_eq!(e.server_code().as_deref(), Some("file_not_found"));
	assert_eq!(data(b), json!({"email": "a", "n": 2}));
}

#[tokio::test(start_paused = true)]
async fn each_member_decodes_its_answer_as_its_own_type() {
	let server = Server::default();
	let batcher = Batcher::default();
	let answer: Answering = |_| {
		Ok(json!([
			{"status": true, "message": "Done.", "code": "done"},
			{"status": true, "message": "Done.", "code": "done", "data": 7},
			{"status": true, "message": "Done.", "code": "done", "data": "seven"},
		]))
	};
	let (_, unit, number, not_a_number) = futures::join!(
		call::<Value, _, _>(&batcher, &WRITE, json!({"n": 0}), server.post(echo)),
		call::<(), _, _>(&batcher, &WRITE, json!({"n": 1}), server.post(answer)),
		call::<u64, _, _>(&batcher, &WRITE, json!({"n": 2}), server.post(answer)),
		call::<u64, _, _>(&batcher, &WRITE, json!({"n": 3}), server.post(answer)),
	);
	data(unit);
	assert_eq!(data(number), 7);
	assert_eq!(item_error(not_a_number).kind(), ErrorKind::Response);
}

#[test]
fn a_request_that_is_not_an_object_does_not_split() {
	assert!(READ.split(&json!([1, 2])).is_none());
	assert!(READ.split(&5).is_none());
}

#[test]
fn a_request_without_a_top_level_field_does_not_split() {
	assert!(WITH_TOP.split(&json!({"n": 1})).is_none());
}

#[test]
fn a_split_moves_the_top_level_fields_out_of_the_item() {
	let split = WITH_TOP
		.split(&json!({"email": "a@example.com", "n": 1}))
		.unwrap();
	assert_eq!(Value::Object(split.top), json!({"email": "a@example.com"}));
	assert_eq!(
		serde_json::from_str::<Value>(split.item.get()).unwrap(),
		json!({"n": 1})
	);
}

#[test]
fn a_split_tags_the_item_with_its_type() {
	let tagged = BulkRoute {
		tag: Some("file"),
		..WRITE
	};
	let split = tagged.split(&json!({"uuid": 1})).unwrap();
	assert_eq!(
		serde_json::from_str::<Value>(split.item.get()).unwrap(),
		json!({"uuid": 1, "type": "file"})
	);
}

/// The errors of three concurrent `v3/item/linked` calls on a client whose requests all fail.
async fn concurrent_linked_errors(window: Duration) -> [String; 3] {
	let client = AuthClient::from_unauthed(
		offline_unauthed_with(ClientConfig::default().with_batch_window(window)),
		Arc::new(RwLock::new(APIKey(Cow::Borrowed("test-key")))),
	);
	let request = |n| linked::Request {
		uuid: Uuid::from_u128(n),
	};
	let (a, b, c) = (request(1), request(2), request(3));
	let (a, b, c) = futures::join!(
		linked::post(&client, &a),
		linked::post(&client, &b),
		linked::post(&client, &c),
	);
	let kinds = [&a, &b, &c].map(|result| result.as_ref().unwrap_err().kind());
	assert_eq!(kinds[1], kinds[2], "the members fail with one kind");
	[a, b, c].map(|result| result.unwrap_err().to_string())
}

#[tokio::test(start_paused = true)]
async fn with_batching_off_every_call_goes_to_its_own_route() {
	for e in concurrent_linked_errors(Duration::ZERO).await {
		assert!(e.contains(linked::ENDPOINT), "{e}");
		assert!(!e.contains("v3/bulk/"), "{e}");
	}
}

#[tokio::test(start_paused = true)]
async fn with_batching_on_the_calls_after_the_first_go_as_one_bulk_request() {
	let [lead, a, b] = concurrent_linked_errors(WINDOW).await;
	assert!(lead.contains(linked::ENDPOINT), "{lead}");
	assert!(!lead.contains("v3/bulk/"), "{lead}");
	for e in [a, b] {
		assert!(e.contains("v3/bulk/linked"), "{e}");
	}
}
