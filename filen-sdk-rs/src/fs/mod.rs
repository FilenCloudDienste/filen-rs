// Needs a thread it may block for an archive's codec, which the service-worker build (no atomics)
// does not have.
#[cfg(any(
	not(all(target_family = "wasm", target_os = "unknown")),
	feature = "wasm-full"
))]
pub mod archive;
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

pub use enums::*;
pub use traits::*;
