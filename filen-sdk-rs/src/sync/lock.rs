use std::cmp::min;
use std::future::Future;
use std::time::Duration;
use std::{
	borrow::Cow,
	sync::{Arc, Weak},
	time,
};

use bytes::Bytes;
use chrono::{DateTime, Utc};
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

/// When the server last confirmed a lock's lease: the send time of the confirming request
/// (the acquire, or a refresh) on the monotonic clock the async timers use, and on the wall
/// clock, which keeps running while the system sleeps and the monotonic clock does not.
#[derive(Debug, Clone, Copy)]
struct Confirmed {
	at: Instant,
	wall: DateTime<Utc>,
}

impl Confirmed {
	fn now() -> Self {
		Self {
			at: Instant::now(),
			wall: Utc::now(),
		}
	}

	/// The time since, by whichever clock ran further: the monotonic one stops during a system
	/// suspend, the wall clock can be set back.
	///
	/// Neither clock covers every case. A wall clock set forward reads as a suspend, so a lock
	/// may be taken for lost while the server still holds it; one set back while the system
	/// is suspended hides that suspend from both clocks. Telling them apart takes a clock that
	/// counts suspended time, which the platforms only offer through calls this crate does not
	/// make.
	fn age(&self) -> Duration {
		let wall = (Utc::now() - self.wall).to_std().unwrap_or(Duration::ZERO);
		self.at.elapsed().max(wall)
	}

	/// Whether this confirmation was sent after `other`: by the monotonic clock, which is not set
	/// back, and on a tie (no time passed on it, as on a paused test clock) by the wall clock.
	fn is_newer_than(&self, other: &Self) -> bool {
		(self.at, self.wall) > (other.at, other.wall)
	}
}

/// Records `sent` as the lease's latest confirmation unless a later one is recorded already.
fn record_confirmation(confirmed: &tokio::sync::watch::Sender<Confirmed>, sent: Confirmed) {
	confirmed.send_if_modified(|current| {
		let later = sent.is_newer_than(current);
		if later {
			*current = sent;
		}
		later
	});
}

/// Where a lock request goes: a gateway's lock endpoint.
fn lock_url() -> String {
	gateway_url(api::v3::user::lock::ENDPOINT)
}

/// Represents a lock on a resource, which can be acquired using the [`Client::acquire_lock`] method.
/// The lock is released when the [`ResourceLock`] is dropped; its clones share the lock, which
/// is released once the last of them is dropped.
///
/// While the lock is held, no other client can acquire the lock on the same resource.
/// The lock is automatically released [`LOCK_SERVER_TTL`] after the last refresh the
/// server processed, so a background task refreshes it every [`LOCK_REFRESH_INTERVAL`],
/// retrying a failed refresh.
///
/// A lock whose last confirmed refresh is older than [`LOCK_LOSS_BUDGET`] (by the monotonic or
/// the wall clock, so a system suspend counts) is unconfirmed: [`Self::is_valid`] reads false
/// until the server confirms it again. It is LOST once the server refuses a refresh, or no
/// refresh was confirmed within [`LOCK_SERVER_TTL`]: [`Self::wait_for_loss`] resolves, the
/// keep-alive stops, a release of the lease is sent in case the server still holds it after
/// all, and the cached [`Client::lock_drive`] family acquires a new lock instead of handing it
/// out — another client may hold the resource from that point on.
///
/// The release request is spawned onto the ambient tokio runtime at drop time when one exists,
/// falling back to the runtime the lock was acquired on; if neither is available (or the
/// fallback runtime has shut down), the release is skipped and the server expires the lock on
/// its own after ~60 seconds.
#[derive(Debug, Clone)]
pub struct ResourceLock {
	lease: Arc<SharedLease>,
}

