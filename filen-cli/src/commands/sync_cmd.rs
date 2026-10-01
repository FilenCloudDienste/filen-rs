use std::{collections::HashSet, path::PathBuf, sync::Arc};

use anyhow::{Context, Result};
use filen_sdk_rs::{
	fs::{HasUUID as _, categories::NonRootFileType},
	sync_engine::{SyncEngine, SyncEvent, SyncMode, SyncReport, WatchState, WatchStatus},
};
use tokio::{select, sync::mpsc};

use crate::{CliConfig, auth::LazyClient, ui::UI, util::RemotePath};

// todo: pair management, conflict resolution, deletion approval, other sync modes, --json output

/// Two-way sync between a local directory and a directory in the Filen drive, printing every event
/// the engine reports, until Ctrl-C.
pub(crate) async fn sync(
	config: &CliConfig,
	ui: &mut UI,
	client: &mut LazyClient,
	working_path: &RemotePath,
	local: &str,
	remote: &str,
) -> Result<()> {
	let client = client.get(ui).await?.clone();
	// Subscribed before the slow setup below, so a Ctrl-C pressed during it still stops the sync.
	let mut stop_rx = crate::CTRLC_TX.subscribe();
	let remote_path = working_path.navigate(remote);
	let remote_uuid = match client
		.find_item_at_path(&remote_path.0)
		.await
		.context("Failed to find remote directory")?
	{
		Some(NonRootFileType::Dir(dir)) => dir.uuid(),
		Some(NonRootFileType::Root(_)) => {
			return Err(UI::failure(
				"Cannot sync the root of the Filen drive, choose a directory inside it",
			));
		}
		Some(NonRootFileType::File(_)) => {
			return Err(UI::failure(&format!(
				"{remote_path} is a file, not a directory"
			)));
		}
		None => {
			return Err(UI::failure(&format!(
				"Remote directory {remote_path} not found"
			)));
		}
	};

	// The engine keeps what was last in sync in this database, so a later run of the same pair
	// picks up where this one left off instead of syncing from scratch.
	let db_path = config.config_dir.join("sync-engine.db");
	let engine = Arc::new(
		SyncEngine::open(client, db_path.clone())
			.await
			.map_err(|e| {
				UI::failure(&format!(
					"Failed to open the sync engine at {}: {e}",
					db_path.display()
				))
			})?,
	);
	let pair = engine
		.add_pair(PathBuf::from(local), remote_uuid, SyncMode::TwoWay)
		.await
		.map_err(|e| UI::failure(&format!("Failed to set up the sync pair: {e}")))?;

	// The engine calls its observer synchronously mid-pass, so events go through a channel and are
	// printed here.
	let (events_tx, mut events_rx) = mpsc::unbounded_channel();

	let watch = Arc::clone(&engine)
		.watch_observed(
			pair,
			// Sending fails only once this command has stopped listening, on its way out.
			Box::new(move |event| {
				let _ = events_tx.send(event);
			}),
		)
		.await
		.map_err(|e| UI::failure(&format!("Failed to start syncing: {e}")))?;
	ui.print_success(&format!(
		"Syncing {local} with {remote_path} (Ctrl-C to stop)"
	));
	let mut status = watch.status();
	let mut last_status = WatchStatus::default();
	let mut printed_failures = HashSet::new();
	loop {
		select! {
			Some(event) = events_rx.recv() => print_event(ui, &mut printed_failures, event),
			changed = status.changed() => {
				if changed.is_err() {
					break;
				}
				let current = status.borrow_and_update().clone();
				print_status_change(ui, &last_status, &current);
				if matches!(current.state, WatchState::Stopped | WatchState::PairRemoved) {
					break;
				}
				last_status = current;
			}
			_ = stop_rx.recv() => {
				ui.print_muted("Stopping after the current pass (Ctrl-C again to quit immediately)...");
				// Keep printing while the pass finishes: its last events are the ones being waited for.
				let stop = watch.stop();
				tokio::pin!(stop);
				loop {
					select! {
						() = &mut stop => break,
						Some(event) = events_rx.recv() => print_event(ui, &mut printed_failures, event),
						// Leaves the drive lock to expire on the server rather than wait for the pass.
						_ = stop_rx.recv() => std::process::exit(130),
					}
				}
				break;
			}
		}
	}
	while let Ok(event) = events_rx.try_recv() {
		print_event(ui, &mut printed_failures, event);
	}
	Ok(())
}

