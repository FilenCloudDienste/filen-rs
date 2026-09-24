//! Copying drive items: there is no server-side copy (items are end-to-end encrypted), so a
//! copy reads each decrypted source and writes a new encrypted item. See
//! [`Client::copy_items_to`](crate::auth::Client::copy_items_to).

mod backend;
mod client_impl;
mod engine;
#[cfg(any(feature = "uniffi", feature = "wasm-full"))]
mod js_impl;
mod naming;
mod plan;
mod progress;
mod report;

pub use crate::job::{JobControl, JobController};
pub use client_impl::{CopyConfig, CopyRequest, CopySource, CopySourceDir};
pub use plan::{PlanTotals, RenameReason, RenamedEntry, SkipReason, SkippedEntry};
pub use report::{
	ActiveFile, CopiedTopLevel, CopyCallback, CopyCounts, CopyEvent, CopyPhase, CopyStage,
	CopyUpdate, FailureInfo, PlannedTopLevelItem, RunState, ScanProgress,
};

// The report types are generic over how a failed directory is addressed again, which keeps the
// planner and engine independent of the client; callers only ever see them with CopySourceDir.
pub type CopyReport = report::CopyReport<CopySourceDir>;
pub type CopyFailed = report::CopyFailed<CopySourceDir>;
pub type CopyFailure = report::CopyFailure<CopySourceDir>;
pub type FailedSource = report::FailedSource<CopySourceDir>;
