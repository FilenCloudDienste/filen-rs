//! Whether the cache could reach the server the last time it tried to converge its roots: what a
//! sync pass waiting on a root the cache has not finished listing reads to tell an unreachable
//! server from a slow listing (see `Client::cache_unreachable`).

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
#[derive(Debug, Default)]
pub(crate) struct ResyncReach {
	/// The failure, with the socket connection count its request was sent at.
	failure: Mutex<Option<(Arc<Error>, u64)>>,
	/// The client's count of socket connections (see `SharedClientState::socket_connections`).
	connections: Arc<AtomicU64>,
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
		}
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
	pub(crate) fn failed(&self, since: ReachMark, error: Error) {
		let mut failure = self.failure();
		if error.kind() != ErrorKind::Reqwest {
			*failure = None;
		} else if self.connections() == since.0 {
			*failure = Some((Arc::new(error), since.0));
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
