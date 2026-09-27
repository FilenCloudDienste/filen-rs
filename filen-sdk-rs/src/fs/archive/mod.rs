//! Compressing drive items into an archive and extracting an archive into the drive. Nothing can
//! run on the server (items are end-to-end encrypted), so an archive's codec runs in the SDK:
//! entries are downloaded and decrypted, (de)compressed, then encrypted and uploaded.

// The archive modules land one tested piece at a time, ahead of the extract engine that ties
// them together; until then their items have no production caller.
#![allow(dead_code)]

pub(crate) mod decode;
pub(crate) mod entry_path;
pub(crate) mod format;
pub(crate) mod limits;
pub(crate) mod tar_iter;
