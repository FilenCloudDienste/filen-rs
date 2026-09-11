use std::{
	future::Future,
	sync::{Arc, Weak},
	time::Duration,
};

use crate::{
	ErrorKind,
	auth::Client,
	error::Error,
	sync::lock::{Lease, ResourceLock},
};

pub mod lock;

/// `lock` once its lease is confirmed, `None` once it is lost.
async fn still_held<T>(lock: Arc<T>) -> Result<Option<Arc<T>>, Error>
where
	Arc<T>: Lease,
{
	match lock.confirm().await {
		Ok(()) => Ok(Some(lock)),
		Err(error) if error.kind() == ErrorKind::LockLost => Ok(None),
		Err(error) => Err(error),
	}
}

/// How many locks [`cached_or_acquired`] acquires, at most, for one call: a lock found lost
/// right after it was acquired is acquired once more.
const ACQUISITIONS_PER_CALL: usize = 2;

/// The lock `cache` holds, once its lease is confirmed, or else a new one from `acquire`, which
/// `cache` then holds. Confirming asks the server when the last confirmation is too old (a
/// failing refresh, a system suspend), and a failure to ask it is returned. A lost lock is not
/// handed out: a new one is acquired, replacing the stale cache entry (the lost lock stays alive
/// for its existing holders).
///
/// A new lock is confirmed like a cached one before it is handed out: its lease runs from the
/// send of the poll that got it, and a system suspend while that poll was under way can leave it
/// unconfirmed, or lost, by the time the answer arrives. One lost already is acquired again, once.
async fn cached_or_acquired<T, Fut>(
	cache: &tokio::sync::RwLock<Option<Weak<T>>>,
	mut acquire: impl FnMut() -> Fut,
) -> Result<Arc<T>, Error>
where
	Arc<T>: Lease,
	Fut: Future<Output = Result<Arc<T>, Error>>,
{
	let cached = cache.read().await.as_ref().and_then(Weak::upgrade);
	if let Some(lock) = cached
		&& let Some(lock) = still_held(lock).await?
	{
		return Ok(lock);
	}
	let mut write_lock = cache.write().await;
	if let Some(lock) = write_lock.as_ref().and_then(Weak::upgrade)
		&& let Some(lock) = still_held(lock).await?
	{
		return Ok(lock);
	}
	let mut acquisitions = 0;
	loop {
		let lock = acquire().await?;
		acquisitions += 1;
		match lock.confirm().await {
			Ok(()) => {
				write_lock.replace(Arc::downgrade(&lock));
				return Ok(lock);
			}
			Err(error)
				if error.kind() == ErrorKind::LockLost && acquisitions < ACQUISITIONS_PER_CALL =>
			{
				tracing::warn!(
					"{error}; it was lost as soon as it was acquired, acquiring it again"
				);
			}
			Err(error) => return Err(error),
		}
	}
}

impl Client {
	async fn lock_resource(
		&self,
		lock: &tokio::sync::RwLock<Option<Weak<ResourceLock>>>,
		name: &str,
	) -> Result<Arc<ResourceLock>, Error> {
		self.lock_resource_with(
			lock,
			name,
			lock::MAX_SLEEP_TIME_DEFAULT,
			lock::ATTEMPTS_DEFAULT,
		)
		.await
	}

	/// [`lock_resource`](Self::lock_resource) with a caller-chosen acquisition schedule. The
	/// in-process Weak-cache sharing is identical — only how long a server-side-contended
	/// acquisition keeps polling differs.
	async fn lock_resource_with(
		&self,
		lock: &tokio::sync::RwLock<Option<Weak<ResourceLock>>>,
		name: &str,
		max_sleep_time: Duration,
		attempts: usize,
	) -> Result<Arc<ResourceLock>, Error> {
		cached_or_acquired(lock, || self.acquire_lock(name, max_sleep_time, attempts)).await
	}

	pub async fn lock_drive(&self) -> Result<Arc<lock::ResourceLock>, Error> {
		self.lock_resource(&self.drive_lock, "drive-write").await
	}

