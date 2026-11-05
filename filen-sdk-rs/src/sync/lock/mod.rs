use std::cmp::min;
use std::future::Future;
use std::pin::pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use std::{
	borrow::Cow,
	sync::{Arc, Weak},
	time,
};

use bytes::Bytes;
use chrono::{DateTime, Utc};
use filen_types::{api::v3::user::lock::LockType, fs::Uuid};
use futures::FutureExt;
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
	error::{Error, LockLostError},
	util::{MaybeSend, MaybeSendSync},
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

/// How long a shutdown waits for the lock releases still in flight
/// ([`Client::settle_lock_releases`]): long enough for a round trip on a slow network, short
/// enough that a dead one does not hold the shutdown up. A release left unsent costs the
/// account's other devices the rest of the lease, up to [`LOCK_SERVER_TTL`].
// Reached by what shuts down a cache worker or a sync engine.
#[cfg(any(
	feature = "cache",
	all(test, not(all(target_family = "wasm", target_os = "unknown")))
))]
pub(crate) const LOCK_RELEASE_SETTLE_BOUND: Duration = Duration::from_secs(5);

/// A client's lock requests that may leave a lease behind and are not answered yet: releases,
/// and acquisitions whose grant is released by their own task (see `request_acquire`). They run
/// detached, so this count is what lets a shutdown wait for them before the process exits
/// ([`Client::settle_lock_releases`], [`Client::wait_for_lock_releases`]).
#[derive(Debug)]
pub(crate) struct PendingReleases(tokio::sync::watch::Sender<usize>);

impl Default for PendingReleases {
	fn default() -> Self {
		Self(tokio::sync::watch::Sender::new(0))
	}
}

impl PendingReleases {
	/// Counts one request until the returned guard drops: with the [`OwedRelease`] that holds it,
	/// once the release is answered or given up on, or no longer needed.
	fn track(&self) -> PendingRelease {
		self.0.send_modify(|pending| *pending += 1);
		PendingRelease(self.0.clone())
	}

	/// Waits until no request is pending, for at most `bound`. `false` if some still were.
	#[cfg(any(
		feature = "cache",
		all(test, not(all(target_family = "wasm", target_os = "unknown")))
	))]
	pub(crate) async fn settle(&self, bound: Duration) -> bool {
		timeout(bound, self.settled()).await.is_ok()
	}

	/// Resolves once no request is pending, at once when none is.
	pub(crate) async fn settled(&self) {
		// The sender is a field of `self`, so the channel cannot close while this waits.
		let _ = self.0.subscribe().wait_for(|pending| *pending == 0).await;
	}
}

/// One request counted in [`PendingReleases`] while it lives.
struct PendingRelease(tokio::sync::watch::Sender<usize>);

impl Drop for PendingRelease {
	fn drop(&mut self) {
		self.0.send_modify(|pending| *pending -= 1);
	}
}

/// A release the client may still have to send for the lock `uuid` on `resource`: one being sent,
/// or one a lock request needs should the server grant it to a caller that stopped waiting. It
/// counts in the client's [`PendingReleases`] until [`Self::send`] got an answer or gave up (see
/// [`release`]), or until [`Self::settle`] says none is needed.
///
/// A task that holds one and is cancelled before either, as a runtime shutting down cancels its
/// tasks, drops it unsettled. The release then goes out from a thread of its own, still counted,
/// so the server does not hold the lock until its lease runs out. On wasm, where a task is not
/// cancelled that way (`runtime::spawn_local` holds its worker open), it is not sent and stops
/// counting.
///
/// For a lock request that release is best-effort only. Its task was cancelled with the
/// request's outcome unknown, and the Acquire may still be on its way: a release that reaches the
/// server first releases nothing, and a grant that follows holds the resource until its lease
/// runs out.
struct OwedRelease(Option<ReleaseRequest>);

/// The release an [`OwedRelease`] sends, and its part of the count.
struct ReleaseRequest {
	client: Arc<AuthClient>,
	url: Arc<str>,
	uuid: Uuid,
	resource: Arc<str>,
	confirmed: LeaseConfirmed,
	_pending: PendingRelease,
}

/// When the server last confirmed the lease a release frees, which bounds its retries (see
/// [`release_loop`]).
enum LeaseConfirmed {
	/// A held lease's confirmations, read as they come, so a refresh the keep-alive records after
	/// the drop still extends the retries.
	Watched(tokio::sync::watch::Sender<Confirmed>),
	/// A lock request's grant, which runs from no earlier than the request's send, which comes
	/// after this.
	Sent(Confirmed),
}

