//! Calls to one single route that arrive together, sent as one request to its bulk twin.
//!
//! The first call for an idle key goes out alone at once, exactly as it would without the
//! batcher, and opens a window. Calls for the same key arriving before the window closes join it;
//! when it closes, one of them sends them all as one bulk request and each gets its own item's
//! answer, decoded as its single route decodes it. A window that ends with fewer than two callers
//! sends nothing: they make their single calls.
//!
//! A key is the bulk endpoint, the lane (see [`lane`]) and the request's top-level fields, so only
//! calls that fit in one bulk body share a window.

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
use std::sync::atomic::{AtomicU64, Ordering};
use std::{
	collections::HashMap,
	fmt, mem,
	sync::{Arc, Mutex, MutexGuard},
	time::Duration,
};

use filen_types::api::v3::bulk::Response;
use serde::{Serialize, Serializer};
use serde_json::{Map, Value, value::RawValue};
use tokio::sync::OnceCell;
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
use tokio::time::Instant;
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
use wasmtimer::std::Instant;

use crate::{
	api::v3::bulk::{self, Answer, Body, BulkError, ItemData, Sent, answered},
	error::Error,
	util,
};

#[cfg(all(test, not(all(target_family = "wasm", target_os = "unknown"))))]
mod tests;

/// Where calls to one single route go when several arrive together, and how its request becomes
/// a bulk item.
pub(crate) struct BulkRoute {
	/// The bulk endpoint the calls are sent to.
	pub(crate) endpoint: &'static str,
	/// The most items one bulk request carries; a full window's later callers open a new one.
	pub(crate) cap: usize,
	/// Whether identical items share one slot and its answer, for routes without side effects.
	pub(crate) dedupe: bool,
	/// The `"type"` the bulk item carries when the bulk endpoint serves more than one route.
	pub(crate) tag: Option<&'static str>,
	/// The single request's fields that the bulk body carries once, beside `items`.
	pub(crate) top: &'static [&'static str],
}

/// A single request split into the bulk body's top-level fields and its item.
pub(crate) struct Split {
	pub(crate) top: Map<String, Value>,
	/// `top` as JSON, the part of the window's key that keeps different top-level fields apart.
	top_key: String,
	pub(crate) item: Box<RawValue>,
}

impl BulkRoute {
	/// `request` as the bulk body's top-level fields and an item, or `None` if it is not a JSON
	/// object or lacks one of the [`top`](Self::top) fields.
	pub(crate) fn split(&self, request: &impl Serialize) -> Option<Split> {
		let Value::Object(mut item) = serde_json::to_value(request).ok()? else {
			return None;
		};
		let mut top = Map::new();
		for &field in self.top {
			top.insert(field.to_owned(), item.remove(field)?);
		}
		if let Some(tag) = self.tag {
			item.insert("type".to_owned(), Value::from(tag));
		}
		Some(Split {
			top_key: serde_json::to_string(&top).ok()?,
			top,
			item: serde_json::value::to_raw_value(&item).ok()?,
		})
	}
}

/// The body of a bulk request: `{ <top-level fields>, "items": [...] }`.
#[derive(Serialize, Debug)]
pub(crate) struct BulkBody {
	#[serde(flatten)]
	pub(crate) top: Map<String, Value>,
	pub(crate) items: Vec<Box<RawValue>>,
}

/// A window's bulk request, fixed when the window closes: a member taking over the flush from a
/// dropped one re-sends exactly this.
pub(crate) struct Taken {
	body: BulkBody,
	/// For each slot, its item's position in `body.items`; `None` for a slot nobody waited on any
	/// more, whose item was left out.
	index: Vec<Option<usize>>,
	/// How many callers wait on the answers.
	waiters: usize,
}

impl Serialize for Taken {
	fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
		self.body.serialize(serializer)
	}
}

impl fmt::Debug for Taken {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		self.body.fmt(f)
	}
}

