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
//! `sync` says which sync pairs it starts on, then prints a line for each thing a pass does, with
//! the time and the sync pair; failures go to stderr. A path that fails the same way again on a
//! later pass is not named again: that pass's summary counts it. With `--quiet`, it prints only
//! failures, what needs attention or was settled, and what each pass did. Every line also goes to
//! the log files (unless `--no-log-file` is given), so a sync nobody watches leaves a record. When
//! stderr is a terminal, a line at its bottom shows what the running transfers have moved, redrawn
//! in place; `--quiet` leaves it out.
//!
//! With `--json`, `sync` prints one JSON object per line for each event instead, such as
//! `{"pair":1,"label":"...","event":"uploaded","path":"a.txt"}`. A transfer's `progress` comes at
//! most once a second for each sync pair, with the bytes the pass has moved each way so far
//! (`pass_uploaded_bytes`, `pass_downloaded_bytes`), and `pass_completed` carries the pass's counts,
//! its conflicts, errors and unsyncable paths, and the bytes it moved. `uploaded` and `downloaded`
//! come once a file has landed; the other actions (`creating_remote_dir`, `trashing_remote`,
//! `deleting_local`, `moving_remote` and so on) as they start. What the flags settle is an
//! `approved` object with a `message`. There is no banner and no status line. A failed action, a
//! failed pass and a refused pass are their objects on stdout, like every other event, and nothing
//! of them goes to stderr; failures that are not an event's still go to stderr as text. `--json`
//! takes precedence over `--quiet`: every event is printed.
//!
//! Sync pairs stay registered in the sync database under the config directory after the run that
//! set them up. A run that cannot set up one of its sync pairs registers none of them. Only one
//! `filen sync` can use that database at a time.
//!
//! When the local or the remote directory of a sync pair does not exist, `sync` asks whether to
//! create it, and creates it with any missing directory above it; `--yes` answers yes. Without a
//! terminal to ask at and without `--yes`, the command fails instead. A local path that is a file
//! is an error. `sync` looks up every sync pair's directories, and asks about each missing one
//! once, before it creates any. A sync pair the sync database then refuses, such as one that
//! overlaps another, leaves behind the directories created for it.
//!
//! A sync pairs file is YAML, or JSON as the TypeScript CLI's syncPairs.json was, that lists sync
//! pairs under `syncPairs` or is a list of them. Each has a `local` and a `remote` path, and may
//! have a `mode` (or `syncMode`; `twoWay` if not given), an `alias`, by which `filen sync <alias>`
//! selects the pair of the default sync pairs file, `ignore`, a list of ignore rules in
//! `.filenignore` (gitignore) syntax, and `excludeDotFiles: true`, which stands for the rules `.*`
//! and `!/.filenignore`: every dot file and dot directory is left out of the sync, but the
//! `.filenignore` itself. A leading `~` in a local path is the home directory, and a
//! relative one is relative to the file's directory. The TypeScript CLI's `disableLocalTrash` key
//! is read but only gives a warning, since the local trash cannot be turned off. Any other key is
//! an error.
//!
//! `sync` without sync pairs reads syncPairs.yaml in the config directory, or else syncPairs.json
//! there. With the default config directory, it then falls back to `~/.filen-cli/syncPairs.json`,
//! where the TypeScript CLI kept its sync pairs when its install script had set it up; with
//! `--config-dir`, it does not look in the home directory.
//!
//! A sync pair the TypeScript CLI synced keeps what that CLI last synced: when `sync` sets up a
//! sync pair that has not finished a pass yet, it imports the state the TypeScript CLI kept for the
//! same `<local>:<remote>`, written as that CLI was given it, from `sync/state/v2` in that CLI's data
//! directory (the config directory, or `~/.filen-cli`), and says how many synced files it took
//! over. The first pass then takes a file that neither side has changed since for synced, instead
//! of weighing both copies as new.
//!
//! Before a sync pair is synced, the rules of its sync pairs file that the `.filenignore`
//! at its local root lacks are added to the end of that file, which is created if there is none.
//! No line is ever removed: a rule taken out of the sync pairs file stays in `.filenignore` until
//! you remove it there. A mode that uploads (`twoWay`, `localToRemote`, `localBackup`) syncs that
//! `.filenignore` like any other file, so the rules apply on the remote side too; in a mode that
//! only downloads (`remoteToLocal`, `remoteBackup`), a `.filenignore` at the local root that was
//! never synced stays local and applies on top of the remote's. There is no `--ignore` flag:
//! ignore rules belong to a sync pair, in its `.filenignore` or its sync pairs file.
//!
//! A local file or directory that a sync deletes, or replaces with the remote's version, is not
//! destroyed but moved into `.filen-sync-trash` at the sync pair's local root, under the day it was
//! moved (`.filen-sync-trash/YYYY-MM-DD/<path>`), and deleted from there after 30 days. That
//! folder is never synced. The trash is always on, and there is no flag to turn it off.