impl LeaseConfirmed {
	fn age(&self) -> Duration {
		match self {
			Self::Watched(confirmed) => confirmed.borrow().age(),
			Self::Sent(sent) => sent.age(),
		}
	}
}

impl OwedRelease {
	fn new(
		client: Arc<AuthClient>,
		url: Arc<str>,
		uuid: Uuid,
		resource: Arc<str>,
		confirmed: LeaseConfirmed,
	) -> Self {
		let pending = client.pending_lock_releases.track();
		Self(Some(ReleaseRequest {
			client,
			url,
			uuid,
			resource,
			confirmed,
			_pending: pending,
		}))
	}

	/// Sends the release and waits for the server's answer, or until it is given up on.
	async fn send(mut self) {
		if let Some(request) = &self.0 {
			request.send().await;
		}
		self.0 = None;
	}

	/// No release is needed after all: nothing was granted, or the caller took the grant.
	fn settle(mut self) {
		self.0 = None;
	}
}

impl Drop for OwedRelease {
	fn drop(&mut self) {
		if let Some(request) = self.0.take() {
			request.send_detached();
		}
	}
}

impl ReleaseRequest {
	async fn send(&self) {
		release(&self.client, &self.url, self.uuid, &self.resource, || {
			self.confirmed.age()
		})
		.await;
	}

	/// Sends the release from a new thread, on a runtime of its own that ends with it.
	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	fn send_detached(self) {
		// for the warning should the thread not start, which drops `self`
		let resource = Arc::clone(&self.resource);
		let spawned = std::thread::Builder::new()
			.name("lock-release".to_owned())
			.spawn(move || {
				match tokio::runtime::Builder::new_current_thread()
					.enable_all()
					.build()
				{
					Ok(runtime) => runtime.block_on(self.send()),
					Err(e) => tracing::warn!(
						"No runtime to release lock {} on, relying on server-side expiry: {e}",
						self.resource
					),
				}
			});
		if let Err(e) = spawned {
			tracing::warn!(
				"No thread to release lock {resource} from, relying on server-side expiry: {e}"
			);
		}
	}

	#[cfg(all(target_family = "wasm", target_os = "unknown"))]
	fn send_detached(self) {
		tracing::warn!(
			"The release of lock {} was cancelled, relying on server-side expiry",
			self.resource
		);
	}
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
/// falling back to the runtime the lock was acquired on. When neither is available, or the
/// runtime it went to shuts down before the server answered it, the release is sent from a thread
/// of its own instead. A release failing in transport is sent again while the lease could still
/// stand. Closing the sync engine waits for the releases still in flight, retries included, for a
/// few seconds at most; any other process that exits does not wait for them, so it awaits
/// [`Client::wait_for_lock_releases`] first.
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
	// One [`ResourceLock::confirm`] asks the server at a time; the others wait for its outcome.
	// Holds the error the last one failed with, for those that waited for it.
	confirming: tokio::sync::Mutex<Option<Arc<Error>>>,
	// How many confirmations asked the server so far.
	confirmations: AtomicU64,
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

	/// `Ok` once the lease is confirmed: at once while [`Self::is_valid`], otherwise after
	/// asking the server with a refresh. A lost lease is an [`ErrorKind::LockLost`] error.
	///
	/// A refresh failing in transport ([`ErrorKind::reached_no_server`]) says nothing about the lease, so
	/// the server is asked again, every [`LOCK_RETRY_INTERVAL`], until the lease is confirmed or
	/// lost: no refresh confirmed while the lease could stand loses it, as for the keep-alive. A
	/// refresh failing otherwise is returned as is, without marking the lock lost.
	///
	/// Each refresh is given as long as the lease could still stand, which a newer confirmation
	/// (the keep-alive's) extends, and the wait ends early once a confirmation makes the lease
	/// confirmed or the lock is lost. Concurrent calls share one confirmation: the others wait
	/// for its outcome rather than each asking the server.
	///
	/// An operation that holds the lock across an await calls this right before each write it
	/// sends after that await, so it never writes under a lease another client may hold. A
	/// request sent while the lease is confirmed normally reaches the server inside it: the
	/// confirmation is at most [`LOCK_LOSS_BUDGET`] old against a [`LOCK_SERVER_TTL`] lease, or,
	/// right after the server answered this call's refresh, as old as that round trip.
	pub(crate) async fn confirm(&self) -> Result<(), Error> {
		let lease = &*self.lease;
		let url = lock_url();
		self.confirm_via(|| send_refresh(&lease.client, &url, lease.uuid, &lease.resource))
			.await
	}

