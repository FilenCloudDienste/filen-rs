//! Copying drive items: there is no server-side copy (items are end-to-end encrypted), so a
//! copy reads each decrypted source and writes a new encrypted item. See
//! [`Client::copy_items_to`](crate::auth::Client::copy_items_to).

mod client_impl;
mod engine;
#[cfg(any(feature = "uniffi", feature = "wasm-full"))]
mod js_impl;
mod report;

pub use crate::{
	fs::drive_job::{
		counts::ItemCounts,
		listing::{ItemSource, ItemSourceDir, ScanProgress},
		plan::{PlanTotals, RenameReason, RenamedEntry, SkipReason, SkippedEntry},
	},
	job::{JobControl, JobController},
};
pub use client_impl::{CopyConfig, CopyRequest};
pub use report::{
	ActiveFile, CopiedTopLevel, CopyCallback, CopyEvent, CopyPhase, CopyStage, CopyUpdate,
	FailureInfo, PlannedTopLevelItem, RunState,
};

// The report types are generic over how a failed directory is addressed again, which keeps the
// planner and engine independent of the client; callers only ever see them with ItemSourceDir.
pub type CopyReport = report::CopyReport<ItemSourceDir>;
pub type CopyFailed = report::CopyFailed<ItemSourceDir>;
pub type CopyFailure = report::CopyFailure<ItemSourceDir>;
pub type FailedSource = crate::fs::drive_job::listing::FailedSource<ItemSourceDir>;