/// The server-side lease a [`ResourceLock`] and its clones hold together, released once the
/// last of them is dropped.
#[derive(Debug)]
struct SharedLease {
	uuid: Uuid,
	client: Arc<AuthClient>,
	resource: String,
	// Flipped to false (never back) once the lease is lost. See [`ResourceLock::is_valid`].
	valid: tokio::sync::watch::Sender<bool>,
	// The lease's latest confirmation, shared with the keep-alive task.
	confirmed: tokio::sync::watch::Sender<Confirmed>,
	// The runtime the lock was acquired on. The refresh task is spawned on it, and the release —
	// which can be sent from a thread with no ambient tokio runtime (e.g. a foreign FFI finalizer
	// thread) — falls back to it when there is no ambient runtime to spawn it on.
	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	handle: Option<tokio::runtime::Handle>,
}

impl PartialEq for ResourceLock {
	fn eq(&self, other: &Self) -> bool {
		let (this, other) = (&self.lease, &other.lease);
		this.uuid == other.uuid && this.client == other.client && this.resource == other.resource
	}
}

impl Eq for ResourceLock {}

impl ResourceLock {
	pub fn resource(&self) -> &str {
		&self.lease.resource
	}

	/// Whether the server confirmed this lock's lease within [`LOCK_LOSS_BUDGET`] and has not
	/// refused it since.
	///
	/// `false` means another client may already hold the resource, or soon may: the lock is
	/// lost, or its last confirmation is too old to rely on (a refresh failing, or the system
	/// was suspended). An unconfirmed lock turns valid again once the keep-alive gets a refresh
	/// through; a lost one never does, and holders should re-acquire.
	pub fn is_valid(&self) -> bool {
		!self.lease.is_lost() && self.lease.age() < LOCK_LOSS_BUDGET
	}

	/// Whether the lease is lost: the server refused a refresh, or none was confirmed while the
	/// lease could last.
	pub(crate) fn is_lost(&self) -> bool {
		self.lease.is_lost()
	}

	/// Resolves once the server-side lease is lost; resolves immediately if it already is.
	/// Pends indefinitely while the lock stays held, so callers should race it against their
	/// work. An unconfirmed lock (see [`Self::is_valid`]) is not lost yet.
	pub async fn wait_for_loss(&self) {
		let mut receiver = self.lease.valid.subscribe();
		// The sender is a field of `self`, so it cannot close while borrowed
		// here; a closed channel would mean the lock is gone anyway.
		let _ = receiver.wait_for(|valid| !*valid).await;
	}
}

impl SharedLease {
	fn is_lost(&self) -> bool {
		!*self.valid.borrow()
	}

	fn age(&self) -> Duration {
		self.confirmed.borrow().age()
	}

	/// Marks the lease lost, and sends a release for it in case the server still holds it after
	/// all (a refresh answered too late, a suspend the clocks overstate): the release only frees a
	/// lease this lock's uuid holds, so it never touches another client's.
	fn mark_lost(&self) {
		if self.valid.send_replace(false) {
			tracing::warn!(
				"Lost lock '{}', releasing it in case the server still holds it",
				self.resource,
			);
			self.spawn_release();
		}
	}

	// The release is attempted even for a lock marked lost: if the loss was a false positive
	// (the server still holds our lease), releasing frees the resource for everyone immediately
	// instead of stalling it out to the TTL, and if the lease is truly gone the Release is a
	// harmless no-op keyed to our uuid.
	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	fn spawn_release(&self) {
		// Prefer the ambient runtime: it is alive by definition, while the captured handle's
		// runtime may have shut down since acquisition (spawning there would silently cancel
		// the release).
		let handle = tokio::runtime::Handle::try_current()
			.ok()
			.or_else(|| self.handle.clone());
		let Some(handle) = handle else {
			tracing::warn!(
				"No tokio runtime available to release lock {}, relying on server-side expiry",
				self.resource
			);
			return;
		};
		let client = self.client.clone();
		let uuid = self.uuid;
		let resource = self.resource.clone();
		// Spawning on a handle whose runtime has shut down does not panic: the task is
		// cancelled without being polled and the server expires the lock on its own.
		handle.spawn(async move { release(&client, &lock_url(), uuid, &resource).await });
	}

