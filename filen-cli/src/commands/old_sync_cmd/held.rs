// todo: delete this

// //! Deletions the engine's guard held back: what `sync` says about them, and the question it puts to
// //! the user before it hands an approval to the engine.
// //!
// //! A held batch is reported once, by the first pass that holds it. Every later pass holding the same
// //! batch (the same token) only counts it in its report line, so the question about it is asked at
// //! most once: a batch that changed has a new token and is reported, and asked about, again.

// use std::{collections::HashSet, fmt, mem};

// use filen_sdk_rs::sync_engine::{GuardReason, SyncEvent, SyncReport, WatchStatus};
// use inquire::InquireError;
// use tokio::task::{JoinError, JoinHandle};

// use crate::ui::UI;

// /// How many held items are listed before the rest are only counted.
// const MAX_LISTED: usize = 20;

// /// How many updates about single items an open question holds back for after its answer, before the
// /// rest are only counted. A pass reports one per item it touches, and nothing bounds how long the
// /// question stays open, so keeping them all could grow without limit. A failure left out still
// /// shows in its pass's report if the pass completes, which prints every failure no printed event
// /// carried.
// ///
// /// Only those are bounded: the updates about a pass or the watch as a whole are all kept, a few for
// /// every pass the watch runs while the question is open (one of them that pass's whole report), so
// /// a question left unanswered for long still adds that much per pass.
// const MAX_DEFERRED_ITEMS: usize = 1000;

// /// Said instead of asking when nobody can answer.
// pub(super) const NO_TERMINAL: &str =
// 	"The deletions stay held: approving them needs `filen sync` running in a terminal";

// /// Something to print that arrived while a question had the terminal.
// // An update is moved through one call, or sits in the buffer of an open question. Boxing every
// // event the engine reports, to make the rare buffered status smaller, is the worse trade.
// #[allow(clippy::large_enum_variant)]
// pub(super) enum Update {
// 	Event(SyncEvent),
// 	Status(WatchStatus),
// 	/// A line of the command's own, such as that it is stopping.
// 	Notice(&'static str),
// 	/// How many updates about single items a question dropped beyond the first
// 	/// [`MAX_DEFERRED_ITEMS`], which are not printed.
// 	NotPrinted(usize),
// }

// impl Update {
// 	/// Whether this is about a single item, of which a pass reports any number, and may be dropped
// 	/// past [`MAX_DEFERRED_ITEMS`]. Everything else is kept. The match names every [`SyncEvent`]
// 	/// variant, so a new one does not compile until it is sorted into one side or the other.
// 	///
// 	/// [`SyncEvent::Quarantined`] is about one item but always kept: it is the only place the bin
// 	/// path of a copy moved aside is named (the pass report does not print it), and no later pass
// 	/// says it again. Each one is a copy really moved, so they are as many as the data says, not as
// 	/// many as the time the question stays open allows.
// 	fn is_item(&self) -> bool {
// 		match self {
// 			Self::Event(
// 				SyncEvent::Conflict { .. }
// 				| SyncEvent::Uploading { .. }
// 				| SyncEvent::Downloading { .. }
// 				// Never reaches a question: `Output::deliver` drops progress ticks first.
// 				| SyncEvent::Progress { .. }
// 				| SyncEvent::CreatingRemoteDir { .. }
// 				| SyncEvent::CreatingLocalDir { .. }
// 				| SyncEvent::TrashingRemote { .. }
// 				| SyncEvent::DeletingLocal { .. }
// 				| SyncEvent::MovingRemote { .. }
// 				| SyncEvent::MovingLocal { .. }
// 				| SyncEvent::ActionFailed { .. }
// 				| SyncEvent::AdoptedBaseline { .. },
// 			) => true,
// 			Self::Event(
// 				SyncEvent::PassStarted { .. }
// 				| SyncEvent::Refused { .. }
// 				| SyncEvent::Planned { .. }
// 				| SyncEvent::DeletionsHeld { .. }
// 				| SyncEvent::Quarantined { .. }
// 				| SyncEvent::Interrupted { .. }
// 				| SyncEvent::PassCompleted { .. }
// 				| SyncEvent::PassFailed { .. },
// 			)
// 			| Self::Status(_)
// 			| Self::Notice(_)
// 			| Self::NotPrinted(_) => false,
// 		}
// 	}
// }

