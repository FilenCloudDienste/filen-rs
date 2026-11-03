//! [cli-doc] sync-modes
//! `sync` keeps local directories in sync with directories in your Filen drive. By default it keeps
//! syncing until you press Ctrl+C: a change on either side is synced shortly after it is made, and
//! the passes in flight finish before the command ends. With `--once`, it runs one sync pass for
//! each sync pair and exits when they are done; Ctrl+C then stops it once the passes in flight
//! have finished.
//!
//! Mass deletions held back by the delete guard, and conflicts (a file changed on both sides), are
//! printed as needing attention. `--approve-mass-deletions` and `--keep-local`, `--keep-remote` or
//! `--keep-both` settle them in the first pass; with `--once`, one more pass then applies what was
//! settled before the command exits.
//!
//! `sync --once` exits with 0 when the last pass of every sync pair completed and left nothing
//! undone: no errors, no held deletions, no conflicts and no paused sync pair. Otherwise it exits
//! with 1, after naming each sync pair and what it left. `sync` without `--once` exits with 0 when
//! Ctrl+C stops it and the last pass of every sync pair left nothing undone, and with 1 when a sync
//! pair could not be synced or its last pass left something. Ctrl+C exits `sync --once` with 130,
//! and a second Ctrl+C, which quits without waiting for the passes in flight, exits either with
//! 130. An invalid command line exits with 2.
//!
//! Sync pairs stay registered in the sync database under the config directory after the run that
//! set them up. A run that cannot set up one of its sync pairs registers none of them. Only one
//! `filen sync` can use that database at a time.

use std::{
	borrow::Cow,
	collections::{HashMap, HashSet},
	io,
	path::Path,
	sync::Arc,
};

use anyhow::{Context, Result};
use clap::Subcommand;
use console::style;
use filen_sdk_rs::{
	auth::Client,
	sync_engine::{
		BaselineInUse, ConflictResolution, ConflictResolved, PairId, PairRecord, SyncEngine,
		SyncEvent, TransferDirection, UnsyncablePath, UnsyncableReason,
	},
};
use tokio::sync::mpsc;

use crate::{
	CliConfig, CommandResult,
	commands::CommandContext,
	docs::DocElement,
	interrupt,
	ui::{UI, write_stdout_line},
	util::RemotePath,
};

mod conflict_resolutions;
mod once;
mod outcome;
mod sync_pairs;

use conflict_resolutions::ConflictResolutions;
use outcome::{LastPasses, problems_summary};

#[derive(Debug, Subcommand)]
pub(crate) enum SyncCommands {
	/// Sync one or more sync pairs, continuously until Ctrl-C or once with --once
	Sync {
		/// Pair of local and remote directories to sync.
		/// Format: `<local>:<remote>` (two-way) or `<local>:<mode>:<remote>`, where `<mode>` is one of `twoWay` (or `tw`), `localToRemote` (or `localToCloud`, `ltc`), `remoteToLocal` (or `cloudToLocal`, `ctl`), `localBackup` (or `lb`), `remoteBackup` (or `cloudBackup`, `cb`).
		/// Alternatively, specify a syncPairs.yaml.
		/// If none are specified, the default syncPairs.yaml under the config dir is included.
		// todo: better command docs!
		#[arg(short = 'p', long = "pair")]
		pairs: Vec<String>,
		/// Run one sync pass for each sync pair, then exit.
		#[arg(long)]
		once: bool,
		/// Keep syncing until Ctrl-C, which is what `sync` does without --once.
		/// Kept for compatibility with the TypeScript CLI.
		#[arg(long, conflicts_with = "once")]
		continuous: bool,
		/// Disables the delete guard (applied only on the first sync pass).
		#[arg(long)]
		approve_mass_deletions: bool,
		/// When a conflict occurs at the given path or under it, keep the local side.
		/// The path is relative to the root of each sync pair.
		/// Applied only on the first sync pass.
		#[arg(long, value_name = "RELATIVE_PATH")]
		keep_local: Vec<String>,
		/// When a conflict occurs at the given path or under it, keep the remote side.
		/// The path is relative to the root of each sync pair.
		/// Applied only on the first sync pass.
		#[arg(long, value_name = "RELATIVE_PATH")]
		keep_remote: Vec<String>,
		/// When a conflict occurs at the given path or under it, keep both sides.
		/// The path is relative to the root of each sync pair.
		/// Applied only on the first sync pass.
		#[arg(long, value_name = "RELATIVE_PATH")]
		keep_both: Vec<String>,
	},
}

