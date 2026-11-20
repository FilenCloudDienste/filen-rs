//! The thumbnail server's shutdown gate (`os::win::com`): counts the requests in progress and
//! refuses new ones once closed. Plain `std`, so it is tested on every host.

use std::{
	sync::{Condvar, Mutex, MutexGuard, PoisonError},
	time::Duration,
};

/// Counts the requests in progress (`Initialize` and `GetThumbnail` calls) and refuses new ones
/// once closed, so the server stops once the last one is answered, or once [`Gate::close`] has
/// waited as long as it was told.
#[derive(Default)]
pub(crate) struct Gate {
	state: Mutex<GateState>,
	idle: Condvar,
}

#[derive(Default)]
struct GateState {
	closed: bool,
	active: usize,
}

/// A request in progress, counted until it drops.
pub(crate) struct Active<'a>(&'a Gate);

impl Gate {
	/// The state. A panic while it was held left nothing half-written, as each update is one
	/// assignment, so a poisoned lock is taken as it is.
	fn state(&self) -> MutexGuard<'_, GateState> {
		self.state.lock().unwrap_or_else(PoisonError::into_inner)
	}

	/// Counts a request in, or `None` once the gate is closed.
	pub(crate) fn enter(&self) -> Option<Active<'_>> {
		let mut state = self.state();
		if state.closed {
			return None;
		}
		state.active += 1;
		Some(Active(self))
	}

	/// Whether the gate is closed.
	pub(crate) fn is_closed(&self) -> bool {
		self.state().closed
	}

	/// Refuses every request from now on and waits, at most `within`, for those in progress:
	/// whether they all ended. A `GetThumbnail` is bounded by the service's deadline, but an
	/// `Initialize` may wait on a call into its caller, which nothing bounds, so the wait is.
	pub(crate) fn close(&self, within: Duration) -> bool {
		let mut state = self.state();
		state.closed = true;
		let (state, _) = self
			.idle
			.wait_timeout_while(state, within, |state| state.active > 0)
			.unwrap_or_else(PoisonError::into_inner);
		state.active == 0
	}
}

impl Drop for Active<'_> {
	fn drop(&mut self) {
		let mut state = self.0.state();
		state.active -= 1;
		if state.active == 0 {
			self.0.idle.notify_all();
		}
	}
}

#[cfg(test)]
mod tests {
	use std::{
		sync::Arc,
		thread,
		time::{Duration, Instant},
	};

	use super::Gate;

	#[test]
	fn closing_the_gate_waits_for_the_request_in_progress() {
		let gate = Arc::new(Gate::default());
		let active = gate.enter();
		assert!(active.is_some());
		let closer = thread::spawn({
			let gate = Arc::clone(&gate);
			move || gate.close(Duration::from_secs(60))
		});
		let deadline = Instant::now() + Duration::from_secs(60);
		while !gate.is_closed() {
			assert!(
				Instant::now() < deadline,
				"the closer never closed the gate"
			);
			thread::yield_now();
		}
		assert!(gate.enter().is_none());
		assert!(!closer.is_finished());
		drop(active);
		assert!(closer.join().unwrap());
	}

	#[test]
	fn closing_the_gate_gives_up_on_a_request_past_the_wait() {
		let gate = Gate::default();
		let active = gate.enter();
		assert!(active.is_some());
		assert!(!gate.close(Duration::from_millis(10)));
		assert!(gate.is_closed());
		assert!(gate.enter().is_none());
		// The request still ends cleanly after the wait gave up.
		drop(active);
		assert!(gate.close(Duration::ZERO));
	}

	#[test]
	fn an_idle_gate_closes_at_once_and_stays_closed() {
		let gate = Gate::default();
		assert!(!gate.is_closed());
		drop(gate.enter());
		assert!(gate.close(Duration::ZERO));
		assert!(gate.enter().is_none());
	}
}
