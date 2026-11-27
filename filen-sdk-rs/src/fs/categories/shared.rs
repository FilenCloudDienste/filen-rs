use std::borrow::Cow;

use filen_types::{
	api::v3::{
		dir::color::DirColor,
		shared::{
			in_item::{self, SharedItemDirIn, SharedItemFileIn},
			in_root::{SharedRootDirIn, SharedRootFileIn},
			in_uuid::{SharedDirIn, SharedFileIn},
			out_uuid::SharedFileOut,
		},
	},
	fs::Uuid,
	traits::CowHelpers,
};
#[cfg(feature = "multi-threaded-crypto")]
use rayon::iter::ParallelIterator;
use rsa::PrivateKey;

use crate::{
	Error, api,
	auth::{
		Client,
		http::{ProgressCallback, dyn_progress},
	},
	connect::fs::{
		DirInfo, ShareInfo, SharedDirectory, SharedInItem, SharedRootDirectory, SharedRootFile,
		SharingRole,
	},
	crypto::shared::MetaCrypter,
	fs::{
		HasUUID,
		categories::{Category, DirType, fs::CategoryFS},
		dir::meta::DirectoryMeta,
		file::meta::FileMeta,
	},
	io::{AnonymousRemoteFile, RemoteFile},
	runtime::{blocking_join, do_cpu_intensive},
	util::IntoMaybeParallelIterator,
};

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct Shared;

/// A file row in a shared-dir listing. Shared browsing does not model
/// whole-life file identity by design: the shared wire types carry no
/// `stableUUID`, so the category's file type has no id to hold.
pub type SharedListedFile = AnonymousRemoteFile;

impl Category for Shared {
	type Client = Client;
	type Root = SharedRootDirectory;
	type Dir = SharedDirectory;
	type RootFile = SharedRootFile;
	type File = SharedListedFile;
}

impl CategoryFS for Shared {
	type ListDirContext<'a> = &'a SharingRole;

	async fn list_dir<F>(
		client: &Self::Client,
		parent: &DirType<'_, Self>,
		progress: Option<&F>,
		context: Self::ListDirContext<'_>,
	) -> Result<(Vec<Self::Dir>, Vec<Self::File>), Error>
	where
		F: Fn(u64, Option<u64>) + Send + Sync,
	{
		list_dir_contents(client, parent, dyn_progress(progress), context).await
	}

	async fn list_dir_recursive<F>(
		client: &Self::Client,
		parent: &DirType<'_, Self>,
		progress: Option<&F>,
		context: Self::ListDirContext<'_>,
	) -> Result<(Vec<Self::Dir>, Vec<Self::File>), Error>
	where
		F: Fn(u64, Option<u64>) + Send + Sync,
	{
		list_dir_recursive_contents(client, parent, dyn_progress(progress), context).await
	}

	async fn dir_size(
		client: &Self::Client,
		dir: &DirType<'_, Self>,
		context: Self::ListDirContext<'_>,
	) -> Result<super::fs::DirSizeInfo, Error> {
		let request = api::v3::dir::size::Request {
			uuid: dir.uuid(),
			sharer_id: if let SharingRole::Sharer(ShareInfo { id, .. }) = context {
				Some(*id)
			} else {
				None
			},
			receiver_id: if let SharingRole::Receiver(ShareInfo { id, .. }) = context {
				Some(*id)
			} else {
				None
			},
			trash: false,
		};
		api::v3::dir::size::post(client.client(), &request)
			.await
			.map(|resp| super::fs::DirSizeInfo {
				size: resp.size,
				files: resp.files,
				dirs: resp.dirs,
			})
	}
}

fn blocking_remote_file_from_shared_out(
	shared_file: SharedFileOut<'_>,
	crypter: &impl MetaCrypter,
) -> Result<SharedListedFile, Error> {
	let meta =
		FileMeta::blocking_from_encrypted(shared_file.metadata, crypter, shared_file.version)
			.into_owned_cow();

	Ok(RemoteFile::from_meta(
		shared_file.uuid,
		// shared listings do not model lifetime identity; see `SharedListedFile`
		(),
		shared_file.parent.into(),
		shared_file.size,
		shared_file.chunks,
		shared_file.region.into_owned(),
		shared_file.bucket.into_owned(),
		shared_file.timestamp,
		false,
		meta.into_owned_cow(),
	))
}

