//! The concurrency limits each class of request is sent under: the request pool every request
//! but a lock request takes a place in, the gate chunk transfers take a place in first, and the
//! lock requests' own pool.

use std::sync::Arc;

use tokio::sync::Semaphore;
use tower::{layer::util::Stack, limit::GlobalConcurrencyLimitLayer, util::Either};

/// How many lock requests (acquire, refresh, release) run at once, apart from every other
/// request: a refresh queued behind chunk transfers, or behind listings that take minutes to
/// start answering, can miss the window the lock's keep-alive gives it.
pub(super) const LOCK_REQUEST_CONCURRENCY: usize = 4;

/// How many places of the request pool chunk transfers can never take: whatever transfers do,
/// this many other requests (a listing, a rename, an upload's `done`) go out without waiting for
/// a chunk to finish uploading or downloading.
pub(super) const CONTROL_REQUEST_HEADROOM: usize = 4;

/// Which limits a request is sent under. Fixed per entry point in `auth/http/mod.rs`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RequestClass {
	/// Everything but a chunk transfer or a lock request: listings, metadata changes, sign-in,
	/// the requests that open and finish an upload, the batcher's bulk calls.
	Control,
	/// One file chunk's upload to an ingest host or download from an egest host.
	Transfer,
	/// A drive lock's acquire, refresh or release.
	Lock,
}

/// A class's limits as one layer: a transfer's gate outside its pool place (`Left`), any other
/// class's single limit (`Right`).
pub(super) type ClassLimitLayer = Either<
	Stack<GlobalConcurrencyLimitLayer, GlobalConcurrencyLimitLayer>,
	GlobalConcurrencyLimitLayer,
>;

/// How many chunk transfers run at once for a configured concurrency.
///
/// A `tokio::sync::Semaphore` parks every waiter forever at 0 permits and panics above
/// `Semaphore::MAX_PERMITS`, a ceiling a 32-bit `usize` (wasm32) puts inside `u32`, so
/// `JsClientConfig { concurrency }` can reach it. The ceiling leaves room for the headroom, so
/// the pool (`transfers + CONTROL_REQUEST_HEADROOM`) stays within it too and always keeps its
/// headroom.
pub(super) fn transfer_concurrency(configured: usize) -> usize {
	configured.clamp(1, Semaphore::MAX_PERMITS - CONTROL_REQUEST_HEADROOM)
}

/// The client's request concurrency limits, shared by every clone of its state.
#[derive(Clone)]
pub(super) struct RequestLimits {
	// `Arc` on all three: concurrent requests of every client sharing this state take and give
	// back places in the same semaphores (shared task state); a layer is built from them per
	// request.
	/// The pool: [`transfer_concurrency`] plus [`CONTROL_REQUEST_HEADROOM`] places, taken by every
	/// request but a lock request.
	requests: Arc<Semaphore>,
	/// The gate: [`transfer_concurrency`] places, taken by a chunk transfer BEFORE its pool place,
	/// so transfers hold at most this many pool places and a transfer waiting here holds none.
	transfers: Arc<Semaphore>,
	/// [`LOCK_REQUEST_CONCURRENCY`] places for lock requests, apart from both.
	locks: Arc<Semaphore>,
}

impl RequestLimits {
	pub(super) fn new(configured_concurrency: usize) -> Self {
		let transfers = transfer_concurrency(configured_concurrency);
		Self {
			// cannot overflow or pass MAX_PERMITS: `transfer_concurrency` leaves room for it
			requests: Arc::new(Semaphore::new(transfers + CONTROL_REQUEST_HEADROOM)),
			transfers: Arc::new(Semaphore::new(transfers)),
			locks: Arc::new(Semaphore::new(LOCK_REQUEST_CONCURRENCY)),
		}
	}

