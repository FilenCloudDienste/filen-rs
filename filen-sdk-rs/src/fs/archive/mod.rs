//! Compressing drive items into an archive and extracting an archive into the drive. Nothing can
//! run on the server (items are end-to-end encrypted), so an archive's codec runs in the SDK:
//! entries are downloaded and decrypted, (de)compressed, then encrypted and uploaded.
//!
//! The vocabulary the archive jobs share with copying (their control, sources, plan records,
//! counts and run state) is exported from [`fs::copy`](crate::fs::copy), where it first shipped.

#[cfg(test)]
pub(crate) mod alloc_meter;
pub mod compress;
pub(crate) mod config;
pub(crate) mod decode;
pub(crate) mod dispose;
pub(crate) mod encode;
pub(crate) mod entry_path;
pub mod extract;
pub(crate) mod format;
pub(crate) mod hash;
pub(crate) mod input;
#[cfg(any(feature = "uniffi", feature = "wasm-full"))]
mod js_impl;
pub(crate) mod limits;
pub(crate) mod names;
pub(crate) mod password;
pub(crate) mod sevenz;
pub(crate) mod tar_iter;
#[cfg(test)]
pub(crate) mod test_support;
pub(crate) mod worker;
pub(crate) mod zip;

pub use config::ArchiveConfig;
pub use dispose::{DisposalOutcome, KeptReason, SourceDisposal, SourceDisposition};
pub use password::ArchivePassword;
pub use sevenz::write::{SevenZEncryption, SevenZMethod};
pub use zip::{crypto::AesStrength, write::ZipMethod};
