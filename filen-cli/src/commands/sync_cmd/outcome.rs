//! What a sync left undone, which decides its exit status.

use std::{
	borrow::Cow,
	collections::{HashMap, HashSet},
};

use filen_sdk_rs::sync_engine::{PairId, SyncEvent, SyncReport};

/// What `report`'s pass left undone, each as a short phrase; empty when it left nothing.
/// `resolved` are the conflicts it found that were resolved since, which the next pass applies.
pub(super) fn pass_problems(report: &SyncReport, resolved: &HashSet<String>) -> Vec<String> {
	PassLeft::of(report, resolved).problems()
}

/// What a completed pass left undone.
struct PassLeft {
	paused: bool,
	refused: Option<String>,
	/// The paths of the conflicts it found that have not been resolved since.
	conflicts: HashSet<String>,
	held: usize,
	errors: usize,
	interrupted: usize,
}

impl PassLeft {
	fn of(report: &SyncReport, resolved: &HashSet<String>) -> Self {
		Self {
			paused: report.paused,
			refused: report.refused.as_ref().map(ToString::to_string),
			conflicts: report
				.conflict_paths()
				.filter(|rel_path| !resolved.contains(*rel_path))
				.map(str::to_owned)
				.collect(),
			held: report.held_deletions(),
			errors: report.errors.len(),
			interrupted: report.interrupted,
		}
	}

	/// Each thing left undone, as a short phrase.
	fn problems(&self) -> Vec<String> {
		if self.paused {
			return vec!["paused".to_owned()];
		}
		let mut problems = Vec::new();
		if let Some(reason) = &self.refused {
			problems.push(format!("refused: {reason}"));
		}
		if !self.conflicts.is_empty() {
			problems.push(format!("{} conflict(s)", self.conflicts.len()));
		}
		if self.held > 0 {
			problems.push(format!("{} held deletion(s)", self.held));
		}
		if self.errors > 0 {
			problems.push(format!("{} error(s)", self.errors));
		}
		if self.interrupted > 0 {
			problems.push(format!("{} action(s) not carried out", self.interrupted));
		}
		problems
	}
}

/// How the last pass of a pair ended.
enum LastPass {
	Completed(PassLeft),
	Failed,
}

/// The problem of a pair whose last pass failed outright.
pub(super) fn failed_pass_problems() -> Vec<String> {
	vec!["the pass failed".to_owned()]
}

