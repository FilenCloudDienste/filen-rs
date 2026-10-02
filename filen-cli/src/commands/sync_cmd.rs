use std::{
	collections::{HashMap, HashSet, VecDeque},
	fmt, mem,
	path::PathBuf,
	sync::Arc,
	time::{Duration, Instant},
};

use anyhow::{Context, Result};
use console::{Term, style};
use filen_sdk_rs::{
	fs::{HasUUID as _, categories::NonRootFileType},
	sync_engine::{
		SyncEngine, SyncEvent, SyncMode, SyncReport, TransferDirection, WatchState, WatchStatus,
	},
};
use tokio::{
	select,
	sync::mpsc,
	time::{MissedTickBehavior, interval},
};

use crate::{
	CliConfig,
	auth::LazyClient,
	ui::{self, UI},
	util::RemotePath,
};

// todo: pair management, conflict resolution, deletion approval, other sync modes, --json output

/// How far back the transfer rates on the status line look.
const RATE_WINDOW: Duration = Duration::from_secs(5);
/// The shortest span a rate is averaged over, so the first progress of a pass is not read as a spike.
const MIN_RATE_SPAN: Duration = Duration::from_secs(1);
/// How often the status line is redrawn, so its rates fall away once transfers stop reporting.
const STATUS_REDRAW_INTERVAL: Duration = Duration::from_millis(500);

/// Two-way sync between a local directory and a directory in the Filen drive, printing every event
/// the engine reports, until Ctrl-C. On a terminal, a status line under the events shows the live
/// upload and download rates while a pass runs.
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
	let mut status_line = StatusLine::new(ui);
	let mut redraw = interval(STATUS_REDRAW_INTERVAL);
	redraw.set_missed_tick_behavior(MissedTickBehavior::Skip);
	loop {
		select! {
			Some(event) = events_rx.recv() => status_line.show(ui, &mut printed_failures, event),
			_ = redraw.tick() => status_line.draw(),
			changed = status.changed() => {
				if changed.is_err() {
					break;
				}
				let current = status.borrow_and_update().clone();
				status_line.print_above(|| print_status_change(ui, &last_status, &current));
				if matches!(current.state, WatchState::Stopped | WatchState::PairRemoved) {
					break;
				}
				last_status = current;
			}
			_ = stop_rx.recv() => {
				status_line.print_above(|| {
					ui.print_muted("Stopping after the current pass (Ctrl-C again to quit immediately)...");
				});
				// Keep printing while the pass finishes: its last events are the ones being waited for.
				let stop = watch.stop();
				tokio::pin!(stop);
				loop {
					select! {
						() = &mut stop => break,
						Some(event) = events_rx.recv() => status_line.show(ui, &mut printed_failures, event),
						_ = redraw.tick() => status_line.draw(),
						// Leaves the drive lock to expire on the server rather than wait for the pass.
						_ = stop_rx.recv() => {
							status_line.clear();
							std::process::exit(130);
						}
					}
				}
				break;
			}
		}
	}
	while let Ok(event) = events_rx.try_recv() {
		status_line.show(ui, &mut printed_failures, event);
	}
	status_line.clear();
	Ok(())
}

/// The dim line kept under the printed events while a pass runs, redrawn in place with the live
/// transfer rates.
///
/// Writes to the terminal itself like the upload and download progress line, since it has to stay
/// on one row that every printed line first clears and then draws again below itself.
struct StatusLine {
	term: Term,
	state: LineState,
	rates: TransferRates,
}

/// Where the status line stands.
#[derive(Clone, Copy, PartialEq, Eq)]
enum LineState {
	/// Never drawn: in quiet or JSON mode, or when stdout is redirected, where the redraws would end
	/// up in the captured output.
	Disabled,
	/// Off the screen: between passes, or while a line is printed above it.
	Hidden,
	/// On the screen, to be cleared before anything else is printed.
	Shown,
}

impl StatusLine {
	fn new(ui: &UI) -> Self {
		let term = Term::stdout();
		let state = if term.is_term() && !ui.is_quiet() && !ui.json {
			LineState::Hidden
		} else {
			LineState::Disabled
		};
		Self {
			term,
			state,
			rates: TransferRates::default(),
		}
	}

