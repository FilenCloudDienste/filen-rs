//! `sync --dry-run`: what a pass of each pair would do, applying nothing.

use std::{borrow::Cow, collections::HashMap};

use anyhow::Result;
use filen_sdk_rs::sync_engine::{
	PairId, PairRecord, PlanOutcome, PlannedAction, PlannedActionKind, PlannedTransfer, SyncEngine,
	UnsyncablePath,
};
use serde_json::{Value, json};

use super::{
	manage::{awaiting_listing, conflict_line, conflict_object, node_name},
	paused_message, unsyncable_notices,
};
use crate::ui::{self, UI};

/// Plans a pass of each of `pairs`, labelled by `labels`, and prints what it would do: the
/// actions, the deletions the delete guard would hold back and why, the conflicts it would leave
/// and the paths it cannot sync. With `--json`, one object per pair. A paused pair is said to be
/// paused instead, on stderr, as `sync` says it: `sync` runs no pass of it.
pub(super) async fn print_plans(
	ui: &mut UI,
	engine: &SyncEngine,
	pairs: &[PairRecord],
	labels: &HashMap<PairId, String>,
) -> Result<()> {
	for pair in pairs {
		// every pair of the run has a label; should one not, it is named by its id
		let label = labels.get(&pair.id).map_or_else(
			|| Cow::Owned(format!("sync pair {}", pair.id)),
			|label| Cow::Borrowed(label.as_str()),
		);
		if pair.paused {
			ui.print_failure(&paused_message(pair.id, label));
			continue;
		}
		let plan = awaiting_listing(async || engine.plan_pair(pair.id).await, ui)
			.await
			.map_err(|e| {
				UI::failure(&format!(
					"Failed to plan a pass of the sync pair {label}: {e}"
				))
			})?;
		if ui.json {
			ui.print(&plan_object(pair.id, &label, &plan).to_string());
		} else {
			for line in plan_lines(&label, &plan) {
				ui.print(&line);
			}
		}
	}
	if !ui.json {
		ui.print_muted(
			"This was a dry run: nothing was synced. The sync pairs stay registered, as `sync` \
			 registers them.",
		);
	}
	Ok(())
}

/// The lines that say what a pass of the pair labelled `label` would do, as `plan` says.
fn plan_lines(label: &str, plan: &PlanOutcome) -> Vec<String> {
	let mut lines = vec![format!("A pass of the sync pair {label} would:")];
	if let Some(reason) = plan.refused {
		lines.push(format!("  refuse to run: {reason}"));
		return lines;
	}
	lines.extend(
		plan.actions
			.iter()
			.map(|action| format!("  {}", action_text(action))),
	);
	if let Some(reason) = &plan.held_reason {
		lines.push(format!(
			"  hold back {} deletion(s): {reason}",
			plan.held.len()
		));
		lines.extend(
			plan.held
				.iter()
				.map(|action| format!("    held: {}", action_text(action))),
		);
	}
	lines.extend(
		plan.conflicts
			.iter()
			.map(|conflict| format!("  leave a conflict: {}", conflict_line(conflict))),
	);
	lines.extend(
		unsyncable_notices(&plan.unsyncable)
			.into_iter()
			.map(|notice| format!("  leave alone: {notice}")),
	);
	if lines.len() == 1 {
		lines.push("  do nothing".to_string());
	}
	lines
}

/// What `action` would do.
fn action_text(action: &PlannedAction) -> String {
	let path = &action.rel_path;
	let size = match action.size {
		Some(size) => format!(" ({})", ui::format_size(size)),
		None => String::new(),
	};
	let node = node_name(action.node);
	match &action.kind {
		PlannedActionKind::UploadFile => match action.transfer {
			Some(PlannedTransfer::Replace) => {
				format!("upload {path}{size}, replacing the remote file")
			}
			Some(PlannedTransfer::Create) | None => format!("upload {path}{size}"),
		},
		PlannedActionKind::DownloadFile => match action.transfer {
			Some(PlannedTransfer::Replace) => {
				format!("download {path}{size}, replacing the local file")
			}
			Some(PlannedTransfer::Create) | None => format!("download {path}{size}"),
		},
		PlannedActionKind::CreateRemoteDir => format!("create the remote directory {path}"),
		PlannedActionKind::CreateLocalDir => format!("create the local directory {path}"),
		PlannedActionKind::MoveRemote { to } => format!("move the remote {node} {path} to {to}"),
		PlannedActionKind::MoveLocal { to } => format!("move the local {node} {path} to {to}"),
		PlannedActionKind::TrashRemote => format!("trash the remote {node} {path}"),
		PlannedActionKind::DeleteLocal => {
			format!("move the local {node} {path} into .filen-sync-trash")
		}
		PlannedActionKind::AdoptBaseline => format!("record {path} as already in sync"),
	}
}

