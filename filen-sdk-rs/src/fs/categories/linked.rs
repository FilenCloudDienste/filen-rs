use std::borrow::Cow;

use filen_types::api::v3::dir::color::DirColor;
#[cfg(feature = "multi-threaded-crypto")]
use rayon::iter::ParallelIterator;

use crate::{
	Error,
	api::{self},
	auth::{
		http::{ProgressCallback, dyn_progress},
		unauth::UnauthClient,
	},
	connect::DirPublicLink,
	fs::{
		HasUUID,
		categories::{Category, DirType, fs::CategoryFS},
		dir::{LinkedDirectory, RootDirectoryWithMeta},
		file::{LinkedFile, meta::FileMeta},
	},
	io::{AnonymousRemoteFile, RemoteDirectory, RemoteFile},
	runtime::{blocking_join, do_cpu_intensive},
	util::IntoMaybeParallelIterator,
};

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct Linked;

impl Category for Linked {
	type Client = UnauthClient;
	type Root = RootDirectoryWithMeta;
	type Dir = LinkedDirectory;
	type RootFile = LinkedFile;
	type File = AnonymousRemoteFile;
}

impl CategoryFS for Linked {
	type ListDirContext<'a> = Cow<'a, DirPublicLink>;
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
		parent: &super::DirType<'_, Self>,
		context: Self::ListDirContext<'_>,
	) -> Result<super::fs::DirSizeInfo, Error> {
		let request = api::v3::dir::size::link::Request {
			uuid: parent.uuid(),
			link_uuid: (*context.uuid()),
		};
		api::v3::dir::size::link::post(client, &request)
			.await
			.map(|resp| super::fs::DirSizeInfo {
				size: resp.size,
				files: resp.files,
				dirs: resp.dirs,
			})
	}
}

impl From<LinkedDirectory> for DirType<'static, Linked> {
	fn from(value: LinkedDirectory) -> Self {
		Self::Dir(Cow::Owned(value))
	}
}

impl From<RootDirectoryWithMeta> for DirType<'static, Linked> {
	fn from(value: RootDirectoryWithMeta) -> Self {
		Self::Root(Cow::Owned(value))
	}
}

impl<'a> From<&'a LinkedDirectory> for DirType<'a, Linked> {
	fn from(value: &'a LinkedDirectory) -> Self {
		Self::Dir(Cow::Borrowed(value))
	}
}

impl<'a> From<&'a RootDirectoryWithMeta> for DirType<'a, Linked> {
	fn from(value: &'a RootDirectoryWithMeta) -> Self {
		Self::Root(Cow::Borrowed(value))
	}
}

/// The body of [`CategoryFS::list_dir`] for [`Linked`], outside the trait method's progress
/// generic so it is compiled once.
async fn list_dir_contents(
	client: &UnauthClient,
	parent: &DirType<'_, Linked>,
	progress: Option<&ProgressCallback<'_>>,
	context: Cow<'_, DirPublicLink>,
) -> Result<(Vec<LinkedDirectory>, Vec<AnonymousRemoteFile>), Error> {
	let crypter = context.crypter();

	let response = api::v3::dir::link::content::post_large(
		client,
		&api::v3::dir::link::content::Request {
			uuid: (*context.uuid()),
			password: context.get_password_hash()?,
			parent: parent.uuid(),
		},
		progress,
	)
	.await?;

	do_cpu_intensive(|| {
		let (dirs, files) = blocking_join!(
			|| response
				.dirs
				.into_maybe_par_iter()
				.map(|d| {
					LinkedDirectory(RemoteDirectory::blocking_from_encrypted(
						d.uuid,
						d.parent.into(),
						d.color,
						false,
						d.timestamp,
						d.metadata,
						crypter,
					))
				})
				.collect::<Vec<_>>(),
			|| response
				.files
				.into_maybe_par_iter()
				.map(|f| {
					let meta = FileMeta::blocking_from_encrypted(f.metadata, crypter, f.version);
					Ok::<AnonymousRemoteFile, Error>(RemoteFile::from_meta(
						f.uuid,
						// link surfaces never carry a stable id on the wire
						(),
						f.parent.into(),
						f.size,
						f.chunks,
						f.region,
						f.bucket,
						f.timestamp,
						false,
						meta,
					))
				})
				.collect::<Result<Vec<_>, Error>>()
		);

		Ok((dirs, files?))
	})
	.await
}

/// The body of [`CategoryFS::list_dir_recursive`] for [`Linked`], outside the trait method's
/// progress generic so it is compiled once.
async fn list_dir_recursive_contents(
	client: &UnauthClient,
	parent: &DirType<'_, Linked>,
	progress: Option<&ProgressCallback<'_>>,
	context: Cow<'_, DirPublicLink>,
) -> Result<(Vec<LinkedDirectory>, Vec<AnonymousRemoteFile>), Error> {
	let response = api::v3::dir::download::link::post_large(
		client,
		&api::v3::dir::download::link::Request {
			uuid: (*context.uuid()),
			password: context.get_password_hash()?,
			parent: parent.uuid(),
			skip_cache: false,
		},
		progress,
	)
	.await?;

	let crypter = context.crypter();

	do_cpu_intensive(|| {
		let (dirs, files) = blocking_join!(
			|| response
				.dirs
				.into_maybe_par_iter()
				.filter_map(|response_dir| {
					Some(LinkedDirectory(RemoteDirectory::blocking_from_encrypted(
						response_dir.uuid,
						match response_dir.parent {
							// the request returns the base dir for the request as one of its dirs, we filter it out here
							None => return None,
							Some(parent) => parent,
						},
						// link rows carry no color or favorited
						DirColor::Default,
						false,
						response_dir.timestamp,
						response_dir.meta,
						crypter,
					)))
				})
				.collect::<Vec<_>>(),
			|| response
				.files
				.into_maybe_par_iter()
				.map(|f| {
					let meta = FileMeta::blocking_from_encrypted(f.metadata, crypter, f.version);
					Ok::<AnonymousRemoteFile, Error>(RemoteFile::from_meta(
						f.uuid,
						// link surfaces never carry a stable id on the wire
						(),
						f.parent,
						f.chunks_size,
						f.chunks,
						f.region,
						f.bucket,
						f.timestamp,
						false,
						meta,
					))
				})
				.collect::<Result<Vec<_>, _>>()
		);
		Ok((dirs, files?))
	})
	.await
}
