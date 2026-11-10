//! [cli-doc] sync-pairs
//! The sync pairs `sync` has set up stay registered in the sync database, and these commands
//! manage them. They name a sync pair by the id `list-sync-pairs` prints, by its alias in the
//! default sync pairs file, or by its local directory. They open the same database as `sync`, so
//! they cannot run while a `filen sync` is running: stop it first.
//!
//! A sync pair whose mode `set-sync-mode` changed must be given to `sync` in its new mode from then
//! on, in a sync pairs file too: `sync` refuses a sync pair given in another mode than the one it is
//! registered in, and says which one that is.

use std::{borrow::Cow, fmt::Display, iter, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use filen_sdk_rs::{
	ErrorKind,
	auth::Client,
	sync_engine::{
		Backlog, ConflictReason, ConflictResolution, PairRecord, PlannedConflict, PlannedNodeKind,
		SyncEngine, SyncMode,
	},
};
use serde_json::{Value, json};

use super::{
	conflict_resolutions, json_events, open_engine, pair_address, resolve_failure_message,
	resolved_conflict_lines, sync_dirs, sync_pairs,
};
use crate::{CliConfig, auth::LazyClient, ui::UI};

/// A sync engine opened for a command that manages its pairs, logged in.
struct Managing<'c> {
	client: &'c Arc<Client>,
	engine: SyncEngine,
}

impl<'c> Managing<'c> {
	async fn open(config: &CliConfig, ui: &mut UI, client: &'c mut LazyClient) -> Result<Self> {
		let client = client.get(ui).await?;
		let engine = open_engine(config, client).await?;
		Ok(Self { client, engine })
	}

	async fn pairs(&self) -> Result<Vec<PairRecord>> {
		self.engine
			.list_pairs()
			.await
			.context("Failed to list the sync pairs")
	}

	/// The registered pair `given` names (see [`pair_address::find_pair`]), with its label.
	async fn find(&self, config: &CliConfig, given: &str) -> Result<(PairRecord, String)> {
		let pair = pair_address::find_pair(self.pairs().await?, given, &sync_dirs(config))?;
		let label = self.label(&pair).await;
		Ok((pair, label))
	}

	async fn label(&self, pair: &PairRecord) -> String {
		sync_pairs::pair_labels(self.client, std::slice::from_ref(pair))
			.await
			.remove(&pair.id)
			.unwrap_or_else(|| format!("sync pair {}", pair.id))
	}
}

/// `list-sync-pairs`: each registered pair with its id, whether it is paused, and whether it has
/// completed a first pass.
pub(super) async fn list_sync_pairs(
	config: &CliConfig,
	ui: &mut UI,
	client: &mut LazyClient,
) -> Result<()> {
	let managing = Managing::open(config, ui, client).await?;
	let pairs = managing.pairs().await?;
	if ui.json {
		let mut remote_paths = sync_pairs::remote_paths(managing.client, &pairs).await;
		let pairs = pairs
			.iter()
			.map(|pair| {
				json!({
					"id": pair.id,
					"local": pair.local_root,
					"remote": remote_paths.remove(&pair.id).flatten(),
					"remote_uuid": pair.remote_root,
					"mode": sync_pairs::sync_mode_str(pair.mode),
					"paused": pair.paused,
					"first_pass_completed": pair.first_pass_completed,
				})
			})
			.collect::<Vec<_>>();
		return ui.print_json(json!(pairs));
	}
	if pairs.is_empty() {
		ui.print_muted(
			"No sync pairs are registered. `filen sync` registers the sync pairs it syncs.",
		);
		return Ok(());
	}
	let mut labels = sync_pairs::pair_labels(managing.client, &pairs).await;
	for pair in &pairs {
		let label = labels
			.remove(&pair.id)
			.unwrap_or_else(|| format!("sync pair {}", pair.id));
		ui.print(&pair_line(pair, &label));
	}
	Ok(())
}

/// The line `list-sync-pairs` prints for `pair`, labelled `label`.
fn pair_line(pair: &PairRecord, label: &str) -> String {
	let mut line = format!("{}: {label}", pair.id);
	if pair.paused {
		line.push_str(" (paused)");
	}
	if !pair.first_pass_completed {
		line.push_str(" (no pass completed yet)");
	}
	line
}