fn blocking_remote_file_from_shared_in(
	shared_file: SharedFileIn<'_>,
	private_key: &PrivateKey,
) -> Result<AnonymousRemoteFile, Error> {
	let meta = FileMeta::blocking_from_rsa_encrypted(
		shared_file.metadata,
		private_key,
		shared_file.version,
	)
	.into_owned_cow();

	Ok(RemoteFile::from_meta(
		shared_file.uuid,
		// shared-in surfaces never carry a stable id on the wire
		(),
		shared_file.parent.into(),
		shared_file.size,
		shared_file.chunks,
		shared_file.region.into_owned(),
		shared_file.bucket.into_owned(),
		shared_file.timestamp,
		false,
		meta,
	))
}

fn blocking_shared_dir_from_shared_in(
	shared_dir: SharedDirIn<'static>,
	private_key: &PrivateKey,
) -> SharedDirectory {
	SharedDirectory::from_dir_info(DirInfo {
		uuid: shared_dir.uuid,
		parent: shared_dir.parent,
		color: shared_dir.color,
		timestamp: shared_dir.timestamp,
		metadata: DirectoryMeta::blocking_from_rsa_encrypted(shared_dir.metadata, private_key),
	})
}

pub(crate) async fn list_all_in_shared(
	client: &Client,
	callback: Option<&ProgressCallback<'_>>,
) -> Result<(Vec<SharedRootDirectory>, Vec<SharedRootFile>), Error> {
	let response = api::v3::shared::in_root::post_large(client.client(), callback).await?;

	let priv_key = client.private_key();

	do_cpu_intensive(|| {
		let (dirs, files) = blocking_join!(
			|| {
				response
					.dirs
					.into_maybe_par_iter()
					.map(|d| SharedRootDirectory::blocking_from_shared_in(d, priv_key))
					.collect::<Result<Vec<_>, _>>()
			},
			|| {
				response
					.files
					.into_maybe_par_iter()
					.map(|f| SharedRootFile::blocking_from_shared_in(f, priv_key))
					.collect::<Result<Vec<_>, _>>()
			}
		);
		Ok((dirs?, files?))
	})
	.await
}

/// `None` for `item_not_shared` alone. Every other error, a not-found code from elsewhere
/// included, stays an error: a caller deletes its local state on `None`, so only the answer the
/// endpoint gives after a completed check may produce it.
fn none_if_not_shared<T>(result: Result<T, Error>) -> Result<Option<T>, Error> {
	match result {
		Ok(value) => Ok(Some(value)),
		Err(e) if e.server_code().as_deref() == Some(in_item::ITEM_NOT_SHARED) => Ok(None),
		Err(e) => Err(e),
	}
}

pub(crate) async fn get_in_shared_item(
	client: &Client,
	uuid: Uuid,
) -> Result<Option<SharedInItem>, Error> {
	let Some(response) = none_if_not_shared(
		api::v3::shared::in_item::post(client.client(), &in_item::Request { uuid }).await,
	)?
	else {
		return Ok(None);
	};
	let key = client.private_key();
	do_cpu_intensive(|| blocking_shared_in_item_from_response(response, key))
		.await
		.map(Some)
}

/// Builds a [`SharedInItem`] from a `v3/shared/in/item` row through the constructors the
/// `v3/shared/in` listings use: a row with no parent is a share root, one with a parent an item
/// inside a shared folder. The row's `writeAccess` goes only where those types already hold it
/// (a root folder's [`SharedDirInfo`](crate::connect::fs::SharedDirInfo)); nothing enforces it.
fn blocking_shared_in_item_from_response(
	response: in_item::Response<'static>,
	private_key: &PrivateKey,
) -> Result<SharedInItem, Error> {
	match response {
		in_item::Response::Dir(dir) => blocking_shared_in_dir(dir, private_key),
		in_item::Response::File(file) => blocking_shared_in_file(file, private_key),
	}
}

