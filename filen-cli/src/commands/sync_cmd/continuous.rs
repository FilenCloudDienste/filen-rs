//! `sync` without `--once`: each pair watched until Ctrl-C.

use std::{collections::HashMap, pin, sync::Arc};

use anyhow::Result;
use filen_sdk_rs::sync_engine::{
	ConflictResolution, PairId, PairRecord, SyncEngine, SyncEvent, WatchConfig, WatchHandle,
	WatchStatus,
};
use log::warn;
use tokio::sync::{mpsc, watch};

use super::{
	EventLog, LineKind, Settlement, approved_deletions_line,
	outcome::{LastPasses, problems_summary},
	paused_message,
	questions::{self, Answered, Decision, Questions},
	resolve_failure_message, resolved_conflict_lines,
};
use crate::{interrupt, memory, ui::UI};

/// Watches each of `pairs` on `timings` until Ctrl-C, printing what their passes do, and asks about what they
/// leave for a person to settle when someone can answer ([`questions`]).
pub(super) async fn sync_continuously(
	ui: &mut UI,
	engine: Arc<SyncEngine>,
	pairs: &[PairRecord],
	timings: WatchConfig,
	log: EventLog<'_>,
	questions: Questions<(PairId, Update)>,
) -> Result<()> {
	let (events_tx, mut events_rx) = mpsc::unbounded_channel();
	// before the watches start, so that Ctrl-C stops every watch that started rather than
	// dropping them
	let mut ctrl_c = interrupt::own();
	let watches = pairs.iter().filter(|pair| !pair.paused).map(async |pair| {
		let pair_id = pair.id;
		let events_tx = events_tx.clone();
		let watch = Arc::clone(&engine)
			.watch_with(
				pair_id,
				timings,
				Box::new(move |event| {
					let _ = events_tx.send((pair_id, Update::Event(event)));
				}),
			)
			.await;
		(pair_id, watch)
	});
	let watches = futures::future::join_all(watches).await;
	// each pair that is not synced, named on its own line now and counted in the exit status
	let mut not_started = HashMap::new();
	// the engine refuses to watch a paused pair, which is no failure of the watch
	for pair in pairs.iter().filter(|pair| pair.paused) {
		ui.print_failure(&paused_message(pair.id, log.label(pair.id)));
		not_started.insert(pair.id, "paused");
	}
	let mut watch_handles = Vec::new();
	for (pair_id, watch) in watches {
		match watch {
			Ok(watch) => {
				forward_status(pair_id, watch.status(), events_tx.clone());
				watch_handles.push(watch);
			}
			Err(e) => {
				let label = log.label(pair_id);
				ui.print_failure(&format!("Failed to watch the sync pair {label}: {e}"));
				not_started.insert(pair_id, "not synced");
			}
		}
	}
	// the senders are the watches' and their statuses' now, so the channel closes once every watch
	// has ended
	drop(events_tx);
	if watch_handles.is_empty() {
		return Err(UI::failure("None of the sync pairs could be synced"));
	}

	// receive events and print them, until Ctrl-C
	let mut watching = Watching {
		engine: &engine,
		log,
		last_passes: LastPasses::default(),
		statuses: HashMap::new(),
		questions,
		stopping: false,
	};
	let stopped_by_ctrl_c = loop {
		tokio::select! {
			update = events_rx.recv() => match update {
				Some((pair_id, update)) => watching.arrive(ui, pair_id, update).await,
				None => break false,
			},
			// Ctrl-C typed at a question's prompt reaches the prompt instead
			() = ctrl_c.pressed() => break true,
			answered = watching.questions.answered(), if watching.questions.is_open() => {
				watching.answered(ui, answered).await;
			}
		}
		watching.ask_next(ui);
	};

	watching.log.clear_status();
	// A question still open stays on the terminal (see `Questions::stop`), which these lines then
	// print over: only a Ctrl-C sent from elsewhere can stop the sync while one is open.
	let _stopping = stopped_by_ctrl_c.then(|| {
		ui.print_muted_to_stderr("Stopping the sync, letting the passes in flight finish...");
		ctrl_c.into_stopping(ui)
	});
	// the flags settle nothing more and nothing more is asked, as the sync is stopping
	watching.stopping = true;
	watching.last_passes.stopping();
	watching.questions.stop();
	// waits for each pair's pass in flight, so it finishes and releases the drive lock
	let stopped = futures::future::join_all(watch_handles.into_iter().map(WatchHandle::stop));
	let mut stopped = pin::pin!(stopped);
	// what the passes in flight do meanwhile, as it happens, which decides what they left
	loop {
		tokio::select! {
			_ = &mut stopped => break,
			Some((pair_id, update)) = events_rx.recv() => {
				watching.arrive(ui, pair_id, update).await;
			}
			answered = watching.questions.answered(), if watching.questions.is_open() => {
				watching.answered(ui, answered).await;
			}
		}
	}
	// The prompt of a question still open cannot be cancelled: left behind, it would go on reading
	// the terminal after the command ends, and in the REPL take the keys typed for the next one.
	if watching.questions.is_open() {
		let answered = watching.questions.answered().await;
		watching.answered(ui, answered).await;
	}
	// what the last of them sent just before it ended
	while let Ok((pair_id, update)) = events_rx.try_recv() {
		watching.arrive(ui, pair_id, update).await;
	}
	let Watching {
		mut log,
		mut last_passes,
		..
	} = watching;
	log.clear_status();

	if !stopped_by_ctrl_c {
		return Err(UI::failure("Every watch of the sync pairs ended"));
	}
	ui.print_muted_notice("Stopped.");
	let outcomes = pairs
		.iter()
		.map(|pair| {
			let problems = match not_started.get(&pair.id) {
				Some(problem) => vec![(*problem).to_owned()],
				None => last_passes.take(pair.id),
			};
			(log.label(pair.id), problems)
		})
		.collect::<Vec<_>>();
	match problems_summary(&outcomes) {
		Some(summary) => Err(UI::failure(&summary)),
		None => Ok(()),
	}
}

