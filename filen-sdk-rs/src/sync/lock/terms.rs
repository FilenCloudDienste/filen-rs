//! The timings a held lock's lease is kept under, from the lease length the server announces in
//! its `lockHello` answer. They only ever get more conservative than the defaults, which are
//! for the 60 s lease the server has always granted.

use std::{cmp::min, time::Duration};

use super::{LOCK_LOSS_BUDGET, LOCK_REFRESH_INTERVAL, LOCK_SERVER_TTL};

/// The shortest lease length taken from a server's announcement: a shorter one (zero included)
/// is read as this, so the refreshes it calls for stay a few seconds apart.
// Read where the socket's announcements are, which builds without a socket (`service-worker`) do
// not have.
#[cfg(any(
	not(all(target_family = "wasm", target_os = "unknown")),
	feature = "wasm-full"
))]
const LEASE_TTL_FLOOR: Duration = Duration::from_secs(10);

/// The timings a held lock is kept under, all derived from one lease length: the server's lease
/// as announced, at most [`LOCK_SERVER_TTL`] and at least [`LEASE_TTL_FLOOR`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LeaseTerms {
	ttl: Duration,
}

impl LeaseTerms {
	/// The terms of a [`LOCK_SERVER_TTL`] lease, which hold until a server announces a shorter
	/// one.
	pub(super) const DEFAULT: Self = Self {
		ttl: LOCK_SERVER_TTL,
	};

	/// How long after its last confirmation the server releases a lease on its own.
	pub(crate) fn ttl(self) -> Duration {
		self.ttl
	}

	/// How often the keep-alive refreshes the lease: [`LOCK_REFRESH_INTERVAL`], or a quarter of
	/// a shorter lease, so a few refreshes can fail before it lapses.
	pub(super) fn refresh_interval(self) -> Duration {
		min(LOCK_REFRESH_INTERVAL, self.ttl / 4)
	}

	/// How old the lease's last confirmation may be for the lock to be relied on without asking
	/// the server again: the share [`LOCK_LOSS_BUDGET`] is of [`LOCK_SERVER_TTL`].
	pub(super) fn loss_budget(self) -> Duration {
		// At most LOCK_LOSS_BUDGET's 25 s in nanoseconds, as the lease is at most
		// LOCK_SERVER_TTL: neither the product nor the result overflows.
		let nanos = LOCK_LOSS_BUDGET.as_nanos() * self.ttl.as_nanos() / LOCK_SERVER_TTL.as_nanos();
		Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX))
	}

	/// How long to sleep before the next refresh, given how long ago the lease was last
	/// confirmed.
	pub(super) fn refresh_delay_after(self, elapsed: Duration) -> Duration {
		self.refresh_interval().saturating_sub(elapsed)
	}
}

/// A client's [`LeaseTerms`]: the default ones, tightened by every lease length a connection of
/// its socket announced. They never loosen again, so a lease held across a reconnect to a server
/// that announces a longer lease keeps the timings of the shortest one it was held under.
#[derive(Debug, Clone)]
pub(crate) struct LeaseTermsSlot(
	// The terms as announced, shared with every keep-alive and confirmation of the client's
	// leases, which a change wakes (clones are handles on one channel).
	tokio::sync::watch::Sender<LeaseTerms>,
);

impl Default for LeaseTermsSlot {
	fn default() -> Self {
		Self(tokio::sync::watch::Sender::new(LeaseTerms::DEFAULT))
	}
}