/// The message a sync fails with when one of `pairs`, each a label and its problems, has any;
/// `None` when none has.
pub(super) fn problems_summary(pairs: &[(Cow<'_, str>, Vec<String>)]) -> Option<String> {
	let with_problems = pairs
		.iter()
		.filter(|(_, problems)| !problems.is_empty())
		.map(|(label, problems)| format!("{label} ({})", problems.join(", ")))
		.collect::<Vec<_>>();
	(!with_problems.is_empty()).then(|| {
		format!(
			"Sync finished with problems in {}",
			with_problems.join(", ")
		)
	})
}

/// What the last pass of each watched pair left undone, as its events tell.
#[derive(Default)]
pub(super) struct LastPasses {
	last: HashMap<PairId, LastPass>,
	/// The conflicts of each pair resolved during the pass in flight.
	resolved: HashMap<PairId, HashSet<String>>,
	/// The pairs whose pass in flight waits for the cache to finish listing the remote folder.
	awaiting_remote: HashSet<PairId>,
	/// Whether the watches are being stopped.
	stopping: bool,
}

impl LastPasses {
	/// Notes `event` of `pair`: a pass that ended replaces what the one before it left.
	pub(super) fn note(&mut self, pair: PairId, event: &SyncEvent) {
		match event {
			SyncEvent::AwaitingRemote => {
				self.awaiting_remote.insert(pair);
			}
			SyncEvent::PassStarted { .. } => {
				self.awaiting_remote.remove(&pair);
			}
			SyncEvent::PassCompleted { report } => {
				self.awaiting_remote.remove(&pair);
				let resolved = self.resolved.remove(&pair).unwrap_or_default();
				self.last
					.insert(pair, LastPass::Completed(PassLeft::of(report, &resolved)));
			}
			SyncEvent::PassFailed { .. } => {
				let awaited_remote = self.awaiting_remote.remove(&pair);
				self.resolved.remove(&pair);
				// A stopped watch ends a pass that waits for the listing at once, failed. The stop
				// did that, not the pass, which had read nothing yet: the pass before it decides.
				if self.stopping && awaited_remote {
					return;
				}
				self.last.insert(pair, LastPass::Failed);
			}
			// every other event is part of a pass that has not ended
			_ => {}
		}
	}

	/// Notes that the watches are being stopped, so that a pass the stop ends is not counted.
	pub(super) fn stopping(&mut self) {
		self.stopping = true;
	}

	/// Notes that the conflict at `rel_path` of `pair`, which the pass in flight found, was resolved.
	pub(super) fn resolved(&mut self, pair: PairId, rel_path: &str) {
		self.resolved
			.entry(pair)
			.or_default()
			.insert(rel_path.to_owned());
	}

	/// Notes that the conflict at `rel_path` of `pair`, which its last pass found, was resolved
	/// after that pass had ended.
	pub(super) fn resolved_since(&mut self, pair: PairId, rel_path: &str) {
		if let Some(LastPass::Completed(left)) = self.last.get_mut(&pair) {
			left.conflicts.remove(rel_path);
		}
	}

	/// What the last pass of `pair` left undone; nothing when no pass of it has ended.
	pub(super) fn take(&mut self, pair: PairId) -> Vec<String> {
		match self.last.remove(&pair) {
			Some(LastPass::Completed(left)) => left.problems(),
			Some(LastPass::Failed) => failed_pass_problems(),
			None => Vec::new(),
		}
	}
}

#[cfg(test)]
mod tests {
	use std::{borrow::Cow, collections::HashSet};

	use filen_sdk_rs::{
		ErrorKind,
		sync_engine::{PlannedConflict, SyncEvent, SyncMode, SyncReport},
	};

	use super::{LastPasses, pass_problems, problems_summary};

	fn conflict(rel_path: &str) -> PlannedConflict {
		PlannedConflict {
			rel_path: rel_path.to_owned(),
			local: None,
			remote: None,
		}
	}

	#[test]
	fn a_pass_that_left_nothing_has_no_problems() {
		assert!(pass_problems(&SyncReport::default(), &HashSet::new()).is_empty());
	}

	#[test]
	fn a_pass_names_its_conflicts_and_errors() {
		let mut report = SyncReport::default();
		report.conflicts = vec![conflict("a.txt"), conflict("b.txt")];
		report.errors = vec!["Failed to upload c.txt".to_owned()];
		report.interrupted = 3;

		assert_eq!(
			pass_problems(&report, &HashSet::new()),
			["2 conflict(s)", "1 error(s)", "3 action(s) not carried out"]
		);
	}

	#[test]
	fn a_paused_pair_is_a_problem() {
		let mut report = SyncReport::default();
		report.paused = true;

		assert_eq!(pass_problems(&report, &HashSet::new()), ["paused"]);
	}

	#[test]
	fn a_conflict_resolved_since_the_pass_is_no_problem() {
		let mut report = SyncReport::default();
		report.conflicts = vec![conflict("a.txt"), conflict("b.txt")];
		let resolved = HashSet::from(["a.txt".to_owned()]);

		assert_eq!(pass_problems(&report, &resolved), ["1 conflict(s)"]);
	}

	#[test]
	fn the_summary_names_each_pair_with_problems() {
		let pairs = [
			(
				Cow::Borrowed("/a <(twoWay)> /a"),
				vec!["2 conflict(s)".to_owned(), "1 error(s)".to_owned()],
			),
			(Cow::Borrowed("/b <(twoWay)> /b"), Vec::new()),
			(
				Cow::Borrowed("/c <(localBackup)> /c"),
				vec!["paused".to_owned()],
			),
		];

		assert_eq!(
			problems_summary(&pairs).as_deref(),
			Some(
				"Sync finished with problems in /a <(twoWay)> /a (2 conflict(s), 1 error(s)), \
				 /c <(localBackup)> /c (paused)"
			)
		);
	}

	#[test]
	fn a_sync_without_problems_has_no_summary() {
		let pairs = [(Cow::Borrowed("/a <(twoWay)> /a"), Vec::new())];

		assert_eq!(problems_summary(&pairs), None);
	}

	/// A later pass that completed cleanly replaces the failure of the one before it.
	#[test]
	fn the_last_pass_decides() {
		let mut passes = LastPasses::default();
		passes.note(
			1,
			&SyncEvent::PassFailed {
				error: "offline".to_owned(),
				kind: ErrorKind::IO,
			},
		);
		passes.note(
			1,
			&SyncEvent::PassCompleted {
				report: SyncReport::default(),
			},
		);

		assert!(passes.take(1).is_empty());
	}

	#[test]
	fn a_conflict_resolved_during_the_pass_that_found_it_is_no_problem() {
		let mut passes = LastPasses::default();
		let mut report = SyncReport::default();
		report.conflicts = vec![conflict("a.txt")];
		passes.resolved(1, "a.txt");
		passes.note(1, &SyncEvent::PassCompleted { report });

		assert!(passes.take(1).is_empty());
	}

	#[test]
	fn a_conflict_resolved_after_the_pass_that_found_it_is_no_problem() {
		let mut passes = LastPasses::default();
		let mut report = SyncReport::default();
		report.conflicts = vec![conflict("a.txt"), conflict("b.txt")];
		passes.note(1, &SyncEvent::PassCompleted { report });
		passes.resolved_since(1, "a.txt");

		assert_eq!(passes.take(1), ["1 conflict(s)"]);
	}

	fn failed() -> SyncEvent {
		SyncEvent::PassFailed {
			error: "the remote listing did not finish".to_owned(),
			kind: ErrorKind::StaleState,
		}
	}

	/// The stop ends a pass waiting for the listing, failed; the pass before it decides.
	#[test]
	fn a_pass_the_stop_ended_while_it_awaited_the_remote_is_no_problem() {
		let mut passes = LastPasses::default();
		passes.note(
			1,
			&SyncEvent::PassCompleted {
				report: SyncReport::default(),
			},
		);
		passes.note(1, &SyncEvent::AwaitingRemote);
		passes.stopping();
		passes.note(1, &failed());

		assert!(passes.take(1).is_empty());
	}

	#[test]
	fn a_pass_that_failed_awaiting_the_remote_before_the_stop_is_a_problem() {
		let mut passes = LastPasses::default();
		passes.note(1, &SyncEvent::AwaitingRemote);
		passes.note(1, &failed());
		passes.stopping();

		assert_eq!(passes.take(1), ["the pass failed"]);
	}

	/// A pass that had read the remote failed on its own, stop or not.
	#[test]
	fn a_pass_that_started_and_failed_during_the_stop_is_a_problem() {
		let mut passes = LastPasses::default();
		passes.note(1, &SyncEvent::AwaitingRemote);
		passes.note(
			1,
			&SyncEvent::PassStarted {
				mode: SyncMode::TwoWay,
			},
		);
		passes.stopping();
		passes.note(1, &failed());

		assert_eq!(passes.take(1), ["the pass failed"]);
	}

	#[test]
	fn a_failed_last_pass_is_a_problem() {
		let mut passes = LastPasses::default();
		passes.note(
			1,
			&SyncEvent::PassFailed {
				error: "offline".to_owned(),
				kind: ErrorKind::IO,
			},
		);

		assert_eq!(passes.take(1), ["the pass failed"]);
	}
}
