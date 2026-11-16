//! What the client knows about its user's locks besides its own lock answers: the leases the
//! socket's lock announcements report, the leases this client holds, and the uuids it let go.
//!
//! Announcements are hints. They arrive in any order, are lost while the socket is disconnected
//! and are not replayed, so whether this client holds a lock is still decided only by the
//! answers to its own lock requests. An announcement that a held lease may be lost only makes
//! the lease unconfirmed until the server answers a refresh of it (see [`super::Suspicion`]).

// Only the websocket task feeds the board, and builds without a socket (`service-worker`) have
// none.
#![cfg_attr(
	all(
		target_family = "wasm",
		target_os = "unknown",
		not(feature = "wasm-full")
	),
	expect(dead_code, reason = "no socket announces locks in this build")
)]

use std::{
	collections::HashMap,
	sync::{Arc, Mutex, MutexGuard, PoisonError, Weak},
	time::Duration,
};

use chrono::{DateTime, Utc};
use filen_types::{
	api::v3::socket::{LockAnnouncement, LockEventType},
	fs::Uuid,
};
use tokio::sync::watch;

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
use tokio::time::Instant;
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
use wasmtimer::std::Instant;

use super::{Confirmed, LOCK_SERVER_TTL, LeaseConfirmed, OwedRelease, SharedLease, lock_url};
use crate::auth::http::AuthClient;

/// How long an ended lease is remembered: long enough for its `lockAcquired`, which can arrive
/// after its end, to find it ended.
const ENDED_LEASE_KEEP: Duration = Duration::from_secs(10 * 60);

/// How long a uuid this client let go is remembered: a lock request of it still on its way can
/// be granted for at most a lease after the release went out (or was given up on), and its
/// announcement takes a little longer to arrive.
const RETIRED_KEEP: Duration = LOCK_SERVER_TTL.saturating_add(Duration::from_secs(10));

/// Whether the server announces the leases of `resource` at all. It does not for a name its
/// store cannot place by a hash tag: one holding a `}` whose first `{` is not followed by a `}`
/// with at least one character in between (`a}b`, `{}x`, `{}a{b}`).
pub(crate) fn announces(resource: &str) -> bool {
	!resource.contains('}')
		|| resource
			.split_once('{')
			.and_then(|(_, after)| after.find('}'))
			.is_some_and(|close| close > 0)
}

/// The client's lock bookkeeping that the socket's lock announcements are checked against (see
/// the module docs). One per `AuthClient`, shared by all its leases and its websocket task.
#[derive(Default)]
pub(crate) struct LockBoard(Mutex<Board>);

#[derive(Default)]
struct Board {
	/// The leases announced per resource since the current connection was authenticated. A
	/// resource missing here had nothing announced since then, so its holder is unknown.
	announced: HashMap<String, Leases>,
	/// What a wait for a resource is woken through: when one of its leases is announced ended,
	/// and when a connection is authenticated, as announcements may have been lost before it.
	wakes: HashMap<String, watch::Sender<()>>,
	/// The leases this client holds, by uuid, until the last handle on one is dropped.
	held: HashMap<Uuid, Held>,
	/// The uuids this client let go, by uuid, for [`RETIRED_KEEP`].
	retired: HashMap<Uuid, Retired>,
}

/// A lease this client holds, and its resource, so an announcement is checked against it without
/// upgrading it under the board's lock (see [`LockBoard::held`]).
struct Held {
	resource: Box<str>,
	lease: Weak<SharedLease>,
}

/// What a lock announcement says about this client's own leases (see [`LockBoard::announced`]).
#[derive(Default)]
pub(crate) struct OwnLeases {
	/// The held leases it says may be lost: its lease of theirs ended, or another lease of their
	/// resource began.
	suspect: Vec<Weak<SharedLease>>,
	/// A uuid this client let go of that a lease began for: a lock request of it was granted
	/// after its release was sent or given up on, as a request whose outcome was unknown can be.
	regranted: Option<Uuid>,
}

