//! Whether the socket can carry lock operations: each connection probes the server with
//! `lockHello` once it is authenticated, and the outcome is published on the client.

use std::{
	fmt::Display,
	ops::RangeInclusive,
	sync::{
		Arc,
		atomic::{AtomicU64, Ordering},
	},
	time::Duration,
};

use filen_types::api::v3::socket::LockHello;
use tokio::sync::watch;

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
use tokio::time::Instant;
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
use wasmtimer::std::Instant;

use super::{
	acks::{Ack, PendingAcks, Settled},
	consts::{LOCK_HELLO_TIMEOUT, MESSAGE_EVENT_PAYLOAD},
};

/// The socket lock protocol versions this client speaks: `1`, and `2`, which adds the lock
/// announcements.
const LOCK_PROTOCOL_VERSIONS: RangeInclusive<u64> = 1..=2;

/// How lock operations can travel right now, as the websocket task last published it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LockTransport {
	/// No websocket task: none was started, or it ended.
	None,
	/// The websocket task is connecting, or waiting to reconnect.
	Connecting,
	/// Connection `generation` is authenticated and waits for the `lockHello` ack.
	Probing { generation: u64 },
	/// Connection `generation` speaks socket lock protocol `version`, with leases of `ttl` as
	/// the server announced it (not clamped).
	Socket {
		generation: u64,
		version: u64,
		ttl: Duration,
	},
	/// Connection `generation` cannot carry lock operations: its server did not answer the
	/// probe in time, or answered with a version this client does not speak.
	Http { generation: u64 },
}

/// The client's [`LockTransport`], published by one websocket task at a time.
///
/// A task publishes through the [`LockTransportClaim`] it was spawned with, and only the task
/// claimed last is heard. An older task can still be running when its successor starts (the
/// last strong request sender was dropped while it still had requests to serve, or it is
/// draining its teardown), and whatever it publishes from then on would describe a connection
/// that no longer carries anything.
#[derive(Default)]
pub(crate) struct LockTransportSlot {
	// Shared with the claims: each one lives in a spawned task (reason: a `'static` boundary).
	shared: Arc<SlotState>,
}

struct SlotState {
	current: watch::Sender<LockTransport>,
	/// The count of claims so far; the latest claim's number is the value itself.
	claims: AtomicU64,
	/// The count of connections so far. Connections are numbered across websocket tasks, so a
	/// task started after another never reuses a generation.
	generations: AtomicU64,
}

impl Default for SlotState {
	fn default() -> Self {
		Self {
			current: watch::Sender::new(LockTransport::None),
			claims: AtomicU64::new(0),
			generations: AtomicU64::new(0),
		}
	}
}

impl LockTransportSlot {
	/// Claims the slot for a websocket task about to be spawned, publishing that it connects:
	/// from here on nothing a task claimed earlier publishes is heard.
	pub(super) fn claim(&self) -> LockTransportClaim {
		let claim = LockTransportClaim {
			slot: Arc::clone(&self.shared),
			number: self.shared.claims.fetch_add(1, Ordering::AcqRel) + 1,
		};
		claim.publish(LockTransport::Connecting);
		claim
	}

	/// Watches the transport, starting from the current one.
	// Read only by the test seam so far: nothing routes lock operations by it yet.
	#[cfg_attr(
		not(any(test, feature = "test-seams")),
		expect(dead_code, reason = "no lock operation reads the transport yet")
	)]
	pub(crate) fn subscribe(&self) -> watch::Receiver<LockTransport> {
		self.shared.current.subscribe()
	}
}

/// One websocket task's right to publish its [`LockTransport`], while no later task claimed the
/// slot. Dropping it publishes [`LockTransport::None`] under the same condition: at the task's
/// end, however it ends, or with the task's input when a worker that never started is given up
/// on.
pub(super) struct LockTransportClaim {
	slot: Arc<SlotState>,
	number: u64,
}

impl LockTransportClaim {
	pub(super) fn publish(&self, transport: LockTransport) {
		self.slot.current.send_if_modified(|current| {
			// Read under the channel's lock: a later claim either is seen here, or publishes
			// after this does.
			if self.slot.claims.load(Ordering::Acquire) != self.number {
				return false;
			}
			*current = transport;
			true
		});
	}

	fn next_generation(&self) -> u64 {
		self.slot.generations.fetch_add(1, Ordering::Relaxed) + 1
	}
}

impl Drop for LockTransportClaim {
	fn drop(&mut self) {
		self.publish(LockTransport::None);
	}
}

/// What an ack of this connection answers.
enum AckWaiter {
	LockHello,
}

/// One connection's lock state: its generation, and the acks it waits for.
pub(super) struct ConnectionLocks<'a> {
	claim: &'a LockTransportClaim,
	generation: u64,
	pending: PendingAcks<AckWaiter>,
}

