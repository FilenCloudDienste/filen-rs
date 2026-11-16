//! What the core reports, as protocol events: the callback the dispatcher and the local engine
//! report to, which never blocks them, and the coalescer that batches its reports into lines.
//! Activity and skipped names past a full queue are counted as dropped; a notice of a local item
//! kept beside what the drive has (a conflict's local side, a local directory kept when its remote
//! one went) is never dropped, as the parent may be the only one to tell the user where it went.

use std::{
	sync::{
		Arc,
		atomic::{AtomicU64, Ordering},
	},
	time::Duration,
};

use tokio::{
	sync::mpsc,
	time::{Instant, sleep_until},
};
use tracing::debug;

use crate::{
	dispatch::CloudFilesCallback,
	helper::protocol::{
		ActivityEntry, ActivityKind, MAX_LIST_ENTRIES, OutMessage, SkippedEntry, SkippedPlace,
	},
	identity::PlaceholderIdentity,
	local::{ConflictKind, LocalReport, LocalSkip, RemoteApplied, UndoneOp, VetoedOp},
	os::UnsupportedKind,
	populate::{SkipReason, SkippedItem},
	root_path::RootPath,
};

/// How many reports wait for the coalescer at most; past it they are counted as dropped, so a
/// report never holds up an engine task.
const QUEUE_LEN: usize = 4096;

/// How many entries a line carries before it is written without waiting for the next flush.
const BATCH: usize = 100;

/// Where every protocol line goes: the writer task.
pub(crate) type Out = mpsc::UnboundedSender<OutMessage>;

/// One report, as the coalescer takes it.
#[derive(Debug)]
pub(crate) enum Entry {
	Activity(ActivityEntry),
	Skipped(SkippedEntry),
	Conflict {
		path: String,
		kept_as: String,
		kind: ConflictKind,
	},
}

impl Entry {
	/// Whether the entry tells where a local item holding data the drive lacks was kept, which is
	/// never dropped.
	fn is_notice(&self) -> bool {
		match self {
			Self::Conflict { .. } => true,
			Self::Activity(entry) => {
				matches!(entry.kind, ActivityKind::KeptLocal | ActivityKind::DirKept)
			}
			Self::Skipped(_) => false,
		}
	}
}

/// How many entries were dropped since the last line, by kind; shared by the callback, which
/// counts them, and the coalescer, which reports and resets them.
#[derive(Debug, Default)]
pub(crate) struct Dropped {
	activity: AtomicU64,
	skipped: AtomicU64,
}

/// The callback the core reports to: each report becomes an entry for the coalescer, or a dropped
/// one when the queue is full. Notices go on a queue of their own that is never full: each stands
/// for an item on disk, so they are as many as the items the provider kept.
pub(crate) struct HelperCallback {
	queue: mpsc::Sender<Entry>,
	notices: mpsc::UnboundedSender<Entry>,
	// Reason for the Arc: the coalescer reads and resets the counts.
	dropped: Arc<Dropped>,
}

impl HelperCallback {
	fn send(&self, entry: Entry) {
		if entry.is_notice() {
			// The coalescer is gone only once the helper stops.
			let _ = self.notices.send(entry);
			return;
		}
		let skipped = matches!(entry, Entry::Skipped(_));
		if self.queue.try_send(entry).is_err() {
			let count = if skipped {
				&self.dropped.skipped
			} else {
				&self.dropped.activity
			};
			count.fetch_add(1, Ordering::Relaxed);
		}
	}
}

/// A callback and the coalescer task writing its reports to `out`, batched every `period`.
pub(crate) fn reporting(out: Out, period: Duration) -> Arc<HelperCallback> {
	let (queue, entries) = mpsc::channel(QUEUE_LEN);
	let (notices, noticed) = mpsc::unbounded_channel();
	let dropped = Arc::new(Dropped::default());
	tokio::spawn(coalesce(
		entries,
		noticed,
		Arc::clone(&dropped),
		out,
		period,
	));
	Arc::new(HelperCallback {
		queue,
		notices,
		dropped,
	})
}