/// A uuid this client let go: it sent a release for it, or gave up on one, on `resource` at
/// `at`.
///
/// `at` is on the wall clock only. On wasm the uuid is retired on whichever worker let the lock
/// go and looked up on the websocket task's worker, and a monotonic `Instant` there counts from
/// each worker's own time origin, so its age read on another worker is off by the gap between
/// them. A wall clock set back keeps a retirement longer, which is harmless as a uuid is never
/// reused; one set forward forgets it early, an accepted gap.
struct Retired {
	resource: Arc<str>,
	at: DateTime<Utc>,
}

impl Retired {
	/// Whether this retirement is still remembered at `now`.
	fn is_kept_at(&self, now: DateTime<Utc>) -> bool {
		(now - self.at)
			.to_std()
			.map_or(true, |age| age < RETIRED_KEEP)
	}
}

/// Who holds a resource, by the announcements since the current connection was authenticated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Holder {
	/// Nothing was announced for the resource since then, or there is no connection.
	Unknown,
	/// Every lease announced acquired since then was also announced ended. A lease whose
	/// announcements were lost can still hold it.
	Free,
	/// This lease was announced acquired and not ended.
	Lease(OpenLease),
}

/// One authenticated connection's hold on the [`LockBoard`]'s announced leases, which it
/// forgets when dropped (see [`LockBoard::connection`]).
pub(crate) struct BoardConnection<'a>(&'a LockBoard);

impl Drop for BoardConnection<'_> {
	fn drop(&mut self) {
		self.0.disconnected();
	}
}

/// A lease announced acquired and not announced ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OpenLease {
	/// The lease's holder: the uuid of the lock request that acquired it.
	pub(crate) holder: Uuid,
	/// The user's lock announcement counter on its `lockAcquired`.
	pub(crate) lock_message_id: u64,
}

/// The leases announced for one resource, applied in any order (the socket lock protocol's
/// client rule): a lease announced acquired is added unless it ended, a lease announced ended
/// stays ended, and applying an announcement twice changes nothing.
#[derive(Default)]
struct Leases(HashMap<Box<str>, LeaseSeen>);

enum LeaseSeen {
	Open(OpenLease),
	/// Announced ended, first seen at this time.
	Ended(Instant),
}

impl Leases {
	/// Adds the lease unless it ended. `true` if that opened it, `false` if it was open or ended
	/// already.
	fn acquired(&mut self, lease_id: &str, lease: OpenLease) -> bool {
		match self.0.get_mut(lease_id) {
			None => {
				self.0.insert(lease_id.into(), LeaseSeen::Open(lease));
				true
			}
			// A lease is announced acquired once. Should it come twice, the higher counter wins,
			// so the order they arrive in does not matter.
			Some(LeaseSeen::Open(open)) => {
				if (lease.lock_message_id, lease.holder) > (open.lock_message_id, open.holder) {
					*open = lease;
				}
				false
			}
			Some(LeaseSeen::Ended(_)) => false,
		}
	}

	/// Marks the lease ended. `true` if that ended it, `false` if it had ended already.
	fn ended(&mut self, lease_id: &str, now: Instant) -> bool {
		match self.0.get_mut(lease_id) {
			None => {
				self.0.insert(lease_id.into(), LeaseSeen::Ended(now));
				true
			}
			Some(seen @ LeaseSeen::Open(_)) => {
				*seen = LeaseSeen::Ended(now);
				true
			}
			Some(LeaseSeen::Ended(_)) => false,
		}
	}

	/// Forgets the leases that ended more than [`ENDED_LEASE_KEEP`] before `now`.
	fn prune(&mut self, now: Instant) {
		self.0.retain(|_, seen| match seen {
			LeaseSeen::Open(_) => true,
			LeaseSeen::Ended(at) => now.saturating_duration_since(*at) < ENDED_LEASE_KEEP,
		});
	}

	/// The open lease, the one with the highest counter while ends are still on their way or
	/// were lost.
	fn holder(&self) -> Holder {
		self.0
			.values()
			.filter_map(|seen| match seen {
				LeaseSeen::Open(open) => Some(*open),
				LeaseSeen::Ended(_) => None,
			})
			.max_by_key(|open| (open.lock_message_id, open.holder))
			.map_or(Holder::Free, Holder::Lease)
	}
}

impl Board {
	/// Wakes the waits for `resource`, and forgets its wake channel once nobody waits on it.
	fn wake(&mut self, resource: &str) {
		let Some(wake) = self.wakes.get(resource) else {
			return;
		};
		if wake.receiver_count() == 0 {
			self.wakes.remove(resource);
		} else {
			wake.send_replace(());
		}
	}

