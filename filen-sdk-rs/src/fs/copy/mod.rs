//! Copying drive items: there is no server-side copy (items are end-to-end encrypted), so a
//! copy reads each decrypted source and writes a new encrypted item. See
//! [`Client::copy_items_to`](crate::auth::Client::copy_items_to).

mod backend;
mod client_impl;
pub(crate) mod control;
pub(crate) mod engine;
mod naming;
mod plan;
mod progress;
mod report;

pub use client_impl::{CopyOptions, CopyRequest, CopySource, CopySourceDir};
pub use control::JobControl;
pub use engine::CopyOutcome;
pub use plan::{PlanTotals, RenameReason, RenamedEntry, SkipReason, SkippedEntry};
pub use report::{
	ActiveFile, CopiedTopLevel, CopyCallback, CopyCounts, CopyEvent, CopyFailure, CopyPhase,
	CopyReport, CopyStage, CopyUpdate, FailedSource, FailureInfo, PlannedTopLevelItem,
	ScanProgress,
};
