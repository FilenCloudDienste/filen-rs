//! `sync --json`: one JSON object per line for each event, which a program can read as it comes.

use filen_sdk_rs::sync_engine::{PairId, SyncEvent, TransferDirection};
use serde_json::{Map, Value, json};

use super::{sync_pairs::sync_mode_str, transfers::Transfers};

/// `event` of `pair`, labelled `label`, as one line of JSON. `transfers` gives the bytes the pass
/// has moved, which a progress event and the pass's summary carry.
pub(super) fn event_line(
	pair: PairId,
	label: &str,
	event: &SyncEvent,
	transfers: &Transfers,
) -> String {
	let (name, fields) = event_fields(pair, event, transfers);
	line(pair, label, name, fields)
}

/// A line the flags settled, as one line of JSON.
pub(super) fn approved_line(pair: PairId, label: &str, message: &str) -> String {
	line(pair, label, "approved", json!({ "message": message }))
}

/// `{"pair":..,"label":..,"event":..}` with `fields` added.
fn line(pair: PairId, label: &str, name: &str, fields: Value) -> String {
	let mut object = Map::new();
	object.insert("pair".to_owned(), json!(pair));
	object.insert("label".to_owned(), json!(label));
	object.insert("event".to_owned(), json!(name));
	if let Value::Object(fields) = fields {
		object.extend(fields);
	}
	Value::Object(object).to_string()
}

fn direction_str(direction: TransferDirection) -> &'static str {
	match direction {
		TransferDirection::Upload => "upload",
		TransferDirection::Download => "download",
	}
}

/// The event's name and its fields.
fn event_fields(pair: PairId, event: &SyncEvent, transfers: &Transfers) -> (&'static str, Value) {
	let moved = |direction| transfers.moved(pair, direction);
	match event {
		SyncEvent::PassStarted { mode } => {
			("pass_started", json!({ "mode": sync_mode_str(*mode) }))
		}
		SyncEvent::Refused { reason } => ("refused", json!({ "reason": reason })),
		SyncEvent::Planned { actions } => ("planned", json!({ "actions": actions })),
		SyncEvent::Conflict { rel_path } => ("conflict", json!({ "path": rel_path })),
		SyncEvent::DeletionsHeld {
			count,
			reason,
			pass_token: _,
		} => (
			"deletions_held",
			json!({ "count": count, "reason": reason }),
		),
		// The engine reports a transfer once it has landed.
		SyncEvent::Uploading { rel_path } => ("uploaded", json!({ "path": rel_path })),
		SyncEvent::Downloading { rel_path } => ("downloaded", json!({ "path": rel_path })),
		SyncEvent::Progress {
			rel_path,
			direction,
			bytes,
			total,
		} => (
			"progress",
			json!({
				"path": rel_path,
				"direction": direction_str(*direction),
				"bytes": bytes,
				"total": total,
				"pass_uploaded_bytes": moved(TransferDirection::Upload),
				"pass_downloaded_bytes": moved(TransferDirection::Download),
			}),
		),
		// The other actions are reported before they run.
		SyncEvent::CreatingRemoteDir { rel_path } => {
			("creating_remote_dir", json!({ "path": rel_path }))
		}
		SyncEvent::CreatingLocalDir { rel_path } => {
			("creating_local_dir", json!({ "path": rel_path }))
		}
		SyncEvent::TrashingRemote { rel_path } => ("trashing_remote", json!({ "path": rel_path })),
		SyncEvent::DeletingLocal { rel_path } => ("deleting_local", json!({ "path": rel_path })),
		SyncEvent::MovingRemote { from, to } => {
			("moving_remote", json!({ "from": from, "to": to }))
		}
		SyncEvent::MovingLocal { from, to } => ("moving_local", json!({ "from": from, "to": to })),
		SyncEvent::Quarantined { rel_path, bin_path } => (
			"moved_aside",
			json!({ "path": rel_path, "to": bin_path.display().to_string() }),
		),
		SyncEvent::ActionFailed { rel_path, error } => {
			("action_failed", json!({ "path": rel_path, "error": error }))
		}
		SyncEvent::Interrupted { actions } => ("interrupted", json!({ "actions": actions })),
		SyncEvent::AdoptedBaseline { rel_path } => ("already_in_sync", json!({ "path": rel_path })),
		SyncEvent::PassCompleted { report } => (
			"pass_completed",
			json!({
				"uploaded": report.uploaded,
				"downloaded": report.downloaded,
				"remote_dirs_created": report.remote_dirs_created,
				"local_dirs_created": report.local_dirs_created,
				"remotely_trashed": report.remotely_trashed,
				"locally_deleted": report.locally_deleted,
				"moved_remote": report.moved_remote,
				"moved_local": report.moved_local,
				"uploaded_bytes": moved(TransferDirection::Upload),
				"downloaded_bytes": moved(TransferDirection::Download),
				"conflicts": report.conflict_paths().collect::<Vec<_>>(),
				"held_deletions": report.held_deletions(),
				"errors": report.errors,
				"unsyncable": report
					.unsyncable
					.iter()
					.map(|path| json!({
						"path": path.rel_path,
						"reason": path.reason.to_string(),
					}))
					.collect::<Vec<_>>(),
				"moved_aside": report.quarantined,
				"interrupted": report.interrupted,
				"paused": report.paused,
			}),
		),
		SyncEvent::PassFailed { error, kind: _ } => ("pass_failed", json!({ "error": error })),
		SyncEvent::AwaitingRemote => ("awaiting_remote", json!({})),
		SyncEvent::Scanning { progress } => (
			"scanning",
			json!({
				"entries": progress.entries,
				"files_to_hash": progress.files_to_hash,
				"bytes_to_hash": progress.bytes_to_hash,
				"files_hashed": progress.files_hashed,
				"bytes_hashed": progress.bytes_hashed,
			}),
		),
	}
}

