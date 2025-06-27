use std::{borrow::Cow, sync::Arc, time};

use filen_types::api::v3::user::lock::LockType;
use futures_timer::Delay;
use log::debug;
use uuid::Uuid;

use crate::{
	api,
	auth::{Client, http::AuthClient},
	error::Error,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceLock {
	uuid: Uuid,
	client: Arc<AuthClient>,
	resource: String,
}

impl ResourceLock {
	pub fn resource(&self) -> &str {
		&self.resource
	}
}

async fn actually_drop(client: &AuthClient, uuid: Uuid, resource: &str) {
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
			debug!("Released lock {}: {}", resource, uuid);
			if !response.released {
				eprintln!("Failed to release lock {}", resource);
			}
		}
		Err(e) => {
			eprintln!("Failed to release lock {}: {}", resource, e);
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

impl Client {
	async fn try_acquire_lock(&self, resource: &str, uuid: Uuid) -> Result<bool, Error> {
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
	) -> Result<ResourceLock, Error> {
		let resource = resource.into();
		let uuid = Uuid::new_v4();
		for attempt in 1..=attempts {
			match self.try_acquire_lock(&resource, uuid).await {
				Ok(false) => {}
				Ok(true) => {
					debug!("Acquired lock {}: {}", resource, uuid);
					return Ok(ResourceLock {
						uuid,
						client: self.arc_client(),
						resource,
					});
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
	) -> Result<ResourceLock, Error> {
		self.acquire_lock(resource, time::Duration::from_secs(1), 86400)
			.await
	}
}
