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