#[cfg(test)]
mod tests {
	use filen_sdk_rs::sync_engine::{
		SyncEvent, SyncMode, SyncReport, TransferDirection, UnsyncablePath, UnsyncableReason,
	};
	use serde_json::{Value, json};

	use super::{approved_line, event_line};
	use crate::commands::sync_cmd::transfers::Transfers;

	/// The line `event_line` gives for `event` of pair 7 labelled `L`, after `transfers`.
	fn parsed(event: &SyncEvent, transfers: &Transfers) -> Value {
		let line = event_line(7, "L", event, transfers);
		assert!(!line.contains('\n'), "{line}");
		serde_json::from_str(&line).unwrap()
	}

	#[test]
	fn an_event_is_one_line_naming_its_pair() {
		assert_eq!(
			parsed(
				&SyncEvent::PassStarted {
					mode: SyncMode::LocalBackup
				},
				&Transfers::default()
			),
			json!({"pair": 7, "label": "L", "event": "pass_started", "mode": "localBackup"})
		);
		assert_eq!(
			parsed(
				&SyncEvent::Uploading {
					rel_path: "a/b.txt".to_string()
				},
				&Transfers::default()
			),
			json!({"pair": 7, "label": "L", "event": "uploaded", "path": "a/b.txt"})
		);
	}

	#[test]
	fn a_progress_event_carries_what_the_pass_has_moved_each_way() {
		let mut transfers = Transfers::default();
		let upload = |rel_path: &str, bytes| SyncEvent::Progress {
			rel_path: rel_path.to_string(),
			direction: TransferDirection::Upload,
			bytes,
			total: 100,
		};
		transfers.note(7, &upload("a", 100));
		let event = upload("b", 30);
		transfers.note(7, &event);

		assert_eq!(
			parsed(&event, &transfers),
			json!({
				"pair": 7,
				"label": "L",
				"event": "progress",
				"path": "b",
				"direction": "upload",
				"bytes": 30,
				"total": 100,
				"pass_uploaded_bytes": 130,
				"pass_downloaded_bytes": 0,
			})
		);
	}

	#[test]
	fn a_completed_pass_carries_its_report() {
		// some of its fields are the engine's own, so it is filled in rather than built
		let mut report = SyncReport::default();
		report.uploaded = 2;
		report.downloaded = 1;
		report.errors = vec!["a: denied".to_string()];
		report.unsyncable = vec![UnsyncablePath {
			rel_path: "nul".to_string(),
			reason: UnsyncableReason::InvalidName {
				detail: "reserved".to_string(),
			},
		}];
		report.quarantined = vec!["c".to_string()];

		assert_eq!(
			parsed(&SyncEvent::PassCompleted { report }, &Transfers::default()),
			json!({
				"pair": 7,
				"label": "L",
				"event": "pass_completed",
				"uploaded": 2,
				"downloaded": 1,
				"remote_dirs_created": 0,
				"local_dirs_created": 0,
				"remotely_trashed": 0,
				"locally_deleted": 0,
				"moved_remote": 0,
				"moved_local": 0,
				"uploaded_bytes": 0,
				"downloaded_bytes": 0,
				"conflicts": [],
				"held_deletions": 0,
				"errors": ["a: denied"],
				"unsyncable": [{
					"path": "nul",
					"reason": "the remote would reject this name: reserved",
				}],
				"moved_aside": ["c"],
				"interrupted": 0,
				"paused": false,
			})
		);
	}

	#[test]
	fn a_held_batch_of_deletions_leaves_its_token_out() {
		assert_eq!(
			parsed(
				&SyncEvent::DeletionsHeld {
					count: 3,
					reason: "many".to_string(),
					pass_token: "token-1".to_string(),
				},
				&Transfers::default()
			),
			json!({"pair": 7, "label": "L", "event": "deletions_held", "count": 3, "reason": "many"})
		);
	}

	#[test]
	fn an_approval_is_one_line() {
		assert_eq!(
			serde_json::from_str::<Value>(&approved_line(7, "L", "Approved 3 deletions")).unwrap(),
			json!({"pair": 7, "label": "L", "event": "approved", "message": "Approved 3 deletions"})
		);
	}
}