// /// What became of a question about a held batch.
// #[derive(Debug, PartialEq, Eq)]
// pub(super) enum Answer {
// 	/// Yes: hand the engine this token.
// 	Approve(String),
// 	/// No, or Esc: the batch stays held.
// 	Keep,
// 	/// Ctrl-C, which the prompt reads as a key rather than a signal while it has the terminal.
// 	Interrupted,
// 	/// The prompt could not be shown or read.
// 	Failed(String),
// 	/// Answered once the sync had begun stopping: nothing is acted on, and the batch stays held.
// 	TooLate,
// }

// /// Whether a held batch can be put to the user.
// #[derive(Debug, Clone, Copy, PartialEq, Eq)]
// enum Mode {
// 	/// Input and output are both a terminal: ask.
// 	Ask,
// 	/// Piped, redirected, or JSON output: say how to approve, and never block.
// 	Announce,
// 	/// The sync is stopping: no new questions, and nothing to add to the pass reports.
// 	Stopping,
// }

// /// What to say about a batch reported for the first time, after listing it.
// #[derive(Debug, PartialEq, Eq)]
// enum Next {
// 	/// It is held for a reason no approval may override.
// 	Unapprovable,
// 	/// It could be approved, but not from here.
// 	Announce,
// 	/// Ask whether to apply it.
// 	Ask,
// }

// /// An open question about one held batch.
// struct Question {
// 	token: String,
// 	/// The prompt, which blocks a thread on the terminal until it is answered.
// 	answer: JoinHandle<Result<bool, InquireError>>,
// 	/// What the engine and the watch reported while the prompt had the terminal, in order: all of it
// 	/// but the updates about single items past the first [`MAX_DEFERRED_ITEMS`].
// 	deferred: Vec<Update>,
// 	/// How many updates about single items arrived while the prompt had the terminal, kept or not.
// 	items: usize,
// }

// /// The held batches this `sync` has reported, and the question it may have open about one.
// pub(super) struct Held {
// 	mode: Mode,
// 	/// The tokens of every batch already reported.
// 	raised: HashSet<String>,
// 	question: Option<Question>,
// }

// impl Held {
// 	/// `can_ask` is whether anyone can answer a question: input and output are a terminal and the
// 	/// output is not JSON.
// 	pub(super) fn new(can_ask: bool) -> Self {
// 		Self {
// 			mode: if can_ask { Mode::Ask } else { Mode::Announce },
// 			raised: HashSet::new(),
// 			question: None,
// 		}
// 	}

// 	/// Whether a question has the terminal, so nothing else may be printed or drawn.
// 	pub(super) fn is_asking(&self) -> bool {
// 		self.question.is_some()
// 	}

// 	/// Keep `update` for after the answer while a question is open, or only count it once the
// 	/// question holds [`MAX_DEFERRED_ITEMS`] updates about single items; otherwise hand it back to be
// 	/// printed now.
// 	pub(super) fn defer(&mut self, update: Update) -> Option<Update> {
// 		let Some(question) = &mut self.question else {
// 			return Some(update);
// 		};
// 		if update.is_item() {
// 			question.items = question.items.saturating_add(1);
// 			if question.items > MAX_DEFERRED_ITEMS {
// 				return None;
// 			}
// 		}
// 		question.deferred.push(update);
// 		None
// 	}

