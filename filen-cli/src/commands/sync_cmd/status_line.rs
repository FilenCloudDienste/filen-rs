//! The line at the bottom of a terminal that says what the running transfers have moved, redrawn
//! in place instead of a line for each progress event.

use std::time::{Duration, Instant};

use filen_sdk_rs::sync_engine::TransferDirection;

use super::transfers::{Running, Transfers};
use crate::ui::{StderrLine, format_size};

/// How often the status line is redrawn at most for progress alone.
const REDRAW_INTERVAL: Duration = Duration::from_millis(250);

/// Says when something done at most once per interval is due again.
pub(super) struct Throttle {
	interval: Duration,
	last: Option<Instant>,
}

impl Throttle {
	pub(super) fn new(interval: Duration) -> Self {
		Self {
			interval,
			last: None,
		}
	}

	/// Whether it is due at `now`: the first time, and once `interval` has passed since the last
	/// time it was. When it is, `now` becomes the last time.
	pub(super) fn due(&mut self, now: Instant) -> bool {
		if let Some(last) = self.last
			&& now.saturating_duration_since(last) < self.interval
		{
			return false;
		}
		self.last = Some(now);
		true
	}
}

/// The status line's text, or `None` when no transfer is running.
pub(super) fn status_text(transfers: &Transfers) -> Option<String> {
	let part = |verb: &str, running: Running| {
		(running.files > 0).then(|| {
			format!(
				"{verb} {} file(s): {} / {}",
				running.files,
				format_size(running.bytes),
				format_size(running.total)
			)
		})
	};
	let parts = [
		part("Uploading", transfers.running(TransferDirection::Upload)),
		part(
			"Downloading",
			transfers.running(TransferDirection::Download),
		),
	]
	.into_iter()
	.flatten()
	.collect::<Vec<_>>();
	(!parts.is_empty()).then(|| parts.join(", "))
}

/// The status line on stderr, which must be a terminal.
pub(super) struct StatusLine {
	line: StderrLine,
	throttle: Throttle,
}

impl StatusLine {
	/// The status line, when stderr is a terminal to draw it on.
	pub(super) fn on_stderr() -> Option<Self> {
		StderrLine::on_terminal().map(|line| Self {
			line,
			throttle: Throttle::new(REDRAW_INTERVAL),
		})
	}

	/// Redraws the line from `transfers`, at most once per [`REDRAW_INTERVAL`].
	pub(super) fn tick(&mut self, transfers: &Transfers, now: Instant) {
		if self.throttle.due(now) {
			self.draw(transfers);
		}
	}

	/// Redraws the line from `transfers` now, as after a line printed above it.
	pub(super) fn draw(&mut self, transfers: &Transfers) {
		match status_text(transfers) {
			Some(text) => self.line.show(&text),
			None => self.line.clear(),
		}
	}

	/// Clears the line, so that the next line printed starts on a clean row.
	pub(super) fn clear(&mut self) {
		self.line.clear();
	}
}

#[cfg(test)]
mod tests {
	use std::time::{Duration, Instant};

	use filen_sdk_rs::sync_engine::{SyncEvent, TransferDirection};

	use super::{Throttle, status_text};
	use crate::commands::sync_cmd::transfers::Transfers;

	#[test]
	fn a_throttle_is_due_at_first_and_then_once_per_interval() {
		let start = Instant::now();
		let mut throttle = Throttle::new(Duration::from_millis(250));

		assert!(throttle.due(start));
		assert!(!throttle.due(start + Duration::from_millis(249)));
		assert!(throttle.due(start + Duration::from_millis(250)));
		assert!(!throttle.due(start + Duration::from_millis(499)));
		assert!(throttle.due(start + Duration::from_millis(500)));
	}

	#[test]
	fn the_status_line_is_empty_without_running_transfers() {
		assert_eq!(status_text(&Transfers::default()), None);
	}

	#[test]
	fn the_status_line_sums_the_running_transfers_of_each_direction() {
		let mut transfers = Transfers::default();
		for (pair, rel_path, direction, bytes, total) in [
			(1, "a", TransferDirection::Upload, 1024, 2048),
			(2, "b", TransferDirection::Upload, 0, 2048),
			(1, "c", TransferDirection::Download, 512, 1024),
		] {
			transfers.note(
				pair,
				&SyncEvent::Progress {
					rel_path: rel_path.to_string(),
					direction,
					bytes,
					total,
				},
			);
		}

		assert_eq!(
			status_text(&transfers).as_deref(),
			Some("Uploading 2 file(s): 1 KiB / 4 KiB, Downloading 1 file(s): 512 B / 1 KiB")
		);
	}
}