/// The next entry, the other queue's first, so a notice follows the activity reported before it;
/// `None` once every callback is gone and both queues are empty.
async fn next_entry(
	entries: &mut mpsc::Receiver<Entry>,
	notices: &mut mpsc::UnboundedReceiver<Entry>,
) -> Option<Entry> {
	tokio::select! {
		biased;
		Some(entry) = entries.recv() => Some(entry),
		Some(entry) = notices.recv() => Some(entry),
		else => None,
	}
}

/// Writes the entries `entries` brings to `out`: activity and skipped names in lines of at most
/// [`BATCH`] entries, each written once it is full or `period` after its first entry, and each
/// conflict at once, after the activity before it. Ends once every callback is gone.
async fn coalesce(
	mut entries: mpsc::Receiver<Entry>,
	mut notices: mpsc::UnboundedReceiver<Entry>,
	dropped: Arc<Dropped>,
	out: Out,
	period: Duration,
) {
	let mut activity = Vec::new();
	let mut skipped = Vec::new();
	let mut due: Option<Instant> = None;
	loop {
		let next = match due {
			Some(at) => tokio::select! {
				entry = next_entry(&mut entries, &mut notices) => Some(entry),
				() = sleep_until(at) => None,
			},
			None => Some(next_entry(&mut entries, &mut notices).await),
		};
		match next {
			// The period ran out.
			None => {
				flush(&out, &dropped, &mut activity, &mut skipped);
				due = None;
			}
			Some(None) => {
				flush(&out, &dropped, &mut activity, &mut skipped);
				return;
			}
			Some(Some(Entry::Conflict {
				path,
				kept_as,
				kind,
			})) => {
				flush_activity(&out, &dropped, &mut activity);
				send(
					&out,
					OutMessage::Conflict {
						path,
						kept_as,
						kind,
					},
				);
			}
			Some(Some(Entry::Activity(entry))) => {
				activity.push(entry);
				if activity.len() >= BATCH {
					flush_activity(&out, &dropped, &mut activity);
				}
			}
			Some(Some(Entry::Skipped(entry))) => {
				skipped.push(entry);
				if skipped.len() >= BATCH {
					flush_skipped(&out, &dropped, &mut skipped);
				}
			}
		}
		let pending = !activity.is_empty()
			|| !skipped.is_empty()
			|| dropped.activity.load(Ordering::Relaxed) > 0
			|| dropped.skipped.load(Ordering::Relaxed) > 0;
		if pending && due.is_none() {
			due = Some(Instant::now() + period);
		}
	}
}

fn send(out: &Out, message: OutMessage) {
	// The writer is gone once the helper stops: nothing is left to tell.
	let _ = out.send(message);
}

fn flush(
	out: &Out,
	dropped: &Dropped,
	activity: &mut Vec<ActivityEntry>,
	skipped: &mut Vec<SkippedEntry>,
) {
	flush_activity(out, dropped, activity);
	flush_skipped(out, dropped, skipped);
}

fn flush_activity(out: &Out, dropped: &Dropped, activity: &mut Vec<ActivityEntry>) {
	let lost = dropped.activity.swap(0, Ordering::Relaxed);
	if activity.is_empty() && lost == 0 {
		return;
	}
	let mut entries = std::mem::take(activity);
	let rest = entries.split_off(entries.len().min(MAX_LIST_ENTRIES));
	*activity = rest;
	send(
		out,
		OutMessage::Activity {
			entries,
			dropped: lost,
		},
	);
}

fn flush_skipped(out: &Out, dropped: &Dropped, skipped: &mut Vec<SkippedEntry>) {
	let lost = dropped.skipped.swap(0, Ordering::Relaxed);
	if skipped.is_empty() && lost == 0 {
		return;
	}
	let mut entries = std::mem::take(skipped);
	*skipped = entries.split_off(entries.len().min(MAX_LIST_ENTRIES));
	send(
		out,
		OutMessage::SkippedNames {
			entries,
			dropped: lost,
		},
	);
}