/// What a call gets from the batcher.
#[derive(Debug)]
pub(crate) enum Reply<Res> {
	/// Make the single call: the caller led the window, the window closed with fewer than two
	/// callers, the server ran none of the items, its answer's result count did not match the
	/// items (a replay, safe for every batched route), or this item's answer was an
	/// `internal_error`.
	Alone,
	/// This item's answer, decoded as the single route decodes it.
	Item(Result<Res, Error>),
	/// The bulk request failed and some of its items may have run: the shared error, unshared.
	Failed(Error),
}

/// The lane of the calling task: one window never spans two runtimes (native) or two web workers
/// (wasm), so one runtime's timer, stall or teardown never holds up another's callers.
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
pub(crate) type Lane = tokio::runtime::Id;
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
pub(crate) type Lane = u64;

/// The calling task's lane, `None` outside a tokio runtime (the call then goes alone).
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
pub(crate) fn lane() -> Option<Lane> {
	tokio::runtime::Handle::try_current()
		.ok()
		.map(|handle| handle.id())
}

/// The calling thread's lane: each web worker runs its own executor, so it gets its own lane.
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
pub(crate) fn lane() -> Option<Lane> {
	static NEXT_LANE: AtomicU64 = AtomicU64::new(0);
	thread_local! {
		static LANE: Lane = NEXT_LANE.fetch_add(1, Ordering::Relaxed);
	}
	Some(LANE.with(|lane| *lane))
}

type Key = (&'static str, Lane, String);

/// The open windows of one [`AuthClient`](super::AuthClient), by key.
#[derive(Default)]
pub(crate) struct Batcher {
	/// `Arc`: a window is shared by the map and by the future of every caller in it, and outlives
	/// its place in the map once it is flushed, a full window is replaced or its deadline passes.
	windows: Mutex<HashMap<Key, Arc<Window>>>,
}

// Every target, wasm included, keeps `Client` `Send + Sync`, and the batcher lives in it.
const _: () = {
	const fn check<T: Send + Sync>() {}
	check::<Batcher>();
};

struct Window {
	/// When the window stops taking callers and its bulk request goes out.
	deadline: Instant,
	phase: Mutex<Phase>,
	/// Set once by the member that sends the bulk request, then read by every member. A member
	/// dropped while sending hands the job to the next one waiting.
	outcome: OnceCell<Outcome>,
}

enum Phase {
	/// Taking callers.
	Open(Slots),
	/// Closed, its request fixed.
	Sent(Arc<Taken>),
	/// Closed with fewer than two callers, who go alone.
	Alone,
}

/// An open window's callers.
struct Slots {
	top: Map<String, Value>,
	items: Vec<Slot>,
	/// Each item's slot by its JSON, for routes that dedupe.
	by_item: HashMap<String, usize>,
}

struct Slot {
	item: Box<RawValue>,
	/// How many callers wait on this item's answer.
	waiters: usize,
}

enum Outcome {
	Answers {
		taken: Arc<Taken>,
		answers: Vec<Answer<()>>,
	},
	Alone,
	Failed(Arc<Error>),
}

enum Joined {
	Lead,
	Member(Arc<Window>, usize),
}

/// A caller's place in a window: dropped before the window closes, it takes the caller off its
/// slot, and a slot nobody waits on is left out of the request.
struct Waiter<'a> {
	window: &'a Window,
	slot: usize,
}

impl Drop for Waiter<'_> {
	fn drop(&mut self) {
		if let Phase::Open(slots) = &mut *lock(&self.window.phase)
			&& let Some(slot) = slots.items.get_mut(self.slot)
		{
			slot.waiters = slot.waiters.saturating_sub(1);
		}
	}
}

/// The guarded value, also after a panic elsewhere: every critical section here leaves the state
/// consistent between statements.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
	mutex.lock().unwrap_or_else(|e| e.into_inner())
}