impl LeaseTermsSlot {
	/// A connection's server announced leases of `ttl`, which tightens the terms to those of a
	/// lease that long, clamped to [`LEASE_TTL_FLOOR`]..=[`LOCK_SERVER_TTL`]. Only a shorter
	/// lease than the default changes them: a longer one could be a server mid-change, or a bad
	/// value, and keeping the default timings with it costs refreshes, never the lock.
	#[cfg(any(
		not(all(target_family = "wasm", target_os = "unknown")),
		feature = "wasm-full"
	))]
	pub(crate) fn observe(&self, ttl: Duration) {
		// whole seconds, as the server announces them
		let announced = Duration::from_secs(ttl.clamp(LEASE_TTL_FLOOR, LOCK_SERVER_TTL).as_secs());
		self.0.send_if_modified(|terms| {
			let tighter = announced < terms.ttl;
			if tighter {
				terms.ttl = announced;
			}
			tighter
		});
	}

	/// The terms as they are now.
	pub(crate) fn current(&self) -> LeaseTerms {
		*self.0.borrow()
	}

	/// Resolves once the terms are no longer `seen`, at once if they changed already: a wait
	/// timed by `seen` is timed by the new terms from then.
	pub(super) async fn changed_from(&self, seen: LeaseTerms) {
		// The sender is borrowed for this call, so the channel cannot close meanwhile.
		let _ = self.0.subscribe().wait_for(|terms| *terms != seen).await;
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn observed(ttls: &[u64]) -> LeaseTerms {
		let slot = LeaseTermsSlot::default();
		for ttl in ttls {
			slot.observe(Duration::from_secs(*ttl));
		}
		slot.current()
	}

	#[test]
	fn the_default_terms_are_the_lock_constants() {
		let terms = LeaseTermsSlot::default().current();
		assert_eq!(terms.ttl(), Duration::from_secs(60));
		assert_eq!(terms.refresh_interval(), Duration::from_secs(15));
		assert_eq!(terms.loss_budget(), Duration::from_secs(25));
		assert_eq!(terms.loss_budget(), LOCK_LOSS_BUDGET);
	}

	#[test]
	fn a_30_s_lease_refreshes_every_7_5_s_and_is_relied_on_for_12_5_s() {
		let terms = observed(&[30]);
		assert_eq!(terms.ttl(), Duration::from_secs(30));
		assert_eq!(terms.refresh_interval(), Duration::from_millis(7_500));
		assert_eq!(terms.loss_budget(), Duration::from_millis(12_500));
	}

	#[test]
	fn a_lease_longer_than_the_default_keeps_the_default_terms() {
		assert_eq!(observed(&[120]), LeaseTerms::DEFAULT);
		assert_eq!(observed(&[61]), LeaseTerms::DEFAULT);
	}

	#[test]
	fn hostile_lease_lengths_are_clamped() {
		assert_eq!(observed(&[u64::MAX]), LeaseTerms::DEFAULT);
		let zero = observed(&[0]);
		assert_eq!(zero.ttl(), Duration::from_secs(10));
		assert_eq!(zero.refresh_interval(), Duration::from_millis(2_500));
		assert_eq!(zero.loss_budget(), Duration::from_nanos(4_166_666_666));
		assert_eq!(observed(&[9]).ttl(), Duration::from_secs(10));
		assert_eq!(observed(&[11]).ttl(), Duration::from_secs(11));
	}

	#[test]
	fn the_terms_only_tighten_across_announcements() {
		assert_eq!(observed(&[30, 60]).ttl(), Duration::from_secs(30));
		assert_eq!(observed(&[30, 120]).ttl(), Duration::from_secs(30));
		assert_eq!(observed(&[30, 20]).ttl(), Duration::from_secs(20));
		assert_eq!(observed(&[20, 30, 0, 60]).ttl(), Duration::from_secs(10));
	}

	#[test]
	fn the_refresh_delay_counts_down_from_the_terms_interval() {
		let terms = observed(&[30]);
		assert_eq!(
			terms.refresh_delay_after(Duration::ZERO),
			Duration::from_millis(7_500)
		);
		assert_eq!(
			terms.refresh_delay_after(Duration::from_secs(5)),
			Duration::from_millis(2_500)
		);
		assert_eq!(
			terms.refresh_delay_after(Duration::from_secs(8)),
			Duration::ZERO
		);
	}
}
