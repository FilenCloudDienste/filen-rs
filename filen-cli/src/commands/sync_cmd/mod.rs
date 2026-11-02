use std::{collections::HashSet, sync::Arc};

use anyhow::{Context, Result};
use clap::Subcommand;
use console::style;
use filen_sdk_rs::{
	auth::Client,
	sync_engine::{
		ConflictResolution, SyncEngine, SyncEvent, TransferDirection, UnsyncablePath,
		UnsyncableReason,
	},
};
use tokio::{select, sync::mpsc};

use crate::{
	CliConfig, CommandResult,
	commands::{CommandContext, sync_cmd::sync_pairs::format_sync_pair},
	docs::DocElement,
	interrupt,
	ui::UI,
	util::RemotePath,
};

mod conflict_resolutions;
mod sync_pairs;

use conflict_resolutions::ConflictResolutions;

#[derive(Debug, Subcommand)]
pub(crate) enum SyncCommands {
	/// Continuously sync one or more sync pairs
	Sync {
		/// Pair of local and remote directories to sync.
		/// Format: `<local>:<remote>` or `<local>:<mode>:<remote>`, where `<mode>` is one of `twoWay`, `localBackup`, `remoteBackup`, `localToRemote`, `remoteToLocal`.
		/// Alternatively, specify a syncPairs.yaml.
		/// If none are specified, the default syncPairs.yaml under the config dir is included.
		// todo: better command docs!
		#[arg(short = 'p', long = "pair")]
		pairs: Vec<String>,
		/// Only run a single sync pass and exit.
		#[arg(long)]
		once: bool,
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
pub(crate) const DOCS: &[DocElement] = &[DocElement::CommandHelp("sync")];

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
				approve_mass_deletions,
				keep_local,
				keep_remote,
				keep_both,
			} => {
				let conflict_resolutions =
					ConflictResolutions::new(keep_local, keep_remote, keep_both)?;
				let client = client.get(ui).await?;
				sync(
					config,
					ui,
					client.clone(),
					working_path,
					&pairs,
					once,
					approve_mass_deletions,
					conflict_resolutions,
				)
				.await?;
			}
		}
		Ok(CommandResult::default())
	}
}

// todo: better command documentation

