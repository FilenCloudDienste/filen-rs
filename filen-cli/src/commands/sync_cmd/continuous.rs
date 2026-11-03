//! `sync` without `--once`: each pair watched until Ctrl-C.

use std::{collections::HashMap, pin, sync::Arc};

use anyhow::Result;
use filen_sdk_rs::sync_engine::{PairId, PairRecord, SyncEngine, SyncEvent, WatchHandle};
use log::warn;
use tokio::sync::mpsc;

use super::{
	EventLog, LineKind, Settlement, approved_deletions_line,
	outcome::{LastPasses, problems_summary},
	paused_message, resolve_failure_message, resolved_conflict_lines,
};
use crate::{interrupt, ui::UI};

/// Watches each of `pairs` until Ctrl-C, printing what their passes do.
pub(super) async fn sync_continuously(
	ui: &mut UI,
	engine: Arc<SyncEngine>,
	pairs: &[PairRecord],
	log: EventLog<'_>,
) -> Result<()> {
	let (events_tx, mut events_rx) = mpsc::unbounded_channel();
	// before the watches start, so that Ctrl-C stops every watch that started rather than
	// dropping them
	let mut ctrl_c = interrupt::own();
	let watches = pairs.iter().filter(|pair| !pair.paused).map(async |pair| {
		let pair_id = pair.id;
		let events_tx = events_tx.clone();
		let watch = Arc::clone(&engine)
			.watch_observed(
				pair_id,
				Box::new(move |event| {
					let _ = events_tx.send((pair_id, event));
				}),
			)
			.await;
		(pair_id, watch)
	});
	let watches = futures::future::join_all(watches).await;
	// the senders are the watches' now, so the channel closes once every watch has ended
	drop(events_tx);
	// each pair that is not synced, named on its own line now and counted in the exit status
	let mut not_started = HashMap::new();
	// the engine refuses to watch a paused pair, which is no failure of the watch
	for pair in pairs.iter().filter(|pair| pair.paused) {
		ui.print_failure(&paused_message(log.label(pair.id)));
		not_started.insert(pair.id, "paused");
	}
	let mut watch_handles = Vec::new();
	for (pair_id, watch) in watches {
		match watch {
			Ok(watch) => watch_handles.push(watch),
			Err(e) => {
				let label = log.label(pair_id);
				ui.print_failure(&format!("Failed to watch the sync pair {label}: {e}"));
				not_started.insert(pair_id, "not synced");
			}
		}
	}
	if watch_handles.is_empty() {
		return Err(UI::failure("None of the sync pairs could be synced"));
	}

	// receive events and print them, until Ctrl-C
	let mut watching = Watching {
		engine: &engine,
		log,
		last_passes: LastPasses::default(),
		stopping: false,
	};
	let stopped_by_ctrl_c = loop {
		let (pair_id, event) = tokio::select! {
			event = events_rx.recv() => match event {
				Some(event) => event,
				None => break false,
			},
			() = ctrl_c.pressed() => break true,
		};
		watching.process(ui, pair_id, &event).await;
	};

	watching.log.clear_status();
	let _stopping = stopped_by_ctrl_c.then(|| {
		ui.print_muted_to_stderr("Stopping the sync, letting the passes in flight finish...");
		ctrl_c.into_stopping(ui)
	});
	// the flags settle nothing more, as the sync is stopping
	watching.stopping = true;
	watching.last_passes.stopping();
	// waits for each pair's pass in flight, so it finishes and releases the drive lock
	let stopped = futures::future::join_all(watch_handles.into_iter().map(WatchHandle::stop));
	let mut stopped = pin::pin!(stopped);
	// what the passes in flight do meanwhile, as it happens, which decides what they left
	loop {
		tokio::select! {
			_ = &mut stopped => break,
			Some((pair_id, event)) = events_rx.recv() => {
				watching.process(ui, pair_id, &event).await;
			}
		}
	}
	// what the last of them sent just before it ended
	while let Ok((pair_id, event)) = events_rx.try_recv() {
		watching.process(ui, pair_id, &event).await;
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

/// What the watches' events are shown and carried out with.
struct Watching<'a, 'l> {
	engine: &'a SyncEngine,
	log: EventLog<'l>,
	/// What the last pass of each pair left, for the exit status.
	last_passes: LastPasses,
	/// Whether the sync is stopping, so that the flags settle nothing more.
	stopping: bool,
}

impl Watching<'_, '_> {
	/// Prints `event` of `pair`, and carries out what the flags settle of it.
	async fn process(&mut self, ui: &mut UI, pair: PairId, event: &SyncEvent) {
		self.last_passes.note(pair, event);
		let settlement = self.log.print(ui, pair, event);
		if self.stopping {
			return;
		}
		match settlement {
			None => {}
			Some(Settlement::ApproveDeletions { count, pass_token }) => {
				self.engine.approve_deletions(pair, pass_token).await;
				self.log
					.print_approved(ui, pair, &approved_deletions_line(count));
			}
			Some(Settlement::ResolveConflict {
				rel_path,
				resolution,
			}) => match self
				.engine
				.resolve_conflict(pair, rel_path, resolution)
				.await
			{
				Ok(resolved) => {
					self.last_passes.resolved(pair, rel_path);
					for line in resolved_conflict_lines(rel_path, resolution, resolved) {
						self.log.print_approved(ui, pair, &line);
					}
					self.apply_soon(ui, pair).await;
				}
				Err(e) => {
					let message = resolve_failure_message(rel_path, self.log.label(pair), &e);
					self.log.print_failure(ui, &message);
				}
			},
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