// 	/// Report the batch `report`'s pass held back, if it is one not reported before: list it and
// 	/// say why it is held, then ask whether to apply it, say how it could be, or say why it cannot.
// 	pub(super) fn raise(&mut self, ui: &mut UI, report: &SyncReport) {
// 		let (Some(reason), Some(token)) = (&report.guard, &report.deletion_token) else {
// 			return;
// 		};
// 		let Some(next) = self.next(reason, token) else {
// 			return;
// 		};
// 		let count = report.held_deletions();
// 		ui.print_warning(&format!("Held back {count} deletion(s) ({reason}):"));
// 		for line in held_lines(&report.held) {
// 			ui.print(&line);
// 		}
// 		match next {
// 			Next::Unapprovable => ui.print_warning(
// 				"They stay held until that clears, and cannot be approved: the guard holds them in case of a missing mount or a backend fault",
// 			),
// 			Next::Announce => ui.print_warning(NO_TERMINAL),
// 			Next::Ask => {
// 				self.question = Some(Question {
// 					// A copy of its own: `raised` keeps the one it holds for the life of the command.
// 					token: token.clone(),
// 					answer: ask(count),
// 					deferred: Vec::new(),
// 					items: 0,
// 				});
// 			}
// 		}
// 	}

// 	/// Wait for the open question's answer, and close it: what was answered, and the updates held
// 	/// back while it was open, ending with how many were not kept if any were not. Never resolves
// 	/// while no question is open.
// 	///
// 	/// Cancel-safe: dropped before the answer, it leaves the question open.
// 	pub(super) async fn answered(&mut self) -> (Answer, Vec<Update>) {
// 		let Some(question) = &mut self.question else {
// 			return std::future::pending().await;
// 		};
// 		let result = (&mut question.answer).await;
// 		let answer = match self.mode {
// 			// The watch that would run the approved pass is being stopped.
// 			Mode::Stopping => Answer::TooLate,
// 			Mode::Ask | Mode::Announce => answer_from(mem::take(&mut question.token), result),
// 		};
// 		let mut deferred = mem::take(&mut question.deferred);
// 		let not_printed = question.items.saturating_sub(MAX_DEFERRED_ITEMS);
// 		if not_printed > 0 {
// 			deferred.push(Update::NotPrinted(not_printed));
// 		}
// 		self.question = None;
// 		(answer, deferred)
// 	}

// 	/// Ask nothing new: the sync is stopping. A question still open stays open, and goes on holding
// 	/// back what is printed, until it is answered; that answer is [`Answer::TooLate`].
// 	///
// 	/// A question can only still be open here if the stop did not come through it: a SIGINT sent
// 	/// from outside the terminal (Ctrl-C typed while the prompt has the terminal answers it), or the
// 	/// watch ending by itself. The prompt's blocking read cannot be cancelled, so `sync` waits for
// 	/// the answer rather than return with the prompt still reading the terminal.
// 	pub(super) fn stop(&mut self) {
// 		self.mode = Mode::Stopping;
// 	}

// 	/// What to do about the batch identified by `token`, or `None` when there is nothing to say:
// 	/// it was reported before, or the sync is stopping.
// 	fn next(&mut self, reason: &GuardReason, token: &str) -> Option<Next> {
// 		let next = match (self.mode, approvable(reason)) {
// 			(Mode::Stopping, _) => return None,
// 			(_, false) => Next::Unapprovable,
// 			(Mode::Ask, true) => Next::Ask,
// 			(Mode::Announce, true) => Next::Announce,
// 		};
// 		self.raised.insert(token.to_owned()).then_some(next)
// 	}
// }

// /// What the prompt's `result` means for the batch identified by `token`.
// fn answer_from(token: String, result: Result<Result<bool, InquireError>, JoinError>) -> Answer {
// 	match result {
// 		Ok(Ok(true)) => Answer::Approve(token),
// 		Ok(Ok(false) | Err(InquireError::OperationCanceled)) => Answer::Keep,
// 		Ok(Err(InquireError::OperationInterrupted)) => Answer::Interrupted,
// 		Ok(Err(error)) => Answer::Failed(error.to_string()),
// 		Err(error) => Answer::Failed(error.to_string()),
// 	}
// }

// /// Whether the user may release a batch held for `reason`. A volume over the limit and a first
// /// sync against a populated destination are judgements a person can make. An incomplete scan, a
// /// remote view that never converged and a remote that listed nothing are what a missing mount or a
// /// backend fault look like: those batches stay held until the condition clears.
// fn approvable(reason: &GuardReason) -> bool {
// 	match reason {
// 		GuardReason::ExceededThreshold { .. } | GuardReason::FirstSyncWithDeletions { .. } => true,
// 		GuardReason::ScanIncomplete
// 		| GuardReason::RemoteUnconverged { .. }
// 		| GuardReason::RemoteEmptied { .. } => false,
// 	}
// }

