use std::borrow::Cow;

use filen_types::{
	api::v3::{
		dir::color::DirColor,
		shared::{in_uuid::SharedFileIn, out_uuid::SharedFileOut},
	},
	traits::CowHelpers,
};
#[cfg(feature = "multi-threaded-crypto")]
use rayon::iter::ParallelIterator;
use rsa::RsaPrivateKey;

use crate::{
	Error, api,
	auth::Client,
	connect::fs::{
		DirInfo, ShareInfo, SharedDirectory, SharedRootDirectory, SharedRootFile, SharingRole,
	},
	crypto::shared::MetaCrypter,
	fs::{
		HasUUID,
		categories::{Category, DirType, fs::CategoryFS},
		dir::meta::DirectoryMeta,
		file::meta::FileMeta,
	},
	io::RemoteFile,
	runtime::{blocking_join, do_cpu_intensive},
	util::IntoMaybeParallelIterator,
};

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct Shared;

impl Category for Shared {
	type Client = Client;
	type Root = SharedRootDirectory;
	type Dir = SharedDirectory;
	type RootFile = SharedRootFile;
	type File = RemoteFile;
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
		match &context {
			SharingRole::Sharer(_) => {
				let response = api::v3::shared::in_uuid::post_large(
					client.client(),
					&api::v3::shared::in_uuid::Request {
						uuid: *parent.uuid(),
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
								.map(|d| {
									SharedDirectory::from_dir_info(DirInfo {
										uuid: d.uuid,
										parent: d.parent,
										color: d.color,
										timestamp: d.timestamp,
										metadata: DirectoryMeta::blocking_from_rsa_encrypted(
											d.metadata, key,
										),
									})
								})
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
						uuid: *parent.uuid(),
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

	async fn list_dir_recursive<F>(
		client: &Self::Client,
		parent: &DirType<'_, Self>,
		progress: Option<&F>,
		context: Self::ListDirContext<'_>,
	) -> Result<(Vec<Self::Dir>, Vec<Self::File>), Error>
	where
		F: Fn(u64, Option<u64>) + Send + Sync,
	{
		if let SharingRole::Receiver(_) = context {
			let (dirs, files) =
				super::normal::list_recursive_parent_uuid(client, *parent.uuid(), progress).await?;
			return Ok((
				dirs.into_iter()
					.map(|d| SharedDirectory { inner: d })
					.collect(),
				files,
			));
		}

		let response = api::v3::dir::download::shared::post_large(
			client.client(),
			&api::v3::dir::download::shared::Request {
				uuid: *parent.uuid(),
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

	async fn dir_size(
		client: &Self::Client,
		dir: &DirType<'_, Self>,
		context: Self::ListDirContext<'_>,
	) -> Result<super::fs::DirSizeInfo, Error> {
		let request = api::v3::dir::size::Request {
			uuid: *dir.uuid(),
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
) -> Result<RemoteFile, Error> {
	let meta =
		FileMeta::blocking_from_encrypted(shared_file.metadata, crypter, shared_file.version)
			.into_owned_cow();

	Ok(RemoteFile::from_meta(
		shared_file.uuid,
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
	private_key: &RsaPrivateKey,
) -> Result<RemoteFile, Error> {
	let meta = FileMeta::blocking_from_rsa_encrypted(
		shared_file.metadata,
		private_key,
		shared_file.version,
	)
	.into_owned_cow();

	Ok(RemoteFile::from_meta(
		shared_file.uuid,
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

pub(crate) async fn list_all_in_shared<F>(
	client: &Client,
	callback: Option<&F>,
) -> Result<(Vec<SharedRootDirectory>, Vec<SharedRootFile>), Error>
where
	F: Fn(u64, Option<u64>) + Send + Sync,
{
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

pub(crate) async fn list_all_out_shared<F>(
	client: &Client,
	user_id: Option<u64>,
	callback: Option<&F>,
) -> Result<(Vec<SharedRootDirectory>, Vec<SharedRootFile>), Error>
where
	F: Fn(u64, Option<u64>) + Send + Sync,
{
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