	/// The limits a request of `class` is sent under, as one layer.
	pub(super) fn layer(&self, class: RequestClass) -> ClassLimitLayer {
		match class {
			RequestClass::Control => Either::Right(concurrency_limit(&self.requests)),
			// `Stack::new(inner, outer)`: the gate is the OUTER layer, so its place is taken
			// first. Reversed, a transfer would sit on a pool place while it waits for the gate,
			// and queued transfers would eat the headroom.
			RequestClass::Transfer => Either::Left(Stack::new(
				concurrency_limit(&self.requests),
				concurrency_limit(&self.transfers),
			)),
			RequestClass::Lock => Either::Right(concurrency_limit(&self.locks)),
		}
	}
}

/// A layer that takes one place of `semaphore` for each request sent through it.
fn concurrency_limit(semaphore: &Arc<Semaphore>) -> GlobalConcurrencyLimitLayer {
	GlobalConcurrencyLimitLayer::with_semaphore(Arc::clone(semaphore))
}

#[cfg(all(test, not(all(target_family = "wasm", target_os = "unknown"))))]
mod tests {
	use std::convert::Infallible;

	use tokio::sync::{OwnedSemaphorePermit, oneshot};
	use tower::{ServiceBuilder, ServiceExt};

	use super::*;

	/// A request of `class` sent under `limits` to a service that answers once `done` fires.
	fn request(
		limits: &RequestLimits,
		class: RequestClass,
		done: oneshot::Receiver<()>,
	) -> impl Future<Output = Result<(), Infallible>> + use<> {
		ServiceBuilder::new()
			.layer(limits.layer(class))
			.service(tower::service_fn(
				|done: oneshot::Receiver<()>| async move {
					// a dropped sender answers too: the test only needs the request to end
					let _ = done.await;
					Ok::<_, Infallible>(())
				},
			))
			.oneshot(done)
	}

	/// (requests, transfers, locks) places free.
	fn free(limits: &RequestLimits) -> (usize, usize, usize) {
		(
			limits.requests.available_permits(),
			limits.transfers.available_permits(),
			limits.locks.available_permits(),
		)
	}

	/// Takes `n` places of `semaphore` until the permit is dropped.
	fn hold(semaphore: &Arc<Semaphore>, n: u32) -> OwnedSemaphorePermit {
		Arc::clone(semaphore)
			.try_acquire_many_owned(n)
			.expect("a fresh semaphore has the places")
	}

	#[tokio::test]
	async fn a_control_request_holds_one_pool_place_until_it_is_done() {
		let limits = RequestLimits::new(2);
		let (answer, done) = oneshot::channel();
		let mut sent = Box::pin(request(&limits, RequestClass::Control, done));
		assert!(futures::poll!(sent.as_mut()).is_pending());
		assert_eq!(free(&limits), (5, 2, 4));

		answer.send(()).expect("the request is still waiting");
		// awaited by value, so the finished request (and the place it holds) is dropped
		assert_eq!(sent.await, Ok(()));
		assert_eq!(free(&limits), (6, 2, 4));
	}

	#[tokio::test]
	async fn a_lock_request_holds_one_lock_place_and_no_pool_place() {
		let limits = RequestLimits::new(2);
		let (_answer, done) = oneshot::channel();
		let mut sent = Box::pin(request(&limits, RequestClass::Lock, done));
		assert!(futures::poll!(sent.as_mut()).is_pending());
		assert_eq!(free(&limits), (6, 2, 3));
	}

	#[tokio::test]
	async fn a_lock_request_gets_its_place_while_the_pool_is_full() {
		let limits = RequestLimits::new(2);
		let _pool = hold(&limits.requests, 6);
		let (answer, done) = oneshot::channel();
		let mut sent = Box::pin(request(&limits, RequestClass::Lock, done));
		assert!(futures::poll!(sent.as_mut()).is_pending());
		assert_eq!(free(&limits), (0, 2, 3));

		answer.send(()).expect("the request is still waiting");
		assert_eq!(sent.await, Ok(()));
		assert_eq!(free(&limits), (0, 2, 4));
	}