// /// The held items, one indented line each: the first [`MAX_LISTED`], then how many more there are.
// fn held_lines(held: &[impl fmt::Display]) -> Vec<String> {
// 	let mut lines: Vec<String> = held
// 		.iter()
// 		.take(MAX_LISTED)
// 		.map(|action| format!("  {action}"))
// 		.collect();
// 	if let Some(more) = held.len().checked_sub(MAX_LISTED).filter(|&more| more > 0) {
// 		lines.push(format!("  … and {more} more"));
// 	}
// 	lines
// }

// /// Put the question on the terminal. The prompt blocks its thread until it is answered, so it runs
// /// on a blocking one, raced against everything else the sync is waiting for.
// fn ask(count: usize) -> JoinHandle<Result<bool, InquireError>> {
// 	let message = format!("Apply these {count} deletion(s)?");
// 	tokio::task::spawn_blocking(move || {
// 		inquire::Confirm::new(&message)
// 			.with_default(false)
// 			.with_help_message(
// 				"local deletions go to .filen-sync-trash, remote ones to the Filen trash; no keeps them held",
// 			)
// 			.prompt()
// 	})
// }

// #[cfg(test)]
// pub(super) mod test_support {
// 	use super::*;

// 	/// A [`Held`] with a question about `token` open, answered by `answer` instead of a prompt.
// 	pub(in super::super) fn asking(
// 		token: &str,
// 		answer: JoinHandle<Result<bool, InquireError>>,
// 	) -> Held {
// 		Held {
// 			mode: Mode::Ask,
// 			raised: HashSet::from([token.to_owned()]),
// 			question: Some(Question {
// 				token: token.to_owned(),
// 				answer,
// 				deferred: Vec::new(),
// 				items: 0,
// 			}),
// 		}
// 	}
// }

// #[cfg(test)]
// mod tests {
// 	use super::{test_support::asking, *};

// 	fn over_the_limit() -> GuardReason {
// 		GuardReason::ExceededThreshold {
// 			deletions: 30,
// 			limit: 20,
// 		}
// 	}

// 	#[test]
// 	fn a_short_batch_is_listed_whole() {
// 		assert_eq!(
// 			held_lines(&[
// 				"trash remote file \"a\"",
// 				"delete local file \"b\" (to quarantine)"
// 			]),
// 			vec![
// 				"  trash remote file \"a\"",
// 				"  delete local file \"b\" (to quarantine)"
// 			]
// 		);
// 		let none: [&str; 0] = [];
// 		assert_eq!(held_lines(&none), Vec::<String>::new());
// 	}

// 	#[test]
// 	fn a_long_batch_lists_twenty_and_counts_the_rest() {
// 		let items: Vec<String> = (0..25).map(|i| format!("item {i}")).collect();
// 		let lines = held_lines(&items);
// 		assert_eq!(lines.len(), 21);
// 		assert_eq!(lines[19], "  item 19");
// 		assert_eq!(lines[20], "  … and 5 more");

// 		let exactly: Vec<String> = (0..20).map(|i| format!("item {i}")).collect();
// 		assert_eq!(
// 			held_lines(&exactly).last().map(String::as_str),
// 			Some("  item 19"),
// 			"twenty fit without a count"
// 		);
// 	}

// 	#[test]
// 	fn only_a_volume_or_first_sync_hold_can_be_approved() {
// 		assert!(approvable(&over_the_limit()));
// 		assert!(approvable(&GuardReason::FirstSyncWithDeletions {
// 			deletions: 4
// 		}));
// 		for reason in [
// 			GuardReason::ScanIncomplete,
// 			GuardReason::RemoteUnconverged { deletions: 4 },
// 			GuardReason::RemoteEmptied { deletions: 4 },
// 		] {
// 			assert!(!approvable(&reason), "{reason:?}");
// 		}
// 	}

