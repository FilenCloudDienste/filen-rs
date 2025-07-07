use std::sync::Weak;
use std::{borrow::Cow, sync::Arc, time};

use filen_types::{api::v3::user::lock::LockType, fs::UuidStr};
use futures_timer::Delay;
use log::debug;

use crate::{
	api,
	auth::{Client, http::AuthClient},
	error::Error,
};

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

#[cfg(not(feature = "tokio"))]
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

#[cfg(feature = "tokio")]
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

#[cfg(not(feature = "tokio"))]
fn keep_lock_alive(_lock: Weak<ResourceLock>) {
	use log::warn;
	warn!(
		"Keep-alive for locks is not supported in non-tokio builds. The lock will not be refreshed automatically and will time out in 30 seconds."
	);
}

impl Client {
	async fn try_acquire_lock(&self, resource: &str, uuid: UuidStr) -> Result<bool, Error> {
		let response = api::v3::user::lock::post(
			self.client(),
			&api::v3::user::lock::Request {
				uuid,
				r#type: LockType::Acquire,
				resource: Cow::Borrowed(resource),
			},
		)
		.await?;
		Ok(response.acquired)
	}

	pub async fn acquire_lock(
		&self,
		resource: impl Into<String>,
		sleep_time: time::Duration,
		attempts: usize,
	) -> Result<Arc<ResourceLock>, Error> {
		let resource = resource.into();
		let uuid = UuidStr::new_v4();
		for attempt in 1..=attempts {
			match self.try_acquire_lock(&resource, uuid).await {
				Ok(false) => {}
				Ok(true) => {
					debug!("Acquired lock {resource}: {uuid}");
					let lock = Arc::new(ResourceLock {
						uuid,
						client: self.arc_client(),
						resource,
					});
					let weak_lock = Arc::downgrade(&lock);
					keep_lock_alive(weak_lock);
					return Ok(lock);
				}
				Err(e) => return Err(e),
			}
			if attempt < attempts {
				Delay::new(sleep_time).await;
			}
		}
		Err(Error::Custom(format!(
			"Failed to acquire lock after {attempts} attempts"
		)))
	}

	pub async fn acquire_lock_with_default(
		&self,
		resource: impl Into<String>,
	) -> Result<Arc<ResourceLock>, Error> {
		self.acquire_lock(resource, time::Duration::from_secs(1), 86400)
			.await
	}
}