/// What a continuous sync says when it could not have a watch apply a resolution at once.
const APPLIED_LATER_LINE: &str = "The resolution is applied by the next pass, which the next \
	change or the periodic full pass starts";

/// What a watch sends to be shown, in the order it happened.
// Nearly every update is an event, which the engine hands over by value; boxing each of them to
// shrink the rare status is the worse trade.
#[allow(clippy::large_enum_variant)]
pub(super) enum Update {
	Event(SyncEvent),
	/// The watch's health, which it publishes after every pass.
	Status(WatchStatus),
}

impl Update {
	/// Whether this is about a single item, of which a pass reports any number, so that an open
	/// question may leave it out past [`questions::MAX_DEFERRED_ITEMS`]. The match names every
	/// event, so that a new one is sorted into one side or the other.
	///
	/// A copy moved aside is about one item but always kept: it is the only line that says where
	/// the copy went.
	fn is_item(&self) -> bool {
		match self {
			Self::Event(
				SyncEvent::Conflict { .. }
				| SyncEvent::Uploading { .. }
				| SyncEvent::Downloading { .. }
				| SyncEvent::Progress { .. }
				| SyncEvent::CreatingRemoteDir { .. }
				| SyncEvent::CreatingLocalDir { .. }
				| SyncEvent::TrashingRemote { .. }
				| SyncEvent::DeletingLocal { .. }
				| SyncEvent::MovingRemote { .. }
				| SyncEvent::MovingLocal { .. }
				| SyncEvent::ActionFailed { .. }
				| SyncEvent::AdoptedBaseline { .. }
				| SyncEvent::Scanning { .. },
			) => true,
			Self::Event(
				SyncEvent::PassStarted { .. }
				| SyncEvent::Refused { .. }
				| SyncEvent::Planned { .. }
				| SyncEvent::DeletionsHeld { .. }
				| SyncEvent::Quarantined { .. }
				| SyncEvent::Interrupted { .. }
				| SyncEvent::PassCompleted { .. }
				| SyncEvent::PassFailed { .. }
				| SyncEvent::AwaitingRemote,
			)
			| Self::Status(_) => false,
		}
	}
}