use std::{
	borrow::Cow,
	collections::{HashMap, HashSet},
	path::Path,
	sync::Arc,
	time::{Duration, Instant},
};

use anyhow::{Context, Result};
use clap::Subcommand;
use console::style;
use filen_sdk_rs::sync_engine::{
	BaselineInUse, ConflictResolution, ConflictResolved, PairId, PairRecord, SyncEngine, SyncEvent,
	UnsyncablePath, UnsyncableReason,
};
use log::info;

use crate::{
	CliConfig, CommandResult, auth::LazyClient, commands::CommandContext, docs::DocElement, ui::UI,
	util::RemotePath,
};

mod conflict_resolutions;
mod continuous;
mod failures;
mod ignore_file;
mod json_events;
mod once;
mod outcome;
mod status_line;
mod sync_pairs;
mod transfers;

use conflict_resolutions::ConflictResolutions;
use failures::Failures;
use status_line::{StatusLine, Throttle};
use sync_pairs::SyncDirs;
use transfers::Transfers;

#[derive(Debug, Subcommand)]
pub(crate) enum SyncCommands {
	/// Sync one or more sync pairs, continuously until Ctrl-C or once with --once
	Sync {
		/// Pair of local and remote directories to sync.
		/// Format: `<local>:<remote>` (two-way) or `<local>:<mode>:<remote>`, where `<mode>` is one of `twoWay` (or `tw`), `localToRemote` (or `localToCloud`, `ltc`), `remoteToLocal` (or `cloudToLocal`, `ctl`), `localBackup` (or `lb`), `remoteBackup` (or `cloudBackup`, `cb`).
		/// The remote path is what follows the last `:`, so a remote path with a `:` in it can only be given in a sync pairs file.
		/// Alternatively, the path to a sync pairs file: YAML, or JSON as the TypeScript CLI's syncPairs.json.
		/// If none are given, the pairs of syncPairs.yaml in the config dir, or else of syncPairs.json there, or else, with the default config dir, of the TypeScript CLI's ~/.filen-cli/syncPairs.json.
		/// Or the alias a pair has (its `alias` key) in that default sync pairs file.
		#[arg(value_name = "PAIR")]
		pairs: Vec<String>,
		/// A sync pair, as a PAIR argument gives it.
		#[arg(short = 'p', long = "pair", value_name = "PAIR")]
		pair_options: Vec<String>,
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
				pair_options,
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
				let pairs = given_pairs(pairs, pair_options);
				sync(config, ui, client, working_path, &pairs, once, &settle).await?;
			}
		}
		Ok(CommandResult::default())
	}
}

/// The sync pairs given as arguments, then those given with `--pair`.
fn given_pairs(pairs: Vec<String>, pair_options: Vec<String>) -> Vec<String> {
	pairs.into_iter().chain(pair_options).collect()
}

/// What the flags settle of what the first pass of each pair finds.
struct Settle {
	approve_mass_deletions: bool,
	conflict_resolutions: ConflictResolutions,
}