fn blocking_shared_in_dir(
	dir: SharedItemDirIn<'static>,
	private_key: &PrivateKey,
) -> Result<SharedInItem, Error> {
	let SharedItemDirIn {
		uuid,
		parent,
		metadata,
		sharer_email,
		sharer_id,
		write_access,
		color,
		timestamp,
		is_sync,
		is_default,
	} = dir;
	Ok(match parent {
		None => SharedInItem::RootDir(SharedRootDirectory::blocking_from_shared_in(
			SharedRootDirIn {
				uuid,
				metadata,
				sharer_email,
				sharer_id,
				write_access,
				color,
				timestamp,
				is_sync,
				is_default,
			},
			private_key,
		)?),
		Some(parent) => SharedInItem::Dir {
			dir: blocking_shared_dir_from_shared_in(
				SharedDirIn {
					uuid,
					parent,
					metadata,
					write_access,
					color,
					timestamp,
					is_sync,
					is_default,
				},
				private_key,
			),
			role: SharingRole::Sharer(ShareInfo {
				email: sharer_email.into_owned(),
				id: sharer_id,
			}),
		},
	})
}

fn blocking_shared_in_file(
	file: SharedItemFileIn<'_>,
	private_key: &PrivateKey,
) -> Result<SharedInItem, Error> {
	let SharedItemFileIn {
		uuid,
		parent,
		metadata,
		bucket,
		region,
		chunks,
		size,
		version,
		sharer_email,
		sharer_id,
		write_access,
		timestamp,
	} = file;
	Ok(match parent {
		None => SharedInItem::RootFile(SharedRootFile::blocking_from_shared_in(
			SharedRootFileIn {
				uuid,
				metadata,
				bucket,
				region,
				chunks,
				size,
				version,
				sharer_email,
				sharer_id,
				write_access,
				timestamp,
			},
			private_key,
		)?),
		Some(parent) => SharedInItem::File {
			file: blocking_remote_file_from_shared_in(
				SharedFileIn {
					uuid,
					parent,
					metadata,
					bucket,
					region,
					chunks,
					size,
					version,
					write_access,
					timestamp,
				},
				private_key,
			)?,
			role: SharingRole::Sharer(ShareInfo {
				email: sharer_email.into_owned(),
				id: sharer_id,
			}),
		},
	})
}

pub(crate) async fn list_all_out_shared(
	client: &Client,
	user_id: Option<u64>,
	callback: Option<&ProgressCallback<'_>>,
) -> Result<(Vec<SharedRootDirectory>, Vec<SharedRootFile>), Error> {
	let response = api::v3::shared::out_root::post_large(
		client.client(),
		&api::v3::shared::out_root::Request {
			receiver_id: user_id,
		},
		callback,
	)
	.await?;

	let crypter = client.crypter();

	do_cpu_intensive(|| {
		let (dirs, files) = blocking_join!(
			|| {
				response
					.dirs
					.into_maybe_par_iter()
					.map(|d| SharedRootDirectory::blocking_from_shared_out(d, &*crypter))
					.collect::<Result<Vec<_>, _>>()
			},
			|| {
				response
					.files
					.into_maybe_par_iter()
					.map(|f| SharedRootFile::blocking_from_shared_out(f, &*crypter))
					.collect::<Result<Vec<_>, _>>()
			}
		);
		Ok((dirs?, files?))
	})
	.await
}

impl From<SharedDirectory> for DirType<'static, Shared> {
	fn from(value: SharedDirectory) -> Self {
		Self::Dir(Cow::Owned(value))
	}
}

impl From<SharedRootDirectory> for DirType<'static, Shared> {
	fn from(value: SharedRootDirectory) -> Self {
		Self::Root(Cow::Owned(value))
	}
}

impl<'a> From<&'a SharedDirectory> for DirType<'a, Shared> {
	fn from(value: &'a SharedDirectory) -> Self {
		Self::Dir(Cow::Borrowed(value))
	}
}

impl<'a> From<&'a SharedRootDirectory> for DirType<'a, Shared> {
	fn from(value: &'a SharedRootDirectory) -> Self {
		Self::Root(Cow::Borrowed(value))
	}
}