/// Sends each status of `pair`'s watch that `status` receives, starting with the one it holds, to
/// `updates`, until the watch has ended.
fn forward_status(
	pair: PairId,
	mut status: watch::Receiver<WatchStatus>,
	updates: mpsc::UnboundedSender<(PairId, Update)>,
) {
	tokio::spawn(async move {
		// The one the watch started with: it may have lost a change source before its first pass.
		// Each is cloned out of the channel, which keeps the latest for the watch to replace.
		let mut current = status.borrow_and_update().clone();
		loop {
			if updates.send((pair, Update::Status(current))).is_err() {
				return;
			}
			if status.changed().await.is_err() {
				return;
			}
			// The watch publishes its status once a pass has returned and dropped what it held.
			tokio::task::spawn_blocking(memory::release_free_memory);
			current = status.borrow_and_update().clone();
		}
	});
}

/// Where the pass that found a conflict is when it is resolved.
#[derive(Clone, Copy)]
enum PassState {
	/// Still running: the flags resolve a conflict as its event comes.
	InFlight,
	/// Ended: a question is asked once it has.
	Ended,
}

/// What the watches' events are shown and carried out with.
struct Watching<'a, 'l> {
	engine: &'a SyncEngine,
	log: EventLog<'l>,
	/// What the last pass of each pair left, for the exit status.
	last_passes: LastPasses,
	/// The health of each pair's watch as last shown.
	statuses: HashMap<PairId, WatchStatus>,
	/// What is asked about at the terminal, and what is held back while a question is open.
	questions: Questions<(PairId, Update)>,
	/// Whether the sync is stopping, so that the flags settle nothing more.
	stopping: bool,
}

impl Watching<'_, '_> {
	/// Shows `update` of `pair` as it arrives, or holds it back while a question is open.
	async fn arrive(&mut self, ui: &mut UI, pair: PairId, update: Update) {
		// A transfer's progress only redraws the status line, which a question takes off the
		// terminal; the next one after the answer shows where the transfer is.
		if self.questions.is_open()
			&& let Update::Event(SyncEvent::Progress { .. }) = update
		{
			return;
		}
		let is_item = update.is_item();
		if let Some((pair, update)) = self.questions.defer((pair, update), is_item) {
			self.process(ui, pair, update).await;
		}
	}

	/// Asks the next question, if there is one and none is open.
	fn ask_next(&mut self, ui: &mut UI) {
		let Some(question) = self.questions.take_next() else {
			return;
		};
		let pair = question.pair();
		let mut intro = question.intro().into_iter();
		if let Some(first) = intro.next() {
			self.log.print_needs_attention(ui, pair, &first);
		}
		for line in intro {
			self.log.print_line(ui, pair, LineKind::Kept, &line);
		}
		// the prompt has the terminal until it is answered
		self.log.clear_status();
		let label = self.log.label(pair);
		self.questions.open(question, &label);
	}

