//! The questions `sync` asks at the terminal about what a pass left for a person to settle: a
//! batch of deletions the delete guard held back, and a conflict.
//!
//! A question is asked once a pass has ended, with what its report says, and at most once about
//! the same thing: a held batch is not asked about again while the guard holds it under the same
//! token, which changes when the batch does, and a conflict is not asked about again while it
//! lasts. What the flags settle in the first pass is not asked about.
//!
//! An approval only lives in the engine of this process, so it can only be given here, while the
//! sync runs: there is no command to give it later.
//!
//! The prompt blocks a thread on the terminal, so it runs on a blocking one, and the sync goes on
//! receiving what its watches report meanwhile, holding it back ([`Questions::defer`]) to print
//! after the answer. Ctrl-C typed at the prompt reaches the prompt as a key, not as a signal: it
//! answers "not now" and leaves the sync running; a further Ctrl-C stops it.

use std::{
	collections::{HashMap, HashSet, VecDeque},
	fmt,
	future::{self, Future},
	io::IsTerminal,
	pin::Pin,
};

use filen_sdk_rs::sync_engine::{
	ConflictResolution, GuardReason, PairId, PlannedAction, SyncReport,
};
use inquire::InquireError;
use tokio::task::JoinError;

use crate::ui::UI;

/// How many held deletions a question lists before it only counts the rest.
const MAX_LISTED: usize = 20;

/// How many updates about single items an open question holds back for after its answer, before
/// the rest are only counted. A pass reports one for each item it touches, and nothing bounds how
/// long a question stays open, so keeping them all could grow without limit. What is not about a
/// single item, a pass's start and summary and a watch's health, is always kept: a few for each
/// pass that runs while the question is open.
pub(super) const MAX_DEFERRED_ITEMS: usize = 1000;

/// Whether someone can answer a question at the terminal: stdout and stderr are terminals, a
/// prompt can be answered ([`UI::ensure_can_prompt`]), the output is not JSON, and `--no-input`
/// was not given. `--yes` answers none of them.
pub(super) fn can_ask(ui: &UI) -> bool {
	ui.output_is_read_by_a_person()
		&& std::io::stderr().is_terminal()
		&& ui.ensure_can_prompt("").is_ok()
}

/// Deletions the guard held back in a pair's pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct HeldBatch {
	pub(super) pair: PairId,
	/// What the engine's approval names the batch by.
	pub(super) token: String,
	pub(super) count: usize,
	reason: GuardReason,
	/// The held actions, one line each, the first [`MAX_LISTED`] of them.
	listed: Vec<String>,
}

/// Something a person is asked to settle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Question {
	Deletions(HeldBatch),
	Conflict { pair: PairId, rel_path: String },
}

impl Question {
	pub(super) fn pair(&self) -> PairId {
		match self {
			Self::Deletions(batch) => batch.pair,
			Self::Conflict { pair, .. } => *pair,
		}
	}

	/// The lines printed before the prompt, which say what it is about.
	pub(super) fn intro(&self) -> Vec<String> {
		match self {
			Self::Deletions(batch) => {
				let mut lines = vec![format!(
					"Held back {} deletion(s): {}",
					batch.count, batch.reason
				)];
				lines.extend(batch.listed.iter().cloned());
				if may_be_a_fault(&batch.reason) {
					lines.push(
						"A hold like this can mean a missing mount or a fault of the server: apply \
						 them only if you know they are meant"
							.to_owned(),
					);
				}
				lines
			}
			Self::Conflict { rel_path, .. } => {
				vec![format!(
					"Conflict at {rel_path}: it changed on both sides since they were last in sync"
				)]
			}
		}
	}
}

/// What is said of `question` when the answer was "not now".
pub(super) fn left_line(question: &Question) -> String {
	match question {
		Question::Deletions(batch) => format!(
			"The {} deletion(s) stay held, and are not asked about again unless they change",
			batch.count
		),
		Question::Conflict { rel_path, .. } => {
			format!("The conflict at {rel_path} stays, and is not asked about again while it lasts")
		}
	}
}