/// `printed_failures` holds this pass's failures already printed as they happened, so the report at
/// the end of the pass prints only the ones no event carried (e.g. the drive lock being unavailable).
fn print_event(ui: &mut UI, printed_failures: &mut HashSet<String>, event: SyncEvent) {
	match event {
		SyncEvent::PassStarted { .. } => {
			printed_failures.clear();
			ui.print_muted("Sync pass started")
		}
		SyncEvent::Planned { actions } => ui.print_muted(&format!("Planned {actions} action(s)")),
		SyncEvent::Refused { reason } => ui.print_failure(&format!("Sync pass refused: {reason}")),
		SyncEvent::Conflict { rel_path } => ui.print_warning(&format!(
			"Conflict: {rel_path} changed on both sides and is left untouched"
		)),
		SyncEvent::DeletionsHeld { count, reason, .. } => ui.print_warning(&format!(
			"Held back {count} deletion(s) ({reason}); approving them is not supported here yet"
		)),
		SyncEvent::Uploading { rel_path } => ui.print(&format!("upload    {rel_path}")),
		SyncEvent::Downloading { rel_path } => ui.print(&format!("download  {rel_path}")),
		SyncEvent::CreatingRemoteDir { rel_path } => ui.print(&format!("mkdir ↑   {rel_path}")),
		SyncEvent::CreatingLocalDir { rel_path } => ui.print(&format!("mkdir ↓   {rel_path}")),
		SyncEvent::TrashingRemote { rel_path } => ui.print(&format!("trash ↑   {rel_path}")),
		SyncEvent::DeletingLocal { rel_path } => ui.print(&format!("delete ↓  {rel_path}")),
		SyncEvent::MovingRemote { from, to } => ui.print(&format!("move ↑    {from} -> {to}")),
		SyncEvent::MovingLocal { from, to } => ui.print(&format!("move ↓    {from} -> {to}")),
		SyncEvent::AdoptedBaseline { rel_path } => ui.print_muted(&format!("in sync   {rel_path}")),
		SyncEvent::Quarantined { rel_path, bin_path } => ui.print_warning(&format!(
			"Moved the local copy of {rel_path} aside to {}",
			bin_path.display()
		)),
		SyncEvent::ActionFailed { rel_path, error } => {
			// The same text the pass records in its report's errors.
			let failure = format!("{rel_path}: {error}");
			ui.print_failure(&failure);
			printed_failures.insert(failure);
		}
		SyncEvent::Interrupted { actions } => ui.print_warning(&format!(
			"Sync pass interrupted, {actions} action(s) left for the next pass"
		)),
		SyncEvent::PassCompleted { report } => print_report(ui, printed_failures, &report),
		SyncEvent::PassFailed { error } => ui.print_failure(&format!("Sync pass failed: {error}")),
		// Byte counts per transfer, several per second: too noisy for a line each.
		SyncEvent::Progress { .. } => {}
		other => ui.print_muted(&format!("{other:?}")),
	}
}

fn print_report(ui: &mut UI, printed_failures: &HashSet<String>, report: &SyncReport) {
	for error in &report.errors {
		if !printed_failures.contains(error) {
			ui.print_failure(error);
		}
	}
	let counts = [
		(report.uploaded, "uploaded"),
		(report.downloaded, "downloaded"),
		(report.remote_dirs_created, "remote dirs created"),
		(report.local_dirs_created, "local dirs created"),
		(report.remotely_trashed, "trashed remotely"),
		(report.locally_deleted, "deleted locally"),
		(report.moved_remote, "moved remotely"),
		(report.moved_local, "moved locally"),
		(report.conflicts.len(), "conflicts"),
		(report.held_deletions(), "deletions held"),
		(report.unsyncable.len(), "unsyncable"),
		(report.deferred_paths, "deferred"),
		(report.errors.len(), "errors"),
	]
	.into_iter()
	.filter(|(count, _)| *count > 0)
	.map(|(count, what)| format!("{count} {what}"))
	.collect::<Vec<_>>();
	if counts.is_empty() {
		ui.print_success("Sync pass completed, everything is in sync");
	} else if report.errors.is_empty() {
		ui.print_success(&format!("Sync pass completed: {}", counts.join(", ")));
	} else {
		ui.print_warning(&format!("Sync pass completed: {}", counts.join(", ")));
	}
	for path in &report.unsyncable {
		ui.print_warning(&format!("Cannot sync {path}"));
	}
}

/// Print what changed in the watch's health; pass failures themselves arrive as events.
fn print_status_change(ui: &mut UI, last: &WatchStatus, current: &WatchStatus) {
	if current.consecutive_failures > last.consecutive_failures {
		ui.print_muted(&format!(
			"{} pass(es) failed in a row, retrying with backoff",
			current.consecutive_failures
		));
	}
	if let Some(reason) = &current.local_events_degraded
		&& last.local_events_degraded.is_none()
	{
		ui.print_warning(&format!(
			"Not watching local changes live ({reason}); they are picked up by the periodic pass"
		));
	}
	if let Some(reason) = &current.remote_events_degraded
		&& last.remote_events_degraded.is_none()
	{
		ui.print_warning(&format!(
			"Not watching remote changes live ({reason}); they are picked up by the periodic pass"
		));
	}
	if current.state == WatchState::PairRemoved {
		ui.print_failure("The sync pair was removed, stopping");
	}
}
