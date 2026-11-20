//! Test-only (`test-seams`): holds the job about to convert a new ordinary file or directory whose
//! name starts with a given prefix, just before the convert, until released or a time limit runs
//! out, so the VM harness can rename the item from a user's process in the window between the
//! job's checks and its convert: a few tens of milliseconds otherwise, which sixty tries in the VM
//! never hit. One hold per arming, process-wide; never in a shipping build.

use std::{
	sync::{Condvar, Mutex, MutexGuard, PoisonError},
	time::{Duration, Instant},
};

use tracing::info;

use crate::{log_safe::path_tag, root_path::RootPath};

/// The hold as armed, and the convert it holds.
struct Hold {
	/// The prefix the next held item's name starts with, lowercased, and how long it is held at most.
	armed: Option<(String, Duration)>,
	/// The item held before its convert, while it is.
	held: Option<RootPath>,
	/// Set by [`release_convert`] for the convert held.
	released: bool,
}

static HOLD: Mutex<Hold> = Mutex::new(Hold {
	armed: None,
	held: None,
	released: false,
});
/// Wakes the held convert once released.
static RELEASED: Condvar = Condvar::new();

fn hold() -> MutexGuard<'static, Hold> {
	HOLD.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Arms the hold: the next convert of a new ordinary file or directory whose name starts with
/// `prefix`, in any case, waits just before it converts until [`release_convert`], or until `limit`
/// passes. Replaces a hold armed before that caught nothing yet.
pub fn hold_next_convert(prefix: &str, limit: Duration) {
	hold().armed = Some((prefix.to_lowercase(), limit));
}

/// The item a hold keeps from its convert now, if one is held.
pub fn held_convert() -> Option<RootPath> {
	hold().held.clone()
}

/// Lets the held convert go on, and disarms a hold that caught nothing yet.
pub fn release_convert() {
	let mut hold = hold();
	hold.armed = None;
	if hold.held.is_some() {
		hold.released = true;
		RELEASED.notify_all();
	}
}

/// Called by a job just before it converts the new ordinary item at `path`, on the blocking pool:
/// waits there while the hold is armed for the item's name, as [`hold_next_convert`] says.
pub(super) fn before_convert(path: &RootPath) {
	let mut hold = hold();
	let matches = |(prefix, _): &(String, Duration)| {
		path.name()
			.is_some_and(|name| name.as_str().to_lowercase().starts_with(prefix.as_str()))
	};
	let Some((_, limit)) = hold.armed.take_if(|armed| matches(armed)) else {
		return;
	};
	hold.held = Some(path.clone());
	hold.released = false;
	info!(item = %path_tag(path), "paused before convert");
	let deadline = Instant::now() + limit;
	while !hold.released {
		let left = deadline.saturating_duration_since(Instant::now());
		if left.is_zero() {
			break;
		}
		hold = RELEASED
			.wait_timeout(hold, left)
			.unwrap_or_else(PoisonError::into_inner)
			.0;
	}
	let released = hold.released;
	hold.held = None;
	hold.released = false;
	info!(item = %path_tag(path), released, "convert goes on");
}

#[cfg(test)]
mod tests {
	use std::thread;

	use super::*;
	use crate::test_support::root_path;

	#[test]
	fn a_held_convert_waits_for_its_release_and_holds_once() {
		hold_next_convert("Hold-Unit-", Duration::from_secs(60));
		before_convert(&root_path("other.txt"));
		assert_eq!(held_convert(), None, "another name is not held");
		let path = root_path("d\\hold-unit-1.tmp");
		let job = thread::spawn({
			let path = path.clone();
			move || before_convert(&path)
		});
		while held_convert().is_none() {
			thread::yield_now();
		}
		assert_eq!(held_convert(), Some(path.clone()));
		release_convert();
		job.join().expect("the held job ends");
		assert_eq!(held_convert(), None);
		// Disarmed by the hold it made: the next convert of the name goes on at once.
		before_convert(&path);
	}
}
