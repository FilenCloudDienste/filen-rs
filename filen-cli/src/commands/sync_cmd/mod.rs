use std::{
	collections::{HashMap, HashSet},
	sync::Arc,
};

use anyhow::{Context, Result};
use console::style;
use filen_sdk_rs::sync_engine::{ConflictResolution, SyncEngine, SyncEvent, TransferDirection};
use tokio::sync::mpsc;

use crate::{
	CliConfig, auth::LazyClient, commands::sync_cmd::sync_pairs::format_sync_pair, ui::UI,
	util::RemotePath,
};

mod sync_pairs;

// todo: better command documentation

pub(crate) async fn sync(
	config: &CliConfig,
	ui: &mut UI,
	client: &mut LazyClient,
	working_path: &RemotePath,
	pair_definitions: &Vec<String>,
	approve_mass_deletions: bool,
	conflict_resolutions: HashMap<String, ConflictResolution>,
) -> Result<()> {
	let client = client.get(ui).await?;

	// todo: handle ctrl-c better?

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
	sync_pairs::register_sync_pairs(&engine, client, working_path, pair_definitions)
		.await
		.context("Failed to add sync pairs")?;

	// start engine and send events
	let (events_tx, mut events_rx) = mpsc::unbounded_channel();
	let pairs = engine
		.list_pairs()
		.await
		.context("Failed to list sync pairs")?;
	let watch_handles = pairs.iter().map(async |pair| {
		// todo: tmp
		engine
			.set_delete_guard(
				pair.id,
				filen_sdk_rs::sync_engine::DeleteGuard::new(0, 0.000001)?,
			)
			.await?;
		let pair = pair.clone();
		let pair_friendly_name = format_sync_pair(client, &pair).await?;
		let events_tx = events_tx.clone();
		let watch = Arc::clone(&engine)
			.watch_observed(
				pair.id,
				Box::new(move |event| {
					let _ = events_tx.send((pair.id, event));
				}),
			)
			.await
			.with_context(|| format!("Failed to watch sync pair {}", pair_friendly_name))?;
		Ok::<_, anyhow::Error>(watch)
	});
	let _watch_handles = futures::future::join_all(watch_handles).await;

	// receive events and print them
	let mut pairs_that_have_had_passes = HashSet::new();
	while let Some((pair_id, event)) = events_rx.recv().await {
		let pair = pairs.iter().find(|p| p.id == pair_id).with_context(|| {
			format!("Received an event for unknown sync pair with id {pair_id}")
		})?;
		let pair_friendly_name = format_sync_pair(client, pair).await?;
		let print_event = |msg: &str| {
			println!(
				"{} {}: {}",
				style(format!(
					"[{}]",
					chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
				))
				.dim(),
				style(format!("[{}]", pair_friendly_name)).bold(),
				msg,
			);
		};
		print_event(&format_sync_event(&event));
		let print_needs_attention = |msg: &str| {
			print_event(&format!(
				"{} {}",
				style("[NEEDS ATTENTION]").yellow().bold(),
				msg
			))
		};
		let print_action_taken =
			|msg: &str| print_event(&format!("{} {}", style("[APPROVED]").green().bold(), msg));

		// remember whether (and check if) this pair is on its first pass
		// this way, mass deletion approvals and conflict resolutions are only applied
		// on the first pass, where we can be sure they were intentional
		if let SyncEvent::PassCompleted { .. } | SyncEvent::PassFailed { .. } = event {
			pairs_that_have_had_passes.insert(pair_id);
		}
		let is_first_pass = !pairs_that_have_had_passes.contains(&pair_id);

		// approve deletions
		if let SyncEvent::DeletionsHeld {
			ref count,
			ref pass_token,
			..
		} = event
		{
			if is_first_pass && approve_mass_deletions {
				engine.approve_deletions(pair_id, pass_token).await;
				print_action_taken(&format!("Approved {} deletions", count));
			} else {
				print_needs_attention(&format!(
					"Held back {} deletions. If you wish to execute them, re-run the command with --approve-mass-deletions",
					count
				));
			}
		}

		// resolve conflicts
		if let SyncEvent::Conflict { ref rel_path } = event {
			if is_first_pass
				&& let Some((_, resolution)) = conflict_resolutions
					.iter()
					.find(|(path, _)| path.starts_with(rel_path))
			{
				let trashed_path = engine
					.resolve_conflict(pair_id, rel_path, *resolution)
					.await
					.with_context(|| {
						format!(
							"Failed to resolve conflict for {rel_path} in sync pair {}",
							pair_friendly_name
						)
					})?;
				print_action_taken(&format!(
					"Conflict resolved for {rel_path}, {}",
					match resolution {
						ConflictResolution::KeepLocal => "kept local version",
						ConflictResolution::KeepRemote => "kept remote version",
						ConflictResolution::KeepBoth => "kept both versions",
					}
				));
				if let Some(trashed_path) = trashed_path {
					print_action_taken(&format!(
						"Conflict resolved for {rel_path}, moved aside to {}",
						trashed_path.display()
					));
				}
			} else {
				print_needs_attention(&format!(
					"Conflict detected. To resolve it, re-run the command with --keep-local/--keep-remote/--keep-both \"{}\"",
					rel_path,
				));
			}
		}
	}

	Ok(())
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
	}
}
