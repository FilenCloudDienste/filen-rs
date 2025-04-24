use std::{borrow::Cow, time};

use filen_types::api::v3::user::lock::LockType;
use futures_timer::Delay;
use uuid::Uuid;

use crate::{api, auth::Client, error::Error};

pub struct ResourceLock<'a> {
	uuid: Uuid,
	client: &'a Client,
	resource: String,
}

impl ResourceLock<'_> {
	pub fn resource(&self) -> &str {
		&self.resource
	}
}

impl Drop for ResourceLock<'_> {
	// async drop is not supported in Rust
	// so we need to use a blocking executor
	fn drop(&mut self) {
		futures::executor::block_on(async move {
			match api::v3::user::lock::post(
				self.client.client(),
				&api::v3::user::lock::Request {
					uuid: self.uuid,
					r#type: LockType::Release,
					resource: Cow::Borrowed(&self.resource),
				},
			)
			.await
			{
				Ok(response) => {
					if !response.released {
						eprintln!("Failed to release lock {}", self.resource);
					}
				}
				Err(e) => {
					eprintln!("Failed to release lock {}: {}", self.resource, e);
				}
			}
		})
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
	) -> Result<ResourceLock<'_>, Error> {
		let resource = resource.into();
		let uuid = Uuid::new_v4();
		for attempt in 1..=attempts {
			match self.try_acquire_lock(&resource, uuid).await {
				Ok(false) => {}
				Ok(true) => {
					return Ok(ResourceLock {
						uuid,
						client: self,
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
			"Failed to acquire lock after {} attempts",
			attempts
		)))
	}

	pub async fn acquire_lock_with_default(
		&self,
		resource: impl Into<String>,
	) -> Result<ResourceLock<'_>, Error> {
		self.acquire_lock(resource, time::Duration::from_secs(1), 86400)
			.await
	}
}