impl Batcher {
	/// Sends `split` with the calls for the same key that arrive within `window`, through `post`.
	/// The lock order is always the map, then a window's phase; nothing holds either across an
	/// await.
	pub(crate) async fn call<Res, P, Fut>(
		&self,
		route: &BulkRoute,
		lane: Lane,
		split: Split,
		window: Duration,
		post: P,
	) -> Reply<Res>
	where
		Res: ItemData,
		P: FnOnce(Arc<Taken>) -> Fut,
		Fut: Future<Output = Result<Response<'static, Box<RawValue>>, Error>>,
	{
		let Joined::Member(window, slot) = self.join(route, lane, split, window) else {
			return Reply::Alone;
		};
		let _waiter = Waiter {
			window: &window,
			slot,
		};
		let flush = async {
			let outcome = window.flush(route, post).await;
			// the window takes no callers any more: the map lets go of it, so its body and answers
			// go with its last member instead of waiting for the next call to prune it
			self.forget(&window);
			outcome
		};
		match window.outcome.get_or_init(|| flush).await {
			Outcome::Answers { taken, answers } => {
				let Some(answer) = taken
					.index
					.get(slot)
					.copied()
					.flatten()
					.and_then(|i| answers.get(i))
				else {
					return Reply::Alone;
				};
				answered::<Res>(answer.copy_as()).map_or(Reply::Alone, Reply::Item)
			}
			Outcome::Alone => Reply::Alone,
			Outcome::Failed(e) => Reply::Failed(Error::unshared(Arc::clone(e))),
		}
	}

	fn join(&self, route: &BulkRoute, lane: Lane, split: Split, window: Duration) -> Joined {
		let Split { top, top_key, item } = split;
		let now = Instant::now();
		let mut windows = lock(&self.windows);
		windows.retain(|_, w| w.deadline > now);
		let key = (route.endpoint, lane, top_key);
		let (deadline, item) = match windows.get(&key) {
			Some(open) => match open.try_push(route, item) {
				Ok(slot) => return Joined::Member(Arc::clone(open), slot),
				Err(Refused::Full(item)) => (open.deadline, item),
				// taken by its flush already: this caller starts a window of its own
				Err(Refused::Closed) => {
					windows.insert(key, Arc::new(Window::new(now + window, top)));
					return Joined::Lead;
				}
			},
			None => {
				windows.insert(key, Arc::new(Window::new(now + window, top)));
				return Joined::Lead;
			}
		};
		// a full window's next caller opens one with the same deadline, without a lead
		let fresh = Arc::new(Window::new(deadline, top));
		let joined = match fresh.try_push(route, item) {
			Ok(slot) => Joined::Member(Arc::clone(&fresh), slot),
			// a route with a cap of 0 takes nothing
			Err(_) => Joined::Lead,
		};
		windows.insert(key, fresh);
		joined
	}

	/// Removes `window` from the map, and not a newer window under the same key.
	fn forget(&self, window: &Arc<Window>) {
		lock(&self.windows).retain(|_, open| !Arc::ptr_eq(open, window));
	}
}

enum Refused {
	Full(Box<RawValue>),
	Closed,
}

impl Window {
	fn new(deadline: Instant, top: Map<String, Value>) -> Self {
		Self {
			deadline,
			phase: Mutex::new(Phase::Open(Slots {
				top,
				items: Vec::new(),
				by_item: HashMap::new(),
			})),
			outcome: OnceCell::new(),
		}
	}

	/// Adds a waiter for `item` and returns its slot.
	fn try_push(&self, route: &BulkRoute, item: Box<RawValue>) -> Result<usize, Refused> {
		let mut phase = lock(&self.phase);
		let Phase::Open(slots) = &mut *phase else {
			return Err(Refused::Closed);
		};
		if route.dedupe
			&& let Some(&slot) = slots.by_item.get(item.get())
			&& let Some(shared) = slots.items.get_mut(slot)
		{
			shared.waiters += 1;
			return Ok(slot);
		}
		if slots.items.len() >= route.cap {
			return Err(Refused::Full(item));
		}
		let slot = slots.items.len();
		if route.dedupe {
			// an owned copy of the item's JSON: the key must outlive the borrow of `item`, which
			// moves into the slot below
			slots.by_item.insert(item.get().to_owned(), slot);
		}
		slots.items.push(Slot { item, waiters: 1 });
		Ok(slot)
	}