/// The commands that close the docs' "Transferring Files" section.
pub(crate) const DOCS: &[DocElement] = &[
	DocElement::DocFragment("sync-modes"),
	DocElement::CommandHelp("sync"),
];

impl SyncCommands {
	pub(super) async fn execute(self, ctx: CommandContext<'_>) -> Result<CommandResult> {
		let CommandContext {
			config,
			ui,
			client,
			working_path,
		} = ctx;
		match self {
			Self::Sync {
				pairs,
				once,
				// the default, which `--once` turns off
				continuous: _,
				approve_mass_deletions,
				keep_local,
				keep_remote,
				keep_both,
			} => {
				let settle = Settle {
					approve_mass_deletions,
					conflict_resolutions: ConflictResolutions::new(
						keep_local,
						keep_remote,
						keep_both,
					)?,
				};
				// before logging in, so that a bad definition fails at once
				let parsed = sync_pairs::parse_sync_pairs(&config.config_dir, &pairs)?;
				let client = client.get(ui).await?;
				sync(
					config,
					ui,
					client.clone(),
					working_path,
					parsed,
					once,
					&settle,
				)
				.await?;
			}
		}
		Ok(CommandResult::default())
	}
}

/// What the flags settle of what the first pass of each pair finds.
struct Settle {
	approve_mass_deletions: bool,
	conflict_resolutions: ConflictResolutions,
}

async fn sync(
	config: &CliConfig,
	ui: &mut UI,
	client: Arc<Client>,
	working_path: &RemotePath,
	parsed: Vec<sync_pairs::ParsedPair>,
	once: bool,
	settle: &Settle,
) -> Result<()> {
	// before the engine is opened, so that a bad definition leaves its database as it was
	let pair_definitions = sync_pairs::resolve_sync_pairs(&client, working_path, parsed).await?;

	// init sync engine
	let db_path = config.config_dir.join("sync-engine.db");
	let engine = Arc::new(
		SyncEngine::open(client.clone(), db_path.clone())
			.await
			.map_err(|e| UI::failure(&open_failure(&e, &db_path)))?,
	);
	let pair_ids = sync_pairs::register_sync_pairs(&engine, &client, &pair_definitions)
		.await
		.context("Failed to add sync pairs")?;

	// only this run's pairs: the database also holds every pair earlier runs registered
	let pairs = sync_pairs::records_of(
		engine
			.list_pairs()
			.await
			.context("Failed to list sync pairs")?,
		&pair_ids,
	)?;
	// once, before the passes start: labelling an event must not ask the server for anything
	let labels = sync_pairs::pair_labels(&client, &pairs).await;
	let log = EventLog::new(&labels, settle);
	if once {
		once::sync_once(ui, &engine, &pairs, log, settle).await
	} else {
		sync_continuously(ui, engine, &pairs, log).await
	}
}

/// Watches each of `pairs` until Ctrl-C, printing what their passes do.
async fn sync_continuously(
	ui: &mut UI,
	engine: Arc<SyncEngine>,
	pairs: &[PairRecord],
	mut log: EventLog<'_>,
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
	let mut last_passes = LastPasses::default();
	let stopped_by_ctrl_c = loop {
		let (pair_id, event) = tokio::select! {
			event = events_rx.recv() => match event {
				Some(event) => event,
				None => break false,
			},
			() = ctrl_c.pressed() => break true,
		};
		last_passes.note(pair_id, &event);
		match log.print(pair_id, &event) {
			None => {}
			Some(Settlement::ApproveDeletions { count, pass_token }) => {
				engine.approve_deletions(pair_id, pass_token).await;
				log.print_approved(pair_id, &approved_deletions_line(count));
			}
			Some(Settlement::ResolveConflict {
				rel_path,
				resolution,
			}) => match engine.resolve_conflict(pair_id, rel_path, resolution).await {
				Ok(resolved) => {
					last_passes.resolved(pair_id, rel_path);
					for line in resolved_conflict_lines(rel_path, resolution, resolved) {
						log.print_approved(pair_id, &line);
					}
				}
				Err(e) => {
					ui.print_failure(&resolve_failure_message(rel_path, log.label(pair_id), &e))
				}
			},
		}
	};

	let _stopping = stopped_by_ctrl_c.then(|| {
		ui.print_muted_to_stderr("Stopping the sync, letting the passes in flight finish...");
		ctrl_c.into_stopping(ui)
	});
	last_passes.stopping();
	// waits for each pair's pass in flight, so it finishes and releases the drive lock
	futures::future::join_all(watch_handles.into_iter().map(|watch| watch.stop())).await;
	// what the passes in flight did since, which decides what they left; the flags settle nothing
	// more, as the sync has stopped
	while let Ok((pair_id, event)) = events_rx.try_recv() {
		last_passes.note(pair_id, &event);
		let _ = log.print(pair_id, &event);
	}

	if !stopped_by_ctrl_c {
		return Err(UI::failure("Every watch of the sync pairs ended"));
	}
	ui.print_muted("Stopped.");
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

/// Prints the events of a run's pairs, and says which of them the flags settle.
struct EventLog<'a> {
	labels: &'a HashMap<PairId, String>,
	settle: &'a Settle,
	/// The pairs that have finished a pass. Mass deletion approvals and conflict resolutions are
	/// only applied on a pair's first pass, where we can be sure they were intentional.
	passed: HashSet<PairId>,
}

