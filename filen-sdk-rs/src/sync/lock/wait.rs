//! When a lock wait polls again: on its schedule, or early, once the socket's lock
//! announcements say the lock may be free.

use std::{cmp::max, future::pending, time::Duration};

use tokio::sync::watch;

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
use tokio::time::{Instant, sleep_until};
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
use wasmtimer::{std::Instant, tokio::sleep_until};

#[cfg(any(
	not(all(target_family = "wasm", target_os = "unknown")),
	feature = "wasm-full"
))]
use crate::socket::LockTransport;
use crate::auth::http::AuthClient;

use super::{LOCK_SERVER_TTL, board};

/// The least time between the sends of two polls of one wait when a wake brings the second
/// forward, so a burst of announcements costs a poll per this time at most.
pub(super) const EARLY_POLL_FLOOR: Duration = Duration::from_millis(250);

/// What a lock wait for one resource is woken by, and how long its scheduled pauses are.
///
/// A wait is woken when a lease of its resource was announced ended, or a connection was
/// authenticated, as what was announced before it was not delivered. A wake brings a poll
/// forward and leaves the schedule as it was.
///
/// On a connection that delivers the lock announcements (its server answered the `lockHello`
/// probe with protocol 2 or later), the wake is what gets the wait the lock: after each scheduled
/// poll it pauses a lock lease (60 s), or the schedule's delay if that is longer (see
/// [`Self::pause`]),
/// and that one poll a lease is the only safety net against a lost announcement. A server that
/// answers the probe but delivers no announcements leaves every such wait polling once a lease.
/// Without announcements (no socket, a server that predates them, a probe that went unanswered,
/// a resource the server never announces, the service-worker build), a wait polls on the schedule
/// it was given. A wait follows the transport as it changes, in both directions.
pub(super) struct LockWakes {
	wakes: watch::Receiver<()>,
	transport: TransportWatch,
	/// Whether the server announces the resource's leases at all (see [`board::announces`]).
	announced: bool,
}

/// What the next poll of a wait is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum NextPoll {
	/// The scheduled one: its pause ran out.
	Scheduled,
	/// One a wake brought forward. The schedule stays as it was.
	Early,
}

impl LockWakes {
	/// The wakes of `client`'s waits for `resource`.
	#[cfg(any(
		not(all(target_family = "wasm", target_os = "unknown")),
		feature = "wasm-full"
	))]
	pub(super) fn for_client(client: &AuthClient, resource: &str) -> Self {
		Self::new(
			client.lock_board.wakes(resource),
			client.lock_transport.subscribe(),
			resource,
		)
	}

	/// The wakes of `client`'s waits for `resource`, in a build without a socket: no
	/// announcement ever arrives, so the schedule is all there is.
	#[cfg(not(any(
		not(all(target_family = "wasm", target_os = "unknown")),
		feature = "wasm-full"
	)))]
	pub(super) fn for_client(client: &AuthClient, resource: &str) -> Self {
		Self {
			wakes: client.lock_board.wakes(resource),
			transport: TransportWatch,
			announced: board::announces(resource),
		}
	}

	/// The wakes `wakes` of a wait for `resource`, over the lock transport `transport`.
	#[cfg(any(
		not(all(target_family = "wasm", target_os = "unknown")),
		feature = "wasm-full"
	))]
	pub(super) fn new(
		wakes: watch::Receiver<()>,
		transport: watch::Receiver<LockTransport>,
		resource: &str,
	) -> Self {
		Self {
			wakes,
			transport: TransportWatch(transport),
			announced: board::announces(resource),
		}
	}

	/// How long the wait pauses after a scheduled poll whose schedule says `delay`: a lock lease
	/// ([`LOCK_SERVER_TTL`]) while an announcement would wake it, unless `delay` is longer, and
	/// `delay` otherwise.
	pub(super) fn pause(&self, delay: Duration) -> Duration {
		if self.announced && self.transport.announces() {
			max(delay, LOCK_SERVER_TTL)
		} else {
			delay
		}
	}

	/// Forgets the wakes so far: a poll is about to be sent, and only what happens from now on
	/// can free the lock after the server answers it.
	pub(super) fn polling(&mut self) {
		self.wakes.mark_unchanged();
	}

	/// Waits for the next poll of a wait whose pause after its last scheduled poll began at
	/// `paused_at`, with `delay` its scheduled delay (see [`Self::pause`], which follows the
	/// transport as it changes), and whose last poll was sent at `last_sent`.
	pub(super) async fn next_poll(
		&mut self,
		paused_at: Instant,
		delay: Duration,
		last_sent: Instant,
	) -> NextPoll {
		// a closed channel: the board is gone with its client, and no wake will come
		let mut woken = self.wakes.has_changed().unwrap_or(false);
		loop {
			// `None` past what the clock can hold: as good as never
			let due = paused_at.checked_add(self.pause(delay));
			let early = woken
				.then(|| last_sent + EARLY_POLL_FLOOR)
				.filter(|early| due.is_none_or(|due| *early < due));
			let Self {
				wakes, transport, ..
			} = self;
			tokio::select! {
				() = sleep_until_or_never(early.or(due)) => {
					return if early.is_some() {
						NextPoll::Early
					} else {
						NextPoll::Scheduled
					};
				}
				() = changed_or_never(wakes), if !woken => woken = true,
				() = transport.changed() => {}
			}
		}
	}
}

async fn sleep_until_or_never(at: Option<Instant>) {
	match at {
		Some(at) => sleep_until(at).await,
		None => pending().await,
	}
}

/// Resolves when `receiver` sees a change; never once its sender is gone.
async fn changed_or_never<T>(receiver: &mut watch::Receiver<T>) {
	if receiver.changed().await.is_err() {
		pending::<()>().await;
	}
}

/// The client's lock transport, as the websocket task publishes it.
#[cfg(any(
	not(all(target_family = "wasm", target_os = "unknown")),
	feature = "wasm-full"
))]
struct TransportWatch(watch::Receiver<LockTransport>);

#[cfg(any(
	not(all(target_family = "wasm", target_os = "unknown")),
	feature = "wasm-full"
))]
impl TransportWatch {
	fn announces(&self) -> bool {
		self.0.borrow().announces()
	}

	async fn changed(&mut self) {
		changed_or_never(&mut self.0).await;
	}
}

/// A build without a socket (`service-worker`): no announcement ever arrives.
#[cfg(not(any(
	not(all(target_family = "wasm", target_os = "unknown")),
	feature = "wasm-full"
)))]
struct TransportWatch;

#[cfg(not(any(
	not(all(target_family = "wasm", target_os = "unknown")),
	feature = "wasm-full"
)))]
impl TransportWatch {
	fn announces(&self) -> bool {
		false
	}

	async fn changed(&mut self) {
		pending::<()>().await;
	}
}
