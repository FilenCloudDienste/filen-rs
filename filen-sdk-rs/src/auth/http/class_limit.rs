//! The concurrency limits each class of request is sent under: the request pool every request
//! but a lock request takes a place in, and the lock requests' own pool.

use std::sync::Arc;

use tokio::sync::Semaphore;
use tower::limit::GlobalConcurrencyLimitLayer;

/// How many lock requests (acquire, refresh, release) run at once, apart from
/// [`ClientConfig::with_concurrency`](super::ClientConfig::with_concurrency)'s limit: a refresh
/// queued behind that limit's chunk transfers can miss the window the lock's keep-alive gives it.
pub(super) const LOCK_REQUEST_CONCURRENCY: usize = 4;

/// Which limits a request is sent under. Fixed per entry point in `auth/http/mod.rs`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RequestClass {
	/// Every request but a lock request: listings, metadata changes, sign-in, file chunks.
	Control,
	/// A drive lock's acquire, refresh or release.
	Lock,
}

/// The client's request concurrency limits, shared by every clone of its state.
#[derive(Clone)]
pub(super) struct RequestLimits {
	// `Arc` on both: concurrent requests of every client sharing this state take and give back
	// places in the same semaphores (shared task state); a layer is built from them per request.
	/// The pool: as many places as the client's configured concurrency, taken by every request
	/// but a lock request.
	requests: Arc<Semaphore>,
	/// [`LOCK_REQUEST_CONCURRENCY`] places for lock requests, apart from the pool.
	locks: Arc<Semaphore>,
}

impl RequestLimits {
	/// `concurrency` is already clamped to `1..=Semaphore::MAX_PERMITS`: `Semaphore::new` panics
	/// above it, and a pool of 0 parks every request forever.
	pub(super) fn new(concurrency: usize) -> Self {
		Self {
			requests: Arc::new(Semaphore::new(concurrency)),
			locks: Arc::new(Semaphore::new(LOCK_REQUEST_CONCURRENCY)),
		}
	}

	/// The limit a request of `class` is sent under, as a layer.
	pub(super) fn layer(&self, class: RequestClass) -> GlobalConcurrencyLimitLayer {
		let semaphore = match class {
			RequestClass::Control => &self.requests,
			RequestClass::Lock => &self.locks,
		};
		GlobalConcurrencyLimitLayer::with_semaphore(Arc::clone(semaphore))
	}
}

#[cfg(all(test, not(all(target_family = "wasm", target_os = "unknown"))))]
mod tests {
	use std::convert::Infallible;

	use tokio::sync::oneshot;
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

	/// (requests, locks) places free.
	fn free(limits: &RequestLimits) -> (usize, usize) {
		(
			limits.requests.available_permits(),
			limits.locks.available_permits(),
		)
	}

	#[tokio::test]
	async fn a_control_request_holds_one_pool_place_until_it_is_done() {
		let limits = RequestLimits::new(2);
		let (answer, done) = oneshot::channel();
		let mut sent = Box::pin(request(&limits, RequestClass::Control, done));
		assert!(futures::poll!(sent.as_mut()).is_pending());
		assert_eq!(free(&limits), (1, 4));

		answer.send(()).expect("the request is still waiting");
		// awaited by value, so the finished request (and the place it holds) is dropped
		assert_eq!(sent.await, Ok(()));
		assert_eq!(free(&limits), (2, 4));
	}

	#[tokio::test]
	async fn a_lock_request_holds_one_lock_place_and_no_pool_place() {
		let limits = RequestLimits::new(2);
		let (_answer, done) = oneshot::channel();
		let mut sent = Box::pin(request(&limits, RequestClass::Lock, done));
		assert!(futures::poll!(sent.as_mut()).is_pending());
		assert_eq!(free(&limits), (2, 3));
	}

	#[tokio::test]
	async fn a_lock_request_gets_its_place_while_the_pool_is_full() {
		let limits = RequestLimits::new(2);
		let _pool = Arc::clone(&limits.requests)
			.try_acquire_many_owned(2)
			.expect("a fresh pool has 2 places");
		let (answer, done) = oneshot::channel();
		let mut sent = Box::pin(request(&limits, RequestClass::Lock, done));
		assert!(futures::poll!(sent.as_mut()).is_pending());
		assert_eq!(free(&limits), (0, 3));

		answer.send(()).expect("the request is still waiting");
		assert_eq!(sent.await, Ok(()));
		assert_eq!(free(&limits), (0, 4));
	}
}
