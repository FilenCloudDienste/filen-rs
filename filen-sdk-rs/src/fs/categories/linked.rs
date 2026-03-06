use std::borrow::Cow;

#[cfg(feature = "multi-threaded-crypto")]
use rayon::iter::ParallelIterator;

use crate::{
	Error,
	api::{self},
	auth::unauth::UnauthClient,
	connect::{DirPublicLink, MakePasswordSaltAndHash},
	error::MetadataWasNotDecryptedError,
	fs::{
		HasUUID,
		categories::{Category, DirType, fs::CategoryFS},
		dir::{LinkedDirectory, RootDirectoryWithMeta},
		file::{LinkedFile, meta::FileMeta},
	},
	io::{RemoteDirectory, RemoteFile},
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
	type File = RemoteFile;
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
		let crypter = context.crypter().ok_or(MetadataWasNotDecryptedError)?;

		let response = api::v3::dir::link::content::post_large(
			client,
			&api::v3::dir::link::content::Request {
				uuid: context.link_uuid,
				password: Cow::Borrowed(&context.get_password_hash()?),
				parent: *parent.uuid(),
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
						let meta =
							FileMeta::blocking_from_encrypted(f.metadata, crypter, f.version);
						Ok::<RemoteFile, Error>(RemoteFile::from_meta(
							f.uuid,
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

	async fn list_dir_recursive<F>(
		client: &Self::Client,
		parent: &DirType<'_, Self>,
		progress: Option<&F>,
		context: Self::ListDirContext<'_>,
	) -> Result<(Vec<Self::Dir>, Vec<Self::File>), Error>
	where
		F: Fn(u64, Option<u64>) + Send + Sync,
	{
		let response = api::v3::dir::download::link::post_large(
			client,
			&api::v3::dir::download::link::Request {
				uuid: context.link_uuid,
				password: Cow::Borrowed(&context.get_password_hash()?),
				parent: *parent.uuid(),
				skip_cache: false,
			},
			progress,
		)
		.await?;

		let crypter = context.crypter().ok_or(MetadataWasNotDecryptedError)?;

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
							response_dir.color,
							response_dir.favorited,
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
						let meta =
							FileMeta::blocking_from_encrypted(f.metadata, crypter, f.version);
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
		parent: &super::DirType<'_, Self>,
		context: Self::ListDirContext<'_>,
	) -> Result<super::fs::DirSizeInfo, Error> {
		let request = api::v3::dir::size::link::Request {
			uuid: *parent.uuid(),
			link_uuid: context.link_uuid,
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