	#[tokio::test]
	async fn a_transfer_holds_a_transfer_place_and_a_pool_place_until_it_is_done() {
		let limits = RequestLimits::new(2);
		let (answer, done) = oneshot::channel();
		let mut sent = Box::pin(request(&limits, RequestClass::Transfer, done));
		assert!(futures::poll!(sent.as_mut()).is_pending());
		assert_eq!(free(&limits), (5, 1, 4));

		answer.send(()).expect("the request is still waiting");
		assert_eq!(sent.await, Ok(()));
		assert_eq!(free(&limits), (6, 2, 4));
	}

	#[tokio::test]
	async fn a_transfer_takes_its_transfer_place_before_its_pool_place() {
		let limits = RequestLimits::new(2);
		let _pool = hold(&limits.requests, 6);
		let (_answer, done) = oneshot::channel();
		let mut sent = Box::pin(request(&limits, RequestClass::Transfer, done));
		assert!(futures::poll!(sent.as_mut()).is_pending());
		assert_eq!(free(&limits), (0, 1, 4));
	}

	#[tokio::test]
	async fn a_transfer_waiting_for_a_transfer_place_holds_no_pool_place() {
		let limits = RequestLimits::new(2);
		let _gate = hold(&limits.transfers, 2);
		let (_answer, done) = oneshot::channel();
		let mut sent = Box::pin(request(&limits, RequestClass::Transfer, done));
		assert!(futures::poll!(sent.as_mut()).is_pending());
		assert_eq!(free(&limits), (6, 0, 4));
	}

	#[tokio::test]
	async fn a_transfer_dropped_while_it_waits_for_a_pool_place_gives_its_transfer_place_back() {
		let limits = RequestLimits::new(2);
		let _pool = hold(&limits.requests, 6);
		let (_answer, done) = oneshot::channel();
		let mut sent = Box::pin(request(&limits, RequestClass::Transfer, done));
		assert!(futures::poll!(sent.as_mut()).is_pending());
		assert_eq!(free(&limits), (0, 1, 4));

		drop(sent);
		assert_eq!(free(&limits), (0, 2, 4));
	}

	#[tokio::test]
	async fn queued_transfers_leave_the_headroom_to_other_requests() {
		let limits = RequestLimits::new(2);
		// every answer is kept unsent, so each request holds what it took until the test ends
		let mut answers = Vec::new();
		let mut send = |class| {
			let (answer, done) = oneshot::channel::<()>();
			answers.push(answer);
			Box::pin(request(&limits, class, done))
		};

		let mut transfers = [send(RequestClass::Transfer), send(RequestClass::Transfer)];
		for transfer in &mut transfers {
			assert!(futures::poll!(transfer.as_mut()).is_pending());
		}
		assert_eq!(free(&limits), (4, 0, 4));
		let mut queued = send(RequestClass::Transfer);
		assert!(futures::poll!(queued.as_mut()).is_pending());
		assert_eq!(free(&limits), (4, 0, 4));

		let mut controls = Vec::new();
		for left in [3, 2, 1, 0] {
			let mut control = send(RequestClass::Control);
			assert!(futures::poll!(control.as_mut()).is_pending());
			assert_eq!(free(&limits).0, left);
			controls.push(control);
		}
		let mut fifth = send(RequestClass::Control);
		assert!(futures::poll!(fifth.as_mut()).is_pending());
		assert_eq!(free(&limits), (0, 0, 4));
	}

	#[tokio::test]
	async fn the_limits_are_clamped_at_both_ends() {
		assert_eq!(free(&RequestLimits::new(0)), (5, 1, 4));
		assert_eq!(
			free(&RequestLimits::new(usize::MAX)),
			(Semaphore::MAX_PERMITS, Semaphore::MAX_PERMITS - 4, 4)
		);
		assert_eq!(transfer_concurrency(0), 1);
		assert_eq!(transfer_concurrency(usize::MAX), Semaphore::MAX_PERMITS - 4);
	}
}
