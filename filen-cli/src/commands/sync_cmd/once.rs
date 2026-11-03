//! `sync --once`: one pass for each pair, then exit.

use std::{
	collections::{HashMap, HashSet},
	convert::Infallible,
	future::{self, Future},
	pin,
	sync::atomic::{AtomicBool, Ordering},
};

use anyhow::Result;
use filen_sdk_rs::sync_engine::{PairId, PairRecord, SyncEngine, SyncEvent, SyncReport};
use tokio::sync::mpsc;

use super::{
	EventLog, LineKind, Settle, approved_deletions_line,
	outcome::{failed_pass_problems, pass_problems, problems_summary},
	paused_message,
	questions::{self, Decision, Questions},
	resolve_failure_message, resolved_conflict_lines,
};
use crate::{
	error::CliError,
	interrupt::{self, Owning, Stopping},
	memory,
	ui::UI,
};

/// What a pair's pass sends to be printed, in the order it happened.
enum Message {
	/// Boxed, as an event is far larger than the other messages.
	Event(Box<SyncEvent>),
	/// A line saying what the flags settled.
	Approved(String),
	/// The pair is paused, so the pass did not run.
	Paused,
	/// The conflict at `rel_path` could not be resolved.
	ResolveFailed {
		rel_path: String,
		error: filen_sdk_rs::Error,
	},
}

/// Runs one pass of each of `pairs`, printing what it does, and settles what the flags settle of
/// what it found. When it settled anything, one more pass of the pair applies it. Then it asks
/// about what is left for a person to settle, when someone can answer ([`questions`]), and runs
/// one more pass of each pair whose answers settled anything.
///
/// Ctrl-C lets the passes in flight finish, runs no further pass and fails with
/// [`CliError::Interrupted`]; a second Ctrl-C quits.
pub(super) async fn sync_once(
	ui: &mut UI,
	engine: &SyncEngine,
	pairs: &[PairRecord],
	mut log: EventLog<'_>,
	settle: &Settle,
	mut questions: Questions<Infallible>,
) -> Result<()> {
	let mut ctrl_c = CtrlC {
		owning: Some(interrupt::own()),
		_stopping: None,
	};
	let stopping = AtomicBool::new(false);
	// The pairs run at once: the drive lock serialises what they write to the drive, while
	// reading the two sides and the local writes of one overlap another's.
	let reports = run_passes(
		ui,
		&mut log,
		&mut ctrl_c,
		&stopping,
		pairs.iter().map(|pair| pair.id),
		|pair, messages| pass_pair(engine, pair, messages, settle, &stopping),
	)
	.await;
	let mut reports = pairs
		.iter()
		.map(|pair| pair.id)
		.zip(reports)
		.collect::<HashMap<_, _>>();

	if !stopping.load(Ordering::Relaxed) && questions.can_ask() {
		for pair in pairs {
			if let Some(Some(report)) = reports.get(&pair.id)
				&& !report.paused
			{
				questions.raise(pair.id, report);
			}
		}
		let settled = ask(ui, engine, &mut log, &mut ctrl_c, &stopping, &mut questions).await;
		if !settled.is_empty() && !stopping.load(Ordering::Relaxed) {
			let settled = pairs
				.iter()
				.map(|pair| pair.id)
				.filter(|pair| settled.contains(pair))
				.collect::<Vec<_>>();
			let follow_ups = run_passes(
				ui,
				&mut log,
				&mut ctrl_c,
				&stopping,
				settled.iter().copied(),
				|pair, messages| follow_up_pass(engine, pair, messages),
			)
			.await;
			reports.extend(settled.into_iter().zip(follow_ups));
		}
	}

	log.clear_status();
	if stopping.load(Ordering::Relaxed) {
		return Err(CliError::Interrupted.into());
	}
	let outcomes = pairs
		.iter()
		.map(|pair| {
			let problems = match reports.remove(&pair.id).flatten() {
				Some(report) => pass_problems(&report, &HashSet::new()),
				None => failed_pass_problems(),
			};
			(log.label(pair.id), problems)
		})
		.collect::<Vec<_>>();
	match problems_summary(&outcomes) {
		Some(summary) => Err(UI::failure(&summary)),
		None => Ok(()),
	}
}