	/// Waits for the deadline, then sends the window's items with `post` and classifies the
	/// answer.
	async fn flush<P, Fut>(&self, route: &BulkRoute, post: P) -> Outcome
	where
		P: FnOnce(Arc<Taken>) -> Fut,
		Fut: Future<Output = Result<Response<'static, Box<RawValue>>, Error>>,
	{
		util::sleep(self.deadline.saturating_duration_since(Instant::now())).await;
		let Some(taken) = self.take() else {
			return Outcome::Alone;
		};
		tracing::debug!(
			endpoint = route.endpoint,
			items = taken.body.items.len(),
			waiters = taken.waiters,
			"sending batched calls as one bulk request"
		);
		let body = if route.top.is_empty() {
			Body::ItemsOnly
		} else {
			Body::WithTopLevelFields
		};
		let sent = bulk::send::<Box<RawValue>, ()>(
			&taken.body.items,
			route.cap,
			body,
			post(Arc::clone(&taken)),
		)
		.await;
		match sent {
			Ok(Sent::Answers(answers)) => Outcome::Answers {
				answers: answers.into_iter().map(|(_, answer)| answer).collect(),
				taken: Arc::clone(&taken),
			},
			Ok(Sent::Unsupported(_)) => Outcome::Alone,
			Err(e) => classify(route, e),
		}
	}

	/// Closes the window and fixes its request, or `None` when its callers go alone.
	fn take(&self) -> Option<Arc<Taken>> {
		let mut phase = lock(&self.phase);
		let slots = match mem::replace(&mut *phase, Phase::Alone) {
			Phase::Open(slots) => slots,
			Phase::Sent(taken) => {
				*phase = Phase::Sent(Arc::clone(&taken));
				return Some(taken);
			}
			Phase::Alone => return None,
		};
		// waiters, not items: two callers deduped onto one item still go as one bulk request
		let waiters = slots.items.iter().map(|slot| slot.waiters).sum::<usize>();
		if waiters < 2 {
			return None;
		}
		let mut index = Vec::with_capacity(slots.items.len());
		let mut items = Vec::with_capacity(slots.items.len());
		for slot in slots.items {
			if slot.waiters == 0 {
				index.push(None);
			} else {
				index.push(Some(items.len()));
				items.push(slot.item);
			}
		}
		let taken = Arc::new(Taken {
			body: BulkBody {
				top: slots.top,
				items,
			},
			index,
			waiters,
		});
		*phase = Phase::Sent(Arc::clone(&taken));
		Some(taken)
	}
}

/// What a failed bulk request means for its callers.
fn classify(route: &BulkRoute, e: Error) -> Outcome {
	match bulk::code(&e) {
		// any other whole-request code: nothing ran, and each single call gets its own answer
		Some(code) if code != "internal_error" => {
			if code == "invalid_params" {
				tracing::warn!(
					endpoint = route.endpoint,
					error = %e,
					"bulk request refused; sending its calls alone"
				);
			} else {
				tracing::debug!(
					endpoint = route.endpoint,
					error = %e,
					"bulk request refused; sending its calls alone"
				);
			}
			Outcome::Alone
		}
		_ if matches!(
			e.downcast_ref::<BulkError>(),
			Some(BulkError::ResultCount { .. })
		) =>
		{
			// This replays items that may have run, which is safe only because every batched
			// route is replay-safe (see `api::v3::bulk::batched`).
			tracing::error!(
				endpoint = route.endpoint,
				error = %e,
				"bulk answer does not match its items; sending its calls alone"
			);
			Outcome::Alone
		}
		_ => Outcome::Failed(Arc::new(e)),
	}
}