impl<'a> ConnectionLocks<'a> {
	/// Numbers a new authenticated connection, publishes it as probing, and sends the
	/// `lockHello` probe through `send`. A probe that cannot be sent leaves the connection on
	/// HTTP.
	pub(super) fn probe<E: Display>(
		claim: &'a LockTransportClaim,
		now: Instant,
		send: impl FnOnce(String) -> Result<(), E>,
	) -> Self {
		let generation = claim.next_generation();
		let mut locks = Self {
			claim,
			generation,
			pending: PendingAcks::default(),
		};
		claim.publish(LockTransport::Probing { generation });
		let Some(id) = locks
			.pending
			.insert(AckWaiter::LockHello, now + LOCK_HELLO_TIMEOUT)
		else {
			// A fresh connection has no pending acks.
			locks.use_http();
			return locks;
		};
		if let Err(e) = send(format!(r#"{MESSAGE_EVENT_PAYLOAD}{id}["lockHello"]"#)) {
			tracing::warn!("Could not send the lockHello probe ({e}), using HTTP for locks");
			locks.pending.settle(id);
			locks.use_http();
		}
		locks
	}

	/// Hands `ack` to whoever waits for it.
	pub(super) fn on_ack(&mut self, ack: &Ack<'_>) {
		match self.pending.settle(ack.id()) {
			Settled::Answered(AckWaiter::LockHello) => self.on_lock_hello(ack),
			Settled::Stale => {
				tracing::debug!("Ignoring the stale ack {}", ack.id());
			}
			Settled::Unknown => {
				tracing::debug!("Ignoring an ack for no pending event ({})", ack.id());
			}
		}
	}

	fn on_lock_hello(&mut self, ack: &Ack<'_>) {
		let hello = match ack.single_arg::<LockHello>() {
			Ok(hello) => hello,
			Err(e) => {
				tracing::warn!("Malformed lockHello answer ({e}), using HTTP for locks");
				self.use_http();
				return;
			}
		};
		if !LOCK_PROTOCOL_VERSIONS.contains(&hello.version) {
			tracing::debug!(
				"The server speaks socket lock protocol {}, using HTTP for locks",
				hello.version
			);
			self.use_http();
			return;
		}
		tracing::debug!(
			"Socket locks available: protocol {}, lease {} s",
			hello.version,
			hello.ttl_seconds
		);
		self.claim.publish(LockTransport::Socket {
			generation: self.generation,
			version: hello.version,
			ttl: Duration::from_secs(hello.ttl_seconds),
		});
	}

	/// The earliest deadline of an ack this connection waits for.
	pub(super) fn next_deadline(&self) -> Option<Instant> {
		self.pending.next_deadline()
	}

	/// Gives up on every ack due by `now`.
	pub(super) fn expire(&mut self, now: Instant) {
		for waiter in self.pending.expire(now) {
			match waiter {
				AckWaiter::LockHello => {
					tracing::debug!(
						"No lockHello answer within {LOCK_HELLO_TIMEOUT:?}, using HTTP for locks"
					);
					self.use_http();
				}
			}
		}
	}

	/// The connection ended: lock operations wait for the next one.
	pub(super) fn disconnected(self) {
		self.claim.publish(LockTransport::Connecting);
	}

	fn use_http(&self) {
		self.claim.publish(LockTransport::Http {
			generation: self.generation,
		});
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::socket::acks::parse_ack;

	fn current(slot: &LockTransportSlot) -> LockTransport {
		*slot.subscribe().borrow()
	}

	/// Probes a fresh connection, returning the frames it sent.
	fn probed(claim: &LockTransportClaim) -> (ConnectionLocks<'_>, Vec<String>) {
		let mut sent = Vec::new();
		let locks = ConnectionLocks::probe(claim, Instant::now(), |frame| {
			sent.push(frame);
			Ok::<_, &str>(())
		});
		(locks, sent)
	}

	fn answer(locks: &mut ConnectionLocks<'_>, frame: &str) {
		locks.on_ack(&parse_ack(frame).unwrap().unwrap());
	}

	#[tokio::test(start_paused = true)]
	async fn a_probe_is_sent_with_the_first_id_and_published_as_probing() {
		let slot = LockTransportSlot::default();
		assert_eq!(current(&slot), LockTransport::None);
		let claim = slot.claim();
		assert_eq!(current(&slot), LockTransport::Connecting);

		let (_locks, sent) = probed(&claim);
		assert_eq!(sent, [r#"420["lockHello"]"#]);
		assert_eq!(current(&slot), LockTransport::Probing { generation: 1 });
	}

	#[tokio::test(start_paused = true)]
	async fn the_answered_version_decides_the_transport() {
		let socket = |version| LockTransport::Socket {
			generation: 1,
			version,
			ttl: Duration::from_secs(60),
		};
		let http = LockTransport::Http { generation: 1 };
		let cases = [
			(r#"430[{"version":1,"ttlSeconds":60}]"#, socket(1)),
			(r#"430[{"version":2,"ttlSeconds":60}]"#, socket(2)),
			(r#"430[{"version":"2","ttlSeconds":"60"}]"#, socket(2)),
			(r#"430[{"version":0,"ttlSeconds":60}]"#, http),
			(r#"430[{"version":3,"ttlSeconds":60}]"#, http),
			(r#"430[{"version":2}]"#, http),
			(r#"430[{"version":2,"ttlSeconds":60},1]"#, http),
			("430[]", http),
		];
		for (frame, expected) in cases {
			let slot = LockTransportSlot::default();
			let claim = slot.claim();
			let (mut locks, _) = probed(&claim);
			answer(&mut locks, frame);
			assert_eq!(current(&slot), expected, "{frame}");
		}
	}

	#[tokio::test(start_paused = true)]
	async fn an_unanswered_probe_falls_back_to_http_at_its_deadline() {
		let begin = Instant::now();
		let slot = LockTransportSlot::default();
		let claim = slot.claim();
		let (mut locks, _) = probed(&claim);
		assert_eq!(locks.next_deadline(), Some(begin + Duration::from_secs(5)));

		locks.expire(begin + Duration::from_millis(4_999));
		assert_eq!(current(&slot), LockTransport::Probing { generation: 1 });
		locks.expire(begin + Duration::from_secs(5));
		assert_eq!(current(&slot), LockTransport::Http { generation: 1 });
		assert_eq!(locks.next_deadline(), None);

		// The answer that comes too late changes nothing.
		answer(&mut locks, r#"430[{"version":2,"ttlSeconds":60}]"#);
		assert_eq!(current(&slot), LockTransport::Http { generation: 1 });
	}

	#[tokio::test(start_paused = true)]
	async fn an_ack_for_no_pending_event_changes_nothing() {
		let slot = LockTransportSlot::default();
		let claim = slot.claim();
		let (mut locks, _) = probed(&claim);
		answer(&mut locks, r#"431[{"version":2,"ttlSeconds":60}]"#);
		assert_eq!(current(&slot), LockTransport::Probing { generation: 1 });
	}

	#[tokio::test(start_paused = true)]
	async fn a_probe_that_cannot_be_sent_leaves_the_connection_on_http() {
		let slot = LockTransportSlot::default();
		let claim = slot.claim();
		let locks = ConnectionLocks::probe(&claim, Instant::now(), |_| Err("queue full"));
		assert_eq!(current(&slot), LockTransport::Http { generation: 1 });
		assert_eq!(locks.next_deadline(), None);
	}

	#[tokio::test(start_paused = true)]
	async fn each_connection_gets_the_next_generation_and_ids_restart_at_zero() {
		let slot = LockTransportSlot::default();
		let claim = slot.claim();
		let (first, _) = probed(&claim);
		first.disconnected();
		assert_eq!(current(&slot), LockTransport::Connecting);

		let (mut second, sent) = probed(&claim);
		assert_eq!(sent, [r#"420["lockHello"]"#]);
		assert_eq!(current(&slot), LockTransport::Probing { generation: 2 });
		answer(&mut second, r#"430[{"version":2,"ttlSeconds":60}]"#);
		assert_eq!(
			current(&slot),
			LockTransport::Socket {
				generation: 2,
				version: 2,
				ttl: Duration::from_secs(60)
			}
		);
	}

	#[test]
	fn dropping_the_latest_claim_publishes_none() {
		let slot = LockTransportSlot::default();
		let claim = slot.claim();
		drop(claim);
		assert_eq!(current(&slot), LockTransport::None);
	}

	/// A task that ends after its successor was spawned, here with a connection that spoke
	/// socket locks, publishes neither the loss of its connection nor its end over what the
	/// successor publishes.
	#[tokio::test(start_paused = true)]
	async fn a_superseded_claim_publishes_nothing() {
		let slot = LockTransportSlot::default();
		let ending = slot.claim();
		let (mut first, _) = probed(&ending);
		answer(&mut first, r#"430[{"version":2,"ttlSeconds":60}]"#);

		let successor = slot.claim();
		assert_eq!(current(&slot), LockTransport::Connecting);
		let (mut second, _) = probed(&successor);
		answer(&mut second, r#"430[{"version":1,"ttlSeconds":30}]"#);
		let live = LockTransport::Socket {
			generation: 2,
			version: 1,
			ttl: Duration::from_secs(30),
		};
		assert_eq!(current(&slot), live);

		first.disconnected();
		assert_eq!(current(&slot), live);
		drop(ending);
		assert_eq!(current(&slot), live);

		second.disconnected();
		assert_eq!(current(&slot), LockTransport::Connecting);
		drop(successor);
		assert_eq!(current(&slot), LockTransport::None);
	}
}