// 	#[test]
// 	fn a_batch_is_raised_once_and_a_changed_one_again() {
// 		let mut held = Held::new(true);
// 		assert_eq!(held.next(&over_the_limit(), "t1"), Some(Next::Ask));
// 		assert_eq!(
// 			held.next(&over_the_limit(), "t1"),
// 			None,
// 			"the next pass holding the same batch says nothing new"
// 		);
// 		assert_eq!(
// 			held.next(&over_the_limit(), "t2"),
// 			Some(Next::Ask),
// 			"a batch that changed has a new token"
// 		);
// 	}

// 	#[test]
// 	fn what_is_said_depends_on_the_reason_and_the_terminal() {
// 		let mut piped = Held::new(false);
// 		assert_eq!(piped.next(&over_the_limit(), "t1"), Some(Next::Announce));
// 		assert_eq!(
// 			piped.next(&GuardReason::ScanIncomplete, "t2"),
// 			Some(Next::Unapprovable)
// 		);
// 		let mut terminal = Held::new(true);
// 		assert_eq!(
// 			terminal.next(&GuardReason::RemoteEmptied { deletions: 4 }, "t3"),
// 			Some(Next::Unapprovable),
// 			"a terminal does not make a backend fault approvable"
// 		);
// 	}

// 	#[test]
// 	fn a_stopping_sync_raises_nothing() {
// 		let mut held = Held::new(true);
// 		held.stop();
// 		assert_eq!(held.next(&over_the_limit(), "t1"), None);
// 	}

// 	fn prompt_answered(result: Result<bool, InquireError>) -> Answer {
// 		answer_from("t1".to_owned(), Ok(result))
// 	}

// 	#[test]
// 	fn yes_approves_the_batch_the_question_was_about() {
// 		assert_eq!(prompt_answered(Ok(true)), Answer::Approve("t1".to_owned()));
// 	}

// 	#[test]
// 	fn no_and_esc_keep_the_batch_held() {
// 		assert_eq!(prompt_answered(Ok(false)), Answer::Keep);
// 		assert_eq!(
// 			prompt_answered(Err(InquireError::OperationCanceled)),
// 			Answer::Keep
// 		);
// 	}

// 	#[test]
// 	fn ctrl_c_in_the_prompt_interrupts_the_sync() {
// 		assert_eq!(
// 			prompt_answered(Err(InquireError::OperationInterrupted)),
// 			Answer::Interrupted
// 		);
// 	}

// 	#[test]
// 	fn a_prompt_that_cannot_read_the_terminal_fails() {
// 		assert_eq!(
// 			prompt_answered(Err(InquireError::NotTTY)),
// 			Answer::Failed("The input device is not a TTY".to_owned())
// 		);
// 	}

// 	#[tokio::test]
// 	async fn a_prompt_thread_that_never_returned_fails() {
// 		let prompt = tokio::spawn(std::future::pending::<Result<bool, InquireError>>());
// 		prompt.abort();
// 		let error = prompt.await.expect_err("the task was aborted");
// 		let id = error.id();
// 		assert_eq!(
// 			answer_from("t1".to_owned(), Err(error)),
// 			Answer::Failed(format!("task {id} was cancelled"))
// 		);
// 	}

// 	fn notices(updates: &[Update]) -> Vec<&'static str> {
// 		updates
// 			.iter()
// 			.map(|update| match update {
// 				Update::Notice(notice) => *notice,
// 				Update::Event(_) | Update::Status(_) | Update::NotPrinted(_) => "not a notice",
// 			})
// 			.collect()
// 	}

// 	#[tokio::test]
// 	async fn what_a_question_held_back_comes_back_in_order_with_its_answer() {
// 		let mut held = asking("t1", tokio::spawn(async { Ok(true) }));
// 		assert!(held.is_asking());
// 		assert!(held.defer(Update::Notice("first")).is_none());
// 		assert!(held.defer(Update::Notice("second")).is_none());
// 		let (answer, deferred) = held.answered().await;
// 		assert_eq!(answer, Answer::Approve("t1".to_owned()));
// 		assert_eq!(notices(&deferred), ["first", "second"]);
// 		assert!(!held.is_asking(), "the answer closes the question");
// 		assert!(
// 			held.defer(Update::Notice("third")).is_some(),
// 			"nothing is held back once it is answered"
// 		);
// 	}

