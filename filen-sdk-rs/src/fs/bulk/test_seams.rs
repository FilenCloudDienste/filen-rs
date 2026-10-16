//! The `*_for_test` seams that send the `bulk/*` wrappers with no operation of their own, and the
//! endpoints with no wrapper, to the server, so the live `bulk_tests` can drive each endpoint.

use std::fmt::Debug;

use filen_types::{
	api::v3::bulk::{FileOrDir, MAX_READ_ITEMS},
	traits::CowHelpers,
};
use serde::{Serialize, de::DeserializeOwned};

use crate::{
	api::{
		self,
		v3::bulk::{
			Body, ItemData, Sent, answered, delete, dir::link, linked, metadata, r#move, run_each,
			send, share, shared,
		},
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
		Sent::Unsupported(_) => Err(not_run_as_bulk(endpoint)),
		sent => Ok(run_each(sent, single).await),
	}
}

fn not_run_as_bulk(endpoint: &str) -> Error {
	Error::custom(
		ErrorKind::Internal,
		format!("the server did not run {endpoint} as a bulk request"),
	)
}

impl Client {
	/// Posts `request`, whose items are `items`, to the bulk `endpoint` and answers each item's
	/// result in the same order, decoded as `T` by the rule its single route's answer goes
	/// through. For the endpoints no operation wraps. Test code only.
	///
	/// It takes no lock: the caller holds what the single route takes. The body is always sent as
	/// `Body::WithTopLevelFields`, so a whole-request `invalid_params` comes back as the
	/// server's own error: these endpoints have no single calls to fall back on, so an item
	/// answered `internal_error`, or a server without `endpoint`, is an error too.
	/// [`MAX_READ_ITEMS`] is only the local bound; the server enforces each endpoint's own cap.
	pub async fn post_bulk_for_test<I, T: DeserializeOwned>(
		&self,
		endpoint: &'static str,
		request: &(impl Serialize + Debug),
		items: &[I],
	) -> Result<Vec<Result<T, Error>>, Error> {
		match send(
			items,
			MAX_READ_ITEMS,
			Body::WithTopLevelFields,
			self.client().post_auth(endpoint.into(), request),
		)
		.await?
		{
			Sent::Answers(answers) => Ok(answers
				.into_iter()
				.map(|(_, answer)| {
					answered(answer).unwrap_or_else(|| {
						Err(Error::custom(
							ErrorKind::Internal,
							"answered internal_error",
						))
					})
				})
				.collect()),
			Sent::Unsupported(_) => Err(not_run_as_bulk(endpoint)),
		}
	}

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

	/// Shares `request`'s items with its receiver in one `bulk/share` request under the drive lock,
	/// as [`Client::share_dir`] holds it, and answers each item's result in the same order. Nothing
	/// splits the items into chunks: the tests send fewer than the endpoint takes. Test code only.
	pub async fn share_items_for_test(
		&self,
		request: &share::Request<'_>,
	) -> Result<Vec<Result<(), Error>>, Error> {
		let _lock = self.lock_drive().await?;
		answered_by_bulk(
			share::ENDPOINT,
			share::post(self.client(), request).await,
			|item| async move {
				api::v3::item::share::post(
					self.client(),
					&api::v3::item::share::Request {
						uuid: item.uuid,
						parent: item.parent,
						email: request.email.as_borrowed_cow(),
						r#type: item.r#type,
						metadata: item.metadata.as_borrowed_cow(),
					},
				)
				.await
			},
		)
		.await
	}

	/// Adds `request`'s items to its link in one `bulk/dir/link/add` request under the drive lock,
	/// as [`Client::public_link_dir`] holds it, and answers each item's result in the same order.
	/// Nothing splits the items into chunks: the tests send fewer than the endpoint takes. Test
	/// code only.
	pub async fn add_to_dir_link_for_test(
		&self,
		request: &link::add::Request<'_>,
	) -> Result<Vec<Result<(), Error>>, Error> {
		let _lock = self.lock_drive().await?;
		answered_by_bulk(
			link::add::ENDPOINT,
			link::add::post(self.client(), request).await,
			|item| async move {
				api::v3::dir::link::add::post(
					self.client(),
					&api::v3::dir::link::add::Request {
						uuid: item.uuid,
						parent: item.parent,
						link_uuid: request.link_uuid,
						r#type: item.r#type,
						metadata: item.metadata.as_borrowed_cow(),
						key: request.key.as_borrowed_cow(),
						expiration: request.expiration,
					},
				)
				.await
			},
		)
		.await
	}

	/// The public links each of `items` is in, from one `bulk/linked` request, in the same order.
	/// It takes no lock, as a read. Test code only.
	pub async fn linked_for_test(
		&self,
		items: &[linked::Item],
	) -> Result<Vec<Result<api::v3::item::linked::Response<'static>, Error>>, Error> {
		answered_by_bulk(
			linked::ENDPOINT,
			linked::post(self.client(), &linked::Request { items }).await,
			|item| api::v3::item::linked::post(self.client(), item),
		)
		.await
	}

	/// The users each of `items` is shared with, from one `bulk/shared` request, in the same order.
	/// It takes no lock, as a read. Test code only.
	pub async fn shared_for_test(
		&self,
		items: &[shared::Item],
	) -> Result<Vec<Result<api::v3::item::shared::Response<'static>, Error>>, Error> {
		answered_by_bulk(
			shared::ENDPOINT,
			shared::post(self.client(), &shared::Request { items }).await,
			|item| api::v3::item::shared::post(self.client(), item),
		)
		.await
	}
}