/// What is said of `question` when it could not be asked, for `error`, with how to settle it
/// instead.
pub(super) fn failed_line(question: &Question, error: &str) -> String {
	match question {
		Question::Deletions(batch) => format!(
			"Could not ask about the {} held deletion(s): {error}. To apply them, re-run the \
			 command with --approve-mass-deletions",
			batch.count
		),
		Question::Conflict { rel_path, .. } => format!(
			"Could not ask about the conflict at {rel_path}: {error}. To resolve it, re-run the \
			 command with --keep-local/--keep-remote/--keep-both \"{rel_path}\""
		),
	}
}

/// What is said of `question` when it was answered once the sync had begun stopping.
pub(super) fn too_late_line(question: &Question) -> String {
	match question {
		Question::Deletions(batch) => format!(
			"Not acted on, as the sync is stopping: the {} deletion(s) stay held",
			batch.count
		),
		Question::Conflict { rel_path, .. } => {
			format!("Not acted on, as the sync is stopping: the conflict at {rel_path} stays")
		}
	}
}

/// Whether the guard holds deletions for `reason` in case they are not meant at all, rather than
/// because there are many of them or the pair has not synced yet.
fn may_be_a_fault(reason: &GuardReason) -> bool {
	match reason {
		GuardReason::ScanIncomplete
		| GuardReason::RemoteUnconverged { .. }
		| GuardReason::RemoteEmptied { .. } => true,
		GuardReason::FirstSyncWithDeletions { .. }
		| GuardReason::ExceededThreshold { .. }
		| GuardReason::RestoreUnfinished { .. } => false,
	}
}

/// The held actions, one indented line each: the first [`MAX_LISTED`], then how many more there
/// are.
fn held_lines(held: &[PlannedAction]) -> Vec<String> {
	let mut lines = held
		.iter()
		.take(MAX_LISTED)
		.map(|action| format!("  {action}"))
		.collect::<Vec<_>>();
	if let Some(more) = held.len().checked_sub(MAX_LISTED)
		&& more > 0
	{
		lines.push(format!("  … and {more} more"));
	}
	lines
}

/// What became of a question.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Decision {
	/// Yes: apply the batch.
	Approve(HeldBatch),
	Resolve {
		pair: PairId,
		rel_path: String,
		resolution: ConflictResolution,
	},
	/// Not now: no, Esc, or Ctrl-C at the prompt. It is not asked about again unless it changes.
	Left(Question),
	/// The prompt could not be shown or read.
	Failed { question: Question, error: String },
	/// Answered once the sync had begun stopping, so nothing is done.
	TooLate(Question),
}

impl Decision {
	/// What the decision comes to once the sync has begun stopping: what it would settle is not
	/// settled.
	fn too_late(self) -> Self {
		match self {
			Self::Approve(batch) => Self::TooLate(Question::Deletions(batch)),
			Self::Resolve { pair, rel_path, .. } => {
				Self::TooLate(Question::Conflict { pair, rel_path })
			}
			Self::Left(_) | Self::Failed { .. } | Self::TooLate(_) => self,
		}
	}
}

/// An option of a conflict's question.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Choice {
	Keep(ConflictResolution),
	NotNow,
}

const CHOICES: [Choice; 4] = [
	Choice::Keep(ConflictResolution::KeepLocal),
	Choice::Keep(ConflictResolution::KeepRemote),
	Choice::Keep(ConflictResolution::KeepBoth),
	Choice::NotNow,
];

impl fmt::Display for Choice {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(match self {
			Self::Keep(ConflictResolution::KeepLocal) => "Keep the local version",
			Self::Keep(ConflictResolution::KeepRemote) => {
				"Keep the remote version (the local one goes to .filen-sync-trash)"
			}
			Self::Keep(ConflictResolution::KeepBoth) => {
				"Keep both (the local one is renamed to <name>.old.<ext>)"
			}
			Self::NotNow => "Not now",
		})
	}
}

