use std::cmp::min;
use std::sync::Weak;
use std::time::Duration;
use std::{borrow::Cow, sync::Arc, time};

use bytes::Bytes;
use filen_types::{api::v3::user::lock::LockType, fs::UuidStr};
use log::debug;

use crate::ErrorKind;
// use crate::api::{RetryError, retry_wrap};
use crate::auth::http::AuthorizedClient;
use crate::consts::gateway_url;
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

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
fn drop(lock: &mut ResourceLock) {
	let client = lock.client.clone();
	let uuid = lock.uuid;
	let resource = lock.resource.clone();
	tokio::spawn(async move { actually_drop(&client, uuid, &resource).await });
}

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
fn drop(lock: &mut ResourceLock) {
	let client = lock.client.clone();
	let uuid = lock.uuid;
	let resource = lock.resource.clone();
	wasm_bindgen_futures::spawn_local(async move {
		actually_drop(&client, uuid, &resource).await;
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

const LOCK_REFRESH_INTERVAL: time::Duration = time::Duration::from_secs(15);

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
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

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
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

pub(crate) const DEFAULT_NUM_RETRIES: usize = 7;
pub(crate) const DEFAULT_MAX_RETRY_TIME: Duration = Duration::from_secs(30);

fn fibonacci_iter(max_retry_time: Duration) -> impl Iterator<Item = Duration> {
	std::iter::successors(
		Some((
			max_retry_time,
			Duration::from_secs(0),
			Duration::from_millis(250),
		)),
		|&(max, a, b)| Some((max, b, min(max, a + b))),
	)
	.map(|(_, a, _)| a)
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
		let url = gateway_url(api::v3::user::lock::ENDPOINT);
		let endpoint = api::v3::user::lock::ENDPOINT;
		for (i, delay) in (0..attempts).zip(fibonacci_iter(max_sleep_time)) {
			let resp = self
				.arc_client()
				.post_raw_bytes_auth::<api::v3::user::lock::Response>(
					bytes.clone(),
					&url,
					endpoint.into(),
				)
				.await?;

			if !resp.acquired {
				debug!(
					"Attempt {}/{}: Failed to acquire lock on resource: {}. Retrying in {:?}",
					i + 1,
					attempts,
					resource,
					delay
				);
				#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
				tokio::time::sleep(delay).await;
				#[cfg(all(target_family = "wasm", target_os = "unknown"))]
				wasmtimer::tokio::sleep(delay).await
			} else {
				let lock = Arc::new(ResourceLock {
					uuid,
					client: self.arc_client(),
					resource,
				});
				keep_lock_alive(Arc::downgrade(&lock));
				return Ok(lock);
			}
		}

		Err(Error::custom(
			ErrorKind::RetryFailed,
			format!(
				"Failed to acquire lock on resource '{}' after {attempts} attempts",
				resource
			),
		))
	}

	pub async fn acquire_lock_with_default(
		&self,
		resource: impl Into<String>,
	) -> Result<Arc<ResourceLock>, Error> {
		self.acquire_lock(resource, MAX_SLEEP_TIME_DEFAULT, ATTEMPTS_DEFAULT)
			.await
	}
}
