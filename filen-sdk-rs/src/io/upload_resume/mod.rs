//! Journals of resumable uploads.
//!
//! An upload in progress sends its chunks under a file uuid and an upload key, and the server
//! keeps the chunks it acknowledged until the upload is finished or they expire. Another client
//! can finish the upload given the same uuid, keys and parent, but the server lists no chunks it
//! holds: the client's journal of which chunks were acknowledged is the only record of them.
//!
//! A journal is encrypted with the account's master keys before a store sees it, and is kept at
//! most [`JOURNAL_MAX_AGE`] from its first acknowledged chunk, after which the server may have
//! dropped the chunks.

#![warn(unreachable_pub, unused_qualifications)]

mod store;

use chrono::TimeDelta;

pub use store::{FileUploadJournalStore, SealedUploadJournal, UploadJournalId, UploadJournalStore};

/// How long after its first acknowledged chunk a journal is kept. The server keeps the chunks of
/// an unfinished upload at least this long: a probe saw them kept for 3 hours, the backend says
/// about a week, and 24 hours is what this assumes.
pub(crate) const JOURNAL_MAX_AGE: TimeDelta = TimeDelta::hours(24);