// 	fn uploading(i: usize) -> Update {
// 		Update::Event(SyncEvent::Uploading {
// 			rel_path: format!("file {i}"),
// 		})
// 	}

// 	fn uploaded_paths(updates: &[Update]) -> Vec<&str> {
// 		updates
// 			.iter()
// 			.filter_map(|update| match update {
// 				Update::Event(SyncEvent::Uploading { rel_path }) => Some(rel_path.as_str()),
// 				_ => None,
// 			})
// 			.collect()
// 	}

// 	#[tokio::test]
// 	async fn a_question_keeps_a_thousand_item_updates_and_only_counts_the_rest() {
// 		let mut held = asking("t1", tokio::spawn(async { Ok(false) }));
// 		for i in 0..100_000 {
// 			assert!(held.defer(uploading(i)).is_none());
// 		}
// 		assert!(held.defer(Update::Notice("stopping")).is_none());
// 		let (_, deferred) = held.answered().await;
// 		assert_eq!(deferred.len(), 1002);
// 		let uploaded = uploaded_paths(&deferred);
// 		assert_eq!(uploaded.len(), 1000);
// 		assert_eq!(uploaded.first(), Some(&"file 0"));
// 		assert_eq!(uploaded.last(), Some(&"file 999"));
// 		assert!(
// 			matches!(
// 				deferred.as_slice(),
// 				[.., Update::Notice("stopping"), Update::NotPrinted(99_000)]
// 			),
// 			"what is not about one item is kept past the limit, and the count comes last"
// 		);
// 	}

// 	#[tokio::test]
// 	async fn a_copy_moved_aside_is_kept_past_the_limit() {
// 		let mut held = asking("t1", tokio::spawn(async { Ok(false) }));
// 		for i in 0..1000 {
// 			assert!(held.defer(uploading(i)).is_none());
// 		}
// 		let quarantined = SyncEvent::Quarantined {
// 			rel_path: "a".to_owned(),
// 			bin_path: ".filen-sync-trash/a".into(),
// 		};
// 		assert!(held.defer(Update::Event(quarantined.clone())).is_none());
// 		assert!(held.defer(uploading(1000)).is_none());
// 		let (_, deferred) = held.answered().await;
// 		assert_eq!(uploaded_paths(&deferred).len(), 1000);
// 		assert!(
// 			matches!(
// 				deferred.as_slice(),
// 				[.., Update::Event(event), Update::NotPrinted(1)] if *event == quarantined
// 			),
// 			"the only notice that a copy went to the bin is kept, and only the upload is counted"
// 		);
// 	}

// 	#[tokio::test]
// 	async fn the_limit_is_a_thousand_item_updates_and_only_more_are_counted() {
// 		for (items, kept, not_printed) in [(1000, 1000, None), (1001, 1000, Some(1))] {
// 			let mut held = asking("t1", tokio::spawn(async { Ok(false) }));
// 			for i in 0..items {
// 				assert!(held.defer(uploading(i)).is_none());
// 			}
// 			let (_, deferred) = held.answered().await;
// 			assert_eq!(uploaded_paths(&deferred).len(), kept, "{items} items");
// 			let counted = deferred.iter().find_map(|update| match update {
// 				Update::NotPrinted(count) => Some(*count),
// 				_ => None,
// 			});
// 			assert_eq!(counted, not_printed, "{items} items");
// 		}
// 	}

// 	#[tokio::test]
// 	async fn a_question_open_when_the_sync_stops_waits_for_its_answer_and_approves_nothing() {
// 		let mut held = asking("t1", tokio::spawn(async { Ok(true) }));
// 		held.stop();
// 		assert!(
// 			held.is_asking(),
// 			"the prompt cannot be cancelled, so its question stays open"
// 		);
// 		assert!(held.defer(Update::Notice("stopping")).is_none());
// 		let (answer, deferred) = held.answered().await;
// 		assert_eq!(answer, Answer::TooLate);
// 		assert_eq!(notices(&deferred), ["stopping"]);
// 	}
// }
