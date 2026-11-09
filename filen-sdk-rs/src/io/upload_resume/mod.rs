//! Journals of resumable uploads.
//!
//! An upload in progress sends its chunks under a file uuid and an upload key, and the server
//! keeps the chunks it acknowledged until the upload is finished or they expire. Another client
//! can finish the upload given the same uuid, keys and parent, but the server lists no chunks it
//! holds: the client's journal of which chunks were acknowledged is the only record of them.
//!
//! A journal is encrypted with the account's master keys before a store sees it, and is kept at
//! most [`SERVER_RETENTION`] from its first acknowledged chunk, after which the server may have
//! dropped the chunks.
//!
//! An upload is journaled only when its source has at least [`MIN_JOURNALED_CHUNKS`] chunks, and
//! a chunk is recorded only once its response arrived. A journal is dropped, and the upload
//! started over under a new uuid and keys, when the source's size, modification time or inode
//! changed, or when what is left might not be sent before the server drops the chunks
//! ([`RESUME_MARGIN`]). It is removed once the upload is finished.

#![warn(unreachable_pub, unused_qualifications)]

mod backend;
mod engine;
mod policy;
mod record;
mod store;
#[cfg(test)]
mod tests;

use std::time::Duration;

use chrono::TimeDelta;

pub(crate) use backend::ClientResumeBackend;
pub(crate) use engine::{ResumeCallbacks, ResumeSource, upload_resuming};
pub use store::{FileUploadJournalStore, SealedUploadJournal, UploadJournalId, UploadJournalStore};

/// Sources of fewer chunks than this skip the journal: sending them again is cheap.
pub(crate) const MIN_JOURNALED_CHUNKS: u64 = 8;

/// How long the server is assumed to keep the chunks of an unfinished upload, counted from its
/// first acknowledged chunk. A probe saw them kept for 3 hours, the backend says about a week, and
/// 24 hours is what this assumes. Stores purge journals older than this.
pub(crate) const SERVER_RETENTION: TimeDelta = TimeDelta::hours(24);

/// How much of [`SERVER_RETENTION`] an upload leaves unused: one is resumed only when its age plus
/// the estimated time to finish it is less than the retention minus this.
pub(crate) const RESUME_MARGIN: TimeDelta = TimeDelta::hours(2);

/// After how many acknowledged chunks the journal is saved, at the latest.
pub(crate) const JOURNAL_FLUSH_CHUNKS: u64 = 16;

/// After how long since the last save the journal is saved again, once a chunk was acknowledged.
/// A journal that lags only means some chunks are sent again.
pub(crate) const JOURNAL_FLUSH_INTERVAL: Duration = Duration::from_secs(5);

/// How much slower than the rate measured so far the rest of an upload is assumed to go when
/// deciding whether it can still finish in time.
pub(crate) const ESTIMATE_SAFETY_FACTOR: u32 = 2;
