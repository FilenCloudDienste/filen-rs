//! Whether the socket can carry lock operations: each connection probes the server with
//! `lockHello` once it is authenticated, and the outcome is published on the client. A connection
//! that speaks socket locks publishes a channel with it, which hands it lock operations to send
//! as `lock` events until it ends or stops carrying them.

use std::{
	fmt::Display,
	mem,
	ops::RangeInclusive,
	sync::{
		Arc,
		atomic::{AtomicU64, Ordering},
	},
	time::Duration,
};

use filen_types::api::v3::socket::LockHello;
use tokio::sync::{mpsc, oneshot, watch};

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
use tokio::time::{Instant, timeout};
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
use wasmtimer::{std::Instant, tokio::timeout};

use crate::sync::lock::LockPayload;

use super::{
	acks::{Ack, AckArgs, PendingAcks, Settled},
	consts::{
		LOCK_ACK_TIMEOUT, LOCK_DEGRADE_AFTER, LOCK_HELLO_TIMEOUT, LOCK_MAX_IN_FLIGHT,
		LOCK_REPLY_TIMEOUT, MESSAGE_EVENT_PAYLOAD,
	},
};

/// The socket lock protocol versions this client speaks: `1`, and `2`, which adds the lock
/// announcements.
const LOCK_PROTOCOL_VERSIONS: RangeInclusive<u64> = 1..=2;

/// The first socket lock protocol version whose connections deliver the lock announcements.
const LOCK_ANNOUNCEMENTS_VERSION: u64 = 2;

/// How lock operations can travel right now, as the websocket task last published it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LockTransport {
	/// No websocket task: none was started, or it ended.
	None,
	/// The websocket task is connecting, or waiting to reconnect.
	Connecting,
	/// Connection `generation` is authenticated and waits for the `lockHello` ack.
	Probing { generation: u64 },
	/// Connection `generation` speaks socket lock protocol `version`, with leases of `ttl` as
	/// the server announced it (not clamped), and sends the lock operations handed to `commands`.
	Socket {
		generation: u64,
		version: u64,
		ttl: Duration,
		commands: LockCommands,
	},
	/// Connection `generation` cannot carry lock operations: its server did not answer the
	/// probe in time, or answered with a version this client does not speak.
	Http { generation: u64 },
}

impl LockTransport {
	/// Whether the connection delivers the lock announcements: its server answered the probe
	/// with a protocol version that has them. A connection whose probe went unanswered may
	/// deliver them too, but nothing says it does.
	pub(crate) fn announces(&self) -> bool {
		match self {
			Self::Socket { version, .. } => *version >= LOCK_ANNOUNCEMENTS_VERSION,
			Self::None | Self::Connecting | Self::Probing { .. } | Self::Http { .. } => false,
		}
	}
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

	/// Watches the transport, starting from the current one: a lock wait follows it, as only a
	/// connection that delivers the lock announcements wakes the wait.
	pub(crate) fn subscribe(&self) -> watch::Receiver<LockTransport> {
		self.shared.current.subscribe()
	}

	/// The transport as it is now.
	pub(crate) fn current(&self) -> LockTransport {
		// a channel handle at most: a refcount bump
		self.shared.current.borrow().clone()
	}
}

/// What a connection that carries lock operations is handed through its [`LockCommands`].
#[derive(Debug)]
pub(crate) enum LockCommand {
	/// Send `payload` as a `lock` event, and tell `reply` what became of it.
	Send {
		payload: LockPayload,
		reply: oneshot::Sender<LockReply>,
	},
	/// The server no longer accepts the API key the connection authenticated with (a password
	/// change rotated it, or it was revoked): end the connection, so the next one authenticates
	/// with the client's current key.
	Reauthenticate,
}

/// What became of a lock operation handed to a connection.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum LockReply {
	/// The server's ack, not parsed.
	Answered(AckArgs),
	/// Sent, but its ack did not come in time, or the connection ended first: the server may
	/// have run it, or may still run it, or never will.
	Unknown,
	/// Not sent, as the connection has as many `lock` events unanswered as the server takes on
	/// one socket: nothing was done.
	Busy,
	/// Not sent, as the connection stopped carrying lock operations or could not queue the event:
	/// nothing was done.
	NotSent,
}