/// What the prompt about `batch` answered.
fn deletions_decision(
	batch: HeldBatch,
	answer: Result<Result<bool, InquireError>, JoinError>,
) -> Decision {
	match answer {
		Ok(Ok(true)) => Decision::Approve(batch),
		Ok(
			Ok(false) | Err(InquireError::OperationCanceled | InquireError::OperationInterrupted),
		) => Decision::Left(Question::Deletions(batch)),
		Ok(Err(error)) => Decision::Failed {
			question: Question::Deletions(batch),
			error: error.to_string(),
		},
		Err(error) => Decision::Failed {
			question: Question::Deletions(batch),
			error: error.to_string(),
		},
	}
}

/// What the prompt about the conflict at `rel_path` of `pair` answered.
fn conflict_decision(
	pair: PairId,
	rel_path: String,
	answer: Result<Result<Choice, InquireError>, JoinError>,
) -> Decision {
	match answer {
		Ok(Ok(Choice::Keep(resolution))) => Decision::Resolve {
			pair,
			rel_path,
			resolution,
		},
		Ok(
			Ok(Choice::NotNow)
			| Err(InquireError::OperationCanceled | InquireError::OperationInterrupted),
		) => Decision::Left(Question::Conflict { pair, rel_path }),
		Ok(Err(error)) => Decision::Failed {
			question: Question::Conflict { pair, rel_path },
			error: error.to_string(),
		},
		Err(error) => Decision::Failed {
			question: Question::Conflict { pair, rel_path },
			error: error.to_string(),
		},
	}
}

/// The decision a prompt comes to, once it is answered. Boxed, as a question about deletions and
/// one about a conflict wait on prompts of different types.
type Pending = Pin<Box<dyn Future<Output = Decision> + Send>>;

/// Puts `question` about the pair labelled `label` on the terminal, on a blocking thread.
fn ask(question: Question, label: &str) -> Pending {
	match question {
		Question::Deletions(batch) => {
			let message = format!("Apply these {} deletion(s) of {label}?", batch.count);
			let prompt = tokio::task::spawn_blocking(move || {
				inquire::Confirm::new(&message)
					.with_default(false)
					.with_help_message(
						"local deletions go to .filen-sync-trash, remote ones to the Filen trash; \
						 no keeps them held",
					)
					.prompt()
			});
			Box::pin(async move { deletions_decision(batch, prompt.await) })
		}
		Question::Conflict { pair, rel_path } => {
			let message = format!("Which version of {rel_path} in {label} do you keep?");
			let prompt = tokio::task::spawn_blocking(move || {
				inquire::Select::new(&message, CHOICES.to_vec()).prompt()
			});
			Box::pin(async move { conflict_decision(pair, rel_path, prompt.await) })
		}
	}
}

/// An open question, and what arrived while it had the terminal.
struct Open<T> {
	decision: Pending,
	/// What arrived while the prompt had the terminal, in order: all of it but the updates about
	/// single items past the first [`MAX_DEFERRED_ITEMS`].
	deferred: Vec<T>,
	/// How many updates about single items arrived while the prompt had the terminal, kept or not.
	items: usize,
}

/// What came of an open question: its decision, and what was held back while it was open.
pub(super) struct Answered<T> {
	pub(super) decision: Decision,
	pub(super) deferred: Vec<T>,
	/// How many updates about single items were left out of `deferred`.
	pub(super) not_printed: usize,
}

/// The questions of a sync: what it has asked about, what it is to ask about, and the one open.
/// `T` is what it holds back while a question is open.
pub(super) struct Questions<T> {
	can_ask: bool,
	stopping: bool,
	/// The tokens of the held batches asked about, by pair.
	asked_batches: HashMap<PairId, HashSet<String>>,
	/// The conflicts asked about that the last pass of their pair still reported, by pair.
	asked_conflicts: HashMap<PairId, HashSet<String>>,
	queue: VecDeque<Question>,
	open: Option<Open<T>>,
}

impl<T> Questions<T> {
	/// `can_ask`: whether anyone can answer a question ([`can_ask`]). Without that, it asks
	/// nothing.
	pub(super) fn new(can_ask: bool) -> Self {
		Self {
			can_ask,
			stopping: false,
			asked_batches: HashMap::new(),
			asked_conflicts: HashMap::new(),
			queue: VecDeque::new(),
			open: None,
		}
	}

	pub(super) fn can_ask(&self) -> bool {
		self.can_ask
	}

