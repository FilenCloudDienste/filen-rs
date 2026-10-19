use uuid::Uuid;

use super::{
	super::{
		baseline::BaselineEntry,
		plan::{RemoteNode, SyncAction},
		rows::Baseline,
		scan::LocalNode,
		side::Side,
		stage::{Screen, Stage, StagedPlan},
	},
	PassStart, PlannedEntry, StartInputs,
};

/// A pass that held nothing at any path.
struct Unread;

impl StartInputs for Unread {
	fn local(&mut self, _: &str) -> Option<Box<LocalNode>> {
		None
	}

	fn remote(&mut self, _: &str) -> Option<Box<RemoteNode>> {
		None
	}

	fn base(&mut self, _: &str) -> Option<Box<BaselineEntry>> {
		None
	}

	fn base_from(&mut self, _: &str) -> Option<Box<BaselineEntry>> {
		None
	}

	fn parent_uuid(&mut self, _: &str) -> Option<Uuid> {
		None
	}
}

impl PassStart {
	/// What `action`'s kind reads, with nothing read at any of it.
	pub(in crate::sync_engine) fn bare(action: &SyncAction) -> Self {
		Self::of(action, &mut Unread)
	}
}

impl PlannedEntry {
	/// `actions`, a plan as it stands, staged as [`stage`](Self::stage) stages a screened one.
	pub(in crate::sync_engine) fn stage_actions(
		actions: &[SyncAction],
		baseline: Baseline,
		local: Side<LocalNode>,
		remote: Side<RemoteNode>,
		stage: Stage,
	) -> rusqlite::Result<StagedPlan> {
		Self::stage(Screen::new(stage, &[], actions)?, baseline, local, remote)
	}

	/// `action`, with nothing read for it (see [`PassStart::bare`]).
	pub(in crate::sync_engine) fn bare(action: SyncAction) -> Self {
		Self {
			start: PassStart::bare(&action),
			action,
		}
	}
}
