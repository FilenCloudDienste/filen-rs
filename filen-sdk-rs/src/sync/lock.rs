use std::cmp::min;
use std::future::Future;
use std::time::Duration;
use std::{
	borrow::Cow,
	sync::{Arc, Weak},
	time,
};

use bytes::Bytes;
use filen_types::{api::v3::user::lock::LockType, fs::Uuid};
use tracing::debug;

// The keep-alive schedule must use the same clock as the async timers so that the
// paused-clock unit tests (and wasm) stay coherent: tokio's Instant on native,
// wasmtimer's on wasm.
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
use tokio::time::{Instant, sleep, timeout};
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
use wasmtimer::{
	std::Instant,
	tokio::{sleep, timeout},
};

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
/// The lock is automatically released [`LOCK_SERVER_TTL`] after the last refresh the
/// server processed, so a background task refreshes it every
/// [`LOCK_REFRESH_INTERVAL`], retrying failed refreshes within [`LOCK_LOSS_BUDGET`].
///
/// If the server refuses a refresh, or none could be confirmed within the budget
/// (network outage, system suspend, ...), the lock is marked LOST: [`Self::is_valid`]
/// turns false, [`Self::wait_for_loss`] resolves, and the cached [`Client::lock_drive`]
/// family stops handing it out — another client may hold the resource from that point
/// on, so long-running holders should watch for loss rather than assume exclusivity.
///
/// The release request is spawned onto the ambient tokio runtime at drop time when one exists,
/// falling back to the runtime the lock was acquired on; if neither is available (or the
/// fallback runtime has shut down), the release is skipped and the server expires the lock on
/// its own after ~60 seconds.
#[derive(Debug, Clone)]
pub struct ResourceLock {
	uuid: Uuid,
	client: Arc<AuthClient>,
	resource: String,
	// Flipped to false (never back) by the keep-alive task once the server-side
	// lease can no longer be trusted. See [`Self::is_valid`].
	valid: tokio::sync::watch::Sender<bool>,
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

	/// Whether the background keep-alive still holds this lock's server-side lease.
	///
	/// `false` means the server refused a refresh or none was confirmed within
	/// [`LOCK_LOSS_BUDGET`]: the server has released (or is about to release) the
	/// resource and another client may already hold it. Holders should stop
	/// relying on the lock and re-acquire. Never flips back to `true`.
	pub fn is_valid(&self) -> bool {
		*self.valid.borrow()
	}