/// What a pass of `pair`, labelled `label`, would do, as `plan` says, for `--json`.
fn plan_object(pair: PairId, label: &str, plan: &PlanOutcome) -> Value {
	json!({
		"pair": pair,
		"label": label,
		"event": "plan",
		"refused": plan.refused.map(|reason| reason.to_string()),
		"actions": plan.actions.iter().map(action_object).collect::<Vec<_>>(),
		"held": plan.held.iter().map(action_object).collect::<Vec<_>>(),
		"held_reason": plan.held_reason.as_ref().map(ToString::to_string),
		"conflicts": plan.conflicts.iter().map(conflict_object).collect::<Vec<_>>(),
		"unsyncable": plan.unsyncable.iter().map(unsyncable_object).collect::<Vec<_>>(),
	})
}

fn action_object(action: &PlannedAction) -> Value {
	let (name, to) = match &action.kind {
		PlannedActionKind::UploadFile => ("upload", None),
		PlannedActionKind::DownloadFile => ("download", None),
		PlannedActionKind::CreateRemoteDir => ("create_remote_dir", None),
		PlannedActionKind::CreateLocalDir => ("create_local_dir", None),
		PlannedActionKind::MoveRemote { to } => ("move_remote", Some(to)),
		PlannedActionKind::MoveLocal { to } => ("move_local", Some(to)),
		PlannedActionKind::TrashRemote => ("trash_remote", None),
		PlannedActionKind::DeleteLocal => ("delete_local", None),
		PlannedActionKind::AdoptBaseline => ("adopt_baseline", None),
	};
	json!({
		"action": name,
		"path": action.rel_path,
		"to": to,
		"node": node_name(action.node),
		"size": action.size,
		"replaces": action.transfer.map(|transfer| transfer == PlannedTransfer::Replace),
	})
}

fn unsyncable_object(path: &UnsyncablePath) -> Value {
	json!({"path": path.rel_path, "reason": path.reason.to_string()})
}

#[cfg(test)]
mod tests {
	use filen_sdk_rs::sync_engine::{
		GuardReason, PlanOutcome, PlannedAction, PlannedActionKind, PlannedConflict,
		PlannedNodeKind, PlannedTransfer,
	};
	use serde_json::json;

	use super::{plan_lines, plan_object};

	fn upload(path: &str) -> PlannedAction {
		PlannedAction {
			kind: PlannedActionKind::UploadFile,
			rel_path: path.to_string(),
			node: PlannedNodeKind::File,
			size: Some(12),
			transfer: Some(PlannedTransfer::Create),
		}
	}

	fn trash(path: &str) -> PlannedAction {
		PlannedAction {
			kind: PlannedActionKind::TrashRemote,
			rel_path: path.to_string(),
			node: PlannedNodeKind::Dir,
			size: None,
			transfer: None,
		}
	}

	fn a_plan() -> PlanOutcome {
		PlanOutcome {
			actions: vec![upload("a.txt")],
			held: vec![trash("old")],
			held_reason: Some(GuardReason::ExceededThreshold {
				deletions: 1,
				limit: 0,
			}),
			pass_token: Some("t".to_string()),
			conflicts: vec![PlannedConflict {
				rel_path: "c.txt".to_string(),
				local: Some(PlannedNodeKind::File),
				remote: None,
			}],
			..PlanOutcome::default()
		}
	}

	#[test]
	fn a_plan_names_its_actions_held_deletions_and_conflicts() {
		assert_eq!(
			plan_lines("L", &a_plan()),
			[
				"A pass of the sync pair L would:",
				"  upload a.txt (12 B)",
				"  hold back 1 deletion(s): 1 deletion(s) exceed the limit of 0",
				"    held: trash the remote directory old",
				"  leave a conflict: c.txt: local file, remote deleted",
			]
		);
	}

	#[test]
	fn an_empty_plan_says_it_would_do_nothing() {
		assert_eq!(
			plan_lines("L", &PlanOutcome::default()),
			["A pass of the sync pair L would:", "  do nothing"]
		);
	}

	#[test]
	fn a_plan_is_one_json_object() {
		assert_eq!(
			plan_object(1, "L", &a_plan()),
			json!({
				"pair": 1,
				"label": "L",
				"event": "plan",
				"refused": null,
				"actions": [{
					"action": "upload", "path": "a.txt", "to": null, "node": "file", "size": 12,
					"replaces": false,
				}],
				"held": [{
					"action": "trash_remote", "path": "old", "to": null, "node": "directory",
					"size": null, "replaces": null,
				}],
				"held_reason": "1 deletion(s) exceed the limit of 0",
				"conflicts": [{"path": "c.txt", "local": "file", "remote": null}],
				"unsyncable": [],
			})
		);
	}
}