	/// [`Self::confirm`] with the refresh round-trip `refresh`, so it can be scripted in tests.
	async fn confirm_via<Fut>(&self, refresh: impl FnMut() -> Fut) -> Result<(), Error>
	where
		Fut: Future<Output = Result<bool, Error>>,
	{
		let lease = &*self.lease;
		if let Some(settled) = lease.settled() {
			return settled;
		}
		let finished = lease.confirmations.load(Ordering::SeqCst);
		let mut last_failure = lease.confirming.lock().await;
		// A confirmation that finished while this one waited answers for it too.
		if let Some(settled) = lease.settled() {
			return settled;
		}
		if lease.confirmations.load(Ordering::SeqCst) != finished
			&& let Some(failure) = &*last_failure
		{
			return Err(Error::unshared(Arc::clone(failure)));
		}
		let outcome = lease.ask(refresh).await;
		lease.confirmations.fetch_add(1, Ordering::SeqCst);
		match outcome {
			Err(error) if error.kind() != ErrorKind::LockLost => {
				let error = Arc::new(error);
				*last_failure = Some(Arc::clone(&error));
				Err(Error::unshared(error))
			}
			outcome => {
				*last_failure = None;
				outcome
			}
		}
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

	fn lost_error(&self) -> Error {
		LockLostError {
			// the error outlives the lock
			resource: self.resource.clone(),
		}
		.into()
	}

	/// What a confirmation is answered with without asking the server: lost, confirmed recently
	/// enough, or lapsed, which loses it. `None` while it is unconfirmed and could still stand.
	fn settled(&self) -> Option<Result<(), Error>> {
		if self.is_lost() {
			return Some(Err(self.lost_error()));
		}
		let age = self.age();
		if age < LOCK_LOSS_BUDGET {
			return Some(Ok(()));
		}
		// Past the lease, a confirmation could not be trusted (see `ask`): it has lapsed.
		if age >= LOCK_SERVER_TTL {
			self.mark_lost();
			return Some(Err(self.lost_error()));
		}
		None
	}

	/// Asks the server whether it still holds the lease, with the refresh round-trip `refresh`,
	/// until the lease is confirmed or lost (see [`ResourceLock::confirm`]). Only the server's
	/// refusal, or the lease's age, loses it: a refresh that is not answered in time is cut off
	/// once the lease could have lapsed, judged by the newest confirmation, the keep-alive's
	/// included.
	async fn ask<Fut>(&self, mut refresh: impl FnMut() -> Fut) -> Result<(), Error>
	where
		Fut: Future<Output = Result<bool, Error>>,
	{
		let mut lost = self.valid.subscribe();
		let mut confirmations = self.confirmed.subscribe();
		loop {
			if let Some(settled) = self.settled() {
				return settled;
			}
			let sent = Confirmed::now();
			let request = refresh();
			let mut request = pin!(request);
			let answer = loop {
				let seen = *confirmations.borrow_and_update();
				// the lease could stand this long, by the newest confirmation
				let remaining = LOCK_SERVER_TTL.saturating_sub(seen.age());
				tokio::select! {
					answer = timeout(remaining, request.as_mut()) => break answer,
					() = lost.wait_for(|valid| !*valid).map(drop) => return Err(self.lost_error()),
					() = confirmations.wait_for(|newest| newest.is_newer_than(&seen)).map(drop) => {
						if let Some(settled) = self.settled() {
							return settled;
						}
					}
				}
			};
			match answer {
				// Only an answer received inside the lease, for a lock not lost meanwhile, is
				// trusted: an older server confirms any refresh, also one for a lease that lapsed
				// and was handed to another client.
				Ok(Ok(true)) if !self.is_lost() && self.age() < LOCK_SERVER_TTL => {
					record_confirmation(&self.confirmed, sent);
					return Ok(());
				}
				Ok(Ok(false)) => {
					self.mark_lost();
					return Err(self.lost_error());
				}
				// Failed in transport: the server may still hold the lease, so it is asked again
				// after a pause, unless a confirmation or the loss arrives first.
				Ok(Err(error)) if error.kind().reached_no_server() => {
					let seen = *confirmations.borrow_and_update();
					tokio::select! {
						() = sleep(LOCK_RETRY_INTERVAL) => {}
						() = lost.wait_for(|valid| !*valid).map(drop) => {}
						() = confirmations.wait_for(|newest| newest.is_newer_than(&seen)).map(drop) => {}
					}
				}
				Ok(Err(error)) => return Err(error),
				// Answered after the loss or past the lease, or unanswered while the lease could
				// stand: the next look at the lease settles it.
				Ok(Ok(true)) | Err(_) => {}
			}
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
		let owed = self.owed_release();
		match handle {
			// Spawning on a handle whose runtime has shut down does not panic: the task is
			// cancelled without being polled, and `owed` sends the release from its own thread.
			Some(handle) => drop(handle.spawn(owed.send())),
			// dropped unsent, `owed` sends the release from its own thread
			None => drop(owed),
		}
	}

	#[cfg(all(target_family = "wasm", target_os = "unknown"))]
	fn spawn_release(&self) {
		let owed = self.owed_release();
		// `runtime::spawn_local` (unlike raw `wasm_bindgen_futures::spawn_local`) holds the
		// spawning SDK worker open until the future completes, so a worker whose other tasks
		// finish cannot close mid-request and silently skip the release. On the main thread it
		// degrades to a plain `spawn_local`.
		crate::runtime::spawn_local(owed.send());
	}

	/// The release of this lease, counted in the client's [`PendingReleases`] from now until it
	/// is answered or given up on (see [`release`]). It reads the lease's confirmations as they
	/// come, so a refresh the keep-alive records after the drop still extends the retries.
	fn owed_release(&self) -> OwedRelease {
		OwedRelease::new(
			Arc::clone(&self.client),
			lock_url().into(),
			self.uuid,
			self.resource.as_str().into(),
			LeaseConfirmed::Watched(self.confirmed.clone()),
		)
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

/// The serialized Release of the lock `uuid` on `resource`. The server only releases a lease
/// `uuid` holds, and every lease is acquired under a uuid of its own, so this body can never
/// free a later lease of the same client, however late it arrives.
fn release_body(uuid: Uuid, resource: &str) -> Result<Bytes, Error> {
	Ok(Bytes::from_owner(serde_json::to_vec(
		&api::v3::user::lock::Request {
			uuid,
			r#type: LockType::Release,
			resource: Cow::Borrowed(resource),
		},
	)?))
}

/// One Release round-trip with the serialized request `body`, sent to `url`: the server's
/// `released` flag.
async fn send_release(client: &AuthClient, url: &str, body: Bytes) -> Result<bool, Error> {
	client
		.post_raw_bytes_auth_lock::<api::v3::user::lock::Response>(
			body,
			url,
			api::v3::user::lock::ENDPOINT.into(),
		)
		.await
		.map(|response| response.released)
}

/// Releases the lock `uuid` on `resource`, sent to `url`, retrying a release that fails in
/// transport while the lease could still stand (see [`release_loop`]). `lease_age` is the time
/// since the server last confirmed the lease.
async fn release(
	client: &AuthClient,
	url: &str,
	uuid: Uuid,
	resource: &str,
	lease_age: impl Fn() -> Duration,
) {
	let body = match release_body(uuid, resource) {
		Ok(body) => body,
		Err(e) => {
			tracing::warn!("Failed to release lock {resource}: {e}");
			return;
		}
	};
	// refcount bump: every attempt resends the same Release body
	release_loop(
		|| send_release(client, url, body.clone()),
		lease_age,
		resource,
	)
	.await;
}

/// The first pause before a failed release is sent again; each further one doubles, up to
/// [`RELEASE_RETRY_MAX`].
const RELEASE_RETRY_FIRST: Duration = Duration::from_millis(250);
/// The longest pause between two attempts of a release.
const RELEASE_RETRY_MAX: Duration = Duration::from_secs(4);

/// Sends a release with `send` (one round trip, resolving to the server's `released` flag)
/// until the server answers it.
///
/// A release failing in transport ([`ErrorKind::reached_no_server`]: no response, an unverified certificate, a timeout, an HTTP
/// error status once the retry layer gave up on it) leaves the lease on the server, refusing
/// every other client on the account until it expires, so it is sent again after a pause
/// growing from [`RELEASE_RETRY_FIRST`] to [`RELEASE_RETRY_MAX`]. Only while the lease could
/// stand: `lease_age` is the time since the server last confirmed it, and once it reaches
/// [`LOCK_SERVER_TTL`] the server has expired the lease on its own, so no retry starts and one
/// in flight is cut off. The first attempt is sent whatever the age, as a lock taken for lost
/// releases its lease in case the clocks overstate the age.
///
/// An answer ends it, `released: false` included (the lease was not held, or not by this
/// uuid), as does any other error: sending it again would be answered the same.
///
/// Generic over the round trip so the schedule is unit-testable on a paused clock.
async fn release_loop<F, Fut>(mut send: F, lease_age: impl Fn() -> Duration, resource: &str)
where
	F: FnMut() -> Fut,
	Fut: Future<Output = Result<bool, Error>>,
{
	let mut pause = RELEASE_RETRY_FIRST;
	let mut answer = send().await;
	loop {
		match answer {
			Ok(true) => {
				debug!("Released lock {resource}");
				return;
			}
			Ok(false) => {
				tracing::warn!("Server held no lease to release for lock {resource}");
				return;
			}
			Err(e) if e.kind().reached_no_server() => {
				// a retry starting past the lease would find it expired
				if LOCK_SERVER_TTL.saturating_sub(lease_age()) <= pause {
					tracing::warn!(
						"Failed to release lock {resource}, the server expires its lease on its own: {e}"
					);
					return;
				}
				tracing::warn!("Failed to release lock {resource}, retrying in {pause:?}: {e}");
			}
			Err(e) => {
				tracing::warn!("Failed to release lock {resource}: {e}");
				return;
			}
		}
		sleep(pause).await;
		pause = min(pause * 2, RELEASE_RETRY_MAX);
		let remaining = LOCK_SERVER_TTL.saturating_sub(lease_age());
		answer = match timeout(remaining, send()).await {
			Ok(answer) => answer,
			Err(_) => {
				tracing::warn!(
					"Release of lock {resource} unanswered within its lease, the server expires it on its own"
				);
				return;
			}
		};
	}
}

const LOCK_REFRESH_INTERVAL: time::Duration = time::Duration::from_secs(15);
/// Server-side lease for a held lock: the server auto-releases a lock 60s after the last
/// acquire/refresh it processed. Measured against the live API: another client got a resource
/// 60s after the holder's acquire, and 90s after an acquire refreshed at 30s.
const LOCK_SERVER_TTL: time::Duration = time::Duration::from_secs(60);
/// How old the last server-confirmed refresh of a held lock may be for the lock to be relied on
/// without asking the server again ([`ResourceLock::is_valid`]). The margin below
/// [`LOCK_SERVER_TTL`] absorbs the send-to-server-processing skew and the time a write takes to
/// reach the server.
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
	/// was marked lost otherwise ([`ResourceLock::confirm`]) — the server-side lease can no
	/// longer be trusted.
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

/// Send one Acquire for `uuid` and return the server's answer — from a task of its own, so a caller
/// that stops waiting (a `select!` that moved on, a dropped future) does not cut the request short.
/// A request cut short can still be granted server-side, and a lease nobody knows of holds the
/// resource for every client until it expires. Whoever sees the answer decides what happens to it:
/// the caller, if it is still waiting, and otherwise the task, which releases what was granted.
/// Only the task can do that safely — a release sent the moment the caller gave up can reach the
/// server before the grant does, and then releases nothing. The task counts in the client's
/// [`PendingReleases`] until it handed the answer over or released the grant, so a process that
/// exits after its caller gave up still waits for that release. A task cancelled before either
/// sends the release from a thread of its own (see [`OwedRelease`]), as the request may have been
/// granted; a release of a lease `uuid` does not hold is a no-op. That release is best-effort: it
/// can reach the server before the Acquire still on its way, release nothing, and leave the grant
/// to hold the resource until its lease runs out.
async fn request_acquire(
	client: Arc<AuthClient>,
	url: Arc<str>,
	body: Bytes,
	uuid: Uuid,
	resource: Arc<str>,
) -> Result<bool, Error> {
	let (answer, answered) = tokio::sync::oneshot::channel();
	// a lease granted to this request runs from no earlier than its send, which comes after this
	let sent = Confirmed::now();
	// Counted for its whole round trip: a grant can still arrive for a caller that is gone.
	let owed = OwedRelease::new(
		Arc::clone(&client),
		Arc::clone(&url),
		uuid,
		resource,
		LeaseConfirmed::Sent(sent),
	);
	// Detached, not cancelled: the task must outlive a caller that stopped waiting.
	drop(crate::runtime::spawn_task_maybe_send(async move {
		let acquired = send_acquire(&client, &url, body).await;
		match answer.send(acquired) {
			Err(Ok(true)) => owed.send().await,
			// the caller took the answer, a grant with it, or nothing was granted
			Ok(()) | Err(Ok(false) | Err(_)) => owed.settle(),
		}
	}));
	answered.await.unwrap_or_else(|_| {
		Err(Error::custom(
			ErrorKind::Internal,
			"the lock request's task ended without an answer",
		))
	})
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
/// A failed request ([`ErrorKind::reached_no_server`]: no response, an unverified certificate, a timeout, or an HTTP error status,
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
					&& e.kind().reached_no_server()
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
		// Shared with each attempt's request task, which can outlive this call (see
		// `request_acquire`).
		let url: Arc<str> = lock_url().into();
		let shared_resource: Arc<str> = resource.as_str().into();
		let client = self.arc_client();
		let acquired_at = acquire_loop(
			|| {
				request_acquire(
					Arc::clone(&client),
					Arc::clone(&url),
					// refcount bump: every poll resends the same Acquire body
					bytes.clone(),
					uuid,
					Arc::clone(&shared_resource),
				)
			},
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
			confirming: tokio::sync::Mutex::new(None),
			confirmations: AtomicU64::new(0),
			#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
			handle: tokio::runtime::Handle::try_current().ok(),
		});
		keep_lock_alive(&lease);
		Ok(Arc::new(ResourceLock { lease }))
	}

	/// Waits, for at most `bound`, until the lock requests this client sent from tasks of their
	/// own are answered: the releases of the leases it let go, and acquisitions whose grant would
	/// be released. A shutdown calls this before the process may exit, which would otherwise cut
	/// them off and leave a lease that blocks every other device on the account until it lapses.
	#[cfg(feature = "cache")]
	pub(crate) async fn settle_lock_releases(&self, bound: Duration) {
		if !self.client().pending_lock_releases.settle(bound).await {
			tracing::warn!(
				"lock requests still unanswered after {bound:?}; the server expires their leases on its own"
			);
		}
	}

	pub async fn acquire_lock_with_default(
		&self,
		resource: impl Into<String>,
	) -> Result<Arc<ResourceLock>, Error> {
		self.acquire_lock(resource, MAX_SLEEP_TIME_DEFAULT, ATTEMPTS_DEFAULT)
			.await
	}
}

/// A held lock whose server-side lease can be lost while it is held: the client's
/// [`ResourceLock`], or a test's fake.
pub(crate) trait Lease: MaybeSendSync {
	/// See [`ResourceLock::confirm`].
	fn confirm(&self) -> impl Future<Output = Result<(), Error>> + MaybeSend;
}

impl Lease for Arc<ResourceLock> {
	fn confirm(&self) -> impl Future<Output = Result<(), Error>> + MaybeSend {
		ResourceLock::confirm(self)
	}
}

/// `None` while the lease of `held` is confirmed; once it is lost, a lock newly taken with
/// `retake`, for the caller to go on under. For a write that must happen whatever became of the
/// held lease: one following a write that already landed (adding a new item to the links and
/// shares it belongs in), or one depending on nothing read under the held lock. Any other
/// failure to confirm or to retake is returned.
pub(crate) async fn confirm_or_retake<L, Fut>(
	held: &L,
	retake: impl FnOnce() -> Fut,
) -> Result<Option<L>, Error>
where
	L: Lease,
	Fut: Future<Output = Result<L, Error>>,
{
	match held.confirm().await {
		Ok(()) => Ok(None),
		Err(error) if error.kind() == ErrorKind::LockLost => {
			tracing::warn!("{error}; taking the lock again to go on");
			retake().await.map(Some)
		}
		Err(error) => Err(error),
	}
}

/// Locks for unit tests that never reach a server.
#[cfg(all(test, not(all(target_family = "wasm", target_os = "unknown"))))]
pub(crate) mod test_support;

#[cfg(test)]
mod tests;
