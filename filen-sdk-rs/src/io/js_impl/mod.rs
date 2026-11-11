//! The path transfers of the uniffi and napi bindings: a file downloaded to a path or uploaded
//! from one, and a directory downloaded or uploaded recursively. The bodies are shared; each
//! binding hands them its own callbacks (`uniffi_impl`, `napi_impl`).

use std::{borrow::Cow, path::PathBuf, sync::Arc};

use crate::{
	Error,
	auth::Client,
	error::FilenSdkError,
	fs::{
		categories::{Linked, Normal, Shared},
		dir::RemoteDirectory,
		file::enums::RemoteFileType,
	},
	io::{
		DirDownloadCallback, DirUploadCallback, client_impl::IoSharedClientExt,
		dir_download::CategoryDirDownloadExtPub,
	},
	js::{
		AnyDirWithContext, AnyFile, Dir, DirByCategoryWithContext, File,
		FileBuilderParamsOptionalName, NonRootItem,
	},
	util::MaybeSendCallback,
};

#[cfg(feature = "napi")]
mod napi_impl;
#[cfg(feature = "uniffi")]
mod uniffi_impl;

// No napi export uploads a directory yet.
#[cfg_attr(feature = "napi", allow(dead_code))]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct UploadError {
	pub path: String,
	pub error: Arc<FilenSdkError>,
}

// No napi export downloads a directory yet.
#[cfg_attr(feature = "napi", allow(dead_code))]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct DownloadError {
	pub error: Arc<Error>,
	pub path: String,
	pub item: NonRootItem,
}

/// Uploads the contents of the local directory `dir_path` into `target`.
// No napi export uploads a directory yet.
#[cfg_attr(feature = "napi", allow(dead_code))]
pub(crate) async fn upload_dir<C: DirUploadCallback + ?Sized>(
	client: Arc<Client>,
	dir_path: String,
	target: Dir,
	callback: &C,
) -> Result<(), Error> {
	let target: RemoteDirectory = target.into();
	client
		.upload_dir_recursively(PathBuf::from(dir_path), callback, &target)
		.await
}

/// Downloads `target` and everything below it into the local directory `dir_path`.
// No napi export downloads a directory yet.
#[cfg_attr(feature = "napi", allow(dead_code))]
pub(crate) async fn download_dir<C>(
	client: Arc<Client>,
	dir_path: String,
	target: AnyDirWithContext,
	callback: &C,
) -> Result<(), Error>
where
	C: DirDownloadCallback<Normal> + DirDownloadCallback<Shared> + DirDownloadCallback<Linked>,
{
	match DirByCategoryWithContext::from(target) {
		DirByCategoryWithContext::Normal(dir) => {
			Normal::download_dir_recursively(client, dir_path, callback, dir, ()).await
		}
		DirByCategoryWithContext::Shared(dir, sharing_role) => {
			Shared::download_dir_recursively(client, dir_path, callback, dir, &sharing_role).await
		}
		DirByCategoryWithContext::Linked(dir, public_link) => {
			Linked::download_dir_recursively(
				Arc::new(client.get_unauthed()),
				dir_path,
				callback,
				dir,
				Cow::Owned(public_link.try_into()?),
			)
			.await
		}
	}
}

/// Uploads the local file at `file_path` as `params` describe it, handing `progress` the bytes
/// uploaded since its last call.
pub(crate) async fn upload_file(
	client: Arc<Client>,
	params: FileBuilderParamsOptionalName,
	file_path: String,
	progress: Option<MaybeSendCallback<'static, u64>>,
) -> Result<File, Error> {
	client
		.upload_file_from_path_with_builder(params.try_into()?, PathBuf::from(file_path), progress)
		.await
		.map(|(file, _)| File::from(file))
}

/// Downloads `file` to `file_path`, handing `progress` the bytes downloaded since its last call.
pub(crate) async fn download_file_to_path(
	client: Arc<Client>,
	file: AnyFile,
	file_path: String,
	progress: Option<MaybeSendCallback<'static, u64>>,
) -> Result<(), Error> {
	let file: RemoteFileType = file.try_into()?;
	IoSharedClientExt::download_file_to_path(client.as_ref(), &file, file_path.as_ref(), progress)
		.await
}