/// The body of [`CategoryFS::list_dir`] for [`Shared`], outside the trait method's progress
/// generic so it is compiled once.
async fn list_dir_contents(
	client: &Client,
	parent: &DirType<'_, Shared>,
	progress: Option<&ProgressCallback<'_>>,
	context: &SharingRole,
) -> Result<(Vec<SharedDirectory>, Vec<SharedListedFile>), Error> {
	match &context {
		SharingRole::Sharer(_) => {
			let response = api::v3::shared::in_uuid::post_large(
				client.client(),
				&api::v3::shared::in_uuid::Request {
					uuid: parent.uuid(),
				},
				progress,
			)
			.await?;
			let key = client.private_key();

			do_cpu_intensive(|| {
				let (dirs, files) = blocking_join!(
					|| {
						response
							.dirs
							.into_maybe_par_iter()
							.map(|d| blocking_shared_dir_from_shared_in(d, key))
							.collect::<Vec<_>>()
					},
					|| {
						response
							.files
							.into_maybe_par_iter()
							.map(|f| blocking_remote_file_from_shared_in(f, key))
							.collect::<Result<Vec<_>, _>>()
					}
				);

				Ok((dirs, files?))
			})
			.await
		}
		SharingRole::Receiver(_) => {
			let response = api::v3::shared::out_uuid::post_large(
				client.client(),
				&api::v3::shared::out_uuid::Request {
					uuid: parent.uuid(),
					receiver_id: match context {
						SharingRole::Sharer(share_info) => share_info.id,
						SharingRole::Receiver(share_info) => share_info.id,
					},
				},
				progress,
			)
			.await?;
			let crypter = client.crypter();

			do_cpu_intensive(|| {
				let (dirs, files) = blocking_join!(
					|| {
						response
							.dirs
							.into_maybe_par_iter()
							.map(|d| {
								SharedDirectory::from_dir_info(DirInfo {
									uuid: d.uuid,
									parent: d.parent,
									color: d.color,
									timestamp: d.timestamp,
									metadata: DirectoryMeta::blocking_from_encrypted(
										d.metadata, &*crypter,
									),
								})
							})
							.collect::<Vec<_>>()
					},
					|| {
						response
							.files
							.into_maybe_par_iter()
							.map(|f| blocking_remote_file_from_shared_out(f, &*crypter))
							.collect::<Result<Vec<_>, _>>()
					}
				);
				Ok((dirs, files?))
			})
			.await
		}
	}
}

/// The body of [`CategoryFS::list_dir_recursive`] for [`Shared`], outside the trait method's
/// progress generic so it is compiled once.
async fn list_dir_recursive_contents(
	client: &Client,
	parent: &DirType<'_, Shared>,
	progress: Option<&ProgressCallback<'_>>,
	context: &SharingRole,
) -> Result<(Vec<SharedDirectory>, Vec<SharedListedFile>), Error> {
	if let SharingRole::Receiver(_) = context {
		let (dirs, files) =
			super::normal::list_recursive_parent_uuid(client, parent.uuid(), progress).await?;
		return Ok((
			dirs.into_iter()
				.map(|d| SharedDirectory { inner: d })
				.collect(),
			// The normal listing reports stable ids, but shared browsing does
			// not model lifetime identity; see `SharedListedFile`.
			files.into_iter().map(RemoteFile::into_anonymous).collect(),
		));
	}

	let response = api::v3::dir::download::shared::post_large(
		client.client(),
		&api::v3::dir::download::shared::Request {
			uuid: parent.uuid(),
			skip_cache: true,
		},
		progress,
	)
	.await?;

	let key = client.private_key();

	do_cpu_intensive(|| {
		let (dirs, files) = blocking_join!(
			|| {
				response
					.dirs
					.into_maybe_par_iter()
					.filter_map(|d| match d.parent {
						Some(parent) => Some(SharedDirectory::from_dir_info(DirInfo {
							uuid: d.uuid,
							parent,
							color: DirColor::Default,
							timestamp: d.timestamp,
							metadata: DirectoryMeta::blocking_from_rsa_encrypted(d.meta, key),
						})),
						None => None,
					})
					.collect::<Vec<_>>()
			},
			|| {
				response
					.files
					.into_maybe_par_iter()
					.map(|f| {
						let meta =
							FileMeta::blocking_from_rsa_encrypted(f.metadata, key, f.version)
								.into_owned_cow();

						RemoteFile::from_meta(
							f.uuid,
							// shared-in surfaces never carry a stable id on the wire
							(),
							f.parent.into(),
							f.chunks_size,
							f.chunks,
							f.region.into_owned(),
							f.bucket.into_owned(),
							f.timestamp,
							false,
							meta,
						)
					})
					.collect::<Vec<_>>()
			}
		);
		Ok((dirs, files))
	})
	.await
}