#[allow(clippy::too_many_arguments)]
pub(crate) async fn sync(
	config: &CliConfig,
	ui: &mut UI,
	client: Arc<Client>,
	working_path: &RemotePath,
	pair_definitions: &Vec<String>,
	once: bool,
	approve_mass_deletions: bool,
	conflict_resolutions: ConflictResolutions,
) -> Result<()> {
	// init sync engine
	let db_path = config.config_dir.join("sync-engine.db");
	let engine = Arc::new(
		SyncEngine::open(client.clone(), db_path.clone())
			.await
			.map_err(|e| {
				UI::failure(&format!(
					"Failed to open the sync engine at {}: {e}",
					db_path.display()
				))
			})?,
	);
	let default_pair_definitions = vec![
		config
			.config_dir
			.join("syncPairs.yaml")
			.to_str()
			.context("Failed to construct default syncPairs.yaml path")?
			.to_string(),
	];
	let pair_definitions = if pair_definitions.is_empty() {
		// if there are no args, put default {config_dir}/syncPairs.yaml
		&default_pair_definitions
	} else {
		pair_definitions
	};
	let pair_ids = sync_pairs::register_sync_pairs(&engine, &client, working_path, pair_definitions)
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
	// before the watches start, so that Ctrl-C stops every watch that started rather than
	// dropping them
	let mut ctrl_c = interrupt::own();

	let (events_tx, mut events_rx) = mpsc::unbounded_channel();

	// start syncing
	let watch_handles = futures::future::join_all(pairs.iter().map(async |pair| {
		let pair = pair.clone();
		let pair_friendly_name = format_sync_pair(&client, &pair).await?;
		let events_tx = events_tx.clone();
		if once {
			engine
				.sync_once_observed(pair.id, &mut move |event| {
					let _ = events_tx.send((pair.id, event));
				})
				.await
				.with_context(|| format!("Failed to watch sync pair {}", pair_friendly_name))?;
			// todo: print report
			Ok::<_, anyhow::Error>(None)
		} else {
			let watch_handle = engine
				.clone()
				.watch_observed(
					pair.id,
					Box::new(move |event| {
						let _ = events_tx.send((pair.id, event));
					}),
				)
				.await
				.with_context(|| format!("Failed to watch sync pair {}", pair_friendly_name))?;
			Ok(Some(watch_handle))
		}
	}))
	.await
	.into_iter()
	.filter_map(Result::transpose)
	.collect::<Result<Vec<_>>>()?;
	let stop_handles = futures::future::join_all(watch_handles.into_iter().map(|h| h.stop())); // won't be called until awaited

	let mut pairs_that_have_had_passes = HashSet::new();

	loop {
		select! {
			() = ctrl_c.pressed() => {
				ui.print_muted("Stopping...");
				stop_handles.await;
				ui.print_muted("Stopped.");
				events_rx.close();
				break;
			},
			event = events_rx.recv() => {
				let Some((pair_id, event)) = event else { break };
				// receive and print events

				let pair = pairs.iter().find(|p| p.id == pair_id).with_context(|| {
					format!("Received an event for unknown sync pair with id {pair_id}")
				})?;
				let pair_friendly_name = format_sync_pair(&client, pair).await?;
				let mut print_event = |msg: &str| {
					ui.print(&format!(
						"{} {}: {}",
						style(format!(
							"[{}]",
							chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
						))
						.dim(),
						style(format!("[{}]", pair_friendly_name)).bold(),
						msg,
					));
				};
				print_event(&format_sync_event(&event));
				let needs_attention_tag = style("[NEEDS ATTENTION]").yellow().bold();
				let approved_tag = style("[APPROVED]").green().bold();

				// remember whether (and check if) this pair is on its first pass
				// this way, mass deletion approvals and conflict resolutions are only applied
				// on the first pass, where we can be sure they were intentional
				if let SyncEvent::PassCompleted { .. } | SyncEvent::PassFailed { .. } = event {
					pairs_that_have_had_passes.insert(pair_id);
				}
				let is_first_pass = !pairs_that_have_had_passes.contains(&pair_id);

				// if --once is set, stop after all have completed
				if once && pairs_that_have_had_passes.len() == pairs.len() {
					ui.print_success("All pairs synced.");
					break;
				}

				// approve deletions
				if let SyncEvent::DeletionsHeld {
					ref count,
					ref pass_token,
					..
				} = event
				{
					if is_first_pass && approve_mass_deletions {
						engine.approve_deletions(pair_id, pass_token).await;
						print_event(&format!("{} Approved {} deletions", approved_tag, count));
					} else {
						print_event(&format!(
							"{} Held back {} deletions. If you wish to execute them, re-run the command with --approve-mass-deletions",
							needs_attention_tag, count
						));
					}
				}

				// paths the engine leaves alone, which no other event names
				if let SyncEvent::PassCompleted { ref report } = event {
					for notice in unsyncable_notices(&report.unsyncable) {
						print_event(&format!("{} {}", needs_attention_tag, notice));
					}
				}

				// resolve conflicts
				if let SyncEvent::Conflict { ref rel_path } = event {
					if is_first_pass && let Some(resolution) = conflict_resolutions.for_path(rel_path) {
						let resolved = engine
							.resolve_conflict(pair_id, rel_path, resolution)
							.await
							.with_context(|| {
								format!(
									"Failed to resolve conflict for {rel_path} in sync pair {}",
									pair_friendly_name
								)
							})?;
						print_event(&format!(
							"{} Conflict resolved for {rel_path}, {}",
							approved_tag,
							match resolution {
								ConflictResolution::KeepLocal => "kept local version",
								ConflictResolution::KeepRemote => "kept remote version",
								ConflictResolution::KeepBoth => "kept both versions",
							}
						));
						if let Some(kept_as) = resolved.kept_as {
							print_event(&format!(
								"{} Conflict resolved for {rel_path}, local copy kept as {kept_as}",
								approved_tag
							));
						}
						if let Some(trashed_path) = resolved.quarantined {
							print_event(&format!(
								"{} Conflict resolved for {rel_path}, moved aside to {}",
								approved_tag,
								trashed_path.display()
							));
						}
					} else {
						print_event(&format!(
							"{} Conflict detected. To resolve it, re-run the command with --keep-local/--keep-remote/--keep-both \"{}\"",
							needs_attention_tag,
							rel_path,
						));
					}
				}
			}
		}
	}

	Ok(())
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

	use super::unsyncable_notices;

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

	#[test]
	fn a_pass_that_left_nothing_alone_has_no_notices() {
		assert!(unsyncable_notices(&[]).is_empty());
	}
}