	/// [`lock_drive`](Self::lock_drive) with a BOUNDED acquisition: the same in-process lock
	/// sharing, but a server-side-contended acquisition gives up with
	/// [`ErrorKind::RetryFailed`](crate::ErrorKind::RetryFailed) after `attempts` polls instead
	/// of waiting out the multi-hour default schedule. Used by the cache worker's resync, which
	/// must yield back to draining events when the lock is contended rather than parking on it,
	/// and by a sync-engine pass, which keeps the default attempt count and takes only the
	/// tighter poll cadence — a pass that misses the release has to be back inside its own
	/// confirmation window.
	#[cfg(feature = "cache")]
	pub(crate) async fn lock_drive_bounded(
		&self,
		max_sleep_time: Duration,
		attempts: usize,
	) -> Result<Arc<lock::ResourceLock>, Error> {
		self.lock_resource_with(&self.drive_lock, "drive-write", max_sleep_time, attempts)
			.await
	}

	pub async fn lock_notes(&self) -> Result<Arc<lock::ResourceLock>, Error> {
		self.lock_resource(&self.notes_lock, "notes-write").await
	}

	pub async fn lock_chats(&self) -> Result<Arc<lock::ResourceLock>, Error> {
		self.lock_resource(&self.chats_lock, "chats-write").await
	}

	pub async fn lock_auth(&self) -> Result<Arc<lock::ResourceLock>, Error> {
		self.lock_resource(&self.auth_lock, "auth").await
	}
}

#[cfg(any(feature = "wasm-full", feature = "uniffi"))]
mod js_impl {
	use std::sync::Arc;

	use filen_macros::js_type;

	use crate::{
		Error,
		auth::JsClient,
		runtime::do_on_commander,
		sync::lock::{self},
	};

	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen::prelude::wasm_bindgen
	)]
	#[cfg_attr(feature = "uniffi", derive(uniffi::Object))]
	pub struct ResourceLock(Arc<lock::ResourceLock>);

	#[cfg(all(target_family = "wasm", target_os = "unknown"))]
	#[wasm_bindgen::prelude::wasm_bindgen]
	impl ResourceLock {
		/// Utility function to be able to immediately drop the lock from JS
		#[wasm_bindgen::prelude::wasm_bindgen]
		pub fn drop(self) {}
	}

	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen::prelude::wasm_bindgen
	)]
	#[cfg_attr(feature = "uniffi", uniffi::export)]
	impl ResourceLock {
		/// The resource this lock is for
		#[cfg_attr(
			all(target_family = "wasm", target_os = "unknown"),
			wasm_bindgen::prelude::wasm_bindgen
		)]
		pub fn resource(&self) -> String {
			self.0.resource().to_string()
		}

		/// Whether the server confirmed the lock's lease recently enough to rely on it.
		/// `false` means another client may hold the resource, or soon may: the lock is
		/// unconfirmed (a refresh failing, or the system was suspended), which reads `true`
		/// again once its keep-alive gets a refresh through, or lost (a refused refresh, or
		/// none confirmed while the lease could last), which never does. Both read `false`
		/// here: do not rely on the lock while it does, and once it has read `false` for
		/// longer than the lease (60 seconds), acquire a new lock.
		#[cfg_attr(
			all(target_family = "wasm", target_os = "unknown"),
			wasm_bindgen::prelude::wasm_bindgen(js_name = "isValid")
		)]
		pub fn is_valid(&self) -> bool {
			self.0.is_valid()
		}
	}

	#[js_type(import)]
	pub struct AcquireLockParams {
		resource: String,
		#[cfg_attr(feature = "wasm-full", tsify(type = "number"), serde(default))]
		#[cfg_attr(feature = "uniffi", uniffi(default = None))]
		max_sleep_time: Option<u32>,
		#[cfg_attr(feature = "wasm-full", tsify(type = "number"), serde(default))]
		#[cfg_attr(feature = "uniffi", uniffi(default = None))]
		attempts: Option<u32>,
	}

	// `js_class = "Client"` is load-bearing: `JsClient` is exported under the JS name
	// "Client" (auth/js_impls.rs), and without the matching js_class this block's methods
	// land on a separate, unconstructible "JsClient" class — unreachable from JS.
	#[filen_macros::js_exports]
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen::prelude::wasm_bindgen(js_class = "Client")
	)]
	#[cfg_attr(feature = "uniffi", uniffi::export)]
	impl JsClient {
		#[cfg_attr(
			all(target_family = "wasm", target_os = "unknown"),
			wasm_bindgen::prelude::wasm_bindgen(js_name = "lockDrive")
		)]
		pub async fn lock_drive(&self) -> Result<ResourceLock, Error> {
			let this = self.inner();
			do_on_commander(move || async move { this.lock_drive().await.map(ResourceLock) }).await
		}

		#[cfg_attr(
			all(target_family = "wasm", target_os = "unknown"),
			wasm_bindgen::prelude::wasm_bindgen(js_name = "acquireLock")
		)]
		pub async fn acquire_lock(&self, params: AcquireLockParams) -> Result<ResourceLock, Error> {
			let this = self.inner();
			do_on_commander(move || async move {
				this.acquire_lock(
					params.resource,
					params
						.max_sleep_time
						.map(|t| std::time::Duration::from_secs(t.into()))
						.unwrap_or(lock::MAX_SLEEP_TIME_DEFAULT),
					params
						.attempts
						.map(|a| usize::try_from(a).unwrap_or(usize::MAX))
						.unwrap_or(lock::ATTEMPTS_DEFAULT),
				)
				.await
				.map(ResourceLock)
			})
			.await
		}
	}
}

