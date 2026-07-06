use std::cmp::min;
use std::time::Duration;
use std::{borrow::Cow, sync::Arc, time};

use bytes::Bytes;
use filen_types::{api::v3::user::lock::LockType, fs::UuidStr};
use tracing::debug;

use crate::{
	ErrorKind, api,
	auth::{Client, http::AuthClient},
	consts::gateway_url,
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
///
/// The release request is spawned onto the ambient tokio runtime at drop time when one exists,
/// falling back to the runtime the lock was acquired on; if neither is available (or the
/// fallback runtime has shut down), the release is skipped and the server expires the lock on
/// its own after ~30 seconds.
#[derive(Debug, Clone)]
pub struct ResourceLock {
	uuid: UuidStr,
	client: Arc<AuthClient>,
	resource: String,
	// The runtime the lock was acquired on. The refresh task is spawned on it, and Drop —
	// which can run on a thread with no ambient tokio runtime (e.g. a foreign FFI finalizer
	// thread) — falls back to it when there is no ambient runtime to spawn the release on.
	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	handle: Option<tokio::runtime::Handle>,
}

impl PartialEq for ResourceLock {
	fn eq(&self, other: &Self) -> bool {
		self.uuid == other.uuid && self.client == other.client && self.resource == other.resource
	}
}

impl Eq for ResourceLock {}

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
				tracing::warn!("Failed to release lock {resource}");
			}
		}
		Err(e) => {
			tracing::warn!("Failed to release lock {resource}: {e}");
		}
	}
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
fn drop(lock: &mut ResourceLock) {
	// Prefer the ambient runtime: it is alive by definition, while the captured handle's
	// runtime may have shut down since acquisition (spawning there would silently cancel
	// the release).
	let handle = tokio::runtime::Handle::try_current()
		.ok()
		.or_else(|| lock.handle.take());
	let Some(handle) = handle else {
		tracing::warn!(
			"No tokio runtime available to release lock {}, relying on server-side expiry",
			lock.resource
		);
		return;
	};
	let client = lock.client.clone();
	let uuid = lock.uuid;
	let resource = lock.resource.clone();
	// Spawning on a handle whose runtime has shut down does not panic: the task is
	// cancelled without being polled and the server expires the lock on its own.
	handle.spawn(async move { actually_drop(&client, uuid, &resource).await });
}

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
fn drop(lock: &mut ResourceLock) {
	let client = lock.client.clone();
	let uuid = lock.uuid;
	let resource = lock.resource.clone();
	// `runtime::spawn_local` (unlike raw `wasm_bindgen_futures::spawn_local`) holds the
	// spawning SDK worker open until the future completes, so a worker whose other tasks
	// finish cannot close mid-request and silently skip the release. On the main thread it
	// degrades to a plain `spawn_local`.
	crate::runtime::spawn_local(async move {
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

/// How long to sleep before the first refresh, given how much of
/// [`LOCK_REFRESH_INTERVAL`] already elapsed before the refresh task got polled.
fn refresh_delay_after(elapsed: Duration) -> Duration {
	LOCK_REFRESH_INTERVAL.saturating_sub(elapsed)
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
fn keep_lock_alive(lock: &Arc<ResourceLock>) {
	use std::time::Instant;
	use tracing::warn;

	let initial_update = Instant::now();
	let weak = Arc::downgrade(lock);
	let refresh = async move {
		tokio::time::sleep(refresh_delay_after(initial_update.elapsed())).await;
		loop {
			if let Some(lock) = weak.upgrade() {
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
					warn!("Failed to refresh lock: {}", lock.resource);
					return;
				} else {
					debug!("Refreshed lock: {}", lock.resource);
				}
			} else {
				return;
			}
			tokio::time::sleep(LOCK_REFRESH_INTERVAL).await;
		}
	};
	// Same no-ambient-runtime hazard as Drop: spawn on the runtime the lock was acquired
	// on. The ambient fallback is unreachable through `acquire_lock` (reqwest already
	// requires a tokio context there) but keeps behavior identical when no handle was
	// captured.
	match &lock.handle {
		Some(handle) => handle.spawn(refresh),
		None => tokio::spawn(refresh),
	};
}

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
fn keep_lock_alive(lock: &Arc<ResourceLock>) {
	use wasmtimer::std::Instant;

	let initial_update = Instant::now();
	let lock = Arc::downgrade(lock);
	// `runtime::spawn_local` pins the spawning SDK worker open for the life of the refresh
	// loop (it exits when the lock drops or a refresh fails), so the worker cannot close
	// under a held lock and silently stop refreshing it — the server would expire the lock
	// while the client still believes it holds it.
	crate::runtime::spawn_local(async move {
		wasmtimer::tokio::sleep(refresh_delay_after(initial_update.elapsed())).await;
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
					tracing::warn!("Failed to refresh lock: {}", lock.resource);
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
	#[tracing::instrument(
		name = "acquire_lock",
		skip_all,
		fields(resource = tracing::field::Empty, attempts),
	)]
	pub async fn acquire_lock(
		&self,
		resource: impl Into<String>,
		max_sleep_time: time::Duration,
		attempts: usize,
	) -> Result<Arc<ResourceLock>, Error> {
		let resource = resource.into();
		tracing::Span::current().record("resource", resource.as_str());
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
					#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
					handle: tokio::runtime::Handle::try_current().ok(),
				});
				keep_lock_alive(&lock);
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

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn refresh_delay_saturates_when_first_poll_is_late() {
		assert_eq!(
			refresh_delay_after(LOCK_REFRESH_INTERVAL + Duration::from_secs(1)),
			Duration::ZERO
		);
		assert_eq!(
			refresh_delay_after(Duration::from_secs(u64::MAX)),
			Duration::ZERO
		);
	}

	#[test]
	fn refresh_delay_subtracts_elapsed_time() {
		assert_eq!(refresh_delay_after(Duration::ZERO), LOCK_REFRESH_INTERVAL);
		assert_eq!(
			refresh_delay_after(Duration::from_secs(5)),
			LOCK_REFRESH_INTERVAL - Duration::from_secs(5)
		);
	}

	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	fn test_lock(handle: Option<tokio::runtime::Handle>) -> ResourceLock {
		use std::sync::RwLock;

		use filen_types::auth::APIKey;

		use crate::auth::{http::ClientConfig, unauth::UnauthClient};

		let unauthed = UnauthClient::from_config(ClientConfig::default()).unwrap();
		ResourceLock {
			uuid: UuidStr::new_v4(),
			client: Arc::new(AuthClient::from_unauthed(
				unauthed,
				Arc::new(RwLock::new(APIKey(Cow::Borrowed("")))),
			)),
			resource: "test-resource".to_string(),
			handle,
		}
	}

	// The runtimes below are deliberately never driven: a `current_thread` runtime only
	// polls tasks inside `block_on`, so the release task spawned by `Drop` is queued (or
	// cancelled) but never runs — no request ever leaves the process.
	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	#[test]
	fn drop_on_thread_without_runtime_does_not_panic() {
		let rt = tokio::runtime::Builder::new_current_thread()
			.enable_all()
			.build()
			.unwrap();
		let lock = rt.block_on(async { test_lock(tokio::runtime::Handle::try_current().ok()) });
		assert!(lock.handle.is_some());
		std::thread::spawn(move || std::mem::drop(lock))
			.join()
			.unwrap();
	}

	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	#[test]
	fn drop_after_runtime_shutdown_does_not_panic() {
		let rt = tokio::runtime::Builder::new_current_thread()
			.enable_all()
			.build()
			.unwrap();
		let lock = rt.block_on(async { test_lock(tokio::runtime::Handle::try_current().ok()) });
		std::mem::drop(rt);
		std::thread::spawn(move || std::mem::drop(lock))
			.join()
			.unwrap();
	}

	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	#[test]
	fn drop_without_captured_handle_does_not_panic() {
		let rt = tokio::runtime::Builder::new_current_thread()
			.enable_all()
			.build()
			.unwrap();
		let lock = rt.block_on(async { test_lock(None) });
		std::mem::drop(rt);
		std::thread::spawn(move || std::mem::drop(lock))
			.join()
			.unwrap();
	}

	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	#[test]
	fn drop_prefers_ambient_runtime_over_dead_captured_handle() {
		let dead = tokio::runtime::Builder::new_current_thread()
			.enable_all()
			.build()
			.unwrap();
		let lock = dead.block_on(async { test_lock(tokio::runtime::Handle::try_current().ok()) });
		std::mem::drop(dead);
		let ambient = tokio::runtime::Builder::new_current_thread()
			.enable_all()
			.build()
			.unwrap();
		// `enter` makes `ambient` the ambient runtime without driving it.
		let _guard = ambient.enter();
		assert_eq!(ambient.metrics().num_alive_tasks(), 0);
		std::mem::drop(lock);
		// The release task must land on the live ambient runtime; spawning it on the dead
		// captured handle would cancel it silently.
		assert_eq!(ambient.metrics().num_alive_tasks(), 1);
	}

	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	#[test]
	fn keep_lock_alive_without_ambient_runtime_does_not_panic() {
		let rt = tokio::runtime::Builder::new_current_thread()
			.enable_all()
			.build()
			.unwrap();
		let lock =
			rt.block_on(async { Arc::new(test_lock(tokio::runtime::Handle::try_current().ok())) });
		// A plain thread has no ambient runtime, so the refresh task must ride the
		// captured handle.
		std::thread::spawn(move || keep_lock_alive(&lock))
			.join()
			.unwrap();
	}
}