/// What the flags settle of what a pass found.
enum Settlement<'e> {
	ApproveDeletions {
		count: usize,
		pass_token: &'e str,
	},
	ResolveConflict {
		rel_path: &'e str,
		resolution: ConflictResolution,
	},
}

impl<'a> EventLog<'a> {
	fn new(labels: &'a HashMap<PairId, String>, settle: &'a Settle) -> Self {
		Self {
			labels,
			settle,
			passed: HashSet::new(),
		}
	}

	/// The label [`sync_pairs::pair_labels`] gave `pair`. Every pair of the run has one; should
	/// an event name another, it is named by its id rather than ending the sync.
	fn label(&self, pair: PairId) -> Cow<'a, str> {
		match self.labels.get(&pair) {
			Some(label) => Cow::Borrowed(label),
			None => Cow::Owned(format!("sync pair {pair}")),
		}
	}

	/// Prints `event` of `pair`, with what needs the user's attention, and returns what the flags
	/// settle of it, which the caller carries out.
	fn print<'e>(&mut self, pair: PairId, event: &'e SyncEvent) -> Option<Settlement<'e>> {
		self.print_line(pair, &format_sync_event(event));
		if let SyncEvent::PassCompleted { .. } | SyncEvent::PassFailed { .. } = event {
			self.passed.insert(pair);
		}
		let is_first_pass = !self.passed.contains(&pair);
		match event {
			SyncEvent::DeletionsHeld {
				count, pass_token, ..
			} => {
				if is_first_pass && self.settle.approve_mass_deletions {
					return Some(Settlement::ApproveDeletions {
						count: *count,
						pass_token,
					});
				}
				self.print_needs_attention(
					pair,
					&format!(
						"Held back {count} deletions. If you wish to execute them, re-run the command with --approve-mass-deletions"
					),
				);
			}
			// paths the engine leaves alone, which no other event names
			SyncEvent::PassCompleted { report } => {
				for notice in unsyncable_notices(&report.unsyncable) {
					self.print_needs_attention(pair, &notice);
				}
			}
			SyncEvent::Conflict { rel_path } => {
				if is_first_pass
					&& let Some(resolution) = self.settle.conflict_resolutions.for_path(rel_path)
				{
					return Some(Settlement::ResolveConflict {
						rel_path,
						resolution,
					});
				}
				self.print_needs_attention(
					pair,
					&format!(
						"Conflict detected. To resolve it, re-run the command with --keep-local/--keep-remote/--keep-both \"{rel_path}\""
					),
				);
			}
			// every other event says all there is to say in its line
			_ => {}
		}
		None
	}

	fn print_line(&self, pair: PairId, msg: &str) {
		write_stdout_line(
			&mut io::stdout().lock(),
			&format!(
				"{} {}: {}",
				style(format!(
					"[{}]",
					chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
				))
				.dim(),
				style(format!("[{}]", self.label(pair))).bold(),
				msg,
			),
		);
	}

	fn print_needs_attention(&self, pair: PairId, msg: &str) {
		self.print_line(
			pair,
			&format!("{} {}", style("[NEEDS ATTENTION]").yellow().bold(), msg),
		);
	}

	fn print_approved(&self, pair: PairId, msg: &str) {
		self.print_line(
			pair,
			&format!("{} {}", style("[APPROVED]").green().bold(), msg),
		);
	}
}

fn approved_deletions_line(count: usize) -> String {
	format!("Approved {count} deletions")
}

