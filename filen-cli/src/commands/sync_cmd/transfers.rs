//! The file transfers of each pair's pass, as its events tell them: what is running and how many
//! bytes the pass has moved, for the status line and the JSON output.

use std::{collections::HashMap, mem};

use filen_sdk_rs::sync_engine::{PairId, SyncEvent, TransferDirection};

/// The transfers of the passes of a run's pairs.
#[derive(Default)]
pub(super) struct Transfers(HashMap<PairId, PassTransfers>);

impl Transfers {
	pub(super) fn note(&mut self, pair: PairId, event: &SyncEvent) {
		self.0.entry(pair).or_default().note(event);
	}

	/// The bytes the transfers of `pair`'s current or last pass going `direction` have moved,
	/// finished or not.
	pub(super) fn moved(&self, pair: PairId, direction: TransferDirection) -> u64 {
		self.0.get(&pair).map_or(0, |pass| pass.moved(direction))
	}

	/// What the transfers going `direction` that are running now have moved, over every pair.
	pub(super) fn running(&self, direction: TransferDirection) -> Running {
		self.0
			.values()
			.map(|pass| pass.running(direction))
			.fold(Running::default(), Running::plus)
	}
}

/// The transfers of one pass of a pair.
#[derive(Default)]
struct PassTransfers {
	upload: Direction,
	download: Direction,
}

impl PassTransfers {
	fn note(&mut self, event: &SyncEvent) {
		match event {
			SyncEvent::PassStarted { .. } => *self = Self::default(),
			SyncEvent::Progress {
				rel_path,
				direction,
				bytes,
				total,
			} => self
				.direction(*direction)
				.progress(rel_path, *bytes, *total),
			SyncEvent::Uploading { rel_path } => {
				self.upload.in_flight.remove(rel_path);
			}
			SyncEvent::Downloading { rel_path } => {
				self.download.in_flight.remove(rel_path);
			}
			// the event does not say which way the failed action went
			SyncEvent::ActionFailed { rel_path, .. } => {
				self.upload.in_flight.remove(rel_path);
				self.download.in_flight.remove(rel_path);
			}
			// what the pass moved stays for its summary
			SyncEvent::Interrupted { .. }
			| SyncEvent::PassCompleted { .. }
			| SyncEvent::PassFailed { .. } => {
				self.upload.in_flight.clear();
				self.download.in_flight.clear();
			}
			SyncEvent::Refused { .. }
			| SyncEvent::Planned { .. }
			| SyncEvent::Conflict { .. }
			| SyncEvent::DeletionsHeld { .. }
			| SyncEvent::CreatingRemoteDir { .. }
			| SyncEvent::CreatingLocalDir { .. }
			| SyncEvent::TrashingRemote { .. }
			| SyncEvent::DeletingLocal { .. }
			| SyncEvent::MovingRemote { .. }
			| SyncEvent::MovingLocal { .. }
			| SyncEvent::Quarantined { .. }
			| SyncEvent::AdoptedBaseline { .. }
			| SyncEvent::AwaitingRemote
			| SyncEvent::Scanning { .. } => {}
		}
	}

	fn direction(&mut self, direction: TransferDirection) -> &mut Direction {
		match direction {
			TransferDirection::Upload => &mut self.upload,
			TransferDirection::Download => &mut self.download,
		}
	}

	fn moved(&self, direction: TransferDirection) -> u64 {
		match direction {
			TransferDirection::Upload => self.upload.moved,
			TransferDirection::Download => self.download.moved,
		}
	}

	fn running(&self, direction: TransferDirection) -> Running {
		let direction = match direction {
			TransferDirection::Upload => &self.upload,
			TransferDirection::Download => &self.download,
		};
		direction
			.in_flight
			.values()
			.map(|&(bytes, total)| Running {
				files: 1,
				bytes,
				total,
			})
			.fold(Running::default(), Running::plus)
	}
}

/// The transfers of a pass going one way.
#[derive(Default)]
struct Direction {
	/// The bytes each running transfer has moved, and the size of its file, by path.
	in_flight: HashMap<String, (u64, u64)>,
	/// The bytes the pass's transfers have moved, finished or not.
	moved: u64,
}

