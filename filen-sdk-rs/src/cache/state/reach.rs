//! Whether the cache could reach the server the last time it tried to converge its roots: what a
//! sync pass waiting on a root the cache has not finished listing reads to tell an unreachable
//! server from a slow listing (see `Client::cache_unreachable`). And whether the cache's socket
//! subscription is live right now: what a sync pass reads to tell a cache that hears every remote
//! change from one that is only as current as its last listing.

use std::sync::{
	Arc, Mutex, MutexGuard, PoisonError,
	atomic::{AtomicU64, Ordering},
};

use crate::{Error, ErrorKind};

/// The transport failure ([`ErrorKind::Reqwest`]) the cache's latest request toward converging its
/// roots ended on, if it ended on one: the drive lock, the remote drive id read before the listing
/// or by a gap-check, or a root's listing. Worker-wide, as the resync is: one attempt converges
/// every registered root, so its failure holds back every root it has not converged, and a root it
/// already converged is read as such before this is asked.
///
/// Only an outcome the cache observed changes it, never time passing: an attempt still waiting on
/// the server leaves the latest outcome standing, so a slow listing is never taken for an
/// unreachable server. Any other outcome clears it, a success or a refusal of any other kind
/// alike, the server having answered.
///
/// A failure also stops standing once the socket of the cache's client connects again, which
/// shows the server reachable before the cache's next attempt runs. The socket counts the
/// connection before any of its listeners hears of it (see
/// `DisconnectedListenerManager::into_connected`), and a failure stands only while the count is
/// the one its request was sent at: a watch that connection resumes never reads the failure it
/// resumed from, whichever listener runs first, and a request sent before the connection that
/// fails after it is not recorded.
///
/// Beside it, whether the cache's socket listener is live: connected, authenticated and in the
/// routing table, so every drive event reaches the cache. Set by the cache's socket callback as the
/// socket's own events arrive (see `make_socket_event_callback`), before the worker hears of them,
/// and `false` until the first `authSuccess`, so a socket that never connects (a firewall that
/// blocks it) reads as not live from the start.
#[derive(Debug)]
pub(crate) struct ResyncReach {
	/// The failure, with the socket connection count its request was sent at.
	failure: Mutex<Option<(Arc<Error>, u64)>>,
	/// The client's count of socket connections (see `SharedClientState::socket_connections`).
	connections: Arc<AtomicU64>,
	/// Whether the cache's socket listener is live (see the type's docs).
	live: tokio::sync::watch::Sender<bool>,
}

impl Default for ResyncReach {
	fn default() -> Self {
		Self::new(Arc::default())
	}
}

/// A point in the socket's connections (see [`ResyncReach::mark`]).
#[derive(Debug, Clone, Copy)]
pub(crate) struct ReachMark(u64);

impl ResyncReach {
	/// Read against the socket connections counted in `connections`.
	pub(crate) fn new(connections: Arc<AtomicU64>) -> Self {
		Self {
			failure: Mutex::new(None),
			connections,
			live: tokio::sync::watch::Sender::new(false),
		}
	}

	/// Record whether the cache's socket listener is live. Wakes a listener only on a change.
	pub(crate) fn set_live(&self, live: bool) {
		self.live.send_if_modified(|was| {
			let changed = *was != live;
			*was = live;
			changed
		});
	}

	/// Whether the cache's socket listener is live now.
	// Used only by the native sync engine; other builds compile it unused.
	#[cfg_attr(
		not(all(
			feature = "sync-engine",
			not(all(target_family = "wasm", target_os = "unknown"))
		)),
		allow(dead_code)
	)]
	pub(crate) fn live(&self) -> bool {
		*self.live.borrow()
	}

	/// Hears every change of [`live`](Self::live).
	// Used only by the native sync engine; other builds compile it unused.
	#[cfg_attr(
		not(all(
			feature = "sync-engine",
			not(all(target_family = "wasm", target_os = "unknown"))
		)),
		allow(dead_code)
	)]
	pub(crate) fn liveness(&self) -> tokio::sync::watch::Receiver<bool> {
		self.live.subscribe()
	}

	fn failure(&self) -> MutexGuard<'_, Option<(Arc<Error>, u64)>> {
		self.failure.lock().unwrap_or_else(PoisonError::into_inner)
	}

	fn connections(&self) -> u64 {
		self.connections.load(Ordering::SeqCst)
	}

	/// Where the socket's connections stand now: what a request toward converging takes as it is
	/// sent, for [`failed`](Self::failed).
	pub(crate) fn mark(&self) -> ReachMark {
		ReachMark(self.connections())
	}

	/// The server answered.
	pub(crate) fn reached(&self) {
		*self.failure() = None;
	}

	/// A request sent at `since` failed with `error`. A transport failure is recorded unless the
	/// socket has connected since; any other failure is the server answering.
	pub(crate) fn failed(&self, since: ReachMark, error: impl Into<Arc<Error>>) {
		let error = error.into();
		let mut failure = self.failure();
		if error.kind() != ErrorKind::Reqwest {
			*failure = None;
		} else if self.connections() == since.0 {
			*failure = Some((error, since.0));
		}
	}

	/// The transport failure that left the cache unable to reach the server, while it stands.
	pub(crate) fn unreachable(&self) -> Option<Arc<Error>> {
		let connections = self.connections();
		self.failure()
			.as_ref()
			.filter(|(_, at)| *at == connections)
			.map(|(error, _)| Arc::clone(error))
	}
}
