//! Helpers for the live tests of resumable uploads.

use std::path::Path;

use filen_sdk_rs::{
	Error,
	consts::CHUNK_SIZE_U64,
	io::{FileUploadJournalStore, SealedUploadJournal, UploadJournalId, UploadJournalStore},
	util::MaybeSendBoxFuture,
};
use tokio::sync::Notify;

/// A journal store over a directory that wakes `saved` each time it saves a journal, so a test can
/// drop an upload once its journal is on disk, and, when `keep_on_remove` is set, keeps every
/// journal it is asked to remove, as a process killed right after `upload/done` would.
#[derive(Debug)]
pub struct NotifyingJournalStore {
	inner: FileUploadJournalStore,
	pub saved: Notify,
	keep_on_remove: bool,
}

impl NotifyingJournalStore {
	pub async fn open(dir: &Path, keep_on_remove: bool) -> Self {
		Self {
			inner: FileUploadJournalStore::open(dir.to_owned())
				.await
				.expect("opening the upload journal store"),
			saved: Notify::new(),
			keep_on_remove,
		}
	}
}

impl UploadJournalStore for NotifyingJournalStore {
	fn load<'a>(
		&'a self,
		id: &'a UploadJournalId,
	) -> MaybeSendBoxFuture<'a, Result<Option<SealedUploadJournal>, Error>> {
		self.inner.load(id)
	}

	fn save<'a>(
		&'a self,
		id: &'a UploadJournalId,
		journal: &'a SealedUploadJournal,
	) -> MaybeSendBoxFuture<'a, Result<(), Error>> {
		Box::pin(async move {
			self.inner.save(id, journal).await?;
			self.saved.notify_one();
			Ok(())
		})
	}

	fn remove<'a>(&'a self, id: &'a UploadJournalId) -> MaybeSendBoxFuture<'a, Result<(), Error>> {
		if self.keep_on_remove {
			return Box::pin(async { Ok(()) });
		}
		self.inner.remove(id)
	}
}

/// 24 chunks and 7 bytes that differ from chunk to chunk and with `seed`: enough chunks for an
/// upload to be journaled, and more than the upload acknowledges before it first saves its
/// journal, so that an upload dropped at that save still has chunks to send.
pub fn resumable_upload_contents(seed: u64) -> Vec<u8> {
	(0..24 * CHUNK_SIZE_U64 + 7)
		.map(|i| u8::try_from((i * 31 + i / 7919 + seed) % 251).expect("a value below 251"))
		.collect()
}