	/// Count `event` into the rates and print it above the line.
	fn show(&mut self, ui: &mut UI, printed_failures: &mut HashSet<String>, event: SyncEvent) {
		if self.state != LineState::Disabled {
			self.rates.record(&event, Instant::now());
		}
		// A progress tick prints nothing, so the line stays as it is until the next redraw.
		if !matches!(event, SyncEvent::Progress { .. }) {
			self.print_above(|| print_event(ui, printed_failures, event));
		}
	}

	/// Run `print` with the line out of its way, then draw the line again under what it printed.
	fn print_above(&mut self, print: impl FnOnce()) {
		self.clear();
		print();
		self.draw();
	}

	/// Draw the line with the current rates, or take it off the screen between passes.
	fn draw(&mut self) {
		if self.state == LineState::Disabled {
			return;
		}
		let Some(rates) = self.rates.current(Instant::now()) else {
			self.clear();
			return;
		};
		let line = ui::truncate_to_width(&rates.to_string(), usize::from(self.term.size().1));
		let _ = self.term.clear_line();
		let _ = self.term.write_str(&style(line).dim().to_string());
		let _ = self.term.flush();
		self.state = LineState::Shown;
	}

	/// Erase the line, so what is printed next starts on a clean row.
	fn clear(&mut self) {
		if self.state == LineState::Shown {
			let _ = self.term.clear_line();
			let _ = self.term.flush();
			self.state = LineState::Hidden;
		}
	}
}

/// The upload and download rates of the running pass, over its last [`RATE_WINDOW`].
#[derive(Default)]
struct TransferRates {
	upload: DirectionRates,
	download: DirectionRates,
	/// When the running pass started; `None` between passes, when there is nothing to show.
	pass_started: Option<Instant>,
	/// The directory creation the previous event announced, already counted. The engine creates
	/// directories one at a time and reports a failed one with the very next event, which takes the
	/// count back.
	announced_dir: Option<(TransferDirection, String)>,
}

/// What moved one way within the window.
#[derive(Default)]
struct DirectionRates {
	/// Bytes moved, one `(when, how many)` per progress tick, oldest first.
	moved: VecDeque<(Instant, u64)>,
	/// When each file transfer finished, oldest first.
	files: VecDeque<Instant>,
	/// When each directory was created on the side this direction writes to (the remote for
	/// uploads, the local tree for downloads), oldest first.
	dirs: VecDeque<Instant>,
	/// The running byte count each transfer still in flight last reported, to turn the cumulative
	/// counts of its progress into the bytes moved since its previous tick.
	in_flight: HashMap<String, u64>,
}

/// The rates shown on the status line.
#[derive(Debug, PartialEq, Eq)]
struct Rates {
	upload: Rate,
	download: Rate,
}

/// One direction's rates.
#[derive(Debug, PartialEq, Eq)]
struct Rate {
	/// Bytes per second.
	bytes: u64,
	/// Finished file transfers per second, in tenths (12 is 1.2 files/s).
	file_tenths: u64,
	/// Directories created per second on the side this direction writes to, in tenths.
	dir_tenths: u64,
}

impl TransferRates {
	fn record(&mut self, event: &SyncEvent, now: Instant) {
		let announced_dir = self.announced_dir.take();
		match event {
			// What came before belongs to another pass, and the span starts over.
			SyncEvent::PassStarted { .. } => {
				*self = Self {
					pass_started: Some(now),
					..Self::default()
				};
			}
			SyncEvent::PassCompleted { .. } | SyncEvent::PassFailed { .. } => {
				*self = Self::default();
			}
			SyncEvent::Progress {
				rel_path,
				direction,
				bytes,
				..
			} => self.direction(*direction).progress(rel_path, *bytes, now),
			SyncEvent::Uploading { rel_path } => self.upload.complete(rel_path, now),
			SyncEvent::Downloading { rel_path } => self.download.complete(rel_path, now),
			SyncEvent::CreatingRemoteDir { rel_path } => {
				self.announce_dir(TransferDirection::Upload, rel_path, now);
			}
			SyncEvent::CreatingLocalDir { rel_path } => {
				self.announce_dir(TransferDirection::Download, rel_path, now);
			}
			// The transfer is over: the bytes it moved did move and stay counted, and its path counts
			// from zero should it move again.
			SyncEvent::ActionFailed { rel_path, .. } => {
				self.upload.in_flight.remove(rel_path);
				self.download.in_flight.remove(rel_path);
				// The directory the previous event announced was not created after all. Nothing was
				// counted after it, so it is the newest count, if the window has not dropped it yet.
				if let Some((direction, dir)) = announced_dir
					&& dir == *rel_path
				{
					self.direction(direction).dirs.pop_back();
				}
			}
			_ => {}
		}
	}

