//! `sync --once`: one pass for each pair, then exit.

use std::{
	collections::HashSet,
	future, pin,
	sync::atomic::{AtomicBool, Ordering},
};

use anyhow::Result;
use filen_sdk_rs::sync_engine::{PairId, PairRecord, SyncEngine, SyncEvent, SyncReport};
use tokio::sync::mpsc;

use super::{
	EventLog, Settle, approved_deletions_line,
	outcome::{failed_pass_problems, pass_problems, problems_summary},
	paused_message, resolve_failure_message, resolved_conflict_lines,
};
use crate::{error::CliError, interrupt, ui::UI};

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
/// what it found. When it settled anything, one more pass of the pair applies it.
///
/// Ctrl-C lets the passes in flight finish, runs no further pass and fails with
/// [`CliError::Interrupted`]; a second Ctrl-C quits.
pub(super) async fn sync_once(
	ui: &mut UI,
	engine: &SyncEngine,
	pairs: &[PairRecord],
	mut log: EventLog<'_>,
	settle: &Settle,
) -> Result<()> {
	let mut ctrl_c = Some(interrupt::own());
	let stopping = AtomicBool::new(false);
	let (messages_tx, mut messages_rx) = mpsc::unbounded_channel();
	// The pairs run at once: the drive lock serialises what they write to the drive, while
	// reading the two sides and the local writes of one overlap another's.
	let passes = futures::future::join_all(
		pairs
			.iter()
			.map(|pair| pass_pair(engine, pair.id, messages_tx.clone(), settle, &stopping)),
	);
	// each pass holds its own sender, so the channel closes once every pass has ended
	drop(messages_tx);
	let mut passes = pin::pin!(passes);
	let mut _stopping = None;
	let reports = loop {
		tokio::select! {
			reports = &mut passes => break reports,
			Some((pair, message)) = messages_rx.recv() => print(ui, &mut log, pair, message),
			() = async {
				match ctrl_c.as_mut() {
					Some(ctrl_c) => ctrl_c.pressed().await,
					None => future::pending().await,
				}
			} => {
				ui.print_muted_to_stderr(
					"Stopping the sync, letting the passes in flight finish...",
				);
				stopping.store(true, Ordering::Relaxed);
				_stopping = ctrl_c.take().map(|ctrl_c| ctrl_c.into_stopping(ui));
			}
		}
	};
	// what the passes sent after the last message the loop printed
	while let Some((pair, message)) = messages_rx.recv().await {
		print(ui, &mut log, pair, message);
	}
	if stopping.load(Ordering::Relaxed) {
		return Err(CliError::Interrupted.into());
	}
	let outcomes = pairs
		.iter()
		.zip(reports)
		.map(|(pair, report)| {
			let problems = match report {
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

fn print(ui: &mut UI, log: &mut EventLog<'_>, pair: PairId, message: Message) {
	match message {
		// what the flags settle of it, the pass settles once it has ended
		Message::Event(event) => {
			let _ = log.print(ui, pair, &event);
		}
		Message::Approved(line) => log.print_approved(ui, pair, &line),
		Message::Paused => {
			let message = paused_message(log.label(pair));
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
	let report = engine.sync_once_observed(pair, &mut observer).await.ok()?;
	if report.paused {
		send(Message::Paused);
		return Some(report);
	}
	if stopping.load(Ordering::Relaxed)
		|| !settle_report(engine, pair, &report, settle, &send).await
	{
		return Some(report);
	}
	engine.sync_once_observed(pair, &mut observer).await.ok()
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
