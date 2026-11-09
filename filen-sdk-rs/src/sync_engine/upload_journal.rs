//! The journals of a pair's unfinished uploads, kept in the pair's baseline DB (`upload_journal`,
//! see the baseline's schema), so that an upload a pass did not finish (it was paused, cancelled
//! or the engine went down) is continued by a later pass, of this engine or of the next one to
//! open the DB.

use std::fmt;

use chrono::DateTime;

use super::{
	baseline::PairId,
	engine::{SharedStore, db_error, locked},
};
use crate::{
	Error, ErrorKind,
	io::{SealedUploadJournal, UploadJournalId, UploadJournalStore},
	util::MaybeSendBoxFuture,
};

/// The [`UploadJournalStore`] of one pair's uploads, over its baseline connection.
///
/// Each call is one single-row statement, so it takes the store on the caller's thread as every
/// other single-row write does (see [`SharedStore`]); the guard is never held across an await.
pub(super) struct PairUploadJournal {
	store: SharedStore,
	pair: PairId,
}

impl PairUploadJournal {
	pub(super) fn new(store: SharedStore, pair: PairId) -> Self {
		Self { store, pair }
	}
}

impl fmt::Debug for PairUploadJournal {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("PairUploadJournal")
			.field("pair", &self.pair)
			.finish_non_exhaustive()
	}
}

impl UploadJournalStore for PairUploadJournal {
	fn load<'a>(
		&'a self,
		id: &'a UploadJournalId,
	) -> MaybeSendBoxFuture<'a, Result<Option<SealedUploadJournal>, Error>> {
		Box::pin(async move {
			let row = locked(&self.store)
				.upload_journal(self.pair, id.as_bytes())
				.map_err(|e| db_error(e, "reading an upload journal"))?;
			let Some((started_at, sealed)) = row else {
				return Ok(None);
			};
			let started_at = DateTime::from_timestamp_millis(started_at).ok_or_else(|| {
				Error::custom(
					ErrorKind::Conversion,
					format!("upload journal {id:?} started at {started_at} ms, out of range"),
				)
			})?;
			Ok(Some(SealedUploadJournal::new(started_at, sealed)))
		})
	}

	fn save<'a>(
		&'a self,
		id: &'a UploadJournalId,
		journal: &'a SealedUploadJournal,
	) -> MaybeSendBoxFuture<'a, Result<(), Error>> {
		Box::pin(async move {
			locked(&self.store)
				.save_upload_journal(
					self.pair,
					id.as_bytes(),
					journal.started_at().timestamp_millis(),
					journal.sealed(),
				)
				.map_err(|e| db_error(e, "saving an upload journal"))
		})
	}

	fn remove<'a>(&'a self, id: &'a UploadJournalId) -> MaybeSendBoxFuture<'a, Result<(), Error>> {
		Box::pin(async move {
			locked(&self.store)
				.remove_upload_journal(self.pair, id.as_bytes())
				.map_err(|e| db_error(e, "removing an upload journal"))
		})
	}
}

#[cfg(test)]
mod tests {
	use std::{
		path::Path,
		sync::{Arc, Mutex},
	};

	use chrono::{TimeZone as _, Utc};
	use filen_types::fs::Uuid;

	use super::*;
	use crate::sync_engine::{baseline::BaselineStore, mode::SyncMode};

	#[tokio::test]
	async fn the_pair_journal_loads_what_it_saved_and_misses_another_pairs() {
		let store = BaselineStore::open_in_memory().unwrap();
		let (pair, _) = store
			.create_pair("/a", Uuid::from_u128(1), SyncMode::TwoWay)
			.unwrap();
		let (other, _) = store
			.create_pair("/b", Uuid::from_u128(2), SyncMode::TwoWay)
			.unwrap();
		let store = Arc::new(Mutex::new(store));
		let journal = PairUploadJournal::new(Arc::clone(&store), pair);
		let other_journal = PairUploadJournal::new(store, other);
		let id = UploadJournalId::new(Path::new("/a/f.bin"), Uuid::from_u128(3), "f.bin");
		let sealed = SealedUploadJournal::new(
			Utc.timestamp_millis_opt(1_800_000_000_123).unwrap(),
			"sealed".to_owned(),
		);

		journal.save(&id, &sealed).await.unwrap();

		assert_eq!(journal.load(&id).await.unwrap(), Some(sealed));
		assert_eq!(other_journal.load(&id).await.unwrap(), None);
		journal.remove(&id).await.unwrap();
		assert_eq!(journal.load(&id).await.unwrap(), None);
	}
}