/// What resolving the conflict at `rel_path` with `resolution` did, `resolved` saying where the
/// losing local copy went, if anywhere: kept beside the winner, or moved aside into the trash.
fn resolved_conflict_lines(
	rel_path: &str,
	resolution: ConflictResolution,
	resolved: ConflictResolved,
) -> Vec<String> {
	let kept = match resolution {
		ConflictResolution::KeepLocal => "kept local version",
		ConflictResolution::KeepRemote => "kept remote version",
		ConflictResolution::KeepBoth => "kept both versions",
	};
	let mut lines = vec![format!("Conflict resolved for {rel_path}, {kept}")];
	if let Some(kept_as) = resolved.kept_as {
		lines.push(format!(
			"Conflict resolved for {rel_path}, local copy kept as {kept_as}"
		));
	}
	if let Some(quarantined) = resolved.quarantined {
		lines.push(format!(
			"Conflict resolved for {rel_path}, moved aside to {}",
			quarantined.display()
		));
	}
	lines
}

fn resolve_failure_message(rel_path: &str, label: Cow<'_, str>, e: &filen_sdk_rs::Error) -> String {
	format!("Failed to resolve the conflict for {rel_path} in the sync pair {label}: {e}")
}

fn paused_message(label: Cow<'_, str>) -> String {
	format!("The sync pair {label} is paused, so it is not synced")
}

/// Why the sync engine's database at `db_path` could not be opened, as `error` says.
fn open_failure(error: &filen_sdk_rs::Error, db_path: &Path) -> String {
	if error.downcast_ref::<BaselineInUse>().is_some() {
		format!(
			"Another `filen sync` (or another program using {}) is running; only one can use the \
			 sync database at a time",
			db_path.display()
		)
	} else {
		format!(
			"Failed to open the sync engine at {}: {error}",
			db_path.display()
		)
	}
}

/// One line for each path the pass left alone and why, so files that go unsynced, a name
/// collision's above all, do not hide behind a pass that otherwise completed without errors.
fn unsyncable_notices(unsyncable: &[UnsyncablePath]) -> Vec<String> {
	unsyncable
		.iter()
		.map(|path| {
			let hint = match path.reason {
				UnsyncableReason::LocalNameCollision | UnsyncableReason::RemoteNameCollision => {
					". Rename one of them to sync both"
				}
				UnsyncableReason::RemoteNameNotStorable { .. } => {
					". Rename it in the cloud to sync it"
				}
				UnsyncableReason::InvalidName { .. }
				| UnsyncableReason::RepeatedFailure { .. }
				| UnsyncableReason::RemoteUndecodable
				| UnsyncableReason::RemoteInvalidName { .. }
				| UnsyncableReason::RemoteBrokenParent
				| UnsyncableReason::LocalAlias { .. } => "",
			};
			format!("Cannot sync {}: {}{hint}", path.rel_path, path.reason)
		})
		.collect()
}

