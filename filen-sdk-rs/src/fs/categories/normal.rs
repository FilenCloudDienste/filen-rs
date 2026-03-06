use std::borrow::Cow;

use filen_types::fs::ParentUuid;
#[cfg(feature = "multi-threaded-crypto")]
use rayon::iter::ParallelIterator;

use crate::{
	Error, api,
	auth::Client,
	fs::{
		HasParent, HasUUID,
		categories::{Category, DirType, NonRootItemType, fs::CategoryFS},
		dir::RootDirectory,
		file::meta::FileMeta,
	},
	io::{RemoteDirectory, RemoteFile},
	runtime::{blocking_join, do_cpu_intensive},
	util::IntoMaybeParallelIterator,
};

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct Normal;

impl Category for Normal {
	type Client = Client;
	type Root = RootDirectory;
	type Dir = RemoteDirectory;
	type RootFile = RemoteFile;
	type File = RemoteFile;
}

impl CategoryFS for Normal {
	type ListDirContext<'a> = ();
	async fn list_dir<F>(
		client: &Self::Client,
		parent: &DirType<'_, Self>,
		progress: Option<&F>,
		_context: Self::ListDirContext<'_>,
	) -> Result<(Vec<Self::Dir>, Vec<Self::File>), Error>
	where
		F: Fn(u64, Option<u64>) + Send + Sync,
	{
		list_parent_uuid(client, ParentUuid::Uuid(*parent.uuid()), progress).await
	}

	async fn list_dir_recursive<F>(
		client: &Self::Client,
		parent: &DirType<'_, Self>,
		progress: Option<&F>,
		_context: Self::ListDirContext<'_>,
	) -> Result<(Vec<Self::Dir>, Vec<Self::File>), Error>
	where
		F: Fn(u64, Option<u64>) + Send + Sync,
	{
		let response = api::v3::dir::download::post_large(
			client.client(),
			&api::v3::dir::download::Request {
				uuid: ParentUuid::Uuid(*parent.uuid()),
				skip_cache: false,
			},
			progress,
		)
		.await?;

		let crypter = client.crypter();

		do_cpu_intensive(|| {
			let (dirs, files) = blocking_join!(
				|| response
					.dirs
					.into_maybe_par_iter()
					.filter_map(|response_dir| {
						Some(RemoteDirectory::blocking_from_encrypted(
							response_dir.uuid,
							match response_dir.parent {
								// the request returns the base dir for the request as one of its dirs, we filter it out here
								None => return None,
								Some(parent) => parent,
							},
							response_dir.color,
							response_dir.favorited,
							response_dir.timestamp,
							response_dir.meta,
							&*crypter,
						))
					})
					.collect::<Vec<_>>(),
				|| response
					.files
					.into_maybe_par_iter()
					.map(|f| {
						let meta =
							FileMeta::blocking_from_encrypted(f.metadata, &*crypter, f.version);
						Ok::<RemoteFile, Error>(RemoteFile::from_meta(
							f.uuid,
							f.parent,
							f.chunks_size,
							f.chunks,
							f.region,
							f.bucket,
							f.timestamp,
							f.favorited,
							meta,
						))
					})
					.collect::<Result<Vec<_>, _>>()
			);
			Ok((dirs, files?))
		})
		.await
	}

	async fn dir_size(
		client: &Self::Client,
		dir: &DirType<'_, Self>,
		_context: Self::ListDirContext<'_>,
	) -> Result<super::fs::DirSizeInfo, Error> {
		let request = match dir {
			DirType::Root(r) => api::v3::dir::size::Request {
				uuid: *r.uuid(),
				sharer_id: None,
				receiver_id: None,
				trash: false,
			},
			DirType::Dir(d) => api::v3::dir::size::Request {
				uuid: *d.uuid(),
				sharer_id: None,
				receiver_id: None,
				// todo fix ParentUuid::Trash not being listed as the parent for items listed from trash
				// the old parent is listed instead.
				//
				// need to refactor ParentUuid::Trash to be able to represent the old parent for trashed items to fix this properly
				trash: *d.parent() == ParentUuid::Trash,
			},
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

pub(crate) async fn list_parent_uuid<F>(
	client: &Client,
	parent_uuid: ParentUuid,
	progress: Option<&F>,
) -> Result<(Vec<RemoteDirectory>, Vec<RemoteFile>), Error>
where
	F: Fn(u64, Option<u64>) + Send + Sync,
{
	let (files, dirs) = api::v3::dir::content::post_large(
		client.client(),
		&api::v3::dir::content::Request { uuid: parent_uuid },
		progress,
	)
	.await
	.map(|resp| (resp.files, resp.dirs))?;

	let crypter = client.crypter();

	do_cpu_intensive(|| {
		let (dirs, files) = blocking_join!(
			|| dirs
				.into_maybe_par_iter()
				.map(|d| {
					RemoteDirectory::blocking_from_encrypted(
						d.uuid,
						d.parent,
						d.color,
						d.favorited.unwrap_or(false),
						d.timestamp,
						d.meta,
						&*crypter,
					)
				})
				.collect::<Vec<_>>(),
			|| files
				.into_maybe_par_iter()
				.map(|f| {
					let meta = FileMeta::blocking_from_encrypted(f.metadata, &*crypter, f.version);
					Ok::<RemoteFile, Error>(RemoteFile::from_meta(
						f.uuid,
						f.parent,
						f.size,
						f.chunks,
						f.region,
						f.bucket,
						f.timestamp,
						f.favorited,
						meta,
					))
				})
				.collect::<Result<Vec<_>, _>>()
		);

		Ok((dirs, files?))
	})
	.await
}

impl From<RemoteDirectory> for DirType<'static, Normal> {
	fn from(value: RemoteDirectory) -> Self {
		Self::Dir(Cow::Owned(value))
	}
}

impl From<RootDirectory> for DirType<'static, Normal> {
	fn from(value: RootDirectory) -> Self {
		Self::Root(Cow::Owned(value))
	}
}

impl<'a> From<&'a RemoteDirectory> for DirType<'a, Normal> {
	fn from(value: &'a RemoteDirectory) -> Self {
		Self::Dir(Cow::Borrowed(value))
	}
}

impl<'a> From<&'a RootDirectory> for DirType<'a, Normal> {
	fn from(value: &'a RootDirectory) -> Self {
		Self::Root(Cow::Borrowed(value))
	}
}

impl From<RemoteDirectory> for NonRootItemType<'static, Normal> {
	fn from(value: RemoteDirectory) -> Self {
		Self::Dir(Cow::Owned(value))
	}
}

impl From<RemoteFile> for NonRootItemType<'static, Normal> {
	fn from(value: RemoteFile) -> Self {
		Self::File(Cow::Owned(value))
	}
}

impl<'a> From<&'a RemoteDirectory> for NonRootItemType<'a, Normal> {
	fn from(value: &'a RemoteDirectory) -> Self {
		Self::Dir(Cow::Borrowed(value))
	}
}

impl<'a> From<&'a RemoteFile> for NonRootItemType<'a, Normal> {
	fn from(value: &'a RemoteFile) -> Self {
		Self::File(Cow::Borrowed(value))
	}
}