impl CloudFilesCallback for HelperCallback {
	fn on_skipped_items(&self, directory: PlaceholderIdentity, items: &[SkippedItem]) {
		let uuid = match directory {
			PlaceholderIdentity::Root { root } => root,
			PlaceholderIdentity::Dir { uuid } => uuid,
			PlaceholderIdentity::File { .. } | PlaceholderIdentity::Detached { .. } => {
				debug!(
					?directory,
					"skipped items reported for an item that is no directory"
				);
				return;
			}
		};
		for item in items {
			self.send(Entry::Skipped(SkippedEntry {
				place: SkippedPlace::Dir(uuid),
				name: item.name().map(str::to_owned),
				reason: skip_reason(item.reason()),
			}));
		}
	}

	fn on_local_report(&self, report: &LocalReport) {
		if let Some(entry) = entry(report) {
			self.send(entry);
		}
	}
}

/// The wire's spelling of a remote item's skip `reason`.
fn skip_reason(reason: &SkipReason) -> &'static str {
	match reason {
		SkipReason::Name(_) => "invalidName",
		SkipReason::CaseCollision { .. } => "caseCollision",
		SkipReason::Undecryptable => "undecryptable",
		SkipReason::NegativeSize(_) => "negativeSize",
		SkipReason::UnrepresentableTime(_) => "unrepresentableTime",
		SkipReason::MalformedId => "malformedId",
		SkipReason::Refused(_) => "refused",
	}
}

/// The wire's spelling of a local item's skip `reason`; `None` for what is ignored by default,
/// which is reported at debug level only.
fn local_skip_reason(reason: &LocalSkip) -> Option<&'static str> {
	Some(match reason {
		LocalSkip::Name(_) => "invalidName",
		LocalSkip::DriveRefusedName => "driveRefusedName",
		LocalSkip::DefaultIgnored => return None,
		LocalSkip::Unsupported(UnsupportedKind::Symlink) => "symlink",
		LocalSkip::Unsupported(UnsupportedKind::Junction) => "junction",
		LocalSkip::Unsupported(UnsupportedKind::ForeignReparsePoint) => "foreignReparsePoint",
		LocalSkip::Unrepresentable => "unrepresentable",
		LocalSkip::UnknownPlaceholder => "unknownPlaceholder",
	})
}

fn activity(kind: ActivityKind, path: &RootPath) -> Entry {
	Entry::Activity(ActivityEntry {
		kind,
		path: path.to_string(),
		to: None,
		failure: None,
	})
}

fn moved(kind: ActivityKind, from: &RootPath, to: &RootPath) -> Entry {
	Entry::Activity(ActivityEntry {
		kind,
		path: from.to_string(),
		to: Some(to.to_string()),
		failure: None,
	})
}