/// Ctrl-C, while `sync --once` runs.
struct CtrlC {
	/// Until the first Ctrl-C, which the sync handles itself.
	owning: Option<Owning>,
	/// From the first Ctrl-C on, which lets a second one quit.
	_stopping: Option<Stopping>,
}

impl CtrlC {
	/// Resolves on the first Ctrl-C; never after it.
	async fn pressed(&mut self) {
		match self.owning.as_mut() {
			Some(owning) => owning.pressed().await,
			None => future::pending().await,
		}
	}

	/// Says that the sync stops, sets `stopping` and lets a second Ctrl-C quit.
	fn stop(&mut self, ui: &mut UI, log: &mut EventLog<'_>, stopping: &AtomicBool) {
		log.clear_status();
		ui.print_muted_to_stderr("Stopping the sync, letting the passes in flight finish...");
		stopping.store(true, Ordering::Relaxed);
		self._stopping = self.owning.take().map(|owning| owning.into_stopping(ui));
	}
}

/// Runs `pass` for each of `pairs` at once, printing what each sends until all have ended, and
/// returns what each returned, in the order of `pairs`.
async fn run_passes<P, F>(
	ui: &mut UI,
	log: &mut EventLog<'_>,
	ctrl_c: &mut CtrlC,
	stopping: &AtomicBool,
	pairs: impl Iterator<Item = PairId>,
	pass: P,
) -> Vec<Option<SyncReport>>
where
	P: Fn(PairId, mpsc::UnboundedSender<(PairId, Message)>) -> F,
	F: Future<Output = Option<SyncReport>>,
{
	let (messages_tx, mut messages_rx) = mpsc::unbounded_channel();
	let passes = futures::future::join_all(pairs.map(|pair| pass(pair, messages_tx.clone())));
	// each pass holds its own sender, so the channel closes once every pass has ended
	drop(messages_tx);
	let mut passes = pin::pin!(passes);
	let reports = loop {
		tokio::select! {
			reports = &mut passes => break reports,
			Some((pair, message)) = messages_rx.recv() => print(ui, log, pair, message),
			() = ctrl_c.pressed() => ctrl_c.stop(ui, log, stopping),
		}
	};
	// what the passes sent after the last message the loop printed
	while let Some((pair, message)) = messages_rx.recv().await {
		print(ui, log, pair, message);
	}
	reports
}

/// Asks each of `questions` in turn, and carries out what the answers decide. Returns the pairs
/// whose answers settled anything, which only a further pass applies.
///
/// No pass runs meanwhile, so nothing else is printed while a question is open. A Ctrl-C sent from
/// elsewhere while one is open stops the sync once the question is answered: its prompt cannot be
/// taken off the terminal. Ctrl-C typed at the prompt only answers "not now".
async fn ask(
	ui: &mut UI,
	engine: &SyncEngine,
	log: &mut EventLog<'_>,
	ctrl_c: &mut CtrlC,
	stopping: &AtomicBool,
	questions: &mut Questions<Infallible>,
) -> HashSet<PairId> {
	let mut settled = HashSet::new();
	while let Some(question) = questions.take_next() {
		let pair = question.pair();
		let mut intro = question.intro().into_iter();
		if let Some(first) = intro.next() {
			log.print_needs_attention(ui, pair, &first);
		}
		for line in intro {
			log.print_line(ui, pair, LineKind::Kept, &line);
		}
		log.clear_status();
		let label = log.label(pair);
		questions.open(question, &label);
		let answered = tokio::select! {
			answered = questions.answered() => answered,
			() = ctrl_c.pressed() => {
				ctrl_c.stop(ui, log, stopping);
				questions.stop();
				questions.answered().await
			}
		};
		match answered.decision {
			Decision::Approve(batch) => {
				engine.approve_deletions(batch.pair, &batch.token).await;
				log.print_approved(ui, batch.pair, &approved_deletions_line(batch.count));
				settled.insert(batch.pair);
			}
			Decision::Resolve {
				pair,
				rel_path,
				resolution,
			} => match engine.resolve_conflict(pair, &rel_path, resolution).await {
				Ok(resolved) => {
					for line in resolved_conflict_lines(&rel_path, resolution, resolved) {
						log.print_approved(ui, pair, &line);
					}
					settled.insert(pair);
				}
				Err(error) => {
					let message = resolve_failure_message(&rel_path, log.label(pair), &error);
					log.print_failure(ui, &message);
				}
			},
			Decision::Left(question) => {
				log.print_needs_attention(ui, question.pair(), &questions::left_line(&question));
			}
			Decision::Failed { question, error } => {
				let line = questions::failed_line(&question, &error);
				log.print_line(ui, question.pair(), LineKind::Failure, &line);
			}
			Decision::TooLate(question) => {
				log.print_needs_attention(
					ui,
					question.pair(),
					&questions::too_late_line(&question),
				);
			}
		}
	}
	settled
}