#[cfg(test)]
mod tests {
	use chrono::DateTime;
	use filen_types::{
		api::v3::shared::in_item::Response, auth::FileEncryptionVersion, error::ResponseError,
		fs::ParentUuid,
	};

	use super::*;
	use crate::{
		ErrorKind,
		fs::{
			HasName, HasParent,
			categories::RootItemType,
			meta_recovery::test_support::{TEST_RSA_KEY, rsa_encrypt},
		},
		io::HasFileInfo,
	};

	const ITEM: Uuid = Uuid::from_u128(0x4f1c0d1e_5a3b_4c7a_9d8e_2b1f0a9c6e33);
	const PARENT: Uuid = Uuid::from_u128(0xa7e20000_0000_4000_8000_000000000001);
	const SHARER_ID: u64 = 123_456;
	const SHARER_EMAIL: &str = "owner@example.com";
	const FILE_META: &str = r#"{"name":"report.pdf","size":2500000,"mime":"application/pdf","key":"12345678901234567890123456789012","lastModified":1719000000000}"#;

	fn sharer() -> SharingRole {
		SharingRole::Sharer(ShareInfo {
			email: SHARER_EMAIL.to_owned(),
			id: SHARER_ID,
		})
	}

	fn dir_row(parent: Option<Uuid>, write_access: bool) -> Response<'static> {
		Response::Dir(SharedItemDirIn {
			uuid: ITEM,
			parent,
			metadata: rsa_encrypt(br#"{"name":"Projects"}"#),
			sharer_email: SHARER_EMAIL.into(),
			sharer_id: SHARER_ID,
			write_access,
			color: DirColor::Blue,
			timestamp: DateTime::from_timestamp(1_764_230_400, 0).unwrap(),
			is_sync: false,
			is_default: false,
		})
	}