/// The entry `report` makes; `None` for what is not reported.
fn entry(report: &LocalReport) -> Option<Entry> {
	Some(match report {
		LocalReport::Uploaded { path, .. } => activity(ActivityKind::Uploaded, path),
		LocalReport::Adopted { path, .. } => activity(ActivityKind::Adopted, path),
		LocalReport::DirCreated { path, .. } => activity(ActivityKind::DirCreated, path),
		LocalReport::Trashed { path } => activity(ActivityKind::Trashed, path),
		LocalReport::Moved { from, to } => moved(ActivityKind::Moved, from, to),
		LocalReport::Conflict {
			path,
			kept_as,
			kind,
		} => Entry::Conflict {
			path: path.to_string(),
			kept_as: kept_as.to_string(),
			kind: *kind,
		},
		LocalReport::RemoteOutlivedDelete { path } => {
			activity(ActivityKind::RemoteOutlivedDelete, path)
		}
		LocalReport::Skipped { path, reason } => Entry::Skipped(SkippedEntry {
			place: SkippedPlace::Path(path.to_string()),
			name: path.name().map(|name| name.as_str().to_owned()),
			reason: local_skip_reason(reason)?,
		}),
		LocalReport::Busy { path } => activity(ActivityKind::Busy, path),
		LocalReport::Parked { path, failure, .. } => Entry::Activity(ActivityEntry {
			kind: ActivityKind::Parked,
			path: path.to_string(),
			to: None,
			failure: Some(*failure),
		}),
		LocalReport::Vetoed { path, op, failure } => Entry::Activity(ActivityEntry {
			kind: match op {
				VetoedOp::Delete => ActivityKind::DeleteVetoed,
				VetoedOp::Rename => ActivityKind::RenameVetoed,
			},
			path: path.to_string(),
			to: None,
			failure: Some(*failure),
		}),
		LocalReport::Undone {
			path,
			op: UndoneOp::Rename,
		} => activity(ActivityKind::RenameUndone, path),
		LocalReport::Remote(applied) => match applied {
			RemoteApplied::Created { path, .. } => activity(ActivityKind::RemoteCreated, path),
			RemoteApplied::Updated { path, .. } => activity(ActivityKind::RemoteUpdated, path),
			RemoteApplied::Moved { from, to } => moved(ActivityKind::RemoteMoved, from, to),
			RemoteApplied::Removed { path } => activity(ActivityKind::RemoteRemoved, path),
			RemoteApplied::KeptLocal { path, .. } => activity(ActivityKind::KeptLocal, path),
			RemoteApplied::DirKept { path, .. } => activity(ActivityKind::DirKept, path),
		},
		LocalReport::Damaged { path, .. } => activity(ActivityKind::Damaged, path),
		LocalReport::Freed { path } => activity(ActivityKind::Freed, path),
		LocalReport::KeptDownloaded { path } => activity(ActivityKind::KeptDownloaded, path),
	})
}

#[cfg(test)]
mod tests {
	use crate::{failure::Failure, test_support::root_path};

	use super::*;

	fn uploaded(n: usize) -> LocalReport {
		LocalReport::Uploaded {
			path: root_path(&format!("f{n}")),
			identity: PlaceholderIdentity::Detached {
				nonce: filen_types::fs::Uuid::from_u128(1),
			},
		}
	}

	fn activity_lens(lines: &[OutMessage]) -> Vec<(usize, u64)> {
		lines
			.iter()
			.filter_map(|line| match line {
				OutMessage::Activity { entries, dropped } => Some((entries.len(), *dropped)),
				_ => None,
			})
			.collect()
	}

	fn drain(lines: &mut mpsc::UnboundedReceiver<OutMessage>) -> Vec<OutMessage> {
		let mut drained = Vec::new();
		while let Ok(line) = lines.try_recv() {
			drained.push(line);
		}
		drained
	}

	#[tokio::test(start_paused = true)]
	async fn a_full_batch_is_written_at_once_and_the_rest_after_the_period() {
		let (out, mut lines) = mpsc::unbounded_channel();
		let callback = reporting(out, Duration::from_secs(1));
		for n in 0..150 {
			callback.on_local_report(&uploaded(n));
		}
		tokio::time::sleep(Duration::from_millis(500)).await;
		assert_eq!(activity_lens(&drain(&mut lines)), [(100, 0)]);
		tokio::time::sleep(Duration::from_millis(600)).await;
		assert_eq!(activity_lens(&drain(&mut lines)), [(50, 0)]);
	}

	#[tokio::test(start_paused = true)]
	async fn a_conflict_is_written_at_once_after_the_activity_before_it() {
		let (out, mut lines) = mpsc::unbounded_channel();
		let callback = reporting(out, Duration::from_secs(1));
		callback.on_local_report(&uploaded(0));
		callback.on_local_report(&LocalReport::Conflict {
			path: root_path("a.txt"),
			kept_as: root_path("a (1).txt"),
			kind: ConflictKind::EditVsEdit,
		});
		tokio::time::sleep(Duration::from_millis(10)).await;
		let lines = drain(&mut lines);
		assert_eq!(activity_lens(&lines), [(1, 0)]);
		assert_eq!(
			lines.last(),
			Some(&OutMessage::Conflict {
				path: "a.txt".to_owned(),
				kept_as: "a (1).txt".to_owned(),
				kind: ConflictKind::EditVsEdit,
			})
		);
	}