	/// Count the directory at `rel_path` as created, until the next event says it was not.
	fn announce_dir(&mut self, direction: TransferDirection, rel_path: &str, now: Instant) {
		self.direction(direction).dirs.push_back(now);
		self.announced_dir = Some((direction, rel_path.to_owned()));
	}

	fn direction(&mut self, direction: TransferDirection) -> &mut DirectionRates {
		match direction {
			TransferDirection::Upload => &mut self.upload,
			TransferDirection::Download => &mut self.download,
		}
	}

	/// The rates at `now`, or `None` when no pass is running. Averaged over the window, or over the
	/// time since the pass started when that is shorter (so a short pass is not under-reported),
	/// but over at least [`MIN_RATE_SPAN`].
	fn current(&mut self, now: Instant) -> Option<Rates> {
		let started = self.pass_started?;
		let span = now
			.saturating_duration_since(started)
			.clamp(MIN_RATE_SPAN, RATE_WINDOW);
		Some(Rates {
			upload: self.upload.rate(now, span),
			download: self.download.rate(now, span),
		})
	}
}

impl DirectionRates {
	fn progress(&mut self, rel_path: &str, bytes: u64, now: Instant) {
		let previous = match self.in_flight.get_mut(rel_path) {
			Some(last) => mem::replace(last, bytes),
			None => {
				self.in_flight.insert(rel_path.to_owned(), bytes);
				0
			}
		};
		self.moved.push_back((now, bytes.saturating_sub(previous)));
	}

	fn complete(&mut self, rel_path: &str, now: Instant) {
		self.files.push_back(now);
		self.in_flight.remove(rel_path);
	}

	/// Drop what fell out of the window before `now`, and average the rest over `span`.
	fn rate(&mut self, now: Instant, span: Duration) -> Rate {
		let expired = |at: Instant| now.saturating_duration_since(at) > RATE_WINDOW;
		while let Some(&(at, _)) = self.moved.front()
			&& expired(at)
		{
			self.moved.pop_front();
		}
		for times in [&mut self.files, &mut self.dirs] {
			while let Some(&at) = times.front()
				&& expired(at)
			{
				times.pop_front();
			}
		}
		// At least MIN_RATE_SPAN, so never zero.
		let millis = span.as_millis();
		let bytes: u128 = self.moved.iter().map(|&(_, bytes)| u128::from(bytes)).sum();
		let tenths = |count: usize| {
			u64::try_from((count as u128).saturating_mul(10_000) / millis).unwrap_or(u64::MAX)
		};
		Rate {
			bytes: u64::try_from(bytes.saturating_mul(1000) / millis).unwrap_or(u64::MAX),
			file_tenths: tenths(self.files.len()),
			dir_tenths: tenths(self.dirs.len()),
		}
	}
}

impl fmt::Display for Rates {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "↑ {}   ↓ {}", self.upload, self.download)
	}
}

impl fmt::Display for Rate {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		// One decimal at every magnitude and right-aligned, so the line keeps its width, and the
		// download half its column, as the rates change between redraws.
		let bytes = humansize::format_size(
			self.bytes,
			humansize::BINARY.decimal_places(1).decimal_zeroes(1),
		);
		let per_second = |tenths: u64| format!("{}.{}", tenths / 10, tenths % 10);
		write!(
			f,
			"{bytes:>10}/s, {:>5} files/s, {:>5} dirs/s",
			per_second(self.file_tenths),
			per_second(self.dir_tenths)
		)
	}
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
		// Byte counts per transfer, several per second: too noisy for a line each, they add up to the
		// rates on the status line instead.
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