/// `remove-sync-pair`: forgets the pair `given` names, once the user confirms it. No file is
/// touched on either side.
pub(super) async fn remove_sync_pair(
	config: &CliConfig,
	ui: &mut UI,
	client: &mut LazyClient,
	given: &str,
) -> Result<()> {
	// before logging in, so that a script learns at once that it needs `--yes`
	ui.ensure_can_confirm(&format!("Remove the sync pair {given}?"))?;
	let managing = Managing::open(config, ui, client).await?;
	let (pair, label) = managing.find(config, given).await?;
	if !ui.prompt_confirm(
		&format!(
			"Remove the sync pair {label}? Its files stay as they are on both sides, and are no \
			 longer synced."
		),
		false,
	)? {
		return Ok(());
	}
	managing
		.engine
		.remove_pair(pair.id)
		.await
		.with_context(|| format!("Failed to remove the sync pair {label}"))?;
	ui.print_success(&format!(
		"Removed the sync pair {label}. No file was deleted."
	));
	Ok(())
}

/// `set-sync-mode`: switches the pair `given` names to `mode` from its next pass on. With
/// `propagate_deletions`, and once the user confirms it, the next pass deletes on the destination
/// what the source no longer has, as `mode` says; without it, the destination keeps it.
pub(super) async fn set_sync_mode(
	config: &CliConfig,
	ui: &mut UI,
	client: &mut LazyClient,
	given: &str,
	mode: SyncMode,
	propagate_deletions: bool,
) -> Result<()> {
	let mode_name = sync_pairs::sync_mode_str(mode);
	if propagate_deletions {
		// before logging in, so that a script learns at once that it needs `--yes`
		ui.ensure_can_confirm(&format!(
			"Switch the sync pair {given} to {mode_name} and propagate deletions?"
		))?;
	}
	let managing = Managing::open(config, ui, client).await?;
	let (pair, label) = managing.find(config, given).await?;
	if pair.mode == mode {
		ui.print(&format!(
			"The sync pair {label} already syncs as {mode_name}"
		));
		return Ok(());
	}
	let backlog = if propagate_deletions {
		if !ui.prompt_confirm(&propagate_deletions_question(&label, mode_name), false)? {
			return Ok(());
		}
		Backlog::Propagate
	} else {
		Backlog::AdoptDestination
	};
	awaiting_listing(
		async || {
			managing
				.engine
				.reconfigure_pair(pair.id, mode, backlog)
				.await
		},
		ui,
	)
	.await
	.map_err(|e| UI::failure(&switch_failure_message(&label, mode_name, &e)))?;
	ui.print_success(&format!(
		"The sync pair {label} syncs as {mode_name} from its next pass"
	));
	Ok(())
}

/// What `set-sync-mode --propagate-deletions` asks before it switches the pair labelled `label`
/// to the mode named `mode_name`.
fn propagate_deletions_question(label: &str, mode_name: &str) -> String {
	format!(
		"Switch the sync pair {label} to {mode_name}? Its next pass then deletes on the \
		 destination what the source no longer has, as {mode_name} does; the delete guard still \
		 holds a large batch of deletions back for approval."
	)
}

/// Why the pair labelled `label` was not switched to the mode named `mode_name`: `error`.
fn switch_failure_message(label: &str, mode_name: &str, error: &impl Display) -> String {
	format!(
		"Failed to switch the sync pair {label} to {mode_name}, so its mode is left as it was: \
		 {error}"
	)
}

/// `list-sync-conflicts`: the conflicts the pair `given` names holds, from the sync database.
pub(super) async fn list_sync_conflicts(
	config: &CliConfig,
	ui: &mut UI,
	client: &mut LazyClient,
	given: &str,
) -> Result<()> {
	let managing = Managing::open(config, ui, client).await?;
	let (pair, label) = managing.find(config, given).await?;
	let conflicts = managing
		.engine
		.list_conflicts(pair.id)
		.await
		.with_context(|| format!("Failed to list the conflicts of the sync pair {label}"))?;
	if ui.json {
		let conflicts = conflicts.iter().map(conflict_object).collect::<Vec<_>>();
		return ui.print_json(json!(conflicts));
	}
	if conflicts.is_empty() {
		ui.print_muted(&format!("The sync pair {label} holds no conflicts."));
		return Ok(());
	}
	for conflict in &conflicts {
		ui.print(&conflict_line(conflict));
	}
	ui.print_muted(&format!(
		"Resolve one with `filen resolve-sync-conflict {} <path> --keep-local|--keep-remote|--keep-both`.",
		pair.id
	));
	Ok(())
}

