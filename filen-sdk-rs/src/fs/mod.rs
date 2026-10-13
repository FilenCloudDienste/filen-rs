#[cfg(feature = "archive")]
pub mod archive;
// Unused for now outside its tests: the sync engine, which will send its directory creates and
// trashes through it, is not built on this branch.
#[allow(dead_code)]
pub(crate) mod bulk;
pub mod cache;
pub mod categories;
pub mod client_impl;
pub mod copy;
pub mod dir;
pub(crate) mod drive_job;
pub mod enums;
pub mod file;
#[cfg(any(feature = "wasm-full", feature = "uniffi"))]
pub mod js_impl;
pub(crate) mod meta_recovery;
pub mod name;
pub mod traits;
pub mod zip;

// Test seam: what the live tests pass to and get back from `Client::create_dirs_for_test`. Gated
// behind the `test-seams` feature, which production builds never enable.
#[cfg(feature = "test-seams")]
pub use bulk::create_dirs::{CreateDirsError, CreatedDir, NewDir, NewDirParent};
pub use enums::*;
pub use traits::*;