	#[cfg(all(target_family = "wasm", target_os = "unknown"))]
	fn spawn_release(&self) {
		let client = self.client.clone();
		let uuid = self.uuid;
		let resource = self.resource.clone();
		// `runtime::spawn_local` (unlike raw `wasm_bindgen_futures::spawn_local`) holds the
		// spawning SDK worker open until the future completes, so a worker whose other tasks
		// finish cannot close mid-request and silently skip the release. On the main thread it
		// degrades to a plain `spawn_local`.
		crate::runtime::spawn_local(async move {
			release(&client, &lock_url(), uuid, &resource).await;
		});
	}
}

impl Drop for SharedLease {
	// async drop is not supported in Rust, so the release is spawned
	fn drop(&mut self) {
		self.spawn_release();
	}
}

/// One Refresh round-trip for the lock `uuid` on `resource`, sent to `url`: the server's
/// `refreshed` flag.
async fn send_refresh(
	client: &AuthClient,
	url: &str,
	uuid: Uuid,
	resource: &str,
) -> Result<bool, Error> {
	let result = api::v3::user::lock::post(
		client,
		url,
		&api::v3::user::lock::Request {
			uuid,
			r#type: LockType::Refresh,
			resource: Cow::Borrowed(resource),
		},
	)
	.await
	.map(|r| r.refreshed);
	match &result {
		Ok(true) => debug!("Refreshed lock: {resource}"),
		Ok(false) => tracing::warn!("Server refused to refresh lock: {resource}"),
		Err(e) => tracing::warn!("Failed to refresh lock {resource}: {e}"),
	}
	result
}

