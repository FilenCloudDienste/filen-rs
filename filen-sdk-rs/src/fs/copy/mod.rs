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

pub use crate::job::JobControl;
pub use client_impl::{CopyOptions, CopyRequest, CopySource, CopySourceDir};
pub use engine::CopyOutcome;
pub use plan::{PlanTotals, RenameReason, RenamedEntry, SkipReason, SkippedEntry};
pub use report::{
	ActiveFile, CopiedTopLevel, CopyCallback, CopyCounts, CopyEvent, CopyFailure, CopyPhase,
	CopyReport, CopyStage, CopyUpdate, FailedSource, FailureInfo, PlannedTopLevelItem,
	ScanProgress,
};