/// The line `list-sync-conflicts` prints for `conflict`: what each side holds, and for a backup
/// mode's kind mismatch why it is held and what resolves it.
pub(super) fn conflict_line(conflict: &PlannedConflict) -> String {
	let side = |kind: Option<PlannedNodeKind>| kind.map_or("deleted", node_name);
	let sides = format!(
		"{}: local {}, remote {}",
		conflict.rel_path,
		side(conflict.local),
		side(conflict.remote)
	);
	match conflict.reason {
		ConflictReason::Diverged => sides,
		ConflictReason::RemoteKeepsDir
		| ConflictReason::RemoteKeepsFile
		| ConflictReason::LocalKeepsDir
		| ConflictReason::LocalKeepsFile => format!(
			"{sides}, as {}; resolve it with {}",
			conflict.reason,
			conflict_resolutions::resolution_flags(conflict.reason)
		),
	}
}

/// `conflict` for `--json`: its path, what each side holds (`null` for a side that deleted it), why
/// it is held and the resolutions that can clear it.
pub(super) fn conflict_object(conflict: &PlannedConflict) -> Value {
	json!({
		"path": conflict.rel_path,
		"local": conflict.local.map(node_name),
		"remote": conflict.remote.map(node_name),
		"reason": json_events::conflict_reason_str(conflict.reason),
		"resolutions": json_events::resolution_strs(conflict.reason),
	})
}

/// How the output names an item of kind `node`.
pub(super) fn node_name(node: PlannedNodeKind) -> &'static str {
	match node {
		PlannedNodeKind::File => "file",
		PlannedNodeKind::Dir => "directory",
	}
}

/// `resolve-sync-conflict`: resolves the conflict at `path` of the pair `given` names with
/// `resolution`, which its next pass applies.
pub(super) async fn resolve_sync_conflict(
	config: &CliConfig,
	ui: &mut UI,
	client: &mut LazyClient,
	given: &str,
	path: &str,
	resolution: ConflictResolution,
) -> Result<()> {
	let managing = Managing::open(config, ui, client).await?;
	let (pair, label) = managing.find(config, given).await?;
	let path = conflict_resolutions::normalize(path);
	let resolved = managing
		.engine
		.resolve_conflict(pair.id, &path, resolution)
		.await
		.map_err(|e| UI::failure(&resolve_failure_message(&path, Cow::Borrowed(&label), &e)))?;
	for line in resolved_conflict_lines(&path, resolution, resolved) {
		ui.print_success(&line);
	}
	ui.print_muted(NEXT_PASS_NOTE);
	Ok(())
}

/// `retry-sync`: lets the next pass of the pair `given` names try `path`, or every path, again,
/// although it stopped trying after it failed repeatedly.
pub(super) async fn retry_sync(
	config: &CliConfig,
	ui: &mut UI,
	client: &mut LazyClient,
	given: &str,
	path: Option<&str>,
) -> Result<()> {
	let managing = Managing::open(config, ui, client).await?;
	let (pair, label) = managing.find(config, given).await?;
	match path {
		Some(path) => {
			let path = conflict_resolutions::normalize(path);
			managing
				.engine
				.retry_path(pair.id, &path)
				.await
				.with_context(|| format!("Failed to retry {path} in the sync pair {label}"))?;
			ui.print_success(&format!(
				"The next pass of the sync pair {label} tries {path} again"
			));
		}
		None => {
			managing
				.engine
				.retry_all(pair.id)
				.await
				.with_context(|| format!("Failed to retry the paths of the sync pair {label}"))?;
			ui.print_success(&format!(
				"The next pass of the sync pair {label} tries every path that kept failing again"
			));
		}
	}
	ui.print_muted(NEXT_PASS_NOTE);
	Ok(())
}

