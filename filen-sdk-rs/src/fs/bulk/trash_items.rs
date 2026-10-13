use std::sync::Arc;

use filen_types::api::v3::bulk::MAX_WRITE_ITEMS;

use super::Backend;
use crate::{
	api::v3::bulk::{run_each, trash::Item},
	auth::Client,
	error::Error,
};

impl Client {
	/// Moves `items` to the trash under one hold of the drive lock, in `bulk/trash` requests of at
	/// most [`MAX_WRITE_ITEMS`] items in list order, and answers each item's result in the same
	/// order, as its single call would: a missing or already trashed item and the default folder
	/// are `Ok`.
	///
	/// Pass only the topmost items, never their descendants: the server trashes a folder's
	/// contents after the folder, so a descendant sent as its own item usually becomes a trash
	/// entry of its own, with its own events.
	///
	/// The outer error is a lock failure. A request that fails as a whole fails each of its items
	/// with that error, and some unknown prefix of them may have been trashed.
	pub(crate) async fn trash_items(
		&self,
		items: &[Item],
	) -> Result<Vec<Result<(), Error>>, Error> {
		let _lock = self.lock_drive().await?;
		Ok(trash_items(self, items).await)
	}
}

/// [`Client::trash_items`] without the lock, through `backend`.
pub(super) async fn trash_items(backend: &impl Backend, items: &[Item]) -> Vec<Result<(), Error>> {
	let mut results = Vec::with_capacity(items.len());
	for chunk in items.chunks(MAX_WRITE_ITEMS) {
		match backend.bulk_trash(chunk).await {
			Ok(sent) => results.extend(run_each(sent, |item| backend.trash_one(item)).await),
			Err(e) => {
				let e = Arc::new(e);
				results.extend(chunk.iter().map(|_| Err(Error::unshared(Arc::clone(&e)))));
			}
		}
	}
	results
}