/// One more pass of `pair`, which applies what the answers settled. Returns its report, or `None`
/// when it failed, which its [`SyncEvent::PassFailed`] reports.
async fn follow_up_pass(
	engine: &SyncEngine,
	pair: PairId,
	messages: mpsc::UnboundedSender<(PairId, Message)>,
) -> Option<SyncReport> {
	// The receiver goes only once every pass has ended, so a send cannot fail.
	let mut observer = |event: SyncEvent| {
		let _ = messages.send((pair, Message::Event(Box::new(event))));
	};
	let report = engine.sync_once_observed(pair, &mut observer).await;
	tokio::task::spawn_blocking(memory::release_free_memory);
	report.ok()
}

fn print(ui: &mut UI, log: &mut EventLog<'_>, pair: PairId, message: Message) {
	match message {
		// what the flags settle of it, the pass settles once it has ended
		Message::Event(event) => {
			let _ = log.print(ui, pair, &event);
		}
		Message::Approved(line) => log.print_approved(ui, pair, &line),
		Message::Paused => {
			let message = paused_message(pair, log.label(pair));
			log.print_failure(ui, &message);
		}
		Message::ResolveFailed { rel_path, error } => {
			let message = resolve_failure_message(&rel_path, log.label(pair), &error);
			log.print_failure(ui, &message);
		}
	}
}

/// One pass of `pair`, and when the flags settled anything it found, one more to apply that,
/// unless `stopping` is set by then. Returns the report of the last pass that ran, or `None` when
/// it failed, which its [`SyncEvent::PassFailed`] reports.
async fn pass_pair(
	engine: &SyncEngine,
	pair: PairId,
	messages: mpsc::UnboundedSender<(PairId, Message)>,
	settle: &Settle,
	stopping: &AtomicBool,
) -> Option<SyncReport> {
	// The receiver goes only once every pass has ended, so a send cannot fail.
	let send = |message| {
		let _ = messages.send((pair, message));
	};
	let mut observer = |event: SyncEvent| send(Message::Event(Box::new(event)));
	let report = engine.sync_once_observed(pair, &mut observer).await;
	tokio::task::spawn_blocking(memory::release_free_memory);
	let report = report.ok()?;
	if report.paused {
		send(Message::Paused);
		return Some(report);
	}
	if stopping.load(Ordering::Relaxed)
		|| !settle_report(engine, pair, &report, settle, &send).await
	{
		return Some(report);
	}
	let report = engine.sync_once_observed(pair, &mut observer).await;
	tokio::task::spawn_blocking(memory::release_free_memory);
	report.ok()
}

/// Settles what the flags settle of what `report`'s pass found: approves its held deletions and
/// resolves its conflicts. Returns whether it settled anything, which only a further pass applies.
async fn settle_report(
	engine: &SyncEngine,
	pair: PairId,
	report: &SyncReport,
	settle: &Settle,
	send: &impl Fn(Message),
) -> bool {
	let mut settled = false;
	if settle.approve_mass_deletions
		&& let Some(pass_token) = &report.deletion_token
	{
		engine.approve_deletions(pair, pass_token).await;
		send(Message::Approved(approved_deletions_line(
			report.held_deletions(),
		)));
		settled = true;
	}
	for rel_path in report.conflict_paths() {
		let Some(resolution) = settle.conflict_resolutions.for_path(rel_path) else {
			continue;
		};
		match engine.resolve_conflict(pair, rel_path, resolution).await {
			Ok(resolved) => {
				for line in resolved_conflict_lines(rel_path, resolution, resolved) {
					send(Message::Approved(line));
				}
				settled = true;
			}
			Err(error) => send(Message::ResolveFailed {
				rel_path: rel_path.to_owned(),
				error,
			}),
		}
	}
	settled
}