	/// A file row's `writeAccess` lands in none of the types it converts to, so it is fixed.
	fn file_row(parent: Option<Uuid>) -> Response<'static> {
		Response::File(SharedItemFileIn {
			uuid: ITEM,
			parent,
			metadata: rsa_encrypt(FILE_META.as_bytes()),
			bucket: "filen-1".into(),
			region: "de-1".into(),
			chunks: 3,
			size: 2_500_000,
			version: FileEncryptionVersion::V2,
			sharer_email: SHARER_EMAIL.into(),
			sharer_id: SHARER_ID,
			write_access: true,
			timestamp: DateTime::from_timestamp(1_764_230_400, 0).unwrap(),
		})
	}

	fn convert(row: Response<'static>) -> SharedInItem {
		blocking_shared_in_item_from_response(row, &TEST_RSA_KEY).unwrap()
	}

	#[test]
	fn a_folder_row_without_a_parent_is_a_root_dir_carrying_its_sharer_and_access() {
		// both values, so a hardcoded `write_access` fails
		for write_access in [true, false] {
			let SharedInItem::RootDir(root) = convert(dir_row(None, write_access)) else {
				panic!("expected a root dir");
			};
			assert_eq!(root.uuid(), ITEM);
			assert_eq!(root.name(), Some("Projects"));
			assert_eq!(root.sharing_role(), &sharer());
			assert_eq!(root.info.write_access, write_access);
			assert_eq!(root.get_source_id(), SHARER_ID);
		}
	}

	#[test]
	fn a_file_row_without_a_parent_is_a_root_file_carrying_its_sharer() {
		let SharedInItem::RootFile(file) = convert(file_row(None)) else {
			panic!("expected a root file");
		};
		assert_eq!(file.uuid(), ITEM);
		assert_eq!(file.name(), Some("report.pdf"));
		assert_eq!(file.mime(), Some("application/pdf"));
		assert_eq!(file.size(), 2_500_000);
		assert_eq!(file.chunks(), 3);
		assert_eq!(file.sharing_role(), &sharer());
	}

	#[test]
	fn a_folder_row_with_a_parent_is_a_nested_dir_with_the_sharer_as_its_role() {
		let SharedInItem::Dir { dir, role } = convert(dir_row(Some(PARENT), true)) else {
			panic!("expected a nested dir");
		};
		assert_eq!(dir.uuid(), ITEM);
		assert_eq!(dir.parent(), &ParentUuid::Uuid(PARENT));
		assert_eq!(dir.name(), Some("Projects"));
		assert_eq!(role, sharer());
	}

	#[test]
	fn a_file_row_with_a_parent_is_a_nested_file_with_the_sharer_as_its_role() {
		let SharedInItem::File { file, role } = convert(file_row(Some(PARENT))) else {
			panic!("expected a nested file");
		};
		assert_eq!(file.uuid, ITEM);
		assert_eq!(file.parent, ParentUuid::Uuid(PARENT));
		assert_eq!(file.name(), Some("report.pdf"));
		assert_eq!(file.size, 2_500_000);
		assert_eq!(file.region, "de-1");
		assert_eq!(file.bucket, "filen-1");
		assert_eq!(role, sharer());
	}

	fn api_error(code: &str) -> Error {
		ResponseError::ApiError {
			message: None,
			code: Some(code.to_owned()),
		}
		.into()
	}

	#[test]
	fn only_item_not_shared_reads_as_not_shared() {
		assert_eq!(
			none_if_not_shared::<()>(Err(api_error("item_not_shared"))).unwrap(),
			None
		);
		assert_eq!(none_if_not_shared(Ok(7)).unwrap(), Some(7));
	}

	#[test]
	fn another_not_found_code_stays_an_error() {
		let err = none_if_not_shared::<()>(Err(api_error("folder_not_found"))).unwrap_err();
		assert_eq!(err.kind(), ErrorKind::FolderNotFound);
	}

	#[test]
	fn a_transport_or_server_error_stays_an_error() {
		let transport = none_if_not_shared::<()>(Err(Error::custom(ErrorKind::Reqwest, "offline")))
			.unwrap_err();
		assert_eq!(transport.kind(), ErrorKind::Reqwest);
		let server = none_if_not_shared::<()>(Err(api_error("internal_error"))).unwrap_err();
		assert_eq!(server.kind(), ErrorKind::Server);
	}

	/// Never run: it only has to compile, proving each result feeds the calls that browse,
	/// compare with and leave a share.
	#[expect(
		dead_code,
		reason = "a compile-time check of the result's types, never called"
	)]
	async fn a_looked_up_item_feeds_the_existing_share_calls(
		client: &Client,
		uuid: Uuid,
	) -> Result<(), Error> {
		let no_progress = None::<&fn(u64, Option<u64>)>;
		match client.get_shared_in_item(uuid).await? {
			None => {}
			Some(SharedInItem::RootDir(root)) => {
				client
					.list_shared_dir(&DirType::from(&root), root.sharing_role(), no_progress)
					.await?;
				client
					.remove_shared_item(&RootItemType::Dir(Cow::Owned(root)))
					.await?;
			}
			Some(SharedInItem::RootFile(file)) => {
				client
					.remove_shared_item(&RootItemType::File(Cow::Owned(file)))
					.await?;
			}
			Some(SharedInItem::Dir { dir, role }) => {
				client
					.list_shared_dir(&DirType::from(&dir), &role, no_progress)
					.await?;
			}
			Some(SharedInItem::File { file, role: _ }) => {
				// the type `list_shared_dir` lists a shared folder's files as
				let _: &SharedListedFile = &file;
			}
		}
		Ok(())
	}
}