	/// Notes that the flags settled the held batch `token` of `pair`, which is then not asked
	/// about.
	pub(super) fn settled_batch(&mut self, pair: PairId, token: &str) {
		self.asked_batches
			.entry(pair)
			.or_default()
			.insert(token.to_owned());
	}

	/// Notes that the flags settled the conflict at `rel_path` of `pair`, which is then not asked
	/// about.
	pub(super) fn settled_conflict(&mut self, pair: PairId, rel_path: &str) {
		self.asked_conflicts
			.entry(pair)
			.or_default()
			.insert(rel_path.to_owned());
	}

	/// Queues what `report`, of a pass of `pair` that has ended, leaves to ask about, in place of
	/// what an earlier pass of `pair` left that has not been asked yet.
	pub(super) fn raise(&mut self, pair: PairId, report: &SyncReport) {
		if !self.can_ask || self.stopping {
			return;
		}
		self.queue.retain(|question| question.pair() != pair);
		let asked_conflicts = self.asked_conflicts.entry(pair).or_default();
		// a conflict that went away and comes back is asked about again
		let current = report.conflict_paths().collect::<HashSet<_>>();
		asked_conflicts.retain(|rel_path| current.contains(rel_path.as_str()));
		if let (Some(token), Some(reason)) = (&report.deletion_token, &report.guard)
			&& !self
				.asked_batches
				.get(&pair)
				.is_some_and(|asked| asked.contains(token))
		{
			self.queue.push_back(Question::Deletions(HeldBatch {
				pair,
				token: token.clone(),
				count: report.held_deletions(),
				reason: reason.clone(),
				listed: held_lines(&report.held),
			}));
		}
		for rel_path in report.conflict_paths() {
			if !asked_conflicts.contains(rel_path) {
				self.queue.push_back(Question::Conflict {
					pair,
					rel_path: rel_path.to_owned(),
				});
			}
		}
	}

	/// The next question to ask, now noted as asked, or `None` when one is open, the sync is
	/// stopping or none is left. The caller prints its [`Question::intro`] and then [`opens`]
	/// it.
	///
	/// [`opens`]: Questions::open
	pub(super) fn take_next(&mut self) -> Option<Question> {
		if self.open.is_some() || self.stopping {
			return None;
		}
		let question = self.queue.pop_front()?;
		match &question {
			Question::Deletions(batch) => {
				self.asked_batches
					.entry(batch.pair)
					.or_default()
					.insert(batch.token.clone());
			}
			Question::Conflict { pair, rel_path } => {
				self.asked_conflicts
					.entry(*pair)
					.or_default()
					.insert(rel_path.clone());
			}
		}
		Some(question)
	}

	/// Puts `question`, about the pair labelled `label`, on the terminal.
	pub(super) fn open(&mut self, question: Question, label: &str) {
		self.open = Some(Open {
			decision: ask(question, label),
			deferred: Vec::new(),
			items: 0,
		});
	}

	/// Whether a question has the terminal, so nothing else may be printed.
	pub(super) fn is_open(&self) -> bool {
		self.open.is_some()
	}

	/// Holds `update` back for after the answer while a question is open, or only counts it once
	/// the question holds [`MAX_DEFERRED_ITEMS`] updates about single items (`is_item`); otherwise
	/// hands it back to be shown now.
	pub(super) fn defer(&mut self, update: T, is_item: bool) -> Option<T> {
		let Some(open) = &mut self.open else {
			return Some(update);
		};
		if is_item {
			open.items = open.items.saturating_add(1);
			if open.items > MAX_DEFERRED_ITEMS {
				return None;
			}
		}
		open.deferred.push(update);
		None
	}

	/// Waits for the open question's answer, and closes it. Never resolves while none is open.
	///
	/// Cancel-safe: dropped before the answer, it leaves the question open.
	pub(super) async fn answered(&mut self) -> Answered<T> {
		let Some(open) = &mut self.open else {
			return future::pending().await;
		};
		let decision = (&mut open.decision).await;
		let decision = if self.stopping {
			decision.too_late()
		} else {
			decision
		};
		let open = self.open.take().expect("the question was open above");
		Answered {
			decision,
			deferred: open.deferred,
			not_printed: open.items.saturating_sub(MAX_DEFERRED_ITEMS),
		}
	}