	/// Resolves once the server-side lease is lost ([`Self::is_valid`] turns
	/// false); resolves immediately if it already is. Pends indefinitely while
	/// the lock stays healthy, so callers should race it against their work.
	pub async fn wait_for_loss(&self) {
		let mut receiver = self.valid.subscribe();
		// The sender is a field of `self`, so it cannot close while borrowed
		// here; a closed channel would mean the lock is gone anyway.
		let _ = receiver.wait_for(|valid| !*valid).await;
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

// The release is attempted even for a lock marked invalid: if the loss was a false
// positive (the server still holds our lease), releasing frees the resource for
// everyone immediately instead of stalling it out to the TTL, and if the lease is
// truly gone the Release is a harmless no-op keyed to our uuid.
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
/// Server-side lease for a held lock: the server auto-releases a lock 60s after the last
/// acquire/refresh it processed. Measured against the live API: another client got a resource
/// 60s after the holder's acquire, and 90s after an acquire refreshed at 30s.
const LOCK_SERVER_TTL: time::Duration = time::Duration::from_secs(60);
/// How long past the last server-confirmed refresh the keep-alive keeps trusting
/// (and retrying a failing refresh of) a held lock before declaring it lost. The
/// margin below [`LOCK_SERVER_TTL`] absorbs the send-to-server-processing skew, so
/// the client stops trusting the lock strictly before the server can hand the
/// resource to another client.
const LOCK_LOSS_BUDGET: time::Duration = time::Duration::from_secs(25);
/// Delay between retries of a failed refresh within [`LOCK_LOSS_BUDGET`].
const LOCK_RETRY_INTERVAL: time::Duration = time::Duration::from_secs(1);

/// How long to sleep before the first refresh, given how much of
/// [`LOCK_REFRESH_INTERVAL`] already elapsed before the refresh task got polled.
fn refresh_delay_after(elapsed: Duration) -> Duration {
	LOCK_REFRESH_INTERVAL.saturating_sub(elapsed)
}

/// Why a [`refresh_loop`] ended.
#[derive(Debug, PartialEq, Eq)]
enum LoopExit {
	/// Every strong reference to the lock was dropped; Drop handles the release.
	Dropped,
	/// The server refused a refresh, or none was confirmed within
	/// [`LOCK_LOSS_BUDGET`] — the server-side lease can no longer be trusted.
	Lost,
}

/// Drives the refresh schedule for one held lock until it is dropped or lost.
///
/// `refresh` builds the future for a single Refresh round-trip (resolving to the
/// server's `refreshed` flag), or returns `None` once the lock has been dropped.
/// Generic over the refresh action so the schedule and loss budget are
/// unit-testable against a scripted server on a paused clock.
async fn refresh_loop<F, Fut>(mut refresh: F, acquired_at: Instant) -> LoopExit
where
	F: FnMut() -> Option<Fut>,
	Fut: Future<Output = Result<bool, Error>>,
{
	// The request that most recently confirmed the lock server-side, timed from
	// when it was SENT: the server's lease clock restarts when it processes a
	// request, never earlier than its send, so this measurement only ever
	// overestimates the lease age — erring toward declaring loss early, not late.
	let mut last_confirmed = acquired_at;
	let mut attempted = false;
	sleep(refresh_delay_after(acquired_at.elapsed())).await;
	loop {
		let mut remaining = LOCK_LOSS_BUDGET.saturating_sub(last_confirmed.elapsed());
		if remaining.is_zero() {
			if attempted {
				return LoopExit::Lost;
			}
			// The acquire anchor can already be stale before any refresh was ever
			// attempted (its response may arrive long after the send under the
			// shared retry/rate-limit HTTP stack). Give the server one bounded
			// chance to confirm instead of declaring a fresh lock lost unheard:
			// budget + this floor never exceeds LOCK_SERVER_TTL, so trust still
			// ends no later than the lease possibly can.
			remaining = LOCK_SERVER_TTL - LOCK_LOSS_BUDGET;
		}
		let Some(fut) = refresh() else {
			return LoopExit::Dropped;
		};
		attempted = true;
		let attempt_started = Instant::now();
		match timeout(remaining, fut).await {
			Ok(Ok(true)) => {
				last_confirmed = attempt_started;
				sleep(refresh_delay_after(last_confirmed.elapsed())).await;
			}
			// The server no longer recognizes the lock — authoritative loss.
			Ok(Ok(false)) => return LoopExit::Lost,
			// Transport error: outcome unknown, retry while budget remains.
			Ok(Err(_)) => sleep(LOCK_RETRY_INTERVAL).await,
			// The attempt outlived the budget; the next iteration declares loss.
			Err(_) => {}
		}
	}
}

async fn run_keep_alive(weak: Weak<ResourceLock>, acquired_at: Instant) {
	let refresh = || {
		let lock = weak.upgrade()?;
		Some(async move {
			let result = api::v3::user::lock::post(
				lock.client.as_ref(),
				&api::v3::user::lock::Request {
					uuid: lock.uuid,
					r#type: LockType::Refresh,
					resource: Cow::Borrowed(&lock.resource),
				},
			)
			.await
			.map(|r| r.refreshed);
			match &result {
				Ok(true) => debug!("Refreshed lock: {}", lock.resource),
				Ok(false) => tracing::warn!("Server refused to refresh lock: {}", lock.resource),
				Err(e) => tracing::warn!("Failed to refresh lock {}: {e}", lock.resource),
			}
			result
		})
	};
	if refresh_loop(refresh, acquired_at).await == LoopExit::Lost
		&& let Some(lock) = weak.upgrade()
	{
		tracing::warn!(
			"Lost lock '{}': marking it invalid, the server releases it on its own ~{}s after the last confirmed refresh",
			lock.resource,
			LOCK_SERVER_TTL.as_secs(),
		);
		lock.valid.send_replace(false);
	}
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
fn keep_lock_alive(lock: &Arc<ResourceLock>, acquired_at: Instant) {
	let refresh = run_keep_alive(Arc::downgrade(lock), acquired_at);
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
fn keep_lock_alive(lock: &Arc<ResourceLock>, acquired_at: Instant) {
	// `runtime::spawn_local` pins the spawning SDK worker open for the life of the refresh
	// loop (it exits when the lock drops or is lost), so the worker cannot close
	// under a held lock and silently stop refreshing it — the server would expire the lock
	// while the client still believes it holds it.
	crate::runtime::spawn_local(run_keep_alive(Arc::downgrade(lock), acquired_at));
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

/// How many failed requests in a row [`acquire_loop`] waits out once the server has answered
/// one of its polls. Each is followed by that attempt's sleep, so the outage this rides out
/// grows with the schedule: about a second right after the first answer, up to three times
/// `max_sleep_time` late in a long wait. A count rather than a time budget like
/// [`LOCK_LOSS_BUDGET`]: no lease is at stake while waiting, and a device that went offline
/// mid-wait still gets its error after a few polls instead of after the whole schedule.
const ACQUIRE_TRANSPORT_ERROR_LIMIT: usize = 3;

/// Polls for the lock up to `attempts` times on [`fibonacci_iter`]'s schedule, sleeping after
/// every poll that does not get it. Returns the send time of the granting poll, or
/// [`ErrorKind::RetryFailed`] once the attempts run out.
///
/// `poll` builds the future for a single Acquire round-trip, resolving to the server's
/// `acquired` flag. Generic over it so the schedule is unit-testable against a scripted server
/// on a paused clock.
///
/// A failed request ([`ErrorKind::Reqwest`]: no response, a timeout, or an HTTP error status,
/// 5xx/408/429 only once the retry layer gave up on them) is returned at once while no poll has
/// been answered, so an offline caller fails fast. After an answer, up to
/// [`ACQUIRE_TRANSPORT_ERROR_LIMIT`] of them in a row are waited out like a refusal, each using
/// up an attempt: a wait that can last hours must not be lost to one network blip. Natively a
/// stalled request can take the whole read timeout (300 s by default) each time. Any other error
/// is returned as is.
async fn acquire_loop<F, Fut>(
	mut poll: F,
	resource: &str,
	max_sleep_time: Duration,
	attempts: usize,
) -> Result<Instant, Error>
where
	F: FnMut() -> Fut,
	Fut: Future<Output = Result<bool, Error>>,
{
	let mut answered = false;
	let mut transport_errors = 0;
	for (i, delay) in (0..attempts).zip(fibonacci_iter(max_sleep_time)) {
		// The server's lease clock starts when it processes this request, no
		// earlier than now — the refresh schedule is anchored to the send time.
		let attempt_started = Instant::now();
		match poll().await {
			// Always a fresh lease, even after a failed poll the server granted with only the
			// answer lost: the server refuses an Acquire repeated with the holder's own uuid
			// (without restarting its lease) until that lease lapses.
			Ok(true) => return Ok(attempt_started),
			Ok(false) => {
				answered = true;
				transport_errors = 0;
				debug!(
					"Attempt {}/{}: Failed to acquire lock on resource: {}. Retrying in {:?}",
					i + 1,
					attempts,
					resource,
					delay
				);
			}
			Err(e)
				if answered
					&& e.kind() == ErrorKind::Reqwest
					&& transport_errors < ACQUIRE_TRANSPORT_ERROR_LIMIT =>
			{
				transport_errors += 1;
				tracing::warn!(
					"Attempt {}/{}: lock request for resource {resource} failed, retrying in {delay:?}: {e}",
					i + 1,
					attempts,
				);
			}
			Err(e) => return Err(e),
		}
		sleep(delay).await;
	}

	Err(Error::custom(
		ErrorKind::RetryFailed,
		format!("Failed to acquire lock on resource '{resource}' after {attempts} attempts"),
	))
}

impl Client {
	/// Attempts to acquire a lock on the specified resource.
	/// If the lock is acquired, it returns a [`ResourceLock`] that releases the lock when dropped.
	///
	/// Polls up to `attempts` times, sleeping after each poll that does not get the lock on a
	/// schedule that grows to `max_sleep_time`, and fails with [`ErrorKind::RetryFailed`] when
	/// the attempts run out. An error on the first poll is returned at once; once the server has
	/// answered, a few failed requests in a row (no response, or an HTTP error status) are
	/// waited out like refusals.
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
		let uuid = Uuid::new_v4();
		let bytes = Bytes::from_owner(serde_json::to_vec(&api::v3::user::lock::Request {
			uuid,
			r#type: LockType::Acquire,
			resource: Cow::Borrowed(&resource),
		})?);
		let url = gateway_url(api::v3::user::lock::ENDPOINT);
		let endpoint = api::v3::user::lock::ENDPOINT;
		let client = self.arc_client();
		let (client_ref, bytes, url) = (&client, &bytes, url.as_str());
		let acquired_at = acquire_loop(
			move || async move {
				client_ref
					.post_raw_bytes_auth::<api::v3::user::lock::Response>(
						// refcount bump: every poll resends the same Acquire body
						bytes.clone(),
						url,
						endpoint.into(),
					)
					.await
					.map(|resp| resp.acquired)
			},
			&resource,
			max_sleep_time,
			attempts,
		)
		.await?;
		let lock = Arc::new(ResourceLock {
			uuid,
			client,
			resource,
			valid: tokio::sync::watch::Sender::new(true),
			#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
			handle: tokio::runtime::Handle::try_current().ok(),
		});
		keep_lock_alive(&lock, acquired_at);
		Ok(lock)
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
			uuid: Uuid::new_v4(),
			client: Arc::new(AuthClient::from_unauthed(
				unauthed,
				Arc::new(RwLock::new(APIKey(Cow::Borrowed("")))),
			)),
			resource: "test-resource".to_string(),
			valid: tokio::sync::watch::Sender::new(true),
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
		std::thread::spawn(move || keep_lock_alive(&lock, Instant::now()))
			.join()
			.unwrap();
	}

	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	#[test]
	fn drop_of_lost_lock_still_attempts_release() {
		let rt = tokio::runtime::Builder::new_current_thread()
			.enable_all()
			.build()
			.unwrap();
		let lock = rt.block_on(async { test_lock(tokio::runtime::Handle::try_current().ok()) });
		lock.valid.send_replace(false);
		let _guard = rt.enter();
		assert_eq!(rt.metrics().num_alive_tasks(), 0);
		std::mem::drop(lock);
		// A false-positive loss must not strand the resource until the TTL: the
		// release is attempted anyway (harmless no-op if the lease is truly gone).
		assert_eq!(rt.metrics().num_alive_tasks(), 1);
	}

	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	#[tokio::test]
	async fn loss_signal_flips_is_valid_and_wakes_waiters() {
		let lock = test_lock(None);
		assert!(lock.is_valid());
		let wait = lock.wait_for_loss();
		tokio::pin!(wait);
		assert!(futures::poll!(wait.as_mut()).is_pending());
		lock.valid.send_replace(false);
		assert!(!lock.is_valid());
		wait.await;
		// resolves immediately when already lost
		lock.wait_for_loss().await;
	}

	/// One scripted outcome for a [`refresh_loop`] refresh attempt.
	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	#[derive(Clone, Copy)]
	enum Step {
		/// server confirms instantly (refreshed: true)
		Confirm,
		/// server confirms after this many seconds
		SlowConfirm(u64),
		/// server answers refreshed: false
		Refuse,
		/// transport-level failure, instantly
		TransportErr,
		/// the request never completes
		Hang,
		/// the lock was dropped before this attempt
		Drop,
	}

	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	type ScriptedLockFut = std::pin::Pin<Box<dyn Future<Output = Result<bool, Error>> + Send>>;

	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	fn scripted_refresh(steps: &[Step]) -> impl FnMut() -> Option<ScriptedLockFut> {
		let mut steps = std::collections::VecDeque::from(steps.to_vec());
		move || {
			let step = steps
				.pop_front()
				.expect("refresh called more often than scripted");
			match step {
				Step::Drop => None,
				Step::Confirm => Some(Box::pin(async { Ok(true) })),
				Step::Refuse => Some(Box::pin(async { Ok(false) })),
				Step::TransportErr => Some(Box::pin(async {
					Err(Error::custom(
						crate::ErrorKind::Server,
						"scripted refresh failure",
					))
				})),
				Step::Hang => Some(Box::pin(std::future::pending())),
				Step::SlowConfirm(secs) => Some(Box::pin(async move {
					tokio::time::sleep(Duration::from_secs(secs)).await;
					Ok(true)
				})),
			}
		}
	}

	// All timings below are exact: the paused tokio clock only advances by the
	// timers the loop itself registers.

	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	#[tokio::test(start_paused = true)]
	async fn lock_drop_ends_refresh_loop_silently() {
		let begin = Instant::now();
		let exit = refresh_loop(scripted_refresh(&[Step::Confirm, Step::Drop]), begin).await;
		assert_eq!(exit, LoopExit::Dropped);
		// confirm at 15s, drop observed at the 30s refresh
		assert_eq!(begin.elapsed(), Duration::from_secs(30));
	}

	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	#[tokio::test(start_paused = true)]
	async fn server_refusal_is_authoritative_loss() {
		let begin = Instant::now();
		let exit = refresh_loop(scripted_refresh(&[Step::Refuse]), begin).await;
		assert_eq!(exit, LoopExit::Lost);
		assert_eq!(begin.elapsed(), Duration::from_secs(15));
	}

	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	#[tokio::test(start_paused = true)]
	async fn transient_refresh_error_retries_and_recovers() {
		let begin = Instant::now();
		let exit = refresh_loop(
			scripted_refresh(&[Step::TransportErr, Step::Confirm, Step::Confirm, Step::Drop]),
			begin,
		)
		.await;
		// The old keep-alive died forever on the first transport error; the loop
		// must instead retry within the budget and resume the normal cadence.
		assert_eq!(exit, LoopExit::Dropped);
		// err at 15s, retry confirms at 16s, confirm at 31s, drop observed at 46s
		assert_eq!(begin.elapsed(), Duration::from_secs(46));
	}

	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	#[tokio::test(start_paused = true)]
	async fn persistent_refresh_errors_exhaust_loss_budget() {
		let begin = Instant::now();
		// attempts at 15s..=24s, budget exhausted at 25s — exactly 10 attempts
		let exit = refresh_loop(scripted_refresh(&[Step::TransportErr; 10]), begin).await;
		assert_eq!(exit, LoopExit::Lost);
		assert_eq!(begin.elapsed(), LOCK_LOSS_BUDGET);
	}

	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	#[tokio::test(start_paused = true)]
	async fn hung_refresh_is_cut_off_at_the_loss_budget() {
		let begin = Instant::now();
		let exit = refresh_loop(scripted_refresh(&[Step::Hang]), begin).await;
		assert_eq!(exit, LoopExit::Lost);
		// declared lost strictly before the server TTL despite the hang
		assert_eq!(begin.elapsed(), LOCK_LOSS_BUDGET);
	}

	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	#[tokio::test(start_paused = true)]
	async fn stale_acquire_anchor_gets_one_bounded_refresh_attempt() {
		// The acquire response can arrive long after its send (retry/rate-limit
		// stack), leaving the anchor past the loss budget before the keep-alive
		// ever ran. One confirming refresh must rescue the lock, not lose it unheard.
		let begin = Instant::now();
		tokio::time::advance(Duration::from_secs(26)).await;
		let exit = refresh_loop(scripted_refresh(&[Step::Confirm, Step::Drop]), begin).await;
		assert_eq!(exit, LoopExit::Dropped);
		// floored attempt confirms at 26s, drop observed at the 41s refresh
		assert_eq!(begin.elapsed(), Duration::from_secs(41));
	}

	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	#[tokio::test(start_paused = true)]
	async fn stale_acquire_anchor_failed_attempt_is_lost() {
		let begin = Instant::now();
		tokio::time::advance(Duration::from_secs(26)).await;
		let exit = refresh_loop(scripted_refresh(&[Step::TransportErr]), begin).await;
		assert_eq!(exit, LoopExit::Lost);
		// the single floored attempt fails at 26s; after the 1s retry pause the
		// exhausted budget declares the loss — no unbounded extension of trust
		assert_eq!(begin.elapsed(), Duration::from_secs(27));
	}

	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	#[tokio::test(start_paused = true)]
	async fn stale_acquire_anchor_hung_attempt_is_cut_off_at_the_floor() {
		let begin = Instant::now();
		tokio::time::advance(Duration::from_secs(26)).await;
		let exit = refresh_loop(scripted_refresh(&[Step::Hang]), begin).await;
		assert_eq!(exit, LoopExit::Lost);
		// the floored attempt gets exactly LOCK_SERVER_TTL - LOCK_LOSS_BUDGET, 35s
		assert_eq!(begin.elapsed(), Duration::from_secs(61));
	}

	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	#[tokio::test(start_paused = true)]
	async fn slow_confirm_keeps_cadence_anchored_to_send_time() {
		let begin = Instant::now();
		let exit = refresh_loop(scripted_refresh(&[Step::SlowConfirm(5), Step::Drop]), begin).await;
		assert_eq!(exit, LoopExit::Dropped);
		// sent at 15s, confirmed at 20s: the next refresh fires 15s after the
		// SEND (at 30s), not 15s after the response (35s) — a slow round-trip
		// must not stretch the cadence past the server TTL
		assert_eq!(begin.elapsed(), Duration::from_secs(30));
	}

	/// One scripted outcome for an [`acquire_loop`] poll.
	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	#[derive(Clone, Copy)]
	enum AcquireStep {
		/// server answers acquired: true
		Grant,
		/// server answers acquired: true after this many seconds
		SlowGrant(u64),
		/// server answers acquired: false (someone else holds the lock)
		Refuse,
		/// no answer: the request failed in transport
		Unreachable,
		/// the server answered with an error
		Rejected,
	}

	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	fn scripted_acquire(polls: &[AcquireStep]) -> impl FnMut() -> ScriptedLockFut {
		let mut polls = std::collections::VecDeque::from(polls.to_vec());
		move || {
			let poll = polls
				.pop_front()
				.expect("acquire polled more often than scripted");
			match poll {
				AcquireStep::Grant => Box::pin(async { Ok(true) }),
				AcquireStep::SlowGrant(secs) => Box::pin(async move {
					tokio::time::sleep(Duration::from_secs(secs)).await;
					Ok(true)
				}),
				AcquireStep::Refuse => Box::pin(async { Ok(false) }),
				AcquireStep::Unreachable => Box::pin(async {
					Err(Error::custom(
						ErrorKind::Reqwest,
						"scripted transport failure",
					))
				}),
				AcquireStep::Rejected => Box::pin(async {
					Err(Error::custom(ErrorKind::Server, "scripted server error"))
				}),
			}
		}
	}

	// With a 30s ceiling the fibonacci_iter delays run 0, 0.25s, 0.25s, 0.5s, 0.75s, 1.25s, 2s,
	// 3.25s, ... and each is slept after the poll it belongs to.

	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	#[tokio::test(start_paused = true)]
	async fn acquire_refusals_use_up_the_attempts() {
		let begin = Instant::now();
		let err = acquire_loop(
			scripted_acquire(&[AcquireStep::Refuse; 3]),
			"r",
			Duration::from_secs(30),
			3,
		)
		.await
		.unwrap_err();
		assert_eq!(err.kind(), ErrorKind::RetryFailed);
		// 0 + 0.25 + 0.25s: the last attempt's delay is still slept before RetryFailed
		assert_eq!(begin.elapsed(), Duration::from_millis(500));
	}

	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	#[tokio::test(start_paused = true)]
	async fn acquire_delays_stop_growing_at_max_sleep_time() {
		let begin = Instant::now();
		let err = acquire_loop(
			scripted_acquire(&[AcquireStep::Refuse; 5]),
			"r",
			Duration::from_millis(500),
			5,
		)
		.await
		.unwrap_err();
		assert_eq!(err.kind(), ErrorKind::RetryFailed);
		// 0 + 0.25 + 0.25 + 0.5 + 0.5s; uncapped the last delay would be 0.75s
		assert_eq!(begin.elapsed(), Duration::from_millis(1500));
	}

	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	#[tokio::test(start_paused = true)]
	async fn acquire_grant_is_anchored_to_its_send_time() {
		let begin = Instant::now();
		let granted_at = acquire_loop(
			scripted_acquire(&[
				AcquireStep::Refuse,
				AcquireStep::Refuse,
				AcquireStep::SlowGrant(5),
			]),
			"r",
			Duration::from_secs(30),
			10,
		)
		.await
		.unwrap();
		// sent at 0.25s, answered at 5.25s: the lease runs from the send
		assert_eq!(granted_at - begin, Duration::from_millis(250));
		assert_eq!(begin.elapsed(), Duration::from_millis(5250));
	}

	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	#[tokio::test(start_paused = true)]
	async fn acquire_transport_error_on_the_first_poll_fails_fast() {
		let begin = Instant::now();
		let err = acquire_loop(
			scripted_acquire(&[AcquireStep::Unreachable]),
			"r",
			Duration::from_secs(30),
			10,
		)
		.await
		.unwrap_err();
		// Nothing has answered yet, so an offline device gets its error at once.
		assert_eq!(err.kind(), ErrorKind::Reqwest);
		assert_eq!(begin.elapsed(), Duration::ZERO);
	}

	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	#[tokio::test(start_paused = true)]
	async fn acquire_transport_error_after_an_answer_keeps_polling() {
		let begin = Instant::now();
		let granted_at = acquire_loop(
			scripted_acquire(&[
				AcquireStep::Refuse,
				AcquireStep::Unreachable,
				AcquireStep::Grant,
			]),
			"r",
			Duration::from_secs(30),
			10,
		)
		.await
		.unwrap();
		// refused at 0s, unreachable at 0s, granted at 0.25s
		assert_eq!(granted_at - begin, Duration::from_millis(250));
	}

	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	#[tokio::test(start_paused = true)]
	async fn acquire_transport_errors_past_the_limit_fail_the_acquisition() {
		let begin = Instant::now();
		let err = acquire_loop(
			scripted_acquire(&[
				AcquireStep::Refuse,
				AcquireStep::Unreachable,
				AcquireStep::Unreachable,
				AcquireStep::Unreachable,
				AcquireStep::Unreachable,
			]),
			"r",
			Duration::from_secs(30),
			10,
		)
		.await
		.unwrap_err();
		assert_eq!(err.kind(), ErrorKind::Reqwest);
		// the fourth consecutive failure, at 0 + 0.25 + 0.25 + 0.5s, is returned
		assert_eq!(begin.elapsed(), Duration::from_secs(1));
	}

	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	#[tokio::test(start_paused = true)]
	async fn acquire_answer_resets_the_transport_error_count() {
		let begin = Instant::now();
		let granted_at = acquire_loop(
			scripted_acquire(&[
				AcquireStep::Refuse,
				AcquireStep::Unreachable,
				AcquireStep::Unreachable,
				AcquireStep::Unreachable,
				AcquireStep::Refuse,
				AcquireStep::Unreachable,
				AcquireStep::Unreachable,
				AcquireStep::Unreachable,
				AcquireStep::Grant,
			]),
			"r",
			Duration::from_secs(30),
			10,
		)
		.await
		.unwrap();
		// refused again at 1s, so three more failures are waited out; granted at 8.25s
		assert_eq!(granted_at - begin, Duration::from_millis(8250));
	}

	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	#[tokio::test(start_paused = true)]
	async fn acquire_transport_error_uses_up_an_attempt() {
		let begin = Instant::now();
		let err = acquire_loop(
			scripted_acquire(&[AcquireStep::Refuse, AcquireStep::Unreachable]),
			"r",
			Duration::from_secs(30),
			2,
		)
		.await
		.unwrap_err();
		assert_eq!(err.kind(), ErrorKind::RetryFailed);
		assert_eq!(begin.elapsed(), Duration::from_millis(250));
	}

	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	#[tokio::test(start_paused = true)]
	async fn acquire_server_error_after_an_answer_is_returned() {
		let begin = Instant::now();
		let err = acquire_loop(
			scripted_acquire(&[AcquireStep::Refuse, AcquireStep::Rejected]),
			"r",
			Duration::from_secs(30),
			10,
		)
		.await
		.unwrap_err();
		assert_eq!(err.kind(), ErrorKind::Server);
		assert_eq!(begin.elapsed(), Duration::ZERO);
	}
}