#[cfg(test)]
mod tests {
	use std::sync::atomic::{AtomicUsize, Ordering};

	use super::*;
	use crate::error::LockLostError;

	/// A cached lock whose lease confirms, or fails to, as scripted.
	#[derive(Debug)]
	struct ScriptedLock {
		/// `None` confirms the lease; otherwise the kind of error confirming fails with.
		fails_with: Option<ErrorKind>,
	}

	impl Lease for Arc<ScriptedLock> {
		async fn confirm(&self) -> Result<(), Error> {
			match self.fails_with {
				None => Ok(()),
				Some(ErrorKind::LockLost) => Err(LockLostError {
					resource: "test-resource".to_owned(),
				}
				.into()),
				Some(kind) => Err(Error::custom(kind, "scripted refresh")),
			}
		}
	}

	fn scripted(fails_with: Option<ErrorKind>) -> Arc<ScriptedLock> {
		Arc::new(ScriptedLock { fails_with })
	}

	fn cache_of(lock: &Arc<ScriptedLock>) -> tokio::sync::RwLock<Option<Weak<ScriptedLock>>> {
		tokio::sync::RwLock::new(Some(Arc::downgrade(lock)))
	}

	/// An acquisition handing out `locks` in turn, counted in `acquired`.
	fn acquire<'a>(
		acquired: &'a AtomicUsize,
		locks: &'a [Arc<ScriptedLock>],
	) -> impl FnMut() -> std::future::Ready<Result<Arc<ScriptedLock>, Error>> + 'a {
		move || {
			let call = acquired.fetch_add(1, Ordering::SeqCst);
			let lock = locks.get(call).expect("acquired more often than scripted");
			std::future::ready(Ok(Arc::clone(lock)))
		}
	}

	fn empty_cache() -> tokio::sync::RwLock<Option<Weak<ScriptedLock>>> {
		tokio::sync::RwLock::new(None)
	}

	#[tokio::test]
	async fn a_cached_lock_whose_lease_is_confirmed_is_handed_out() {
		let held = scripted(None);
		let cache = cache_of(&held);
		let (acquired, fresh) = (AtomicUsize::new(0), scripted(None));
		let lock = cached_or_acquired(&cache, acquire(&acquired, std::slice::from_ref(&fresh)))
			.await
			.unwrap();
		assert!(Arc::ptr_eq(&lock, &held));
		assert_eq!(acquired.load(Ordering::SeqCst), 0);
	}

	#[tokio::test]
	async fn a_cached_lock_that_cannot_be_confirmed_fails_without_a_new_acquisition() {
		// unconfirmed, and asking the server failed (with an error confirm does not ask again
		// for): the lease may well stand, so no other lock is acquired against it
		let held = scripted(Some(ErrorKind::Server));
		let cache = cache_of(&held);
		let (acquired, fresh) = (AtomicUsize::new(0), scripted(None));
		let error = cached_or_acquired(&cache, acquire(&acquired, std::slice::from_ref(&fresh)))
			.await
			.unwrap_err();
		assert_eq!(error.kind(), ErrorKind::Server);
		assert_eq!(acquired.load(Ordering::SeqCst), 0);
		let cached = cache.read().await.as_ref().and_then(Weak::upgrade);
		assert!(cached.is_some_and(|cached| Arc::ptr_eq(&cached, &held)));
	}

	#[tokio::test]
	async fn a_lost_cached_lock_is_replaced_by_a_new_one() {
		let lost = scripted(Some(ErrorKind::LockLost));
		let cache = cache_of(&lost);
		let (acquired, fresh) = (AtomicUsize::new(0), scripted(None));
		let lock = cached_or_acquired(&cache, acquire(&acquired, std::slice::from_ref(&fresh)))
			.await
			.unwrap();
		assert!(Arc::ptr_eq(&lock, &fresh));
		assert_eq!(acquired.load(Ordering::SeqCst), 1);
		// the next caller gets the new lock from the cache
		let again = cached_or_acquired(&cache, acquire(&acquired, std::slice::from_ref(&fresh)))
			.await
			.unwrap();
		assert!(Arc::ptr_eq(&again, &fresh));
		assert_eq!(acquired.load(Ordering::SeqCst), 1);
	}

	#[tokio::test]
	async fn a_lock_no_one_holds_any_more_is_acquired_again() {
		let cache = cache_of(&scripted(None));
		let (acquired, fresh) = (AtomicUsize::new(0), scripted(None));
		let lock = cached_or_acquired(&cache, acquire(&acquired, std::slice::from_ref(&fresh)))
			.await
			.unwrap();
		assert!(Arc::ptr_eq(&lock, &fresh));
		assert_eq!(acquired.load(Ordering::SeqCst), 1);
	}

	#[tokio::test]
	async fn a_new_lock_lost_on_arrival_is_acquired_again() {
		let cache = empty_cache();
		let locks = [scripted(Some(ErrorKind::LockLost)), scripted(None)];
		let acquired = AtomicUsize::new(0);
		let lock = cached_or_acquired(&cache, acquire(&acquired, &locks))
			.await
			.unwrap();
		assert!(Arc::ptr_eq(&lock, &locks[1]));
		assert_eq!(acquired.load(Ordering::SeqCst), 2);
		let cached = cache.read().await.as_ref().and_then(Weak::upgrade);
		assert!(cached.is_some_and(|cached| Arc::ptr_eq(&cached, &locks[1])));
	}

	#[tokio::test]
	async fn a_new_lock_lost_on_arrival_again_is_an_error() {
		let cache = empty_cache();
		let locks = [
			scripted(Some(ErrorKind::LockLost)),
			scripted(Some(ErrorKind::LockLost)),
		];
		let acquired = AtomicUsize::new(0);
		let error = cached_or_acquired(&cache, acquire(&acquired, &locks))
			.await
			.unwrap_err();
		assert_eq!(error.kind(), ErrorKind::LockLost);
		assert_eq!(acquired.load(Ordering::SeqCst), 2);
		assert!(cache.read().await.is_none());
	}

	#[tokio::test]
	async fn a_new_lock_that_cannot_be_confirmed_is_not_handed_out() {
		let cache = empty_cache();
		let locks = [scripted(Some(ErrorKind::Server))];
		let acquired = AtomicUsize::new(0);
		let error = cached_or_acquired(&cache, acquire(&acquired, &locks))
			.await
			.unwrap_err();
		assert_eq!(error.kind(), ErrorKind::Server);
		assert_eq!(acquired.load(Ordering::SeqCst), 1);
		assert!(cache.read().await.is_none());
	}
}