/// `pause-sync-pair` (with `pause`) and `resume-sync-pair`: pauses or resumes the pair `given`
/// names.
/// The flag is kept in the sync database, so `sync` leaves a paused pair alone until it is resumed.
pub(super) async fn pause_sync_pair(
	config: &CliConfig,
	ui: &mut UI,
	client: &mut LazyClient,
	given: &str,
	pause: bool,
) -> Result<()> {
	let managing = Managing::open(config, ui, client).await?;
	let (pair, label) = managing.find(config, given).await?;
	if pair.paused == pause {
		let state = if pause {
			"already paused"
		} else {
			"not paused"
		};
		ui.print(&format!("The sync pair {label} is {state}"));
		return Ok(());
	}
	if pause {
		managing
			.engine
			.pause_pair(pair.id)
			.await
			.with_context(|| format!("Failed to pause the sync pair {label}"))?;
		ui.print_success(&format!(
			"Paused the sync pair {label}. `filen sync` leaves it alone until `filen \
			 resume-sync-pair {}`.",
			pair.id
		));
	} else {
		managing
			.engine
			.resume_pair(pair.id)
			.await
			.with_context(|| format!("Failed to resume the sync pair {label}"))?;
		ui.print_success(&format!("Resumed the sync pair {label}"));
		ui.print_muted(NEXT_PASS_NOTE);
	}
	Ok(())
}

/// What a command whose change the next pass applies says last.
const NEXT_PASS_NOTE: &str = "`filen sync` runs that pass.";

/// What is printed while a call waits for the cache to finish listing a pair's remote folder.
pub(super) const WAITING_FOR_LISTING: &str = "Waiting for the remote folder to finish indexing...";

/// How long [`awaiting_listing`] waits for the cache to finish listing, as long as a pass waits.
const LISTING_WAIT: Duration = Duration::from_secs(15 * 60);

/// How long [`awaiting_listing`] waits before it calls again the first time, and at most.
const LISTING_POLL: (Duration, Duration) = (Duration::from_millis(500), Duration::from_secs(5));

/// The waits between [`awaiting_listing`]'s calls: doubling from the first to the longest, until
/// they add up to [`LISTING_WAIT`].
fn listing_delays() -> impl Iterator<Item = Duration> {
	let (first, longest) = LISTING_POLL;
	iter::successors(Some(first), move |delay| Some((*delay * 2).min(longest))).scan(
		Duration::ZERO,
		|waited, delay| {
			(*waited < LISTING_WAIT).then(|| {
				*waited += delay;
				delay
			})
		},
	)
}

/// Makes `call`, which needs the cache to have finished listing a pair's remote folder, and fails
/// with [`ErrorKind::StaleState`] at once while it has not, as the cache does after it starts. It
/// is called again after a growing wait ([`listing_delays`]). Before the first wait,
/// [`WAITING_FOR_LISTING`] is printed as a notice, on stderr and not with `--json` or `--quiet`,
/// so that it stays out of the output a program reads, as the JSON of `sync --dry-run --json`.
pub(super) async fn awaiting_listing<T>(
	mut call: impl AsyncFnMut() -> Result<T, filen_sdk_rs::Error>,
	ui: &mut UI,
) -> Result<T, filen_sdk_rs::Error> {
	let mut delays = listing_delays();
	let mut notified = false;
	loop {
		match call().await {
			Err(e) if e.kind() == ErrorKind::StaleState => {
				let Some(delay) = delays.next() else {
					return Err(e);
				};
				if !notified {
					ui.print_muted_notice(WAITING_FOR_LISTING);
					notified = true;
				}
				tokio::time::sleep(delay).await;
			}
			result => return result,
		}
	}
}

#[cfg(test)]
mod tests {
	use std::time::Duration;

	use filen_sdk_rs::{
		ErrorKind,
		sync_engine::{
			ConflictReason, DeleteGuard, PairRecord, PlannedConflict, PlannedNodeKind, SyncMode,
		},
	};
	use filen_types::fs::Uuid;
	use futures::FutureExt as _;

	use serde_json::json;

	use super::{
		LISTING_WAIT, WAITING_FOR_LISTING, awaiting_listing, conflict_line, conflict_object,
		listing_delays, pair_line, propagate_deletions_question, switch_failure_message,
	};
	use crate::ui::test_support::Printed;

	/// What stdout and stderr hold once [`awaiting_listing`] has started to wait for a listing,
	/// at a terminal, with or without `--json`.
	async fn printed_while_awaiting_a_listing(json: bool) -> (String, String) {
		let out = Printed::default();
		let err = Printed::default();
		let mut ui = Printed::split_terminal_ui(&out, &err);
		ui.initialize(false, json, None, true, false);
		let waiting = awaiting_listing(
			async || {
				Err::<(), _>(filen_sdk_rs::Error::custom(
					ErrorKind::StaleState,
					"still listing",
				))
			},
			&mut ui,
		);
		// polled once: the call fails, the notice is printed, and the wait begins, which the test
		// does not sit through
		assert!(waiting.now_or_never().is_none());
		(
			out.lines_or_empty(),
			console::strip_ansi_codes(&err.lines_or_empty()).into_owned(),
		)
	}