async fn sync(
	config: &CliConfig,
	ui: &mut UI,
	client: &mut LazyClient,
	working_path: &RemotePath,
	pair_definitions: &[String],
	once: bool,
	settle: &Settle,
) -> Result<()> {
	// before logging in, so that a bad definition fails at once
	let sync_dirs = SyncDirs::new(
		&config.config_dir,
		config.config_dir_is_default,
		dirs::home_dir().as_deref(),
	);
	let parsed = sync_pairs::parse_sync_pairs(&sync_dirs, pair_definitions)?;
	for warning in sync_pairs::local_trash_warnings(&parsed) {
		ui.print_warning(&warning);
	}
	let client = client.get(ui).await?;

	// before the engine is opened, so that a bad definition leaves its database as it was
	let pair_definitions =
		sync_pairs::resolve_sync_pairs(ui, client, working_path, &sync_dirs, parsed).await?;

	// init sync engine
	let db_path = config.config_dir.join("sync-engine.db");
	let engine = Arc::new(
		SyncEngine::open(client.clone(), db_path.clone())
			.await
			.map_err(|e| UI::failure(&open_failure(&e, &db_path)))?,
	);
	let pair_ids = sync_pairs::register_sync_pairs(ui, &engine, client, &pair_definitions)
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
	let labels = sync_pairs::pair_labels(client, &pairs).await;
	let log = EventLog::new(&labels, settle, Output::of(ui));
	log.print_banners(ui, &pairs, once);
	if once {
		once::sync_once(ui, &engine, &pairs, log, settle).await
	} else {
		continuous::sync_continuously(ui, engine, &pairs, log).await
	}
}

/// Prints the events of a run's pairs, and says which of them the flags settle.
///
/// Every line it prints is also logged, so that an unattended sync leaves a record in the log
/// files. Unlike most printed output, which can hold secrets, a sync's lines hold only paths.
struct EventLog<'a> {
	labels: &'a HashMap<PairId, String>,
	settle: &'a Settle,
	/// The pairs that have finished a pass. Mass deletion approvals and conflict resolutions are
	/// only applied on a pair's first pass, where we can be sure they were intentional.
	passed: HashSet<PairId>,
	/// What the running transfers have moved, for the status line.
	transfers: Transfers,
	/// The failures printed, so that one that comes back on every pass is printed once.
	failures: Failures,
	output: Output,
}

/// How often `--json` prints a transfer's progress at most, for each pair.
const JSON_PROGRESS_INTERVAL: Duration = Duration::from_secs(1);

/// How the events are shown.
enum Output {
	/// A line for each event. With `quiet`, only the lines that must be read: failures, what
	/// needs attention or was settled, and what each pass did. A transfer's progress shows on the
	/// `status` line, when there is one, rather than on a line of its own.
	Text {
		quiet: bool,
		status: Option<StatusLine>,
	},
	/// A JSON object on a line of its own for each event, and for each pair a transfer's progress
	/// as often as its `progress` throttle lets through.
	Json { progress: HashMap<PairId, Throttle> },
}

impl Output {
	/// The output `ui`'s flags ask for.
	fn of(ui: &UI) -> Self {
		if ui.json {
			return Self::Json {
				progress: HashMap::new(),
			};
		}
		let quiet = ui.is_quiet();
		let status = if quiet { None } else { StatusLine::on_stderr() };
		Self::Text { quiet, status }
	}
}

/// Where the line of an event goes.
#[derive(Debug, PartialEq)]
enum LineKind {
	/// To stdout, unless `--quiet` leaves it out.
	Detail,
	/// To stdout, even with `--quiet`.
	Kept,
	/// To stderr.
	Failure,
}