	/// Asks nothing more, as the sync is stopping. A question still open stays open, as its prompt
	/// cannot be taken off the terminal, and its answer settles nothing ([`Decision::TooLate`]).
	pub(super) fn stop(&mut self) {
		self.stopping = true;
		self.queue.clear();
	}
}

#[cfg(test)]
mod tests {
	use filen_sdk_rs::sync_engine::{
		ConflictReason, ConflictResolution, GuardReason, PlannedAction, PlannedActionKind,
		PlannedConflict, PlannedNodeKind, SyncReport,
	};
	use inquire::InquireError;

	use super::{
		Choice, Decision, HeldBatch, MAX_DEFERRED_ITEMS, Open, Question, Questions,
		conflict_decision, deletions_decision, failed_line, held_lines, left_line, too_late_line,
	};

	fn trash(rel_path: &str) -> PlannedAction {
		PlannedAction {
			kind: PlannedActionKind::TrashRemote,
			rel_path: rel_path.to_owned(),
			node: PlannedNodeKind::File,
			size: None,
			transfer: None,
		}
	}

	fn conflict(rel_path: &str) -> PlannedConflict {
		PlannedConflict {
			rel_path: rel_path.to_owned(),
			local: None,
			remote: None,
			reason: ConflictReason::Diverged,
		}
	}

	/// A report that held back the deletion of `a` under `token` and found conflicts at
	/// `conflicts`.
	fn report(token: Option<&str>, conflicts: &[&str]) -> SyncReport {
		let mut report = SyncReport::default();
		if let Some(token) = token {
			report.held = vec![trash("a")];
			report.guard = Some(GuardReason::FirstSyncWithDeletions { deletions: 1 });
			report.deletion_token = Some(token.to_owned());
		}
		report.conflicts = conflicts.iter().map(|path| conflict(path)).collect();
		report
	}

	fn batch(token: &str) -> HeldBatch {
		HeldBatch {
			pair: 1,
			token: token.to_owned(),
			count: 1,
			reason: GuardReason::FirstSyncWithDeletions { deletions: 1 },
			listed: held_lines(&[trash("a")]),
		}
	}

	fn conflict_question(rel_path: &str) -> Question {
		Question::Conflict {
			pair: 1,
			rel_path: rel_path.to_owned(),
		}
	}

	/// Every question `questions` asks from now on, without answering any.
	fn all_asked<T>(questions: &mut Questions<T>) -> Vec<Question> {
		std::iter::from_fn(|| questions.take_next()).collect()
	}

	#[test]
	fn a_pass_raises_its_held_batch_and_each_conflict() {
		let mut questions = Questions::<()>::new(true);
		questions.raise(1, &report(Some("t1"), &["c", "d"]));

		assert_eq!(
			all_asked(&mut questions),
			[
				Question::Deletions(batch("t1")),
				conflict_question("c"),
				conflict_question("d"),
			]
		);
	}

	#[test]
	fn without_a_terminal_nothing_is_asked() {
		let mut questions = Questions::<()>::new(false);
		questions.raise(1, &report(Some("t1"), &["c"]));

		assert_eq!(all_asked(&mut questions), []);
	}

	#[test]
	fn a_batch_asked_about_is_not_asked_again_and_a_changed_one_is() {
		let mut questions = Questions::<()>::new(true);
		questions.raise(1, &report(Some("t1"), &[]));
		assert_eq!(all_asked(&mut questions).len(), 1);

		questions.raise(1, &report(Some("t1"), &[]));
		assert_eq!(all_asked(&mut questions), []);

		questions.raise(1, &report(Some("t2"), &[]));
		assert_eq!(
			all_asked(&mut questions),
			[Question::Deletions(batch("t2"))]
		);
	}

