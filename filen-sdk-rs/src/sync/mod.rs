use std::sync::{Arc, Weak};

use crate::{auth::Client, error::Error, sync::lock::ResourceLock};

pub mod lock;

impl Client {
	async fn lock_resource(
		&self,
		lock: &tokio::sync::RwLock<Option<Weak<ResourceLock>>>,
		name: &str,
	) -> Result<Arc<ResourceLock>, Error> {
		let read_lock = lock.read().await;
		if let Some(weak) = read_lock.as_ref()
			&& let Some(arc) = weak.upgrade()
		{
			return Ok(arc);
		}
		std::mem::drop(read_lock);
		let mut write_lock = lock.write().await;
		if let Some(weak) = write_lock.as_ref()
			&& let Some(arc) = weak.upgrade()
		{
			return Ok(arc);
		}
		let lock = self.acquire_lock_with_default(name).await?;
		let weak = Arc::downgrade(&lock);
		write_lock.replace(weak);
		Ok(lock)
	}

	pub async fn lock_drive(&self) -> Result<Arc<lock::ResourceLock>, Error> {
		self.lock_resource(&self.drive_lock, "drive-write").await
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

	use serde::Deserialize;

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
	}

	#[derive(Deserialize)]
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		derive(tsify::Tsify),
		tsify(from_wasm_abi)
	)]
	#[serde(rename_all = "camelCase")]
	#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
	pub struct AcquireLockParams {
		resource: String,
		#[serde(default)]
		#[cfg_attr(
			all(target_family = "wasm", target_os = "unknown"),
			tsify(type = "number")
		)]
		#[cfg_attr(feature = "uniffi", uniffi(default = None))]
		max_sleep_time: Option<u32>,
		#[serde(default)]
		#[cfg_attr(
			all(target_family = "wasm", target_os = "unknown"),
			tsify(type = "number")
		)]
		#[cfg_attr(feature = "uniffi", uniffi(default = None))]
		attempts: Option<u32>,
	}

	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen::prelude::wasm_bindgen
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