/// What a line of an event is to a reader.
fn line_kind(event: &SyncEvent) -> LineKind {
	match event {
		SyncEvent::Refused { .. }
		| SyncEvent::ActionFailed { .. }
		| SyncEvent::PassFailed { .. } => LineKind::Failure,
		// a copy moved aside is named nowhere else
		SyncEvent::PassCompleted { .. }
		| SyncEvent::Interrupted { .. }
		| SyncEvent::Quarantined { .. } => LineKind::Kept,
		// what needs attention of a conflict or a held deletion has a line of its own
		SyncEvent::PassStarted { .. }
		| SyncEvent::Planned { .. }
		| SyncEvent::Conflict { .. }
		| SyncEvent::DeletionsHeld { .. }
		| SyncEvent::Uploading { .. }
		| SyncEvent::Downloading { .. }
		| SyncEvent::Progress { .. }
		| SyncEvent::CreatingRemoteDir { .. }
		| SyncEvent::CreatingLocalDir { .. }
		| SyncEvent::TrashingRemote { .. }
		| SyncEvent::DeletingLocal { .. }
		| SyncEvent::MovingRemote { .. }
		| SyncEvent::MovingLocal { .. }
		| SyncEvent::AdoptedBaseline { .. }
		| SyncEvent::AwaitingRemote
		| SyncEvent::Scanning { .. } => LineKind::Detail,
	}
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
	fn new(labels: &'a HashMap<PairId, String>, settle: &'a Settle, output: Output) -> Self {
		Self {
			labels,
			settle,
			passed: HashSet::new(),
			transfers: Transfers::default(),
			failures: Failures::default(),
			output,
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

	/// Says which pairs the sync starts on, and how long it runs.
	fn print_banners(&self, ui: &mut UI, pairs: &[PairRecord], once: bool) {
		// a paused pair is said to be paused instead
		for pair in pairs.iter().filter(|pair| !pair.paused) {
			self.print_banner(ui, pair.id, once);
		}
	}

	/// Says that the sync starts on `pair`, and how long it runs. Not with `--quiet`.
	fn print_banner(&self, ui: &mut UI, pair: PairId, once: bool) {
		let until = if once { "once" } else { "Ctrl-C to stop" };
		let banner = format!("Syncing {} ({until})", self.label(pair));
		info!("{banner}");
		if let Output::Text { quiet: false, .. } = self.output {
			ui.print(&banner);
		}
	}

	/// Prints `event` of `pair`, with what needs the user's attention, and returns what the flags
	/// settle of it, which the caller carries out.
	fn print<'e>(
		&mut self,
		ui: &mut UI,
		pair: PairId,
		event: &'e SyncEvent,
	) -> Option<Settlement<'e>> {
		self.print_at(ui, pair, event, Instant::now())
	}

	/// [`EventLog::print`], at `now`.
	fn print_at<'e>(
		&mut self,
		ui: &mut UI,
		pair: PairId,
		event: &'e SyncEvent,
		now: Instant,
	) -> Option<Settlement<'e>> {
		self.transfers.note(pair, event);
		let label = self.label(pair);
		if let Output::Json { progress } = &mut self.output {
			let due = match event {
				SyncEvent::Progress { .. } => progress
					.entry(pair)
					.or_insert_with(|| Throttle::new(JSON_PROGRESS_INTERVAL))
					.due(now),
				_ => true,
			};
			if due {
				ui.print(&json_events::event_line(
					pair,
					&label,
					event,
					&self.transfers,
				));
			}
		}
		let repeat = self.failures.is_repeat(pair, event);
		match format_sync_event(event) {
			// in the log files, which keep every pass, but not printed again
			Some(line) if repeat => info!("[{label}] {}", console::strip_ansi_codes(&line)),
			Some(line) => {
				let line = match (event, self.failures.repeated(pair)) {
					(SyncEvent::PassCompleted { .. }, repeated @ 1..) => {
						with_repeated_failures(&line, repeated)
					}
					_ => line,
				};
				self.print_line(ui, pair, line_kind(event), &line);
			}
			// a transfer's progress shows on the status line
			None => self.tick(now),
		}
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
					ui,
					pair,
					&format!(
						"Held back {count} deletions. If you wish to execute them, re-run the command with --approve-mass-deletions"
					),
				);
			}
			// paths the engine leaves alone, which no other event names
			SyncEvent::PassCompleted { report } => {
				for notice in unsyncable_notices(&report.unsyncable) {
					self.print_needs_attention(ui, pair, &notice);
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
					ui,
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

	/// Logs `msg`, a line of `pair` of the given kind, and prints it with the time and the pair's
	/// label, where its kind says.
	fn print_line(&mut self, ui: &mut UI, pair: PairId, kind: LineKind, msg: &str) {
		let label = self.label(pair);
		info!("[{label}] {}", console::strip_ansi_codes(msg));
		// with `--json`, the event's object was printed instead
		let Output::Text { quiet, status } = &mut self.output else {
			return;
		};
		if *quiet && kind == LineKind::Detail {
			return;
		}
		if let Some(status) = status {
			status.clear();
		}
		let line = format!(
			"{} {}: {}",
			style(format!(
				"[{}]",
				chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
			))
			.dim(),
			style(format!("[{label}]")).bold(),
			msg,
		);
		match kind {
			LineKind::Detail | LineKind::Kept => ui.print(&line),
			LineKind::Failure => ui.print_to_stderr(&line),
		}
		if let Some(status) = status {
			status.draw(&self.transfers);
		}
	}

	fn print_needs_attention(&mut self, ui: &mut UI, pair: PairId, msg: &str) {
		self.print_line(
			ui,
			pair,
			LineKind::Kept,
			&format!("{} {}", style("[NEEDS ATTENTION]").yellow().bold(), msg),
		);
	}

	fn print_approved(&mut self, ui: &mut UI, pair: PairId, msg: &str) {
		if let Output::Json { .. } = self.output {
			ui.print(&json_events::approved_line(pair, &self.label(pair), msg));
		}
		self.print_line(
			ui,
			pair,
			LineKind::Kept,
			&format!("{} {}", style("[APPROVED]").green().bold(), msg),
		);
	}

	/// Prints a failure that is not an event's, such as a conflict that could not be resolved.
	fn print_failure(&mut self, ui: &mut UI, msg: &str) {
		self.clear_status();
		ui.print_failure(msg);
		self.redraw_status();
	}

	/// Redraws the status line, if there is one, as after a line printed above it.
	fn redraw_status(&mut self) {
		if let Output::Text {
			status: Some(status),
			..
		} = &mut self.output
		{
			status.draw(&self.transfers);
		}
	}

	/// Redraws the status line, if there is one, at most as often as it redraws for progress.
	fn tick(&mut self, now: Instant) {
		if let Output::Text {
			status: Some(status),
			..
		} = &mut self.output
		{
			status.tick(&self.transfers, now);
		}
	}

	/// Clears the status line, if there is one, for what is printed next.
	fn clear_status(&mut self) {
		if let Output::Text {
			status: Some(status),
			..
		} = &mut self.output
		{
			status.clear();
		}
	}
}

/// `summary`, a pass's, saying that `repeated` of its failures were left out.
fn with_repeated_failures(summary: &str, repeated: usize) -> String {
	format!("{summary} ({repeated} not printed again, as an earlier pass failed the same way)")
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

/// The line of `event`, or `None` for a transfer's progress, which has none.
fn format_sync_event(event: &SyncEvent) -> Option<String> {
	Some(match event {
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
		// A transfer runs alongside others and reports its event once it has landed.
		SyncEvent::Uploading { rel_path } => format!("Uploaded {rel_path}"),
		SyncEvent::Downloading { rel_path } => format!("Downloaded {rel_path}"),
		SyncEvent::Progress { .. } => return None,
		// The other actions report their event before they run, so an `ActionFailed` can follow; the
		// pass's summary counts what was done.
		SyncEvent::CreatingRemoteDir { rel_path } => {
			format!("Creating remote directory {rel_path}")
		}
		SyncEvent::CreatingLocalDir { rel_path } => {
			format!("Creating local directory {rel_path}")
		}
		SyncEvent::TrashingRemote { rel_path } => format!("Trashing remote {rel_path}"),
		SyncEvent::DeletingLocal { rel_path } => format!("Deleting local {rel_path}"),
		SyncEvent::MovingRemote { from, to } => format!("Moving remote {from} -> {to}"),
		SyncEvent::MovingLocal { from, to } => format!("Moving local {from} -> {to}"),
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
	})
}

#[cfg(test)]
mod tests {
	use std::{
		collections::HashMap,
		path::Path,
		time::{Duration, Instant},
	};

	use filen_sdk_rs::{
		Error, ErrorKind,
		sync_engine::{
			BaselineInUse, SyncMode, SyncReport, TransferDirection, UnsyncablePath,
			UnsyncableReason,
		},
	};
	use serde_json::{Value, json};

	use super::{
		ConflictResolutions, EventLog, Output, Settle, SyncCommands, SyncEvent, format_sync_event,
		given_pairs, open_failure, unsyncable_notices,
	};
	use crate::{commands::Commands, parse_repl_line, ui::test_support::Printed};

	/// The pairs the REPL line `line`, a `sync` command, gives.
	fn pairs_of(line: &str) -> Vec<String> {
		match parse_repl_line(line).unwrap().command {
			Some(Commands::Sync(SyncCommands::Sync {
				pairs,
				pair_options,
				..
			})) => given_pairs(pairs, pair_options),
			other => panic!("expected a sync command, got {other:?}"),
		}
	}

	#[test]
	fn a_sync_pair_can_be_an_argument() {
		assert_eq!(
			pairs_of("sync /home/me/docs:/docs"),
			["/home/me/docs:/docs"]
		);
	}

	#[test]
	fn sync_pairs_given_as_arguments_and_with_pair_are_all_synced() {
		assert_eq!(
			pairs_of("sync /a:/b --pair /c:lb:/d /e:/f"),
			["/a:/b", "/e:/f", "/c:lb:/d"]
		);
	}

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

	/// The engine reports a transfer once it has landed.
	#[test]
	fn a_transfer_is_named_once_it_is_done() {
		assert_eq!(
			format_sync_event(&SyncEvent::Uploading {
				rel_path: "a.txt".to_string()
			})
			.as_deref(),
			Some("Uploaded a.txt")
		);
		assert_eq!(
			format_sync_event(&SyncEvent::Downloading {
				rel_path: "b.txt".to_string()
			})
			.as_deref(),
			Some("Downloaded b.txt")
		);
	}

	/// The engine reports any other action before it runs it, so the line does not claim it is done.
	#[test]
	fn any_other_action_is_named_as_it_starts() {
		let path = || "d".to_string();
		let lines = [
			SyncEvent::CreatingRemoteDir { rel_path: path() },
			SyncEvent::CreatingLocalDir { rel_path: path() },
			SyncEvent::TrashingRemote { rel_path: path() },
			SyncEvent::DeletingLocal { rel_path: path() },
			SyncEvent::MovingRemote {
				from: path(),
				to: "e".to_string(),
			},
			SyncEvent::MovingLocal {
				from: path(),
				to: "e".to_string(),
			},
		]
		.map(|event| format_sync_event(&event).unwrap());

		assert_eq!(
			lines,
			[
				"Creating remote directory d",
				"Creating local directory d",
				"Trashing remote d",
				"Deleting local d",
				"Moving remote d -> e",
				"Moving local d -> e",
			]
		);
	}

	/// Each line `events` of pair 1, labelled `L`, print to stdout and to stderr with `output`,
	/// from the label on.
	fn printed(output: Output, events: &[SyncEvent]) -> (Vec<String>, Vec<String>) {
		let out = Printed::default();
		let err = Printed::default();
		let mut ui = Printed::split_ui(&out, &err);
		let labels = HashMap::from([(1, "L".to_string())]);
		let settle = Settle {
			approve_mass_deletions: false,
			conflict_resolutions: ConflictResolutions::new(vec![], vec![], vec![]).unwrap(),
		};
		let mut log = EventLog::new(&labels, &settle, output);
		for event in events {
			let _ = log.print(&mut ui, 1, event);
		}
		let lines = |printed: &Printed| {
			console::strip_ansi_codes(&printed.lines_or_empty())
				.lines()
				// after the time
				.map(|line| line.split_once("] ").unwrap().1.to_owned())
				.collect()
		};
		(lines(&out), lines(&err))
	}

	/// A pass that uploaded a file, failed another and held back a conflict.
	fn a_pass() -> Vec<SyncEvent> {
		vec![
			SyncEvent::PassStarted {
				mode: SyncMode::TwoWay,
			},
			SyncEvent::Conflict {
				rel_path: "c".to_string(),
			},
			SyncEvent::Uploading {
				rel_path: "a".to_string(),
			},
			SyncEvent::ActionFailed {
				rel_path: "b".to_string(),
				error: "denied".to_string(),
			},
			SyncEvent::PassCompleted {
				report: SyncReport::default(),
			},
		]
	}

	#[test]
	fn every_event_has_a_line_and_failures_go_to_stderr() {
		let (out, err) = printed(
			Output::Text {
				quiet: false,
				status: None,
			},
			&a_pass(),
		);

		assert_eq!(
			out,
			[
				"[L]: Sync pass started",
				"[L]: Conflict: c changed on both sides, left untouched",
				"[L]: [NEEDS ATTENTION] Conflict detected. To resolve it, re-run the command with \
				 --keep-local/--keep-remote/--keep-both \"c\"",
				"[L]: Uploaded a",
				"[L]: Sync pass completed: 0 uploaded, 0 downloaded, 0 remote / 0 local dir(s) \
				 created, 0 trashed remotely, 0 deleted locally, 0 moved remotely, 0 moved locally, \
				 0 conflict(s), 0 error(s)",
			]
		);
		assert_eq!(err, ["[L]: Failed to sync b: denied"]);
	}

	/// A path that fails the same way on the next pass is not named again, but counted.
	#[test]
	fn a_failure_that_comes_back_is_printed_once_and_counted() {
		let failed = || SyncEvent::ActionFailed {
			rel_path: "b".to_string(),
			error: "denied".to_string(),
		};
		let pass = || {
			[
				SyncEvent::PassStarted {
					mode: SyncMode::TwoWay,
				},
				failed(),
				SyncEvent::PassCompleted {
					report: SyncReport::default(),
				},
			]
		};
		let (out, err) = printed(
			Output::Text {
				quiet: false,
				status: None,
			},
			&[pass(), pass()].concat(),
		);

		let summary = "[L]: Sync pass completed: 0 uploaded, 0 downloaded, 0 remote / 0 local dir(s) \
			 created, 0 trashed remotely, 0 deleted locally, 0 moved remotely, 0 moved locally, 0 \
			 conflict(s), 0 error(s)";
		assert_eq!(
			out,
			[
				"[L]: Sync pass started".to_owned(),
				summary.to_owned(),
				"[L]: Sync pass started".to_owned(),
				format!("{summary} (1 not printed again, as an earlier pass failed the same way)"),
			]
		);
		assert_eq!(err, ["[L]: Failed to sync b: denied"]);
	}

	#[test]
	fn quiet_keeps_what_needs_attention_failures_and_the_pass_summary() {
		let (out, err) = printed(
			Output::Text {
				quiet: true,
				status: None,
			},
			&a_pass(),
		);

		assert_eq!(
			out,
			[
				"[L]: [NEEDS ATTENTION] Conflict detected. To resolve it, re-run the command with \
				 --keep-local/--keep-remote/--keep-both \"c\"",
				"[L]: Sync pass completed: 0 uploaded, 0 downloaded, 0 remote / 0 local dir(s) \
				 created, 0 trashed remotely, 0 deleted locally, 0 moved remotely, 0 moved locally, \
				 0 conflict(s), 0 error(s)",
			]
		);
		assert_eq!(err, ["[L]: Failed to sync b: denied"]);
	}

	/// With `--json`, each event is a line of JSON, a transfer's progress at most once a second for
	/// each pair, and what needs attention is left to the events' objects.
	#[test]
	fn json_prints_an_object_for_each_event_and_throttles_progress() {
		let out = Printed::default();
		let err = Printed::default();
		let mut ui = Printed::split_ui(&out, &err);
		let labels = HashMap::from([(1, "L".to_string())]);
		let settle = Settle {
			approve_mass_deletions: false,
			conflict_resolutions: ConflictResolutions::new(vec![], vec![], vec![]).unwrap(),
		};
		let mut log = EventLog::new(
			&labels,
			&settle,
			Output::Json {
				progress: HashMap::new(),
			},
		);
		let progress = |bytes| SyncEvent::Progress {
			rel_path: "a".to_string(),
			direction: TransferDirection::Upload,
			bytes,
			total: 10,
		};
		let start = Instant::now();
		for (event, at) in [
			(
				SyncEvent::Conflict {
					rel_path: "c".to_string(),
				},
				0,
			),
			(progress(1), 0),
			(progress(2), 999),
			(progress(3), 1000),
		] {
			let _ = log.print_at(&mut ui, 1, &event, start + Duration::from_millis(at));
		}
		log.print_approved(&mut ui, 1, "Approved 3 deletions");

		let lines = out
			.lines()
			.lines()
			.map(|line| serde_json::from_str::<Value>(line).unwrap())
			.collect::<Vec<_>>();
		let progress = |bytes: u64| {
			json!({
				"pair": 1, "label": "L", "event": "progress", "path": "a", "direction": "upload",
				"bytes": bytes, "total": 10, "pass_uploaded_bytes": bytes, "pass_downloaded_bytes": 0,
			})
		};
		assert_eq!(
			lines,
			[
				json!({"pair": 1, "label": "L", "event": "conflict", "path": "c"}),
				progress(1),
				progress(3),
				json!({
					"pair": 1, "label": "L", "event": "approved",
					"message": "Approved 3 deletions",
				}),
			]
		);
		assert_eq!(err.lines_or_empty(), "");
	}

	/// With `--json`, a failure that is an event is its object on stdout, and stderr stays empty.
	#[test]
	fn json_prints_failed_actions_and_passes_as_objects_on_stdout() {
		let out = Printed::default();
		let err = Printed::default();
		let mut ui = Printed::split_ui(&out, &err);
		let labels = HashMap::from([(1, "L".to_string())]);
		let settle = Settle {
			approve_mass_deletions: false,
			conflict_resolutions: ConflictResolutions::new(vec![], vec![], vec![]).unwrap(),
		};
		let mut log = EventLog::new(
			&labels,
			&settle,
			Output::Json {
				progress: HashMap::new(),
			},
		);
		for event in [
			SyncEvent::ActionFailed {
				rel_path: "b".to_string(),
				error: "denied".to_string(),
			},
			SyncEvent::PassFailed {
				error: "offline".to_string(),
				kind: ErrorKind::IO,
			},
		] {
			let _ = log.print(&mut ui, 1, &event);
		}

		let events = out
			.lines()
			.lines()
			.map(|line| serde_json::from_str::<Value>(line).unwrap()["event"].clone())
			.collect::<Vec<_>>();
		assert_eq!(events, [json!("action_failed"), json!("pass_failed")]);
		assert_eq!(err.lines_or_empty(), "");
	}

	#[test]
	fn json_takes_precedence_over_quiet() {
		let mut ui = Printed::default().ui();
		ui.set_quiet_and_json(true, true);

		assert!(matches!(Output::of(&ui), Output::Json { .. }));
	}

	/// Without a terminal for the status line, a transfer's progress is not printed.
	#[test]
	fn progress_has_no_line() {
		let (out, err) = printed(
			Output::Text {
				quiet: false,
				status: None,
			},
			&[SyncEvent::Progress {
				rel_path: "a".to_string(),
				direction: TransferDirection::Upload,
				bytes: 1,
				total: 2,
			}],
		);

		assert_eq!((out, err), (Vec::<String>::new(), Vec::<String>::new()));
	}

	#[test]
	fn a_banner_names_each_pair_and_how_long_it_syncs() {
		let out = Printed::default();
		let mut ui = Printed::split_ui(&out, &Printed::default());
		let labels = HashMap::from([(1, "L".to_string())]);
		let settle = Settle {
			approve_mass_deletions: false,
			conflict_resolutions: ConflictResolutions::new(vec![], vec![], vec![]).unwrap(),
		};
		let log = EventLog::new(
			&labels,
			&settle,
			Output::Text {
				quiet: false,
				status: None,
			},
		);

		log.print_banner(&mut ui, 1, true);
		log.print_banner(&mut ui, 1, false);
		let quiet = EventLog::new(
			&labels,
			&settle,
			Output::Text {
				quiet: true,
				status: None,
			},
		);
		quiet.print_banner(&mut ui, 1, false);

		assert_eq!(out.lines(), "Syncing L (once)\nSyncing L (Ctrl-C to stop)");
	}

	#[test]
	fn a_pass_that_left_nothing_alone_has_no_notices() {
		assert!(unsyncable_notices(&[]).is_empty());
	}
}
