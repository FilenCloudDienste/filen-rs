//! What the client knows about its user's locks besides its own lock answers: the leases the
//! socket's lock announcements report, the leases this client holds, and the uuids it let go.
//!
//! Announcements are hints. They arrive in any order, are lost while the socket is disconnected
//! and are not replayed, so whether this client holds a lock is still decided only by the
//! answers to its own lock requests.

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
use filen_types::{api::v3::socket::LockEventType, fs::Uuid};
use tokio::sync::watch;

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
use tokio::time::Instant;
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
use wasmtimer::std::Instant;

use super::{LOCK_SERVER_TTL, SharedLease};

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
	held: HashMap<Uuid, Weak<SharedLease>>,
	/// The uuids this client let go, by uuid, for [`RETIRED_KEEP`].
	retired: HashMap<Uuid, Retired>,
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
	fn acquired(&mut self, lease_id: &str, lease: OpenLease) {
		match self.0.get_mut(lease_id) {
			None => {
				self.0.insert(lease_id.into(), LeaseSeen::Open(lease));
			}
			// A lease is announced acquired once. Should it come twice, the higher counter wins,
			// so the order they arrive in does not matter.
			Some(LeaseSeen::Open(open)) => {
				if (lease.lock_message_id, lease.holder) > (open.lock_message_id, open.holder) {
					*open = lease;
				}
			}
			Some(LeaseSeen::Ended(_)) => {}
		}
	}

	fn ended(&mut self, lease_id: &str, now: Instant) {
		match self.0.get_mut(lease_id) {
			None => {
				self.0.insert(lease_id.into(), LeaseSeen::Ended(now));
			}
			Some(seen @ LeaseSeen::Open(_)) => *seen = LeaseSeen::Ended(now),
			Some(LeaseSeen::Ended(_)) => {}
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
}

impl LockBoard {
	fn board(&self) -> MutexGuard<'_, Board> {
		self.0.lock().unwrap_or_else(PoisonError::into_inner)
	}

	/// Applies a lock announcement with the user's lock announcement counter `lock_message_id`.
	/// An ended lease wakes the waits for its resource.
	pub(crate) fn announced(&self, event: &LockEventType<'_>, lock_message_id: u64) {
		let now = Instant::now();
		let mut board = self.board();
		let (announcement, ended) = match event {
			LockEventType::LockAcquired(announcement) => (announcement, false),
			LockEventType::LockReleased(announcement)
			| LockEventType::LockTimeout(announcement) => (announcement, true),
		};
		let resource = announcement.resource.as_ref();
		let leases = board.announced.entry(resource.to_owned()).or_default();
		leases.prune(now);
		if ended {
			leases.ended(&announcement.lease_id, now);
			board.wake(resource);
		} else {
			leases.acquired(
				&announcement.lease_id,
				OpenLease {
					holder: announcement.uuid,
					lock_message_id,
				},
			);
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
		self.board().held.insert(lease.uuid, Arc::downgrade(lease));
	}

	/// The last handle on the lease `uuid` is gone.
	pub(super) fn let_go(&self, uuid: Uuid) {
		self.board().held.remove(&uuid);
	}

	/// The lease `uuid` this client holds, if it does.
	// The upgraded lease leaves the board's lock before it can be dropped: dropping the last
	// handle on a lease takes that lock again (`let_go`).
	#[cfg_attr(
		not(test),
		expect(
			dead_code,
			reason = "no announcement is checked against the held leases yet"
		)
	)]
	pub(super) fn held(&self, uuid: Uuid) -> Option<Arc<SharedLease>> {
		let held = self.board().held.get(&uuid).cloned();
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
	#[cfg_attr(
		not(test),
		expect(
			dead_code,
			reason = "no announcement is checked against the retired uuids yet"
		)
	)]
	pub(super) fn is_retired(&self, resource: &str, uuid: Uuid) -> bool {
		let now = Utc::now();
		self.board()
			.retired
			.get(&uuid)
			.is_some_and(|retired| &*retired.resource == resource && retired.is_kept_at(now))
	}
}

#[cfg(test)]
mod tests;