	/// A callback whose queue holds `len` entries, and its coalescer's channels.
	fn small_queue(
		len: usize,
	) -> (
		HelperCallback,
		mpsc::Receiver<Entry>,
		mpsc::UnboundedReceiver<Entry>,
		Arc<Dropped>,
	) {
		let (queue, entries) = mpsc::channel(len);
		let (notices, noticed) = mpsc::unbounded_channel();
		let dropped = Arc::new(Dropped::default());
		let callback = HelperCallback {
			queue,
			notices,
			dropped: Arc::clone(&dropped),
		};
		(callback, entries, noticed, dropped)
	}

	/// The lines the coalescer writes for what it was handed, once its period ran out.
	async fn coalesced(
		entries: mpsc::Receiver<Entry>,
		noticed: mpsc::UnboundedReceiver<Entry>,
		dropped: Arc<Dropped>,
	) -> Vec<OutMessage> {
		let (out, mut lines) = mpsc::unbounded_channel();
		tokio::spawn(coalesce(
			entries,
			noticed,
			dropped,
			out,
			Duration::from_secs(1),
		));
		tokio::time::sleep(Duration::from_secs(2)).await;
		drain(&mut lines)
	}

	#[tokio::test(start_paused = true)]
	async fn reports_past_a_full_queue_are_counted_as_dropped_by_kind() {
		let (callback, entries, noticed, dropped) = small_queue(2);
		for n in 0..5 {
			callback.on_local_report(&uploaded(n));
		}
		callback.on_local_report(&LocalReport::Skipped {
			path: root_path("link"),
			reason: LocalSkip::Unsupported(UnsupportedKind::Symlink),
		});
		let lines = coalesced(entries, noticed, dropped).await;
		assert_eq!(activity_lens(&lines), [(2, 3)]);
		assert!(lines.contains(&OutMessage::SkippedNames {
			entries: Vec::new(),
			dropped: 1,
		}));
	}

	#[tokio::test(start_paused = true)]
	async fn a_notice_of_a_kept_item_is_never_dropped() {
		let (callback, entries, noticed, dropped) = small_queue(1);
		callback.on_local_report(&uploaded(0));
		callback.on_local_report(&LocalReport::Conflict {
			path: root_path("a.txt"),
			kept_as: root_path("a (1).txt"),
			kind: ConflictKind::EditVsEdit,
		});
		callback.on_local_report(&LocalReport::Remote(RemoteApplied::DirKept {
			path: root_path("d"),
			kept: 2,
		}));
		let lines = coalesced(entries, noticed, dropped).await;
		assert!(
			lines
				.iter()
				.any(|line| matches!(line, OutMessage::Conflict { .. }))
		);
		assert!(lines.iter().any(|line| matches!(
			line,
			OutMessage::Activity { entries, .. }
				if entries.iter().any(|entry| entry.kind == ActivityKind::DirKept)
		)));
	}

	#[test]
	fn a_park_carries_its_failure_and_a_default_ignored_name_is_not_reported() {
		let Some(Entry::Activity(parked)) = entry(&LocalReport::Parked {
			path: root_path("a"),
			failure: Failure::Unreachable,
			attempts: 3,
		}) else {
			panic!("a park is activity");
		};
		assert_eq!(parked.failure, Some(Failure::Unreachable));
		assert!(
			entry(&LocalReport::Skipped {
				path: root_path("desktop.ini"),
				reason: LocalSkip::DefaultIgnored,
			})
			.is_none()
		);
	}

	#[test]
	fn a_skipped_local_name_carries_its_path_and_name() {
		let Some(Entry::Skipped(skipped)) = entry(&LocalReport::Skipped {
			path: root_path("d\\link"),
			reason: LocalSkip::Unsupported(UnsupportedKind::Symlink),
		}) else {
			panic!("a skip is a skipped name");
		};
		assert_eq!(
			skipped,
			SkippedEntry {
				place: SkippedPlace::Path("d\\link".to_owned()),
				name: Some("link".to_owned()),
				reason: "symlink",
			}
		);
	}
}