	/// The held leases of `resource` whose uuid is `suspect`.
	fn held_on(&self, resource: &str, suspect: impl Fn(Uuid) -> bool) -> Vec<Weak<SharedLease>> {
		self.held
			.iter()
			.filter(|(uuid, held)| &*held.resource == resource && suspect(**uuid))
			// a second handle on the lease, upgraded once the board's lock is left
			.map(|(_, held)| Weak::clone(&held.lease))
			.collect()
	}

	/// Whether this client let `uuid` on `resource` go within [`RETIRED_KEEP`] before `now`.
	fn is_retired_at(&self, resource: &str, uuid: Uuid, now: DateTime<Utc>) -> bool {
		self.retired
			.get(&uuid)
			.is_some_and(|retired| &*retired.resource == resource && retired.is_kept_at(now))
	}
}

impl LockBoard {
	fn board(&self) -> MutexGuard<'_, Board> {
		self.0.lock().unwrap_or_else(PoisonError::into_inner)
	}

	/// Applies a lock announcement with the user's lock announcement counter `lock_message_id`.
	/// An ended lease wakes the waits for its resource.
	///
	/// Returns what it says about this client's own leases, each once, when it first ends or
	/// opens a lease: a held lease whose lease it ends, or of whose resource it opens another
	/// lease, may be lost; and a retired uuid it opens a lease for was granted late. A lease of a
	/// held uuid opening is the client's own grant, and says nothing.
	pub(crate) fn announced(&self, event: &LockEventType<'_>, lock_message_id: u64) -> OwnLeases {
		let now = Instant::now();
		let mut board = self.board();
		let (announcement, ended) = announcement_of(event);
		let resource = announcement.resource.as_ref();
		let holder = announcement.uuid;
		let leases = board.announced.entry(resource.to_owned()).or_default();
		leases.prune(now);
		if ended {
			let newly = leases.ended(&announcement.lease_id, now);
			board.wake(resource);
			if !newly {
				return OwnLeases::default();
			}
			OwnLeases {
				suspect: board.held_on(resource, |uuid| uuid == holder),
				regranted: None,
			}
		} else {
			let newly = leases.acquired(
				&announcement.lease_id,
				OpenLease {
					holder,
					lock_message_id,
				},
			);
			if !newly {
				return OwnLeases::default();
			}
			OwnLeases {
				suspect: board.held_on(resource, |uuid| uuid != holder),
				regranted: board
					.is_retired_at(resource, holder, Utc::now())
					.then_some(holder),
			}
		}
	}

	/// A connection was authenticated: what was announced before it may have been missed, so
	/// every resource's holder is unknown again and every wait is woken.
	pub(crate) fn connected(&self) {
		let mut board = self.board();
		board.announced.clear();
		board.wakes.retain(|_, wake| {
			let waited_on = wake.receiver_count() > 0;
			if waited_on {
				wake.send_replace(());
			}
			waited_on
		});
	}

	/// The connection ended: announcements are missed until the next one.
	pub(crate) fn disconnected(&self) {
		self.board().announced.clear();
	}

	/// A connection was authenticated ([`Self::connected`]), until the returned guard drops,
	/// which ends it ([`Self::disconnected`]) however the task holding it ends: a websocket task
	/// that panics or is cancelled mid-connection must not leave its announcements standing.
	pub(crate) fn connection(&self) -> BoardConnection<'_> {
		self.connected();
		BoardConnection(self)
	}

	/// Who holds `resource`, by the announcements (see [`Holder`]).
	#[cfg_attr(
		not(test),
		expect(dead_code, reason = "nothing reads the announced holders yet")
	)]
	pub(crate) fn holder(&self, resource: &str) -> Holder {
		self.board()
			.announced
			.get(resource)
			.map_or(Holder::Unknown, Leases::holder)
	}

	/// Changes whenever a wait for `resource` should poll again before its scheduled time: a
	/// lease of it was announced ended, or a connection was authenticated.
	pub(crate) fn wakes(&self, resource: &str) -> watch::Receiver<()> {
		let mut board = self.board();
		if let Some(wake) = board.wakes.get(resource) {
			return wake.subscribe();
		}
		// the channels of waits that ended
		board.wakes.retain(|_, wake| wake.receiver_count() > 0);
		let (wake, waiting) = watch::channel(());
		board.wakes.insert(resource.to_owned(), wake);
		waiting
	}

	/// Registers `lease` as held by this client, until [`Self::let_go`].
	pub(super) fn hold(&self, lease: &Arc<SharedLease>) {
		self.board().held.insert(
			lease.uuid,
			Held {
				resource: lease.resource.as_str().into(),
				lease: Arc::downgrade(lease),
			},
		);
	}

	/// The last handle on the lease `uuid` is gone.
	pub(super) fn let_go(&self, uuid: Uuid) {
		self.board().held.remove(&uuid);
	}

	/// The lease `uuid` this client holds, if it does.
	// The upgraded lease leaves the board's lock before it can be dropped: dropping the last
	// handle on a lease takes that lock again (`let_go`).
	#[cfg(test)]
	pub(super) fn held(&self, uuid: Uuid) -> Option<Arc<SharedLease>> {
		let held = self
			.board()
			.held
			.get(&uuid)
			.map(|held| Weak::clone(&held.lease));
		held?.upgrade()
	}

	/// Records that this client let `uuid` on `resource` go, for [`RETIRED_KEEP`]: a release of
	/// it is being sent, or was given up on.
	pub(super) fn retire(&self, uuid: Uuid, resource: &Arc<str>) {
		let now = Utc::now();
		let mut board = self.board();
		board.retired.retain(|_, retired| retired.is_kept_at(now));
		board.retired.insert(
			uuid,
			Retired {
				// refcount bump
				resource: Arc::clone(resource),
				at: now,
			},
		);
	}

	/// Whether this client let `uuid` on `resource` go within [`RETIRED_KEEP`].
	#[cfg(test)]
	pub(super) fn is_retired(&self, resource: &str, uuid: Uuid) -> bool {
		self.board().is_retired_at(resource, uuid, Utc::now())
	}
}

