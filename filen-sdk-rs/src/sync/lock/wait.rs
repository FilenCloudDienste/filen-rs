//! When a lock wait polls again: on its schedule, or early, once the socket's lock
//! announcements say the lock may be free; and the socket a wait keeps open for them once it
//! finds its lock taken.

use std::{cmp::max, convert::Infallible, future::pending, time::Duration};

use tokio::sync::{Notify, watch};

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
use tokio::time::{Instant, sleep_until};
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
use wasmtimer::{std::Instant, tokio::sleep_until};

#[cfg(any(
	not(all(target_family = "wasm", target_os = "unknown")),
	feature = "wasm-full"
))]
use crate::socket::{ListenerHandle, LockTransport};
use crate::{
	Error,
	auth::{Client, http::AuthClient},
};

use super::{LeaseTermsSlot, board};

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
/// poll it pauses a lock lease (the lease length of the client's [`LeaseTerms`](super::LeaseTerms),
/// 60 s unless a server announced a shorter one), or the schedule's delay if that is longer (see
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
	/// The client's lease terms, whose lease length a pause on an announcing connection lasts.
	terms: LeaseTermsSlot,
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
			// a handle on the client's terms
			client.lease_terms.clone(),
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
			// a handle on the client's terms
			terms: client.lease_terms.clone(),
		}
	}

	/// The wakes `wakes` of a wait for `resource`, over the lock transport `transport`, under the
	/// lease terms `terms`.
	#[cfg(any(
		not(all(target_family = "wasm", target_os = "unknown")),
		feature = "wasm-full"
	))]
	pub(super) fn new(
		wakes: watch::Receiver<()>,
		transport: watch::Receiver<LockTransport>,
		resource: &str,
		terms: LeaseTermsSlot,
	) -> Self {
		Self {
			wakes,
			transport: TransportWatch(transport),
			announced: board::announces(resource),
			terms,
		}
	}

	/// How long the wait pauses after a scheduled poll whose schedule says `delay`: a lock lease
	/// (the terms' lease length) while an announcement would wake it, unless `delay` is longer,
	/// and `delay` otherwise. The terms change only when a connection announces its lease length,
	/// which it does before it is published as the transport, so a wait reading them again when
	/// the transport changes is up to date.
	pub(super) fn pause(&self, delay: Duration) -> Duration {
		if self.announced && self.transport.announces() {
			max(delay, self.terms.current().ttl())
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

/// `answer`, the answer to a poll of a lock wait, which notifies `contended` when it is a
/// refusal (see [`while_contended`]).
pub(super) async fn noting_refusal(
	contended: &Notify,
	answer: impl Future<Output = Result<bool, Error>>,
) -> Result<bool, Error> {
	let answer = answer.await;
	if matches!(answer, Ok(false)) {
		// A permit stays stored until the first notification is awaited; later ones add nothing.
		contended.notify_one();
	}
	answer
}

/// Runs the lock wait `wait`, and once `contended` is notified (see [`noting_refusal`]: a poll
/// of it was refused, so another holder may keep it waiting for long) keeps the socket open for
/// the rest of it, for the server's lock announcements to wake it: `open` is called once, and
/// what it resolves to is held until `wait` ends. A wait whose first poll gets the lock opens
/// nothing. A socket that cannot be opened leaves the wait on its schedule.
pub(super) async fn while_contended<T, H, Fut>(
	wait: impl Future<Output = T>,
	contended: &Notify,
	open: impl FnOnce() -> Fut,
) -> T
where
	Fut: Future<Output = Result<Option<H>, Error>>,
{
	let keep_open = async {
		contended.notified().await;
		// held until the wait ends, which drops this future
		let _socket = match open().await {
			Ok(socket) => socket,
			Err(e) => {
				tracing::warn!(
					"Could not open the socket for a contended lock wait, which polls on its schedule: {e}"
				);
				None
			}
		};
		pending::<Infallible>().await
	};
	tokio::select! {
		biased;
		out = wait => out,
		never = keep_open => match never {},
	}
}

/// Keeps the socket open for a contended lock wait of `client` (see [`while_contended`]).
#[cfg(any(
	not(all(target_family = "wasm", target_os = "unknown")),
	feature = "wasm-full"
))]
pub(super) async fn wait_socket(client: &Client) -> Result<Option<ListenerHandle>, Error> {
	client.listen_for_lock_announcements().await
}

/// A build without a socket (`service-worker`) keeps none open for a lock wait.
#[cfg(not(any(
	not(all(target_family = "wasm", target_os = "unknown")),
	feature = "wasm-full"
)))]
pub(super) async fn wait_socket(_client: &Client) -> Result<Option<Infallible>, Error> {
	Ok(None)
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
