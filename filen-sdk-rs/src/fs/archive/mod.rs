//! Compressing drive items into an archive and extracting an archive into the drive. Nothing can
//! run on the server (items are end-to-end encrypted), so an archive's codec runs in the SDK:
//! entries are downloaded and decrypted, (de)compressed, then encrypted and uploaded.

pub mod compress;
pub(crate) mod config;
pub(crate) mod decode;
pub(crate) mod encode;
pub(crate) mod entry_path;
pub mod extract;
pub(crate) mod format;
pub(crate) mod limits;
pub(crate) mod names;
pub(crate) mod tar_iter;
pub(crate) mod worker;

pub use config::ArchiveConfig;