/// The lease `event` announces, and whether it says the lease ended.
fn announcement_of<'a>(event: &'a LockEventType<'_>) -> (&'a LockAnnouncement<'a>, bool) {
	match event {
		LockEventType::LockAcquired(announcement) => (announcement, false),
		LockEventType::LockReleased(announcement) | LockEventType::LockTimeout(announcement) => {
			(announcement, true)
		}
	}
}

/// Applies a lock announcement to `client`'s [`LockBoard`] (see [`LockBoard::announced`]), and
/// acts on what it says about the client's own leases: a held lease it says may be lost is
/// refreshed at once and unconfirmed until the server answers, and a uuid granted after the
/// client let it go is released once more.
///
/// The release of a late grant is best-effort, as the announcement is: a grant whose
/// announcement is lost holds the resource until its lease runs out, as without announcements.
/// A `lockAcquired` that only arrived after the client's own release of a lease it held costs one
/// release that frees nothing.
pub(crate) fn lock_announced(
	client: &Arc<AuthClient>,
	event: &LockEventType<'_>,
	lock_message_id: u64,
) {
	let OwnLeases { suspect, regranted } = client.lock_board.announced(event, lock_message_id);
	// Upgraded here, off the board's lock, which dropping the last handle on one takes.
	for lease in suspect.iter().filter_map(Weak::upgrade) {
		if !lease.is_lost() {
			tracing::info!(
				"A lock announcement says lock '{}' may be lost, asking the server",
				lease.resource
			);
			lease.suspicion.raise();
		}
	}
	if let Some(uuid) = regranted {
		let resource = announcement_of(event).0.resource.as_ref();
		tracing::warn!("Lock '{resource}' was granted after it was let go, releasing it again");
		// Counted in the client's pending releases like any other, without a lock listener
		// record: the listener saw this lock settle already, and sees each lock settle once.
		let owed = OwedRelease::new(
			Arc::clone(client),
			lock_url().into(),
			uuid,
			resource.into(),
			// the late grant was made before its announcement arrived, so its lease runs out no
			// later than a lease from now
			LeaseConfirmed::Sent(Confirmed::now()),
			None,
		);
		drop(crate::runtime::spawn_task_maybe_send(owed.send()));
	}
}

#[cfg(test)]
mod tests;