/// Releases the lock `uuid` on `resource`, sent to `url`. The server only releases it while
/// `uuid` holds it.
async fn release(client: &AuthClient, url: &str, uuid: Uuid, resource: &str) {
	match api::v3::user::lock::post(
		client,
		url,
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

const LOCK_REFRESH_INTERVAL: time::Duration = time::Duration::from_secs(15);
/// Server-side lease for a held lock: the server auto-releases a lock 60s after the last
/// acquire/refresh it processed. Measured against the live API: another client got a resource
/// 60s after the holder's acquire, and 90s after an acquire refreshed at 30s.
const LOCK_SERVER_TTL: time::Duration = time::Duration::from_secs(60);
/// How old the last server-confirmed refresh of a held lock may be for the lock to be relied on
/// ([`ResourceLock::is_valid`]). The margin below [`LOCK_SERVER_TTL`] absorbs the
/// send-to-server-processing skew and the time a write takes to reach the server.
const LOCK_LOSS_BUDGET: time::Duration = time::Duration::from_secs(25);
/// Delay between retries of a failed refresh.
const LOCK_RETRY_INTERVAL: time::Duration = time::Duration::from_secs(1);
/// The longest the keep-alive sleeps before looking at the lease's age again. Its timers run on
/// the monotonic clock, which stops during a system suspend: waking up this often, it sees a
/// suspend through the wall clock in the age and refreshes right away instead of when its
/// timer runs out.
const LOCK_CLOCK_TICK: time::Duration = time::Duration::from_secs(1);

/// How long to sleep before the next refresh, given how long ago the lease was last confirmed.
fn refresh_delay_after(elapsed: Duration) -> Duration {
	LOCK_REFRESH_INTERVAL.saturating_sub(elapsed)
}

/// Sleeps until the next refresh is due, by the age of the lease's last confirmation, in steps
/// of at most [`LOCK_CLOCK_TICK`]. `false` once the lock is lost meanwhile.
async fn until_refresh_due(
	confirmed: &tokio::sync::watch::Sender<Confirmed>,
	valid: &tokio::sync::watch::Sender<bool>,
) -> bool {
	loop {
		if !*valid.borrow() {
			return false;
		}
		let delay = refresh_delay_after(confirmed.borrow().age());
		if delay.is_zero() {
			return true;
		}
		sleep(delay.min(LOCK_CLOCK_TICK)).await;
	}
}

/// Why a [`refresh_loop`] ended.
#[derive(Debug, PartialEq, Eq)]
enum LoopExit {
	/// Every strong reference to the lock was dropped; Drop handles the release.
	Dropped,
	/// The server refused a refresh, none was confirmed within [`LOCK_SERVER_TTL`], or the lock
	/// was marked lost otherwise — the server-side lease can no longer be trusted.
	Lost,
}

/// Drives the refresh schedule for one held lock until it is dropped or lost, recording every
/// confirmation in `confirmed`. `valid` is the lock's loss flag: once it is cleared, by the loop
/// or by anyone else, the loop sends no further refresh and ends.
///
/// `refresh` builds the future for a single Refresh round-trip (resolving to the
/// server's `refreshed` flag), or returns `None` once the lock has been dropped.
/// Generic over the refresh action so the schedule is unit-testable against a scripted
/// server on a paused clock.
///
/// The lock is only declared lost on the server's word, or once no refresh was confirmed for
/// [`LOCK_SERVER_TTL`], when the lease has lapsed whatever the server would answer. Before
/// that, failing refreshes are retried: an unconfirmed lock stops being relied on through
/// [`ResourceLock::is_valid`] and is confirmed again once a refresh gets through, so a network
/// blip or a short system suspend does not cost the lock.
async fn refresh_loop<F, Fut>(
	mut refresh: F,
	confirmed: &tokio::sync::watch::Sender<Confirmed>,
	valid: &tokio::sync::watch::Sender<bool>,
) -> LoopExit
where
	F: FnMut() -> Option<Fut>,
	Fut: Future<Output = Result<bool, Error>>,
{
	// Confirmations are timed from when the request was SENT: the server's lease clock restarts
	// when it processes a request, never earlier than its send, so the age only ever
	// overestimates the lease's.
	loop {
		if !until_refresh_due(confirmed, valid).await {
			return LoopExit::Lost;
		}
		let age = confirmed.borrow().age();
		let Some(remaining) = LOCK_SERVER_TTL.checked_sub(age).filter(|r| !r.is_zero()) else {
			return LoopExit::Lost;
		};
		let Some(fut) = refresh() else {
			return LoopExit::Dropped;
		};
		let sent = Confirmed::now();
		match timeout(remaining, fut).await {
			// Only an answer received inside the lease, for a lock not marked lost meanwhile, is
			// trusted: an older server confirms any refresh, also one for a lease that lapsed and
			// was handed to another client.
			Ok(Ok(true)) if *valid.borrow() && confirmed.borrow().age() < LOCK_SERVER_TTL => {
				record_confirmation(confirmed, sent);
			}
			// Refused (the server no longer recognizes the lock), answered too late, or lost
			// meanwhile.
			Ok(Ok(_)) => return LoopExit::Lost,
			// Transport error: outcome unknown, retry while the lease may still stand.
			Ok(Err(_)) => sleep(LOCK_RETRY_INTERVAL).await,
			// The attempt outlived the lease; the next iteration declares loss.
			Err(_) => {}
		}
	}
}

async fn run_keep_alive(
	weak: Weak<SharedLease>,
	confirmed: tokio::sync::watch::Sender<Confirmed>,
	valid: tokio::sync::watch::Sender<bool>,
) {
	let refresh = || {
		let lease = weak.upgrade()?;
		Some(
			async move { send_refresh(&lease.client, &lock_url(), lease.uuid, &lease.resource).await },
		)
	};
	if refresh_loop(refresh, &confirmed, &valid).await == LoopExit::Lost
		&& let Some(lease) = weak.upgrade()
	{
		lease.mark_lost();
	}
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
fn keep_lock_alive(lease: &Arc<SharedLease>) {
	// second handles on the lease's channels, not copies of them
	let refresh = run_keep_alive(
		Arc::downgrade(lease),
		lease.confirmed.clone(),
		lease.valid.clone(),
	);
	// Same no-ambient-runtime hazard as the release: spawn on the runtime the lock was acquired
	// on. The ambient fallback is unreachable through `acquire_lock` (reqwest already
	// requires a tokio context there) but keeps behavior identical when no handle was
	// captured.
	match &lease.handle {
		Some(handle) => handle.spawn(refresh),
		None => tokio::spawn(refresh),
	};
}

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
fn keep_lock_alive(lease: &Arc<SharedLease>) {
	// `runtime::spawn_local` pins the spawning SDK worker open for the life of the refresh
	// loop (it exits when the lock drops or is lost), so the worker cannot close
	// under a held lock and silently stop refreshing it — the server would expire the lock
	// while the client still believes it holds it.
	// second handles on the lease's channels, not copies of them
	crate::runtime::spawn_local(run_keep_alive(
		Arc::downgrade(lease),
		lease.confirmed.clone(),
		lease.valid.clone(),
	));
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
) -> Result<Confirmed, Error>
where
	F: FnMut() -> Fut,
	Fut: Future<Output = Result<bool, Error>>,
{
	let mut answered = false;
	let mut transport_errors = 0;
	for (i, delay) in (0..attempts).zip(fibonacci_iter(max_sleep_time)) {
		// The server's lease clock starts when it processes this request, no
		// earlier than now — the refresh schedule is anchored to the send time.
		let attempt_started = Confirmed::now();
		match poll().await {
			// A lease that runs from this poll's send, even after an earlier poll the server
			// granted with only the answer lost: a server that grants an Acquire repeated with
			// the holder's own uuid restarts its lease, and one that refuses it grants a fresh
			// lease once that one lapses. Either way the lease starts no earlier than now.
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

/// One Acquire round-trip with the serialized request `body`, sent to `url`: the server's
/// `acquired` flag.
pub(crate) async fn send_acquire(
	client: &AuthClient,
	url: &str,
	body: Bytes,
) -> Result<bool, Error> {
	client
		.post_raw_bytes_auth_lock::<api::v3::user::lock::Response>(
			body,
			url,
			api::v3::user::lock::ENDPOINT.into(),
		)
		.await
		.map(|response| response.acquired)
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
		let url = lock_url();
		let client = self.arc_client();
		let acquired_at = acquire_loop(
			// refcount bump: every poll resends the same Acquire body
			|| send_acquire(&client, &url, bytes.clone()),
			&resource,
			max_sleep_time,
			attempts,
		)
		.await?;
		let lease = Arc::new(SharedLease {
			uuid,
			client,
			resource,
			valid: tokio::sync::watch::Sender::new(true),
			confirmed: tokio::sync::watch::Sender::new(acquired_at),
			#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
			handle: tokio::runtime::Handle::try_current().ok(),
		});
		keep_lock_alive(&lease);
		Ok(Arc::new(ResourceLock { lease }))
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
	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	use filen_types::auth::APIKey;

	use super::*;
	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	use crate::auth::unauth::test_support::offline_unauthed;

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

	/// A client whose every request fails to connect without leaving the machine: a release or
	/// refresh the tests below set off must never reach the gateway.
	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	fn offline_client() -> Arc<AuthClient> {
		Arc::new(AuthClient::from_unauthed(
			offline_unauthed(),
			Arc::new(std::sync::RwLock::new(APIKey(Cow::Borrowed("")))),
		))
	}

	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	fn test_lock(handle: Option<tokio::runtime::Handle>) -> ResourceLock {
		ResourceLock {
			lease: Arc::new(SharedLease {
				uuid: Uuid::new_v4(),
				client: offline_client(),
				resource: "test-resource".to_string(),
				valid: tokio::sync::watch::Sender::new(true),
				confirmed: tokio::sync::watch::Sender::new(Confirmed::now()),
				handle,
			}),
		}
	}

	// The runtimes below are deliberately never driven: a `current_thread` runtime only
	// polls tasks inside `block_on`, so the release task spawned by `Drop` is queued (or
	// cancelled) but never runs.
	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	#[test]
	fn drop_on_thread_without_runtime_does_not_panic() {
		let rt = tokio::runtime::Builder::new_current_thread()
			.enable_all()
			.build()
			.unwrap();
		let lock = rt.block_on(async { test_lock(tokio::runtime::Handle::try_current().ok()) });
		assert!(lock.lease.handle.is_some());
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
	fn dropping_a_clone_keeps_the_lock_until_the_last_one_goes() {
		let rt = tokio::runtime::Builder::new_current_thread()
			.enable_all()
			.build()
			.unwrap();
		let lock = rt.block_on(async { test_lock(tokio::runtime::Handle::try_current().ok()) });
		let _guard = rt.enter();
		std::mem::drop(lock.clone());
		assert_eq!(
			rt.metrics().num_alive_tasks(),
			0,
			"the lock is still held through the original, so nothing is released"
		);
		std::mem::drop(lock);
		assert_eq!(rt.metrics().num_alive_tasks(), 1);
	}

	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	#[test]
	fn keep_lock_alive_without_ambient_runtime_does_not_panic() {
		let rt = tokio::runtime::Builder::new_current_thread()
			.enable_all()
			.build()
			.unwrap();
		let lock = rt.block_on(async { test_lock(tokio::runtime::Handle::try_current().ok()) });
		// A plain thread has no ambient runtime, so the refresh task must ride the
		// captured handle.
		std::thread::spawn(move || keep_lock_alive(&lock.lease))
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
		lock.lease.valid.send_replace(false);
		let _guard = rt.enter();
		assert_eq!(rt.metrics().num_alive_tasks(), 0);
		std::mem::drop(lock);
		// A false-positive loss must not strand the resource until the TTL: the
		// release is attempted anyway (harmless no-op if the lease is truly gone).
		assert_eq!(rt.metrics().num_alive_tasks(), 1);
	}

	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	#[test]
	fn a_lock_declared_lost_sends_a_release_once() {
		let rt = tokio::runtime::Builder::new_current_thread()
			.enable_all()
			.build()
			.unwrap();
		let lock = rt.block_on(async { test_lock(tokio::runtime::Handle::try_current().ok()) });
		let _guard = rt.enter();
		lock.lease.mark_lost();
		assert!(!lock.is_valid());
		assert_eq!(rt.metrics().num_alive_tasks(), 1);
		lock.lease.mark_lost();
		assert_eq!(rt.metrics().num_alive_tasks(), 1, "a loss is released once");
	}

	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	#[tokio::test]
	async fn loss_signal_flips_is_valid_and_wakes_waiters() {
		let lock = test_lock(None);
		assert!(lock.is_valid());
		let wait = lock.wait_for_loss();
		tokio::pin!(wait);
		assert!(futures::poll!(wait.as_mut()).is_pending());
		lock.lease.valid.send_replace(false);
		assert!(!lock.is_valid());
		wait.await;
		// resolves immediately when already lost
		lock.wait_for_loss().await;
	}

	/// A lease confirmation `ago` in the past on the wall clock only, as after a system suspend
	/// the monotonic clock did not count.
	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	fn suspended_for(ago: Duration) -> Confirmed {
		Confirmed {
			at: Instant::now(),
			wall: Utc::now() - chrono::Duration::from_std(ago).unwrap(),
		}
	}

	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	#[tokio::test]
	async fn a_suspend_past_the_budget_leaves_the_lock_unconfirmed_until_the_server_confirms_it() {
		let lock = test_lock(None);
		lock.lease
			.confirmed
			.send_replace(suspended_for(LOCK_LOSS_BUDGET + Duration::from_secs(5)));
		// the monotonic clock did not see the suspend; the wall clock did
		assert!(!lock.is_valid());
		assert!(!lock.is_lost(), "the lease may still stand");
		record_confirmation(&lock.lease.confirmed, Confirmed::now());
		assert!(lock.is_valid());
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

	/// A lease confirmed at `at` on the monotonic clock, and now on the wall clock, which the
	/// paused test clock does not move.
	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	fn confirmed_at(at: Instant) -> tokio::sync::watch::Sender<Confirmed> {
		tokio::sync::watch::Sender::new(Confirmed {
			at,
			wall: Utc::now(),
		})
	}

	/// The loss flag of a lock that is not lost.
	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	fn held() -> tokio::sync::watch::Sender<bool> {
		tokio::sync::watch::Sender::new(true)
	}

	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	#[tokio::test(start_paused = true)]
	async fn an_unconfirmed_lock_is_invalid_until_the_server_confirms_it_again() {
		let lock = test_lock(None);
		tokio::time::advance(LOCK_LOSS_BUDGET).await;
		assert!(!lock.is_valid());
		assert!(!lock.is_lost(), "the lease may still stand");
		record_confirmation(&lock.lease.confirmed, Confirmed::now());
		assert!(lock.is_valid());
	}

	// All timings below are exact: the paused tokio clock only advances by the
	// timers the loop itself registers.

	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	#[tokio::test(start_paused = true)]
	async fn lock_drop_ends_refresh_loop_silently() {
		let begin = Instant::now();
		let exit = refresh_loop(
			scripted_refresh(&[Step::Confirm, Step::Drop]),
			&confirmed_at(begin),
			&held(),
		)
		.await;
		assert_eq!(exit, LoopExit::Dropped);
		// confirm at 15s, drop observed at the 30s refresh
		assert_eq!(begin.elapsed(), Duration::from_secs(30));
	}

	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	#[tokio::test(start_paused = true)]
	async fn server_refusal_is_authoritative_loss() {
		let begin = Instant::now();
		let exit = refresh_loop(
			scripted_refresh(&[Step::Refuse]),
			&confirmed_at(begin),
			&held(),
		)
		.await;
		assert_eq!(exit, LoopExit::Lost);
		assert_eq!(begin.elapsed(), Duration::from_secs(15));
	}

	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	#[tokio::test(start_paused = true)]
	async fn transient_refresh_error_retries_and_recovers() {
		let begin = Instant::now();
		let confirmed = confirmed_at(begin);
		let exit = refresh_loop(
			scripted_refresh(&[Step::TransportErr, Step::Confirm, Step::Confirm, Step::Drop]),
			&confirmed,
			&held(),
		)
		.await;
		assert_eq!(exit, LoopExit::Dropped);
		// err at 15s, retry confirms at 16s, confirm at 31s, drop observed at 46s
		assert_eq!(begin.elapsed(), Duration::from_secs(46));
		assert_eq!(confirmed.borrow().at - begin, Duration::from_secs(31));
	}

	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	#[tokio::test(start_paused = true)]
	async fn failing_refreshes_are_retried_until_the_lease_lapses() {
		let begin = Instant::now();
		// attempts at 15s..=59s, the lease lapses at 60s — exactly 45 attempts. Past the loss
		// budget the lock reads unconfirmed, but is only lost once the server could no longer
		// hold it for us.
		let exit = refresh_loop(
			scripted_refresh(&[Step::TransportErr; 45]),
			&confirmed_at(begin),
			&held(),
		)
		.await;
		assert_eq!(exit, LoopExit::Lost);
		assert_eq!(begin.elapsed(), LOCK_SERVER_TTL);
	}

	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	#[tokio::test(start_paused = true)]
	async fn hung_refresh_is_cut_off_when_the_lease_lapses() {
		let begin = Instant::now();
		let exit = refresh_loop(
			scripted_refresh(&[Step::Hang]),
			&confirmed_at(begin),
			&held(),
		)
		.await;
		assert_eq!(exit, LoopExit::Lost);
		assert_eq!(begin.elapsed(), LOCK_SERVER_TTL);
	}

	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	#[tokio::test(start_paused = true)]
	async fn a_refresh_late_past_the_budget_asks_the_server_before_any_loss() {
		// The loop ran late (a slow acquire answer, a starved runtime): the server may still
		// hold the lease, so it is asked rather than the lock declared lost unheard.
		let begin = Instant::now();
		tokio::time::advance(LOCK_LOSS_BUDGET + Duration::from_secs(5)).await;
		let exit = refresh_loop(
			scripted_refresh(&[Step::Confirm, Step::Drop]),
			&confirmed_at(begin),
			&held(),
		)
		.await;
		assert_eq!(exit, LoopExit::Dropped);
		// confirms at 30s, drop observed at the 45s refresh
		assert_eq!(begin.elapsed(), Duration::from_secs(45));
	}

	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	#[tokio::test(start_paused = true)]
	async fn a_suspend_past_the_ttl_is_loss_by_the_wall_clock() {
		// the monotonic clock did not see the suspend, so only the wall clock can tell
		let confirmed = tokio::sync::watch::Sender::new(suspended_for(
			LOCK_SERVER_TTL + Duration::from_secs(5),
		));
		let exit = refresh_loop(scripted_refresh(&[]), &confirmed, &held()).await;
		assert_eq!(exit, LoopExit::Lost);
	}

	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	#[tokio::test(start_paused = true)]
	async fn a_suspend_while_waiting_is_refreshed_after_at_once() {
		let begin = Instant::now();
		let (confirmed, valid) = (confirmed_at(begin), held());
		let (exit, ()) = tokio::join!(
			refresh_loop(
				scripted_refresh(&[Step::Confirm, Step::Drop]),
				&confirmed,
				&valid
			),
			async {
				sleep(Duration::from_millis(2500)).await;
				// a 40s suspend: the wall clock moved on, the monotonic one did not
				confirmed.send_replace(Confirmed {
					at: begin,
					wall: Utc::now() - chrono::Duration::seconds(40),
				});
			}
		);
		assert_eq!(exit, LoopExit::Dropped);
		// refreshed at the next look at the clock (3s), not when the 15s timer ran out, and
		// the drop observed at the refresh after (18s)
		assert_eq!(begin.elapsed(), Duration::from_secs(18));
	}

	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	#[tokio::test(start_paused = true)]
	async fn a_lock_lost_elsewhere_is_refreshed_no_more() {
		let begin = Instant::now();
		let (confirmed, valid) = (confirmed_at(begin), held());
		let (exit, ()) = tokio::join!(
			// one refresh is scripted: a second would panic
			refresh_loop(scripted_refresh(&[Step::Confirm]), &confirmed, &valid),
			async {
				sleep(Duration::from_millis(20_500)).await;
				valid.send_replace(false);
			}
		);
		assert_eq!(exit, LoopExit::Lost);
		// confirmed at 15s, the loss seen at the next look at the clock
		assert_eq!(begin.elapsed(), Duration::from_secs(21));
	}

	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	#[tokio::test(start_paused = true)]
	async fn a_refresh_answered_after_the_lock_was_lost_does_not_confirm_it() {
		let begin = Instant::now();
		let confirmed = confirmed_at(begin);
		let valid = held();
		let (exit, ()) = tokio::join!(
			refresh_loop(
				scripted_refresh(&[Step::SlowConfirm(5)]),
				&confirmed,
				&valid
			),
			async {
				sleep(Duration::from_secs(17)).await;
				valid.send_replace(false);
			}
		);
		assert_eq!(exit, LoopExit::Lost);
		// sent at 15s, answered at 20s, after the loss at 17s
		assert_eq!(begin.elapsed(), Duration::from_secs(20));
		assert_eq!(confirmed.borrow().at, begin);
	}

	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	#[tokio::test(start_paused = true)]
	async fn slow_confirm_keeps_cadence_anchored_to_send_time() {
		let begin = Instant::now();
		let exit = refresh_loop(
			scripted_refresh(&[Step::SlowConfirm(5), Step::Drop]),
			&confirmed_at(begin),
			&held(),
		)
		.await;
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
		assert_eq!(granted_at.at - begin, Duration::from_millis(250));
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
		assert_eq!(granted_at.at - begin, Duration::from_millis(250));
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
		assert_eq!(granted_at.at - begin, Duration::from_millis(8250));
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