/// The way lock operations reach a connection that carries them, published with it as
/// [`LockTransport::Socket`]. Once the connection ends or stops carrying them, nothing handed
/// over is sent: lock operations are never buffered for a later connection.
#[derive(Debug, Clone)]
pub(crate) struct LockCommands(mpsc::Sender<LockCommand>);

impl PartialEq for LockCommands {
	fn eq(&self, other: &Self) -> bool {
		self.0.same_channel(&other.0)
	}
}

impl Eq for LockCommands {}

impl LockCommands {
	/// A connection's commands, and the receiving end it reads them from.
	fn channel() -> (Self, mpsc::Receiver<LockCommand>) {
		let (sender, receiver) = mpsc::channel(LOCK_MAX_IN_FLIGHT);
		(Self(sender), receiver)
	}

	/// Hands `payload` to the connection to send as a `lock` event, and waits for what became of
	/// it: for at most `wait`, and never longer than [`LOCK_REPLY_TIMEOUT`], since the connection
	/// gives up on the ack after [`LOCK_ACK_TIMEOUT`]. `None` waits as long as the connection
	/// does.
	///
	/// An operation that stops waiting first is [`LockReply::Unknown`]: the connection still sends
	/// it, in its turn, and counts its ack as late if it does not come in time.
	pub(crate) async fn send(&self, payload: LockPayload, wait: Option<Duration>) -> LockReply {
		let (reply, replied) = oneshot::channel();
		// Not waiting for room: the connection takes its commands as they come, so a full channel
		// is one that has as many as the server takes already.
		if self
			.0
			.try_send(LockCommand::Send { payload, reply })
			.is_err()
		{
			return LockReply::NotSent;
		}
		let wait = wait.map_or(LOCK_REPLY_TIMEOUT, |wait| wait.min(LOCK_REPLY_TIMEOUT));
		match timeout(wait, replied).await {
			Ok(Ok(reply)) => reply,
			// Dropped unanswered, as the connection's task ended, or not answered in time: the
			// event may have been sent, or may still be.
			Ok(Err(_)) | Err(_) => LockReply::Unknown,
		}
	}

	/// Asks the connection to end, so the next one authenticates with the client's current API
	/// key. Not waiting for room: a connection with a full channel will be asked again by the next
	/// operation the server refuses for its key.
	pub(crate) fn reauthenticate(&self) {
		let _ = self.0.try_send(LockCommand::Reauthenticate);
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
	/// A `lock` event, for the operation that waits for it.
	Lock(oneshot::Sender<LockReply>),
}

/// One connection's lock state: its generation, the acks it waits for, and the lock operations
/// handed to it.
pub(super) struct ConnectionLocks<'a> {
	claim: &'a LockTransportClaim,
	generation: u64,
	pending: PendingAcks<AckWaiter>,
	/// Where the lock operations handed to this connection arrive: open from the probe finding
	/// socket locks until the connection stops carrying them.
	commands: Option<mpsc::Receiver<LockCommand>>,
	/// How many `lock` acks did not come in time.
	late_acks: usize,
}