fn format_sync_event(event: &SyncEvent) -> String {
	match event {
		SyncEvent::PassStarted { .. } => "Sync pass started".to_string(),
		SyncEvent::Refused { reason } => format!("Sync pass refused: {reason}"),
		SyncEvent::Planned { actions } => format!("Planned {actions} action(s)"),
		SyncEvent::Conflict { rel_path } => {
			format!("Conflict: {rel_path} changed on both sides, left untouched")
		}
		SyncEvent::DeletionsHeld {
			count,
			reason,
			pass_token: _,
		} => format!("Held back {count} deletion(s): {reason}"),
		SyncEvent::Uploading { rel_path } => format!("Uploaded {rel_path}"),
		SyncEvent::Downloading { rel_path } => format!("Downloaded {rel_path}"),
		SyncEvent::Progress {
			rel_path,
			direction,
			bytes,
			total,
		} => {
			let verb = match direction {
				TransferDirection::Upload => "Uploading",
				TransferDirection::Download => "Downloading",
			};
			let percent = (bytes * 100).checked_div(*total).unwrap_or(100);
			format!("{verb} {rel_path}: {percent}% ({bytes}/{total} bytes)")
		}
		SyncEvent::CreatingRemoteDir { rel_path } => {
			format!("Created remote directory {rel_path}")
		}
		SyncEvent::CreatingLocalDir { rel_path } => {
			format!("Created local directory {rel_path}")
		}
		SyncEvent::TrashingRemote { rel_path } => format!("Trashed remote {rel_path}"),
		SyncEvent::DeletingLocal { rel_path } => format!("Deleted local {rel_path}"),
		SyncEvent::MovingRemote { from, to } => format!("Moved remote {from} -> {to}"),
		SyncEvent::MovingLocal { from, to } => format!("Moved local {from} -> {to}"),
		SyncEvent::Quarantined { rel_path, bin_path } => format!(
			"Moved the local copy of {rel_path} aside to {}",
			bin_path.display()
		),
		SyncEvent::ActionFailed { rel_path, error } => {
			format!("Failed to sync {rel_path}: {error}")
		}
		SyncEvent::Interrupted { actions } => {
			format!("Sync pass interrupted, {actions} action(s) not carried out")
		}
		SyncEvent::AdoptedBaseline { rel_path } => {
			format!("{rel_path} is already in sync")
		}
		SyncEvent::PassCompleted { report } => format!(
			"Sync pass completed: {} uploaded, {} downloaded, {} remote / {} local dir(s) created, \
				 {} trashed remotely, {} deleted locally, {} moved remotely, {} moved locally, \
				 {} conflict(s), {} error(s)",
			report.uploaded,
			report.downloaded,
			report.remote_dirs_created,
			report.local_dirs_created,
			report.remotely_trashed,
			report.locally_deleted,
			report.moved_remote,
			report.moved_local,
			report.conflicts.len(),
			report.errors.len(),
		),
		SyncEvent::PassFailed { error, .. } => format!("Sync pass failed: {error}"),
		SyncEvent::AwaitingRemote => "Waiting for the remote folder to finish indexing".to_string(),
		SyncEvent::Scanning { progress } => format!(
			"Scanning local files: {} found, {}/{} file(s) hashed ({}/{} bytes)",
			progress.entries,
			progress.files_hashed,
			progress.files_to_hash,
			progress.bytes_hashed,
			progress.bytes_to_hash,
		),
	}
}

#[cfg(test)]
mod tests {
	use filen_sdk_rs::sync_engine::{UnsyncablePath, UnsyncableReason};

	use std::path::Path;

	use filen_sdk_rs::{Error, ErrorKind, sync_engine::BaselineInUse};

	use super::{open_failure, unsyncable_notices};

	/// A pass that held case twins back completes with no errors, so the twins are named on their
	/// own lines, with what resolves them.
	#[test]
	fn a_pass_that_held_back_name_collisions_names_each_path() {
		let unsyncable = [
			UnsyncablePath {
				rel_path: "docs/Report.txt".to_string(),
				reason: UnsyncableReason::LocalNameCollision,
			},
			UnsyncablePath {
				rel_path: "docs/report.txt".to_string(),
				reason: UnsyncableReason::LocalNameCollision,
			},
			UnsyncablePath {
				rel_path: "R.txt".to_string(),
				reason: UnsyncableReason::RemoteNameCollision,
			},
		];

		assert_eq!(
			unsyncable_notices(&unsyncable),
			[
				"Cannot sync docs/Report.txt: another local item's name is the same once case and \
				 Unicode form are folded. Rename one of them to sync both",
				"Cannot sync docs/report.txt: another local item's name is the same once case and \
				 Unicode form are folded. Rename one of them to sync both",
				"Cannot sync R.txt: another remote item's name is the same once case is folded. \
				 Rename one of them to sync both",
			]
		);
	}

	/// Every other reason a path is left alone is named the same way, without the rename hint.
	#[test]
	fn a_pass_that_left_a_path_alone_for_another_reason_names_it() {
		let unsyncable = [UnsyncablePath {
			rel_path: "nul".to_string(),
			reason: UnsyncableReason::InvalidName {
				detail: "reserved".to_string(),
			},
		}];

		assert_eq!(
			unsyncable_notices(&unsyncable),
			["Cannot sync nul: the remote would reject this name: reserved"]
		);
	}

	/// A second sync on the same database is told what holds it, not shown the engine's error.
	#[test]
	fn a_database_another_sync_holds_says_so() {
		let db_path = Path::new("/config/sync-engine.db");
		let error = Error::custom_with_source(
			ErrorKind::IO,
			BaselineInUse {
				path: db_path.to_path_buf(),
			},
			None::<&str>,
		);

		assert_eq!(
			open_failure(&error, db_path),
			"Another `filen sync` (or another program using /config/sync-engine.db) is running; \
			 only one can use the sync database at a time"
		);
	}

	#[test]
	fn a_pass_that_left_nothing_alone_has_no_notices() {
		assert!(unsyncable_notices(&[]).is_empty());
	}
}