	#[test]
	fn a_conflict_asked_about_is_asked_again_only_once_it_went_away_and_came_back() {
		let mut questions = Questions::<()>::new(true);
		questions.raise(1, &report(None, &["c"]));
		assert_eq!(all_asked(&mut questions).len(), 1);

		questions.raise(1, &report(None, &["c"]));
		assert_eq!(all_asked(&mut questions), []);

		questions.raise(1, &report(None, &[]));
		questions.raise(1, &report(None, &["c"]));
		assert_eq!(all_asked(&mut questions), [conflict_question("c")]);
	}

	#[test]
	fn what_the_flags_settled_is_not_asked_about() {
		let mut questions = Questions::<()>::new(true);
		questions.settled_batch(1, "t1");
		questions.settled_conflict(1, "c");
		questions.raise(1, &report(Some("t1"), &["c", "d"]));

		assert_eq!(all_asked(&mut questions), [conflict_question("d")]);
	}

	#[test]
	fn a_later_pass_replaces_what_an_earlier_one_left_unasked() {
		let mut questions = Questions::<()>::new(true);
		questions.raise(1, &report(Some("t1"), &["c"]));
		questions.raise(2, &report(None, &["e"]));
		questions.raise(1, &report(None, &["d"]));

		assert_eq!(
			all_asked(&mut questions),
			[
				Question::Conflict {
					pair: 2,
					rel_path: "e".to_owned()
				},
				conflict_question("d"),
			]
		);
	}

	#[test]
	fn a_stopping_sync_asks_nothing_more() {
		let mut questions = Questions::<()>::new(true);
		questions.raise(1, &report(None, &["c"]));
		questions.stop();
		questions.raise(1, &report(None, &["d"]));

		assert_eq!(all_asked(&mut questions), []);
	}

	#[test]
	fn a_long_batch_lists_twenty_and_counts_the_rest() {
		let held = (0..25).map(|i| trash(&format!("f{i}"))).collect::<Vec<_>>();
		let lines = held_lines(&held);

		assert_eq!(lines.len(), 21);
		assert_eq!(lines[19], "  trash remote file \"f19\"");
		assert_eq!(lines[20], "  … and 5 more");
		assert_eq!(held_lines(&held[..20]).len(), 20);
	}

	#[test]
	fn a_hold_that_may_be_a_fault_says_so_before_the_question() {
		let mut held = batch("t1");
		held.reason = GuardReason::RemoteEmptied { deletions: 1 };

		assert_eq!(
			Question::Deletions(held).intro(),
			[
				format!(
					"Held back 1 deletion(s): {}",
					GuardReason::RemoteEmptied { deletions: 1 }
				),
				"  trash remote file \"a\"".to_owned(),
				"A hold like this can mean a missing mount or a fault of the server: apply them \
				 only if you know they are meant"
					.to_owned(),
			]
		);
	}

	#[test]
	fn yes_approves_the_batch_and_anything_else_leaves_it() {
		assert_eq!(
			deletions_decision(batch("t1"), Ok(Ok(true))),
			Decision::Approve(batch("t1"))
		);
		for answer in [
			Ok(false),
			Err(InquireError::OperationCanceled),
			// Ctrl-C at the prompt, which reaches it as a key
			Err(InquireError::OperationInterrupted),
		] {
			assert_eq!(
				deletions_decision(batch("t1"), Ok(answer)),
				Decision::Left(Question::Deletions(batch("t1")))
			);
		}
		assert_eq!(
			deletions_decision(batch("t1"), Ok(Err(InquireError::NotTTY))),
			Decision::Failed {
				question: Question::Deletions(batch("t1")),
				error: "The input device is not a TTY".to_owned(),
			}
		);
	}

	#[test]
	fn a_kept_side_resolves_the_conflict_and_not_now_leaves_it() {
		assert_eq!(
			conflict_decision(
				1,
				"c".to_owned(),
				Ok(Ok(Choice::Keep(ConflictResolution::KeepBoth)))
			),
			Decision::Resolve {
				pair: 1,
				rel_path: "c".to_owned(),
				resolution: ConflictResolution::KeepBoth,
			}
		);
		for answer in [
			Ok(Choice::NotNow),
			Err(InquireError::OperationCanceled),
			Err(InquireError::OperationInterrupted),
		] {
			assert_eq!(
				conflict_decision(1, "c".to_owned(), Ok(answer)),
				Decision::Left(conflict_question("c"))
			);
		}
	}