/// What the connection's loop does after a command.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum CommandOutcome {
	/// Go on.
	Served,
	/// End the connection: the server rejects its API key ([`LockCommand::Reauthenticate`]).
	Reauthenticate,
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
			commands: None,
			late_acks: 0,
		};
		claim.publish(LockTransport::Probing { generation });
		let Ok(id) = locks
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
			Settled::Answered(AckWaiter::Lock(reply)) => {
				// An operation that stopped waiting dropped its end: nothing to tell.
				let _ = reply.send(LockReply::Answered(ack.to_owned_args()));
			}
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
		let (commands, receiver) = LockCommands::channel();
		self.commands = Some(receiver);
		self.claim.publish(LockTransport::Socket {
			generation: self.generation,
			version: hello.version,
			ttl: Duration::from_secs(hello.ttl_seconds),
			commands,
		});
	}

	/// Whether lock operations can be handed to this connection: [`Self::next_command`] has
	/// something to wait for.
	pub(super) fn takes_commands(&self) -> bool {
		self.commands.is_some()
	}

	/// The next command handed to this connection, `None` once nobody can hand it one any more.
	/// Pends forever while it takes none ([`Self::takes_commands`]).
	pub(super) async fn next_command(&mut self) -> Option<LockCommand> {
		match &mut self.commands {
			Some(commands) => commands.recv().await,
			None => std::future::pending().await,
		}
	}

	/// Serves `command` ([`Self::next_command`]): a lock operation is sent through `send` as a
	/// `lock` event, due by `now` plus [`LOCK_ACK_TIMEOUT`].
	pub(super) fn on_command<E: Display>(
		&mut self,
		command: Option<LockCommand>,
		now: Instant,
		send: impl FnOnce(String) -> Result<(), E>,
	) -> CommandOutcome {
		match command {
			// Every handle was dropped: the transport moved on.
			None => self.commands = None,
			Some(LockCommand::Reauthenticate) => return CommandOutcome::Reauthenticate,
			Some(LockCommand::Send { payload, reply }) => {
				self.send_lock(&payload, reply, now, send)
			}
		}
		CommandOutcome::Served
	}

	fn send_lock<E: Display>(
		&mut self,
		payload: &LockPayload,
		reply: oneshot::Sender<LockReply>,
		now: Instant,
		send: impl FnOnce(String) -> Result<(), E>,
	) {
		// The acks given up on count: the server counts their events until it answers them.
		if self.pending.len() >= LOCK_MAX_IN_FLIGHT {
			let _ = reply.send(LockReply::Busy);
			return;
		}
		let id = match self
			.pending
			.insert(AckWaiter::Lock(reply), now + LOCK_ACK_TIMEOUT)
		{
			Ok(id) => id,
			Err(waiter) => {
				// Every id is taken, which takes far more than `LOCK_MAX_IN_FLIGHT` acks.
				waiter.not_sent();
				return;
			}
		};
		let frame = format!(r#"{MESSAGE_EVENT_PAYLOAD}{id}["lock",{}]"#, payload.json());
		if let Err(e) = send(frame) {
			tracing::warn!("Could not queue a lock event ({e}), sending it over HTTP");
			if let Settled::Answered(waiter) = self.pending.settle(id) {
				waiter.not_sent();
			}
		}
	}

	/// The earliest deadline of an ack this connection waits for.
	pub(super) fn next_deadline(&self) -> Option<Instant> {
		self.pending.next_deadline()
	}

	/// Gives up on every ack due by `now`. A connection that gave up on [`LOCK_DEGRADE_AFTER`]
	/// `lock` acks uses HTTP for locks from then on.
	pub(super) fn expire(&mut self, now: Instant) {
		for waiter in self.pending.expire(now) {
			match waiter {
				AckWaiter::LockHello => {
					tracing::debug!(
						"No lockHello answer within {LOCK_HELLO_TIMEOUT:?}, using HTTP for locks"
					);
					self.use_http();
				}
				AckWaiter::Lock(reply) => {
					let _ = reply.send(LockReply::Unknown);
					self.late_acks = self.late_acks.saturating_add(1);
				}
			}
		}
		if self.late_acks >= LOCK_DEGRADE_AFTER && self.commands.is_some() {
			tracing::warn!(
				"{} lock acks did not come within {LOCK_ACK_TIMEOUT:?}, using HTTP for locks",
				self.late_acks
			);
			self.use_http();
			self.stop_commands();
		}
	}

	/// The connection ended: lock operations wait for the next one. Published first, so nothing
	/// counts on this connection any more, then every operation it holds is told it ended: one
	/// whose ack it waited for may have run, one not sent yet did not.
	pub(super) fn disconnected(mut self) {
		self.claim.publish(LockTransport::Connecting);
		for waiter in mem::take(&mut self.pending).into_waiters() {
			match waiter {
				AckWaiter::LockHello => {}
				AckWaiter::Lock(reply) => {
					let _ = reply.send(LockReply::Unknown);
				}
			}
		}
		self.stop_commands();
	}

	/// Takes no further commands, and tells every one handed over and not served yet that it was
	/// not sent.
	fn stop_commands(&mut self) {
		let Some(mut commands) = self.commands.take() else {
			return;
		};
		commands.close();
		while let Ok(command) = commands.try_recv() {
			match command {
				LockCommand::Send { reply, .. } => {
					let _ = reply.send(LockReply::NotSent);
				}
				LockCommand::Reauthenticate => {}
			}
		}
	}

	fn use_http(&self) {
		self.claim.publish(LockTransport::Http {
			generation: self.generation,
		});
	}
}