	#[tokio::test]
	async fn the_indexing_notice_stays_out_of_json_stdout() {
		assert_eq!(
			printed_while_awaiting_a_listing(true).await,
			(String::new(), String::new())
		);
	}

	#[tokio::test]
	async fn the_indexing_notice_goes_to_stderr() {
		assert_eq!(
			printed_while_awaiting_a_listing(false).await,
			(String::new(), WAITING_FOR_LISTING.to_owned())
		);
	}

	#[test]
	fn a_conflict_names_what_each_side_holds() {
		let conflict = PlannedConflict {
			rel_path: "docs/a.txt".to_string(),
			local: Some(PlannedNodeKind::File),
			remote: None,
			reason: ConflictReason::Diverged,
		};

		assert_eq!(
			conflict_line(&conflict),
			"docs/a.txt: local file, remote deleted"
		);
	}

	#[test]
	fn a_conflict_object_names_each_side_as_the_line_does() {
		let conflict = PlannedConflict {
			rel_path: "docs".to_string(),
			local: Some(PlannedNodeKind::Dir),
			remote: Some(PlannedNodeKind::File),
			reason: ConflictReason::Diverged,
		};

		assert_eq!(
			conflict_object(&conflict),
			json!({
				"path": "docs", "local": "directory", "remote": "file", "reason": "diverged",
				"resolutions": ["keep_local", "keep_remote", "keep_both"],
			})
		);
		assert_eq!(
			conflict_line(&conflict),
			"docs: local directory, remote file"
		);
	}

	#[test]
	fn a_kind_mismatch_says_why_it_is_held_and_what_resolves_it() {
		let conflict = PlannedConflict {
			rel_path: "docs".to_string(),
			local: Some(PlannedNodeKind::Dir),
			remote: Some(PlannedNodeKind::File),
			reason: ConflictReason::RemoteKeepsFile,
		};

		assert_eq!(
			conflict_line(&conflict),
			"docs: local directory, remote file, as the local side holds a directory where the \
			 remote backup keeps a file; resolve it with --keep-both"
		);
		assert_eq!(
			conflict_object(&conflict),
			json!({
				"path": "docs", "local": "directory", "remote": "file",
				"reason": "remote_keeps_file", "resolutions": ["keep_both"],
			})
		);
	}

	#[test]
	fn the_waits_for_the_listing_double_up_to_five_seconds_for_fifteen_minutes() {
		let delays = listing_delays().collect::<Vec<_>>();

		assert_eq!(
			delays[..6],
			[500, 1000, 2000, 4000, 5000, 5000].map(Duration::from_millis)
		);
		let total = delays.iter().sum::<Duration>();
		assert!(
			total >= LISTING_WAIT && total < LISTING_WAIT + Duration::from_secs(5),
			"{total:?}"
		);
	}

	fn record(paused: bool, first_pass_completed: bool) -> PairRecord {
		PairRecord {
			id: 3,
			local_root: "/home/me/docs".to_string(),
			remote_root: Uuid::from_u128(1),
			mode: SyncMode::TwoWay,
			delete_guard: DeleteGuard::default(),
			paused,
			first_pass_completed,
		}
	}

	#[test]
	fn a_pair_is_listed_by_its_id_and_label() {
		assert_eq!(pair_line(&record(false, true), "L"), "3: L");
	}

	#[test]
	fn a_listed_pair_says_whether_it_is_paused_and_has_completed_a_pass() {
		assert_eq!(
			pair_line(&record(true, false), "L"),
			"3: L (paused) (no pass completed yet)"
		);
	}

	#[test]
	fn propagating_deletions_asks_in_one_line() {
		assert_eq!(
			propagate_deletions_question("L", "localToRemote"),
			"Switch the sync pair L to localToRemote? Its next pass then deletes on the destination \
			 what the source no longer has, as localToRemote does; the delete guard still holds a \
			 large batch of deletions back for approval."
		);
	}

	#[test]
	fn a_failed_switch_says_the_mode_is_left_in_one_line() {
		assert_eq!(
			switch_failure_message("L", "localBackup", &"offline"),
			"Failed to switch the sync pair L to localBackup, so its mode is left as it was: offline"
		);
	}
}
