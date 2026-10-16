//! The `*_for_test` seams that send the `bulk/*` wrappers with no operation of their own to the
//! server, so the live `bulk_tests` can drive each endpoint.

use filen_types::api::v3::bulk::FileOrDir;

use crate::{
	api::{
		self,
		v3::bulk::{ItemData, Sent, delete, metadata, r#move, run_each},
	},
	auth::Client,
	error::{Error, ErrorKind, MetadataWasNotDecryptedError},
	fs::{
		HasUUID,
		dir::{
			RemoteDirectory,
			meta::{DirectoryMeta, DirectoryMetaChanges},
			traits::HasDirMeta,
		},
		file::{
			RemoteFile,
			meta::{FileMeta, FileMetaChanges},
			traits::HasFileMeta,
		},
	},
};

/// Each item's result from `sent`, mapped by [`run_each`], which still resends an item answered
/// `internal_error` as its `single` call. A request the server did not run as a bulk request
/// ([`Sent::Unsupported`]) is an error instead of a run of single calls, so a server without
/// `endpoint` fails the test rather than passing through the single routes.
pub(crate) async fn answered_by_bulk<'a, I, T, F, Fut>(
	endpoint: &str,
	sent: Result<Sent<'a, I, T>, Error>,
	single: F,
) -> Result<Vec<Result<T, Error>>, Error>
where
	T: ItemData,
	F: FnMut(&'a I) -> Fut,
	Fut: Future<Output = Result<T, Error>>,
{
	match sent? {
		Sent::Unsupported(_) => Err(Error::custom(
			ErrorKind::Internal,
			format!("the server did not run {endpoint} as a bulk request"),
		)),
		sent => Ok(run_each(sent, single).await),
	}
}

impl Client {
	/// Deletes `items` permanently in one `bulk/delete/permanent` request under the drive lock and
	/// answers each item's result in the same order. Nothing splits `items` into chunks: the tests
	/// send fewer than the endpoint takes. Test code only.
	pub async fn delete_items_permanently_for_test(
		&self,
		items: &[delete::permanent::Item],
	) -> Result<Vec<Result<(), Error>>, Error> {
		let _lock = self.lock_drive().await?;
		answered_by_bulk(
			delete::permanent::ENDPOINT,
			delete::permanent::post(self.client(), &delete::permanent::Request { items }).await,
			|item| async move {
				match item {
					FileOrDir::File(request) => {
						api::v3::file::delete::permanent::post(self.client(), request).await
					}
					FileOrDir::Dir(request) => {
						api::v3::dir::delete::permanent::post(self.client(), request).await
					}
				}
			},
		)
		.await
	}

	/// Moves `items` in one `bulk/move` request under the drive lock and answers each item's result
	/// in the same order. Nothing splits `items` into chunks: the tests send fewer than the endpoint
	/// takes. Test code only.
	pub async fn move_items_for_test(
		&self,
		items: &[r#move::Item],
	) -> Result<Vec<Result<(), Error>>, Error> {
		let _lock = self.lock_drive().await?;
		answered_by_bulk(
			r#move::ENDPOINT,
			r#move::post(self.client(), &r#move::Request { items }).await,
			|item| async move {
				match item {
					FileOrDir::File(request) => {
						api::v3::file::r#move::post(self.client(), request).await
					}
					FileOrDir::Dir(request) => {
						api::v3::dir::r#move::post(self.client(), request).await
					}
				}
			},
		)
		.await
	}

	/// Sends `items` in one `bulk/metadata` request under the drive lock and answers each item's
	/// result in the same order. Nothing splits `items` into chunks: the tests send fewer than the
	/// endpoint takes. Test code only.
	pub async fn update_metadata_for_test(
		&self,
		items: &[metadata::Item<'_>],
	) -> Result<Vec<Result<(), Error>>, Error> {
		let _lock = self.lock_drive().await?;
		answered_by_bulk(
			metadata::ENDPOINT,
			metadata::post(self.client(), &metadata::Request { items }).await,
			|item| async move {
				match item {
					FileOrDir::File(request) => {
						api::v3::file::metadata::post(self.client(), request).await
					}
					FileOrDir::Dir(request) => {
						api::v3::dir::metadata::post(self.client(), request).await
					}
				}
			},
		)
		.await
	}

	/// The `bulk/metadata` (and `bulk/rename`) item that renames `item` to `name`, built exactly as
	/// [`Client::update_file_metadata`] and [`Client::update_dir_metadata`] build their requests
	/// for a name change. Test code only.
	pub async fn metadata_item_for_test(
		&self,
		item: FileOrDir<&RemoteFile, &RemoteDirectory>,
		name: &str,
	) -> Result<metadata::Item<'static>, Error> {
		match item {
			FileOrDir::File(file) => {
				let changes = FileMetaChanges::default().name(name)?;
				let FileMeta::Decoded(meta) = file.get_meta().borrow_with_changes(&changes)? else {
					return Err(MetadataWasNotDecryptedError.into());
				};
				Ok(FileOrDir::File(
					self.file_metadata_request(file.uuid(), &meta).await?,
				))
			}
			FileOrDir::Dir(dir) => {
				let changes = DirectoryMetaChanges::default().name(name)?;
				let DirectoryMeta::Decoded(meta) = dir.get_meta().borrow_with_changes(&changes)?
				else {
					return Err(MetadataWasNotDecryptedError.into());
				};
				Ok(FileOrDir::Dir(
					self.dir_metadata_request(dir.uuid(), &meta).await?,
				))
			}
		}
	}
}