	/// Carries out what an answer decided, then shows what was held back while it was asked.
	async fn answered(&mut self, ui: &mut UI, answered: Answered<(PairId, Update)>) {
		let Answered {
			decision,
			deferred,
			not_printed,
		} = answered;
		match decision {
			// the engine wakes the watch, whose next pass applies them
			Decision::Approve(batch) => {
				self.engine
					.approve_deletions(batch.pair, &batch.token)
					.await;
				self.log
					.print_approved(ui, batch.pair, &approved_deletions_line(batch.count));
			}
			Decision::Resolve {
				pair,
				rel_path,
				resolution,
			} => {
				self.resolve(ui, pair, &rel_path, resolution, PassState::Ended)
					.await;
			}
			Decision::Left(question) => {
				self.log.print_needs_attention(
					ui,
					question.pair(),
					&questions::left_line(&question),
				);
			}
			Decision::Failed { question, error } => {
				let line = questions::failed_line(&question, &error);
				self.log
					.print_line(ui, question.pair(), LineKind::Failure, &line);
			}
			Decision::TooLate(question) => {
				self.log.print_needs_attention(
					ui,
					question.pair(),
					&questions::too_late_line(&question),
				);
			}
		}
		for (pair, update) in deferred {
			self.process(ui, pair, update).await;
		}
		if not_printed > 0 {
			self.log.print_note(
				ui,
				&format!(
					"{not_printed} more line(s) from while the question was open are not printed"
				),
			);
		}
	}

	/// Shows `update` of `pair`, and carries out what the flags settle of an event.
	async fn process(&mut self, ui: &mut UI, pair: PairId, update: Update) {
		match update {
			Update::Event(event) => self.process_event(ui, pair, &event).await,
			Update::Status(current) => {
				let last = self.statuses.entry(pair).or_default();
				self.log.print_watch_status(ui, pair, last, &current);
				*last = current;
			}
		}
	}

	/// Prints `event` of `pair`, and carries out what the flags settle of it.
	async fn process_event(&mut self, ui: &mut UI, pair: PairId, event: &SyncEvent) {
		self.last_passes.note(pair, event);
		let settlement = self.log.print(ui, pair, event);
		if self.stopping {
			return;
		}
		match settlement {
			None => {}
			Some(Settlement::ApproveDeletions { count, pass_token }) => {
				self.engine.approve_deletions(pair, pass_token).await;
				self.questions.settled_batch(pair, pass_token);
				self.log
					.print_approved(ui, pair, &approved_deletions_line(count));
			}
			Some(Settlement::ResolveConflict {
				rel_path,
				resolution,
			}) => {
				self.questions.settled_conflict(pair, rel_path);
				self.resolve(ui, pair, rel_path, resolution, PassState::InFlight)
					.await;
			}
		}
		// what the pass left is asked about once it has ended
		if let SyncEvent::PassCompleted { report } = event {
			self.questions.raise(pair, report);
		}
	}

	/// Resolves the conflict at `rel_path` of `pair` with `resolution`, found by the pass that
	/// `pass` says, and has the watch apply it.
	async fn resolve(
		&mut self,
		ui: &mut UI,
		pair: PairId,
		rel_path: &str,
		resolution: ConflictResolution,
		pass: PassState,
	) {
		match self
			.engine
			.resolve_conflict(pair, rel_path, resolution)
			.await
		{
			Ok(resolved) => {
				match pass {
					PassState::InFlight => self.last_passes.resolved(pair, rel_path),
					PassState::Ended => self.last_passes.resolved_since(pair, rel_path),
				}
				for line in resolved_conflict_lines(rel_path, resolution, resolved) {
					self.log.print_approved(ui, pair, &line);
				}
				self.apply_soon(ui, pair).await;
			}
			Err(e) => {
				let message = resolve_failure_message(rel_path, self.log.label(pair), &e);
				self.log.print_failure(ui, &message);
			}
		}
	}

	/// Has the watch of `pair` run a pass soon, which applies a conflict resolution. A resolution
	/// only takes effect on the pair's next pass, and nothing else wakes a watch for it, so without
	/// this it would wait for the next change or the periodic full pass, minutes away.
	async fn apply_soon(&mut self, ui: &mut UI, pair: PairId) {
		if let Err(e) = self.engine.request_pass(pair).await {
			warn!("Could not ask the watch of sync pair {pair} for a pass: {e}");
			self.log
				.print_line(ui, pair, LineKind::Kept, APPLIED_LATER_LINE);
		}
	}
}
