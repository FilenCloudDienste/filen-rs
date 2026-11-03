//! What a watch says about its own health between the events of its passes: passes failing in a
//! row, a change source it lost, and its end.

use filen_sdk_rs::{
	ErrorKind,
	sync_engine::{WatchState, WatchStatus},
};

/// A line about a change of a watch's health.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Notice {
	/// Whether it is a failure or a warning, for stderr, rather than news for stdout.
	pub(super) failure: bool,
	pub(super) text: String,
}

impl Notice {
	fn failure(text: String) -> Self {
		Self {
			failure: true,
			text,
		}
	}

	fn news(text: String) -> Self {
		Self {
			failure: false,
			text,
		}
	}
}

/// What changed from `last` to `current`, the status of one watch. `on_linux` adds the hint that
/// applies to Linux's file watcher.
///
/// A single failed pass is not named: its own event already says so. From the second in a row on,
/// each further one is, and the first already when the server no longer accepts the session,
/// which no retry fixes. A lost change source is named once, as the watch never gets it back.
pub(super) fn notices(last: &WatchStatus, current: &WatchStatus, on_linux: bool) -> Vec<Notice> {
	let mut notices = Vec::new();
	let unauthenticated = current.last_error_kind == Some(ErrorKind::Unauthenticated);
	let failures = current.consecutive_failures;
	if failures > last.consecutive_failures && (failures >= 2 || unauthenticated) {
		let error = current.last_error.as_deref().unwrap_or("unknown error");
		let hint = if unauthenticated {
			". The session is no longer accepted: run `filen logout` and log in again"
		} else {
			", retrying after a growing delay"
		};
		notices.push(Notice::failure(format!(
			"Sync failing ({failures} pass(es) in a row): {error}{hint}"
		)));
	}
	if failures == 0 && last.consecutive_failures > 0 {
		notices.push(Notice::news(format!(
			"Sync works again, after {} failed pass(es)",
			last.consecutive_failures
		)));
	}
	if let Some(reason) = &current.local_events_degraded
		&& last.local_events_degraded.is_none()
	{
		let hint = if on_linux {
			". If the tree has many directories, raising fs.inotify.max_user_watches (sysctl) lets \
			 the watcher cover them"
		} else {
			""
		};
		notices.push(Notice::failure(format!(
			"Not watching local changes as they happen ({reason}): they are synced by the periodic \
			 full pass instead{hint}"
		)));
	}
	if let Some(reason) = &current.remote_events_degraded
		&& last.remote_events_degraded.is_none()
	{
		notices.push(Notice::failure(format!(
			"Not hearing about remote changes as they happen ({reason}): they are synced by the \
			 periodic full pass instead"
		)));
	}
	if current.state != last.state {
		match current.state {
			WatchState::PairRemoved => notices.push(Notice::failure(
				"The sync pair was removed, so its sync has stopped".to_owned(),
			)),
			// the sync stops its watches itself, and pausing a pair is up to it too
			WatchState::Running | WatchState::Paused | WatchState::Stopped => {}
		}
	}
	notices
}

#[cfg(test)]
mod tests {
	use filen_sdk_rs::{
		ErrorKind,
		sync_engine::{WatchState, WatchStatus},
	};

	use super::{Notice, notices};

	fn failing(failures: u32, kind: ErrorKind) -> WatchStatus {
		WatchStatus {
			consecutive_failures: failures,
			last_error: Some("offline".to_owned()),
			last_error_kind: Some(kind),
			..WatchStatus::default()
		}
	}

	fn texts(notices: Vec<Notice>) -> Vec<(bool, String)> {
		notices
			.into_iter()
			.map(|notice| (notice.failure, notice.text))
			.collect()
	}

	#[test]
	fn an_unchanged_status_says_nothing() {
		let status = failing(3, ErrorKind::IO);

		assert!(notices(&status, &status, true).is_empty());
	}

	#[test]
	fn one_failed_pass_is_left_to_its_event_and_the_next_ones_are_named() {
		let healthy = WatchStatus::default();

		assert!(notices(&healthy, &failing(1, ErrorKind::IO), false).is_empty());
		assert_eq!(
			texts(notices(
				&failing(1, ErrorKind::IO),
				&failing(2, ErrorKind::IO),
				false
			)),
			[(
				true,
				"Sync failing (2 pass(es) in a row): offline, retrying after a growing delay"
					.to_owned()
			)]
		);
	}

	#[test]
	fn a_session_the_server_no_longer_accepts_says_to_log_in_again_at_once() {
		assert_eq!(
			texts(notices(
				&WatchStatus::default(),
				&failing(1, ErrorKind::Unauthenticated),
				false
			)),
			[(
				true,
				"Sync failing (1 pass(es) in a row): offline. The session is no longer accepted: \
				 run `filen logout` and log in again"
					.to_owned()
			)]
		);
	}

	#[test]
	fn a_pass_that_works_after_failures_says_so() {
		assert_eq!(
			texts(notices(
				&failing(4, ErrorKind::IO),
				&WatchStatus::default(),
				false
			)),
			[(
				false,
				"Sync works again, after 4 failed pass(es)".to_owned()
			)]
		);
	}

	#[test]
	fn a_lost_local_watcher_is_named_once_with_the_linux_hint_on_linux() {
		let degraded = WatchStatus {
			local_events_degraded: Some("too many watches".to_owned()),
			..WatchStatus::default()
		};

		assert_eq!(
			texts(notices(&WatchStatus::default(), &degraded, true)),
			[(
				true,
				"Not watching local changes as they happen (too many watches): they are synced by \
				 the periodic full pass instead. If the tree has many directories, raising \
				 fs.inotify.max_user_watches (sysctl) lets the watcher cover them"
					.to_owned()
			)]
		);
		assert_eq!(
			texts(notices(&WatchStatus::default(), &degraded, false)),
			[(
				true,
				"Not watching local changes as they happen (too many watches): they are synced by \
				 the periodic full pass instead"
					.to_owned()
			)]
		);
		assert!(notices(&degraded, &degraded, true).is_empty());
	}

	#[test]
	fn a_lost_remote_subscription_is_named_once() {
		let degraded = WatchStatus {
			remote_events_degraded: Some("refused".to_owned()),
			..WatchStatus::default()
		};

		assert_eq!(
			texts(notices(&WatchStatus::default(), &degraded, false)),
			[(
				true,
				"Not hearing about remote changes as they happen (refused): they are synced by the \
				 periodic full pass instead"
					.to_owned()
			)]
		);
		assert!(notices(&degraded, &degraded, false).is_empty());
	}

	#[test]
	fn a_removed_pair_ends_its_watch() {
		let removed = WatchStatus {
			state: WatchState::PairRemoved,
			..WatchStatus::default()
		};

		assert_eq!(
			texts(notices(&WatchStatus::default(), &removed, false)),
			[(
				true,
				"The sync pair was removed, so its sync has stopped".to_owned()
			)]
		);
		let stopped = WatchStatus {
			state: WatchState::Stopped,
			..WatchStatus::default()
		};
		assert!(notices(&WatchStatus::default(), &stopped, false).is_empty());
	}
}