#[cfg(test)]
mod tests {
	use super::*;

	const MIB: u64 = 1024 * 1024;

	fn at(start: Instant, millis: u64) -> Instant {
		start + Duration::from_millis(millis)
	}

	fn started(start: Instant) -> TransferRates {
		let mut rates = TransferRates::default();
		rates.record(
			&SyncEvent::PassStarted {
				mode: SyncMode::TwoWay,
			},
			start,
		);
		rates
	}

	fn progress(rel_path: &str, direction: TransferDirection, bytes: u64) -> SyncEvent {
		SyncEvent::Progress {
			rel_path: rel_path.to_owned(),
			direction,
			bytes,
			total: 10 * MIB,
		}
	}

	fn rate(bytes: u64, file_tenths: u64, dir_tenths: u64) -> Rate {
		Rate {
			bytes,
			file_tenths,
			dir_tenths,
		}
	}

	#[test]
	fn cumulative_progress_becomes_bytes_moved_per_direction() {
		let start = Instant::now();
		let mut rates = started(start);
		rates.record(
			&progress("a", TransferDirection::Upload, 1000),
			at(start, 100),
		);
		rates.record(
			&progress("a", TransferDirection::Upload, 3000),
			at(start, 300),
		);
		rates.record(
			&progress("b", TransferDirection::Upload, 2000),
			at(start, 300),
		);
		rates.record(
			&progress("c", TransferDirection::Download, 500),
			at(start, 400),
		);
		assert_eq!(
			rates.current(at(start, 5000)),
			Some(Rates {
				upload: rate(1000, 0, 0),
				download: rate(100, 0, 0),
			})
		);
	}

	#[test]
	fn progress_older_than_the_window_drops_out() {
		let start = Instant::now();
		let mut rates = started(start);
		rates.record(
			&progress("a", TransferDirection::Upload, 10_000),
			at(start, 1000),
		);
		rates.record(
			&SyncEvent::Uploading {
				rel_path: "b".to_owned(),
			},
			at(start, 1500),
		);
		rates.record(
			&progress("a", TransferDirection::Upload, 15_000),
			at(start, 6000),
		);
		assert_eq!(
			rates.current(at(start, 7000)),
			Some(Rates {
				upload: rate(1000, 0, 0),
				download: rate(0, 0, 0),
			})
		);
	}

	#[test]
	fn files_per_second_counts_only_finished_transfers_of_its_own_direction() {
		let start = Instant::now();
		let mut rates = started(start);
		let path = |p: &str| p.to_owned();
		rates.record(
			&progress("in-flight", TransferDirection::Upload, 10),
			at(start, 500),
		);
		rates.record(
			&SyncEvent::Uploading {
				rel_path: path("a"),
			},
			at(start, 1000),
		);
		rates.record(
			&SyncEvent::Uploading {
				rel_path: path("b"),
			},
			at(start, 2000),
		);
		rates.record(
			&SyncEvent::Downloading {
				rel_path: path("c"),
			},
			at(start, 3000),
		);
		rates.record(
			&SyncEvent::CreatingRemoteDir {
				rel_path: path("d"),
			},
			at(start, 3000),
		);
		rates.record(
			&SyncEvent::CreatingLocalDir {
				rel_path: path("e"),
			},
			at(start, 3000),
		);
		rates.record(
			&SyncEvent::ActionFailed {
				rel_path: path("f"),
				error: "failed".to_owned(),
			},
			at(start, 3000),
		);
		assert_eq!(
			rates.current(at(start, 5000)),
			Some(Rates {
				upload: rate(2, 4, 2),
				download: rate(0, 2, 2),
			})
		);
	}