impl AckWaiter {
	/// The event was not sent after all.
	fn not_sent(self) {
		match self {
			Self::LockHello => {}
			Self::Lock(reply) => {
				let _ = reply.send(LockReply::NotSent);
			}
		}
	}
}

#[cfg(test)]
pub(crate) mod test_support {
	pub(crate) use super::LockCommand;
	use super::*;

	/// A transport as a test compares it: a socket one without its channel.
	#[derive(Debug, Clone, Copy, PartialEq, Eq)]
	pub(crate) enum Seen {
		None,
		Connecting,
		Probing {
			generation: u64,
		},
		Socket {
			generation: u64,
			version: u64,
			ttl: Duration,
		},
		Http {
			generation: u64,
		},
	}

	impl From<&LockTransport> for Seen {
		fn from(transport: &LockTransport) -> Self {
			match *transport {
				LockTransport::None => Self::None,
				LockTransport::Connecting => Self::Connecting,
				LockTransport::Probing { generation } => Self::Probing { generation },
				LockTransport::Socket {
					generation,
					version,
					ttl,
					commands: _,
				} => Self::Socket {
					generation,
					version,
					ttl,
				},
				LockTransport::Http { generation } => Self::Http { generation },
			}
		}
	}

	/// The commands of a connection a test plays, and the end the test reads them from.
	pub(crate) fn scripted_commands() -> (LockCommands, mpsc::Receiver<LockCommand>) {
		LockCommands::channel()
	}

	/// A transport a test published on a client, as its websocket task would, until dropped,
	/// which publishes [`LockTransport::None`].
	pub(crate) struct Published {
		_claim: LockTransportClaim,
	}

	pub(crate) fn publish(slot: &LockTransportSlot, transport: LockTransport) -> Published {
		let claim = slot.claim();
		claim.publish(transport);
		Published { _claim: claim }
	}
}

#[cfg(test)]
mod tests {
	use std::borrow::Cow;

	use filen_types::{
		api::v3::user::lock::{self, LockType},
		fs::Uuid,
	};
	use tokio::sync::oneshot::error::TryRecvError;

	use super::{test_support::Seen, *};
	use crate::socket::acks::{parse_ack, test_support::ack_args};