	#[test]
	fn what_is_said_after_an_answer_names_what_it_was_about() {
		let deletions = Question::Deletions(batch("t1"));
		let conflict = conflict_question("c");

		assert_eq!(
			[left_line(&deletions), left_line(&conflict)],
			[
				"The 1 deletion(s) stay held, and are not asked about again unless they change",
				"The conflict at c stays, and is not asked about again while it lasts",
			]
		);
		assert_eq!(
			[too_late_line(&deletions), too_late_line(&conflict)],
			[
				"Not acted on, as the sync is stopping: the 1 deletion(s) stay held",
				"Not acted on, as the sync is stopping: the conflict at c stays",
			]
		);
		assert_eq!(
			[
				failed_line(&deletions, "no tty"),
				failed_line(&conflict, "no tty")
			],
			[
				"Could not ask about the 1 held deletion(s): no tty. To apply them, re-run the \
				 command with --approve-mass-deletions",
				"Could not ask about the conflict at c: no tty. To resolve it, re-run the command \
				 with --keep-local/--keep-remote/--keep-both \"c\"",
			]
		);
	}

	#[tokio::test]
	async fn a_prompt_thread_that_never_returned_fails() {
		let prompt = tokio::spawn(std::future::pending::<Result<bool, InquireError>>());
		prompt.abort();
		let error = prompt.await.expect_err("the task was aborted");
		let message = error.to_string();

		assert_eq!(
			deletions_decision(batch("t1"), Err(error)),
			Decision::Failed {
				question: Question::Deletions(batch("t1")),
				error: message,
			}
		);
	}

	/// `questions` with a question open whose prompt answers `decision`.
	fn answering(questions: &mut Questions<u32>, decision: Decision) {
		questions.open = Some(Open {
			decision: Box::pin(async move { decision }),
			deferred: Vec::new(),
			items: 0,
		});
	}

	#[tokio::test]
	async fn what_an_open_question_held_back_comes_back_in_order_with_its_answer() {
		let mut questions = Questions::new(true);
		assert_eq!(questions.defer(1, false), Some(1));
		answering(&mut questions, Decision::Approve(batch("t1")));
		assert!(questions.is_open());
		assert_eq!(questions.take_next(), None, "one question at a time");
		assert_eq!(questions.defer(2, true), None);
		assert_eq!(questions.defer(3, false), None);

		let answered = questions.answered().await;

		assert_eq!(answered.decision, Decision::Approve(batch("t1")));
		assert_eq!(answered.deferred, [2, 3]);
		assert_eq!(answered.not_printed, 0);
		assert!(!questions.is_open());
		assert_eq!(questions.defer(4, true), Some(4));
	}

	#[tokio::test]
	async fn an_open_question_keeps_a_thousand_items_and_counts_the_rest() {
		for (items, kept, not_printed) in [
			(MAX_DEFERRED_ITEMS, MAX_DEFERRED_ITEMS, 0),
			(MAX_DEFERRED_ITEMS + 1, MAX_DEFERRED_ITEMS, 1),
		] {
			let mut questions = Questions::new(true);
			answering(&mut questions, Decision::Left(conflict_question("c")));
			for item in 0..items {
				assert_eq!(questions.defer(u32::try_from(item).unwrap(), true), None);
			}
			// what is not about a single item is kept past the limit
			assert_eq!(questions.defer(u32::MAX, false), None);

			let answered = questions.answered().await;

			assert_eq!(answered.deferred.len(), kept + 1, "{items} items");
			assert_eq!(answered.deferred.last(), Some(&u32::MAX));
			assert_eq!(answered.not_printed, not_printed, "{items} items");
		}
	}

	#[tokio::test]
	async fn an_answer_once_the_sync_is_stopping_settles_nothing() {
		let mut questions = Questions::<u32>::new(true);
		answering(&mut questions, Decision::Approve(batch("t1")));
		questions.stop();
		assert!(
			questions.is_open(),
			"its prompt cannot be taken off the terminal"
		);

		assert_eq!(
			questions.answered().await.decision,
			Decision::TooLate(Question::Deletions(batch("t1")))
		);
	}
}