	#[test]
	fn a_failed_transfer_counts_from_zero_when_its_path_moves_again() {
		let start = Instant::now();
		let mut rates = started(start);
		rates.record(
			&progress("a", TransferDirection::Upload, 4000),
			at(start, 1000),
		);
		rates.record(
			&SyncEvent::ActionFailed {
				rel_path: "a".to_owned(),
				error: "failed".to_owned(),
			},
			at(start, 2000),
		);
		rates.record(
			&progress("a", TransferDirection::Upload, 1000),
			at(start, 3000),
		);
		assert_eq!(
			rates.current(at(start, 5000)),
			Some(Rates {
				upload: rate(1000, 0, 0),
				download: rate(0, 0, 0),
			})
		);
	}

	#[test]
	fn the_span_is_the_time_since_the_pass_started_but_at_least_a_second() {
		let start = Instant::now();
		let mut rates = started(start);
		rates.record(
			&progress("a", TransferDirection::Download, 4000),
			at(start, 200),
		);
		assert_eq!(
			rates.current(at(start, 200)),
			Some(Rates {
				upload: rate(0, 0, 0),
				download: rate(4000, 0, 0),
			})
		);
		assert_eq!(
			rates.current(at(start, 2000)),
			Some(Rates {
				upload: rate(0, 0, 0),
				download: rate(2000, 0, 0),
			})
		);
	}

	#[test]
	fn there_are_no_rates_outside_a_pass() {
		let start = Instant::now();
		let mut rates = TransferRates::default();
		assert_eq!(rates.current(start), None);
		rates.record(
			&SyncEvent::PassStarted {
				mode: SyncMode::TwoWay,
			},
			start,
		);
		rates.record(
			&progress("a", TransferDirection::Upload, 4000),
			at(start, 500),
		);
		rates.record(
			&SyncEvent::PassFailed {
				error: "failed".to_owned(),
			},
			at(start, 1000),
		);
		assert_eq!(rates.current(at(start, 1500)), None);
	}

	#[test]
	fn directories_count_on_the_side_they_were_created_unless_creating_them_failed() {
		let start = Instant::now();
		let mut rates = started(start);
		let path = |p: &str| p.to_owned();
		let failed = |p: &str| SyncEvent::ActionFailed {
			rel_path: p.to_owned(),
			error: "failed".to_owned(),
		};
		rates.record(
			&SyncEvent::CreatingRemoteDir {
				rel_path: path("up-1"),
			},
			at(start, 1000),
		);
		rates.record(
			&SyncEvent::CreatingRemoteDir {
				rel_path: path("up-2"),
			},
			at(start, 1100),
		);
		rates.record(&failed("up-2"), at(start, 1200));
		rates.record(
			&SyncEvent::CreatingLocalDir {
				rel_path: path("down-1"),
			},
			at(start, 1300),
		);
		// Another path's failure right after a created directory takes nothing back.
		rates.record(&failed("elsewhere"), at(start, 1400));
		rates.record(
			&SyncEvent::CreatingLocalDir {
				rel_path: path("down-2"),
			},
			at(start, 1500),
		);
		// Nor does a failure of the same path that is not the very next event.
		rates.record(
			&SyncEvent::Uploading {
				rel_path: path("file"),
			},
			at(start, 1600),
		);
		rates.record(&failed("down-2"), at(start, 1700));
		assert_eq!(
			rates.current(at(start, 5000)),
			Some(Rates {
				upload: rate(0, 2, 2),
				download: rate(0, 0, 4),
			})
		);
	}

	#[test]
	fn rates_render_as_one_line_that_keeps_its_width() {
		let slow = Rates {
			upload: rate(3 * MIB / 2, 42, 5),
			download: rate(0, 0, 0),
		};
		assert_eq!(
			slow.to_string(),
			"↑    1.5 MiB/s,   4.2 files/s,   0.5 dirs/s   ↓      0.0 B/s,   0.0 files/s,   0.0 dirs/s"
		);
		let fast = Rates {
			upload: rate(1023 * 1024 + 900, 999, 120),
			download: rate(85 * MIB, 123, 999),
		};
		assert_eq!(
			fast.to_string(),
			"↑ 1023.9 KiB/s,  99.9 files/s,  12.0 dirs/s   ↓   85.0 MiB/s,  12.3 files/s,  99.9 dirs/s"
		);
	}
}