	fn current(slot: &LockTransportSlot) -> Seen {
		Seen::from(&*slot.subscribe().borrow())
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
		assert_eq!(current(&slot), Seen::None);
		let claim = slot.claim();
		assert_eq!(current(&slot), Seen::Connecting);

		let (_locks, sent) = probed(&claim);
		assert_eq!(sent, [r#"420["lockHello"]"#]);
		assert_eq!(current(&slot), Seen::Probing { generation: 1 });
	}

	#[tokio::test(start_paused = true)]
	async fn the_answered_version_decides_the_transport() {
		let socket = |version| Seen::Socket {
			generation: 1,
			version,
			ttl: Duration::from_secs(60),
		};
		let http = Seen::Http { generation: 1 };
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
		assert_eq!(current(&slot), Seen::Probing { generation: 1 });
		locks.expire(begin + Duration::from_secs(5));
		assert_eq!(current(&slot), Seen::Http { generation: 1 });
		assert_eq!(locks.next_deadline(), None);

		// The answer that comes too late changes nothing.
		answer(&mut locks, r#"430[{"version":2,"ttlSeconds":60}]"#);
		assert_eq!(current(&slot), Seen::Http { generation: 1 });
	}

	#[tokio::test(start_paused = true)]
	async fn an_ack_for_no_pending_event_changes_nothing() {
		let slot = LockTransportSlot::default();
		let claim = slot.claim();
		let (mut locks, _) = probed(&claim);
		answer(&mut locks, r#"431[{"version":2,"ttlSeconds":60}]"#);
		assert_eq!(current(&slot), Seen::Probing { generation: 1 });
	}

	#[tokio::test(start_paused = true)]
	async fn a_probe_that_cannot_be_sent_leaves_the_connection_on_http() {
		let slot = LockTransportSlot::default();
		let claim = slot.claim();
		let locks = ConnectionLocks::probe(&claim, Instant::now(), |_| Err("queue full"));
		assert_eq!(current(&slot), Seen::Http { generation: 1 });
		assert_eq!(locks.next_deadline(), None);
	}

	#[tokio::test(start_paused = true)]
	async fn each_connection_gets_the_next_generation_and_ids_restart_at_zero() {
		let slot = LockTransportSlot::default();
		let claim = slot.claim();
		let (first, _) = probed(&claim);
		first.disconnected();
		assert_eq!(current(&slot), Seen::Connecting);

		let (mut second, sent) = probed(&claim);
		assert_eq!(sent, [r#"420["lockHello"]"#]);
		assert_eq!(current(&slot), Seen::Probing { generation: 2 });
		answer(&mut second, r#"430[{"version":2,"ttlSeconds":60}]"#);
		assert_eq!(
			current(&slot),
			Seen::Socket {
				generation: 2,
				version: 2,
				ttl: Duration::from_secs(60)
			}
		);
	}

	#[test]
	fn only_a_connection_on_protocol_two_or_later_announces() {
		let socket = |version| LockTransport::Socket {
			generation: 1,
			version,
			ttl: Duration::from_secs(60),
			commands: test_support::scripted_commands().0,
		};
		let cases = [
			(LockTransport::None, false),
			(LockTransport::Connecting, false),
			(LockTransport::Probing { generation: 1 }, false),
			(LockTransport::Http { generation: 1 }, false),
			(socket(1), false),
			(socket(2), true),
			(socket(3), true),
		];
		for (transport, announces) in cases {
			assert_eq!(transport.announces(), announces, "{transport:?}");
		}
	}

	#[test]
	fn dropping_the_latest_claim_publishes_none() {
		let slot = LockTransportSlot::default();
		let claim = slot.claim();
		drop(claim);
		assert_eq!(current(&slot), Seen::None);
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
		assert_eq!(current(&slot), Seen::Connecting);
		let (mut second, _) = probed(&successor);
		answer(&mut second, r#"430[{"version":1,"ttlSeconds":30}]"#);
		let live = Seen::Socket {
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
		assert_eq!(current(&slot), Seen::Connecting);
		drop(successor);
		assert_eq!(current(&slot), Seen::None);
	}

	/// A connection whose probe found socket locks, and the commands it published.
	fn speaking_locks(claim: &LockTransportClaim) -> (ConnectionLocks<'_>, LockCommands) {
		let (mut locks, _) = probed(claim);
		answer(&mut locks, r#"430[{"version":2,"ttlSeconds":60}]"#);
		let LockTransport::Socket { commands, .. } = claim.slot.current.borrow().clone() else {
			panic!("the probe found socket locks");
		};
		(locks, commands)
	}

	/// A Refresh of the lock with uuid `n`.
	fn payload(n: u128) -> LockPayload {
		LockPayload::new(&lock::Request {
			uuid: Uuid::from_u128(n),
			r#type: LockType::Refresh,
			resource: Cow::Borrowed("r"),
		})
		.unwrap()
	}

	/// Hands `locks` the Refresh of lock `n` at `now`, recording the frame it sends in `sent`.
	/// Returns the end its reply arrives at.
	fn hand(
		locks: &mut ConnectionLocks<'_>,
		n: u128,
		now: Instant,
		sent: &mut Vec<String>,
	) -> oneshot::Receiver<LockReply> {
		let (reply, replied) = oneshot::channel();
		let command = LockCommand::Send {
			payload: payload(n),
			reply,
		};
		let outcome = locks.on_command(Some(command), now, |frame| {
			sent.push(frame);
			Ok::<_, &str>(())
		});
		assert_eq!(outcome, CommandOutcome::Served);
		replied
	}

	/// Hands the Refresh of lock `n` to `commands` without the connection serving it yet.
	fn queue(commands: &LockCommands, n: u128) -> oneshot::Receiver<LockReply> {
		let (reply, replied) = oneshot::channel();
		commands
			.0
			.try_send(LockCommand::Send {
				payload: payload(n),
				reply,
			})
			.unwrap();
		replied
	}

	const SOCKET_V2: Seen = Seen::Socket {
		generation: 1,
		version: 2,
		ttl: Duration::from_secs(60),
	};

	#[tokio::test(start_paused = true)]
	async fn a_connection_takes_lock_operations_only_once_its_probe_found_socket_locks() {
		let slot = LockTransportSlot::default();
		let claim = slot.claim();
		let (mut locks, _) = probed(&claim);
		assert!(!locks.takes_commands());
		answer(&mut locks, r#"430[{"version":2,"ttlSeconds":60}]"#);
		assert!(locks.takes_commands());

		// Nobody can hand it one any more once the transport moved on.
		let outcome = locks.on_command(None, Instant::now(), |_| Ok::<_, &str>(()));
		assert_eq!(outcome, CommandOutcome::Served);
		assert!(!locks.takes_commands());
	}

	/// An operation that stops waiting before its ack is due leaves its event to the connection,
	/// which sends it all the same and gives up on its ack at its own deadline.
	#[tokio::test(start_paused = true)]
	async fn an_operation_that_stops_waiting_first_is_unknown_and_stays_with_the_connection() {
		let slot = LockTransportSlot::default();
		let claim = slot.claim();
		let (mut locks, commands) = speaking_locks(&claim);
		let begin = Instant::now();
		let mut sent = 0;
		let connection = async {
			let command = locks.next_command().await;
			locks.on_command(command, Instant::now(), |_| {
				sent += 1;
				Ok::<_, &str>(())
			});
		};
		let (reply, ()) = tokio::join!(
			commands.send(payload(1), Some(Duration::from_secs(2))),
			connection
		);
		assert_eq!(reply, LockReply::Unknown);
		assert_eq!(begin.elapsed(), Duration::from_secs(2));
		assert_eq!(sent, 1);
		assert_eq!(locks.next_deadline(), Some(begin + Duration::from_secs(10)));
	}

	#[tokio::test(start_paused = true)]
	async fn a_lock_operation_goes_out_as_a_lock_event_and_its_ack_answers_it() {
		let slot = LockTransportSlot::default();
		let claim = slot.claim();
		let (mut locks, commands) = speaking_locks(&claim);
		let mut sent = Vec::new();
		let connection = async {
			let command = locks.next_command().await;
			locks.on_command(command, Instant::now(), |frame| {
				sent.push(frame);
				Ok::<_, &str>(())
			});
			answer(&mut locks, r#"431[{"status":true}]"#);
		};
		let (reply, ()) = tokio::join!(commands.send(payload(1), None), connection);
		assert_eq!(
			sent,
			[
				r#"421["lock",{"uuid":"00000000-0000-0000-0000-000000000001","type":"refresh","resource":"r"}]"#
			]
		);
		assert_eq!(reply, LockReply::Answered(ack_args(r#"[{"status":true}]"#)));
		assert_eq!(locks.next_deadline(), None);
	}

	/// The server counts an event until it answers it, whatever the client gave up on: the 33rd
	/// unanswered one is not sent, until an ack, a late one included, frees an id.
	#[tokio::test(start_paused = true)]
	async fn the_thirty_third_unanswered_lock_event_is_busy_counting_those_given_up_on() {
		let begin = Instant::now();
		let late = begin + LOCK_ACK_TIMEOUT;
		let slot = LockTransportSlot::default();
		let claim = slot.claim();
		let (mut locks, _commands) = speaking_locks(&claim);
		let mut sent = Vec::new();

		let mut given_up_on = hand(&mut locks, 0, begin, &mut sent);
		locks.expire(late);
		assert_eq!(given_up_on.try_recv(), Ok(LockReply::Unknown));
		let _waiting: Vec<_> = (1..32)
			.map(|n| hand(&mut locks, n, late, &mut sent))
			.collect();
		assert_eq!(sent.len(), 32);

		let mut busy = hand(&mut locks, 32, late, &mut sent);
		assert_eq!(busy.try_recv(), Ok(LockReply::Busy));
		assert_eq!(sent.len(), 32);

		// The ack of the one given up on (id 1, after the probe's 0) is stale, and frees its id.
		answer(&mut locks, r#"431[{"status":true}]"#);
		let mut taken = hand(&mut locks, 33, late, &mut sent);
		assert_eq!(taken.try_recv(), Err(TryRecvError::Empty));
		assert_eq!(sent.len(), 33);
		assert_eq!(current(&slot), SOCKET_V2);
	}

	/// Two acks that did not come in time move the connection to HTTP for locks: it takes no
	/// further operation, and those handed to it and not served yet are not sent.
	#[tokio::test(start_paused = true)]
	async fn two_late_lock_acks_move_the_connection_to_http() {
		let begin = Instant::now();
		let slot = LockTransportSlot::default();
		let claim = slot.claim();
		let (mut locks, commands) = speaking_locks(&claim);
		let mut sent = Vec::new();
		let mut first = hand(&mut locks, 1, begin, &mut sent);
		let mut second = hand(&mut locks, 2, begin + Duration::from_secs(1), &mut sent);

		locks.expire(begin + LOCK_ACK_TIMEOUT);
		assert_eq!(first.try_recv(), Ok(LockReply::Unknown));
		assert_eq!(second.try_recv(), Err(TryRecvError::Empty));
		assert_eq!(current(&slot), SOCKET_V2);
		assert!(locks.takes_commands());

		let mut queued = queue(&commands, 3);
		locks.expire(begin + Duration::from_secs(1) + LOCK_ACK_TIMEOUT);
		assert_eq!(second.try_recv(), Ok(LockReply::Unknown));
		assert_eq!(current(&slot), Seen::Http { generation: 1 });
		assert!(!locks.takes_commands());
		assert_eq!(queued.try_recv(), Ok(LockReply::NotSent));
		assert_eq!(commands.send(payload(4), None).await, LockReply::NotSent);
		assert_eq!(sent.len(), 2);
	}

	/// A lost connection is published first, then tells the operations it sent that their outcome
	/// is unknown, and those it did not send yet that they were not sent.
	#[tokio::test(start_paused = true)]
	async fn a_lost_connection_answers_every_lock_operation_it_holds() {
		let slot = LockTransportSlot::default();
		let claim = slot.claim();
		let (mut locks, commands) = speaking_locks(&claim);
		let mut sent = Vec::new();
		let mut in_flight = hand(&mut locks, 1, Instant::now(), &mut sent);
		let mut queued = queue(&commands, 2);

		locks.disconnected();
		assert_eq!(current(&slot), Seen::Connecting);
		assert_eq!(in_flight.try_recv(), Ok(LockReply::Unknown));
		assert_eq!(queued.try_recv(), Ok(LockReply::NotSent));
		assert_eq!(commands.send(payload(3), None).await, LockReply::NotSent);
	}

	#[tokio::test(start_paused = true)]
	async fn a_lock_event_that_cannot_be_queued_is_not_sent() {
		let slot = LockTransportSlot::default();
		let claim = slot.claim();
		let (mut locks, _commands) = speaking_locks(&claim);
		let (reply, mut replied) = oneshot::channel();
		let command = LockCommand::Send {
			payload: payload(1),
			reply,
		};
		let outcome = locks.on_command(Some(command), Instant::now(), |_| Err("queue full"));
		assert_eq!(outcome, CommandOutcome::Served);
		assert_eq!(replied.try_recv(), Ok(LockReply::NotSent));
		assert_eq!(locks.pending.len(), 0);
		assert_eq!(locks.next_deadline(), None);
	}

	#[tokio::test(start_paused = true)]
	async fn a_rejected_key_ends_the_connection() {
		let slot = LockTransportSlot::default();
		let claim = slot.claim();
		let (mut locks, commands) = speaking_locks(&claim);
		commands.reauthenticate();
		let command = locks.next_command().await;
		let outcome = locks.on_command(command, Instant::now(), |_| Ok::<_, &str>(()));
		assert_eq!(outcome, CommandOutcome::Reauthenticate);
	}
}
