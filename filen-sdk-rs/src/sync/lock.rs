use std::sync::Weak;
use std::{borrow::Cow, sync::Arc, time};

use bytes::Bytes;
use filen_types::api::response::FilenResponse;
use filen_types::{api::v3::user::lock::LockType, fs::UuidStr};
use log::debug;

use crate::ErrorKind;
use crate::api::{RetryError, retry_wrap};
use crate::auth::http::AuthorizedClient;
use crate::consts::gateway_url;
use crate::error::ErrorExt;
use crate::{
	api,
	auth::{Client, http::AuthClient},
	error::Error,
};

pub(crate) const MAX_SLEEP_TIME_DEFAULT: time::Duration = time::Duration::from_secs(30);
pub(crate) const ATTEMPTS_DEFAULT: usize = 8640; // 8640

/// Represents a lock on a resource, which can be acquired using the [`Client::acquire_lock`] method.
/// The lock is released when the [`ResourceLock`] is dropped.
///
/// While the lock is held, no other client can acquire the lock on the same resource.
/// The lock is automatically released after 30 seconds server side,
/// but it is refreshed every [`LOCK_REFRESH_INTERVAL`] seconds if the feature `tokio` is enabled.
///
/// It is important to keep in mind that the lock can be dropped due to network issues or other errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceLock {
	uuid: UuidStr,
	client: Arc<AuthClient>,
	resource: String,
}

impl ResourceLock {
	pub fn resource(&self) -> &str {
		&self.resource
	}
}

async fn actually_drop(client: &AuthClient, uuid: UuidStr, resource: &str) {
	match api::v3::user::lock::post(
		client,
		&api::v3::user::lock::Request {
			uuid,
			r#type: LockType::Release,
			resource: Cow::Borrowed(resource),
		},
	)
	.await
	{
		Ok(response) => {
			debug!("Released lock {resource}: {uuid}");
			if !response.released {
				eprintln!("Failed to release lock {resource}");
			}
		}
		Err(e) => {
			eprintln!("Failed to release lock {resource}: {e}");
		}
	}
}

#[cfg(feature = "tokio")]
fn drop(lock: &mut ResourceLock) {
	let client = lock.client.clone();
	let uuid = lock.uuid;
	let resource = lock.resource.clone();
	tokio::spawn(async move { actually_drop(&client, uuid, &resource).await });
}

#[cfg(target_arch = "wasm32")]
fn drop(lock: &mut ResourceLock) {
	let client = lock.client.clone();
	let uuid = lock.uuid;
	let resource = lock.resource.clone();
	wasm_bindgen_futures::spawn_local(async move {
		actually_drop(&client, uuid, &resource).await;
	});
}

#[cfg(not(any(feature = "tokio", target_arch = "wasm32")))]
fn drop(lock: &mut ResourceLock) {
	futures::executor::block_on(async move {
		actually_drop(&lock.client, lock.uuid, &lock.resource).await
	});
}

impl Drop for ResourceLock {
	// async drop is not supported in Rust
	// so we need to use a blocking executor
	// or a tokio spawn
	fn drop(&mut self) {
		drop(self);
	}
}

#[cfg(any(all(target_arch = "wasm32", target_os = "unknown"), feature = "tokio"))]
const LOCK_REFRESH_INTERVAL: time::Duration = time::Duration::from_secs(15);

