//! Zip archives, read and written by the SDK itself: the central directory is parsed within the
//! index budget, entries are decrypted and decompressed one at a time, and archives are written
//! front to back with data descriptors, so neither side ever needs the whole archive at once.

pub(crate) mod cp437;
pub(crate) mod crypto;
pub(crate) mod read;
pub(crate) mod write;
