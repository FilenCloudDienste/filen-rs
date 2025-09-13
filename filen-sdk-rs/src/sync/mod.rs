use std::sync::Arc;

use crate::{auth::Client, error::Error};

pub mod lock;

impl Client {
	pub async fn lock_drive(&self) -> Result<Arc<lock::ResourceLock>, Error> {
		let mut guard = self.drive_lock.lock().await;
		match guard.as_ref() {
			Some(lock) => {
				if let Some(lock) = lock.upgrade() {
					Ok(lock)
				} else {
					let lock = self.acquire_lock_with_default("drive-write").await?;
					let weak = Arc::downgrade(&lock);
					guard.replace(weak);
					Ok(lock)
				}
			}
			None => {
				let lock = self.acquire_lock_with_default("drive-write").await?;
				let weak = Arc::downgrade(&lock);
				guard.replace(weak);
				Ok(lock)
			}
		}
	}
}

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
mod js_impl {
	use std::sync::Arc;

	use serde::Deserialize;
	use tsify::Tsify;
	use wasm_bindgen::prelude::wasm_bindgen;

	use crate::{
		Error,
		auth::Client,
		sync::lock::{self},
	};

	#[wasm_bindgen]
	pub struct ResourceLock(Arc<lock::ResourceLock>);

	#[wasm_bindgen]
	impl ResourceLock {
		/// Utility function to be able to immediately drop the lock from JS
		#[wasm_bindgen]
		pub fn drop(self) {}

		/// The resource this lock is for
		#[wasm_bindgen]
		pub fn resource(&self) -> String {
			self.0.resource().to_string()
		}
	}

	#[derive(Deserialize, Tsify)]
	#[tsify(from_wasm_abi)]
	#[serde(rename_all = "camelCase")]
	pub struct AcquireLockParams {
		resource: String,
		#[serde(default)]
		#[tsify(type = "number")]
		max_sleep_time: Option<u32>,
		#[serde(default)]
		#[tsify(type = "number")]
		attempts: Option<u32>,
	}

	#[wasm_bindgen]
	impl Client {
		#[wasm_bindgen(js_name = "lockDrive")]
		pub async fn lock_drive_js(&self) -> Result<ResourceLock, Error> {
			self.lock_drive().await.map(ResourceLock)
		}

		#[wasm_bindgen(js_name = "acquireLock")]
		pub async fn acquire_lock_js(
			&self,
			params: AcquireLockParams,
		) -> Result<ResourceLock, Error> {
			self.acquire_lock(
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
		}
	}
}