#[cfg(feature = "tokio")]
fn keep_lock_alive(lock: Weak<ResourceLock>) {
	use log::error;
	use std::time::Instant;

	let initial_update = Instant::now();
	tokio::spawn(async move {
		tokio::time::sleep(LOCK_REFRESH_INTERVAL - (Instant::now() - initial_update)).await;
		loop {
			if let Some(lock) = lock.upgrade() {
				let good_response = match api::v3::user::lock::post(
					lock.client.as_ref(),
					&api::v3::user::lock::Request {
						uuid: lock.uuid,
						r#type: LockType::Refresh,
						resource: Cow::Borrowed(&lock.resource),
					},
				)
				.await
				{
					Ok(r) => r.refreshed,
					Err(_) => false,
				};

				if !good_response {
					error!("Failed to refresh lock: {}", lock.resource);
					return;
				} else {
					debug!("Refreshed lock: {}", lock.resource);
				}
			} else {
				return;
			}
			tokio::time::sleep(LOCK_REFRESH_INTERVAL).await;
		}
	});
}

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
fn keep_lock_alive(lock: Weak<ResourceLock>) {
	use wasmtimer::std::Instant;

	let initial_update = Instant::now();
	wasm_bindgen_futures::spawn_local(async move {
		wasmtimer::tokio::sleep(LOCK_REFRESH_INTERVAL - (Instant::now() - initial_update)).await;
		loop {
			if let Some(lock) = lock.upgrade() {
				let good_response = match api::v3::user::lock::post(
					lock.client.as_ref(),
					&api::v3::user::lock::Request {
						uuid: lock.uuid,
						r#type: LockType::Refresh,
						resource: Cow::Borrowed(&lock.resource),
					},
				)
				.await
				{
					Ok(r) => r.refreshed,
					Err(_) => false,
				};

				if !good_response {
					log::error!("Failed to refresh lock: {}", lock.resource);
					return;
				} else {
					debug!("Refreshed lock: {}", lock.resource);
				}
			} else {
				return;
			}
			wasmtimer::tokio::sleep(LOCK_REFRESH_INTERVAL).await;
		}
	});
}

#[cfg(not(any(all(target_arch = "wasm32", target_os = "unknown"), feature = "tokio")))]
fn keep_lock_alive(_lock: Weak<ResourceLock>) {
	use log::warn;
	warn!(
		"Keep-alive for locks is not supported in non-tokio builds. The lock will not be refreshed automatically and will time out in 30 seconds."
	);
}

impl Client {
	/// Attempts to acquire a lock on the specified resource.
	/// If the lock is acquired, it returns a [`ResourceLock`] that releases the lock when dropped.
	pub async fn acquire_lock(
		&self,
		resource: impl Into<String>,
		max_sleep_time: time::Duration,
		attempts: usize,
	) -> Result<Arc<ResourceLock>, Error> {
		let resource = resource.into();
		let uuid = UuidStr::new_v4();
		let bytes = Bytes::from_owner(serde_json::to_vec(&api::v3::user::lock::Request {
			uuid,
			r#type: LockType::Acquire,
			resource: Cow::Borrowed(&resource),
		})?);
		let endpoint = api::v3::user::lock::ENDPOINT;
		debug!("Acquiring lock on resource: {resource} with uuid: {uuid}");
		retry_wrap(
			bytes,
			|| {
				self.client()
					.post_auth_request(gateway_url(endpoint))
					.header(
						reqwest::header::CONTENT_TYPE,
						reqwest::header::HeaderValue::from_static("application/json"),
					)
			},
			endpoint,
			async |resp| {
				let body = match resp
					.json::<FilenResponse<api::v3::user::lock::Response>>()
					.await
				{
					Ok(body) => body,
					Err(e) => {
						log::error!("Failed to parse response from {endpoint}: {e}");
						return Err(RetryError::NoRetry(e.with_context(endpoint)));
					}
				};

				let resp = body
					.into_data()
					.map_err(|e| RetryError::NoRetry(e.with_context(endpoint)))?;

				if resp.acquired {
					let lock = Arc::new(ResourceLock {
						uuid,
						client: self.arc_client(),
						resource: resource.clone(),
					});
					keep_lock_alive(Arc::downgrade(&lock));
					Ok(lock)
				} else {
					Err(RetryError::Retry(Error::custom(
						ErrorKind::Server,
						format!("Failed to acquire lock: {}", resp.resource),
					)))
				}
			},
			attempts,
			max_sleep_time,
		)
		.await
	}

	pub async fn acquire_lock_with_default(
		&self,
		resource: impl Into<String>,
	) -> Result<Arc<ResourceLock>, Error> {
		self.acquire_lock(resource, MAX_SLEEP_TIME_DEFAULT, ATTEMPTS_DEFAULT)
			.await
	}
}
