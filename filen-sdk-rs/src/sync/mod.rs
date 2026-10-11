use std::{
	sync::{Arc, Weak},
	time::Duration,
};

use crate::{auth::Client, error::Error, sync::lock::ResourceLock};

pub mod lock;

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
		// A cached lock whose keep-alive lost the server-side lease must not be
		// handed out again: fall through and acquire a fresh one, replacing the
		// stale cache entry (the dead Arc stays alive for its existing holders). One whose
		// lease is only unconfirmed is still handed out: a new acquire could not get the
		// resource while that lease stands.
		let read_lock = lock.read().await;
		if let Some(weak) = read_lock.as_ref()
			&& let Some(arc) = weak.upgrade()
			&& !arc.is_lost()
		{
			return Ok(arc);
		}
		std::mem::drop(read_lock);
		let mut write_lock = lock.write().await;
		if let Some(weak) = write_lock.as_ref()
			&& let Some(arc) = weak.upgrade()
			&& !arc.is_lost()
		{
			return Ok(arc);
		}
		let lock = self.acquire_lock(name, max_sleep_time, attempts).await?;
		let weak = Arc::downgrade(&lock);
		write_lock.replace(weak);
		Ok(lock)
	}

	pub async fn lock_drive(&self) -> Result<Arc<lock::ResourceLock>, Error> {
		self.lock_resource(&self.drive_lock, "drive-write").await
	}

	/// [`lock_drive`](Self::lock_drive) with a BOUNDED acquisition: the same in-process lock
	/// sharing, but a server-side-contended acquisition gives up with
	/// [`ErrorKind::RetryFailed`](crate::ErrorKind::RetryFailed) after `attempts` polls instead
	/// of waiting out the multi-hour default schedule. Used by the cache worker's resync, which
	/// must yield back to draining events when the lock is contended rather than parking on it.
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
		/// unconfirmed (a refresh failing), which reads `true` again once its keep-alive gets
		/// a refresh through, or lost (a refused refresh, or none confirmed while the lease
		/// could last), which never does. Both read `false` here: do not rely on the lock
		/// while it does, and once it has read `false` for longer than the lease (60
		/// seconds), acquire a new lock.
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