impl Direction {
	fn progress(&mut self, rel_path: &str, bytes: u64, total: u64) {
		let previous = match self.in_flight.get_mut(rel_path) {
			Some(transfer) => mem::replace(transfer, (bytes, total)).0,
			None => {
				self.in_flight.insert(rel_path.to_owned(), (bytes, total));
				0
			}
		};
		// a transfer's bytes never go down, but the sum must not wrap on whatever it is told
		self.moved = self.moved.saturating_add(bytes.saturating_sub(previous));
	}
}

/// What running transfers have moved.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Running {
	pub(super) files: usize,
	/// The bytes they have moved.
	pub(super) bytes: u64,
	/// The size of their files, in bytes.
	pub(super) total: u64,
}

impl Running {
	fn plus(self, other: Self) -> Self {
		Self {
			files: self.files.saturating_add(other.files),
			bytes: self.bytes.saturating_add(other.bytes),
			total: self.total.saturating_add(other.total),
		}
	}
}

#[cfg(test)]
mod tests {
	use filen_sdk_rs::sync_engine::{SyncEvent, SyncMode, SyncReport, TransferDirection};

	use super::{Running, Transfers};

	fn progress(rel_path: &str, direction: TransferDirection, bytes: u64) -> SyncEvent {
		SyncEvent::Progress {
			rel_path: rel_path.to_string(),
			direction,
			bytes,
			total: 100,
		}
	}

	#[test]
	fn a_running_transfer_counts_its_latest_bytes() {
		let mut transfers = Transfers::default();
		transfers.note(1, &progress("a", TransferDirection::Upload, 10));
		transfers.note(1, &progress("a", TransferDirection::Upload, 40));
		transfers.note(2, &progress("b", TransferDirection::Upload, 5));

		assert_eq!(
			transfers.running(TransferDirection::Upload),
			Running {
				files: 2,
				bytes: 45,
				total: 200
			}
		);
		assert_eq!(
			transfers.running(TransferDirection::Download),
			Running::default()
		);
		assert_eq!(transfers.moved(1, TransferDirection::Upload), 40);
	}

	#[test]
	fn a_finished_transfer_stops_running_and_stays_counted_for_the_pass() {
		let mut transfers = Transfers::default();
		transfers.note(1, &progress("a", TransferDirection::Download, 100));
		transfers.note(
			1,
			&SyncEvent::Downloading {
				rel_path: "a".to_string(),
			},
		);

		assert_eq!(
			transfers.running(TransferDirection::Download),
			Running::default()
		);
		assert_eq!(transfers.moved(1, TransferDirection::Download), 100);
	}

	#[test]
	fn a_failed_transfer_stops_running() {
		let mut transfers = Transfers::default();
		transfers.note(1, &progress("a", TransferDirection::Upload, 30));
		transfers.note(
			1,
			&SyncEvent::ActionFailed {
				rel_path: "a".to_string(),
				error: "gone".to_string(),
			},
		);

		assert_eq!(
			transfers.running(TransferDirection::Upload),
			Running::default()
		);
		assert_eq!(transfers.moved(1, TransferDirection::Upload), 30);
	}

	#[test]
	fn a_completed_pass_keeps_its_bytes_until_the_next_one_starts() {
		let mut transfers = Transfers::default();
		transfers.note(1, &progress("a", TransferDirection::Upload, 30));
		transfers.note(
			1,
			&SyncEvent::PassCompleted {
				report: SyncReport::default(),
			},
		);
		assert_eq!(
			transfers.running(TransferDirection::Upload),
			Running::default()
		);
		assert_eq!(transfers.moved(1, TransferDirection::Upload), 30);

		transfers.note(
			1,
			&SyncEvent::PassStarted {
				mode: SyncMode::TwoWay,
			},
		);
		assert_eq!(transfers.moved(1, TransferDirection::Upload), 0);
	}

	#[test]
	fn the_bytes_moved_saturate_rather_than_wrap() {
		let mut transfers = Transfers::default();
		transfers.note(1, &progress("a", TransferDirection::Upload, u64::MAX));
		transfers.note(1, &progress("b", TransferDirection::Upload, 1));

		assert_eq!(transfers.moved(1, TransferDirection::Upload), u64::MAX);
	}
}
