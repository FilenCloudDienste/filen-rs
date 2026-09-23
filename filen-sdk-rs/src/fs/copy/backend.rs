//! The copy engine's drive operations, on a logged-in client.

use std::{borrow::Cow, sync::Arc};

use chrono::{DateTime, Utc};
use filen_types::{api::v3::dir::color::DirColor, fs::Uuid};
use tokio::sync::Semaphore;

use crate::{
	Error,
	auth::Client,
	connect::ConnectedTargets,
	crypto,
	fs::{
		categories::{DirType, NonRootItemType, Normal, fs::CategoryFS},
		dir::RemoteDirectory,
		file::{
			BaseFile, RemoteFile,
			enums::RemoteFileType,
			read::fetch_decrypted_chunk_data,
			write::{
				RemoteFileInfo, UploadCompletion, complete_upload, encrypt_and_upload_chunk_data,
				remote_file_from_upload,
			},
		},
		name::ValidatedName,
	},
	sync::lock::ResourceLock,
};

use super::engine::{CopyBackend, CreatedDir, UploadSpec};

pub(crate) struct ClientBackend {
	client: Arc<Client>,
}

impl ClientBackend {
	pub(crate) fn new(client: Arc<Client>) -> Self {
		Self { client }
	}
}

pub(crate) struct ClientUpload {
	file: BaseFile,
	upload_key: String,
}

impl CopyBackend for ClientBackend {
	type DriveLock = Arc<ResourceLock>;
	type Upload = ClientUpload;

	fn memory(&self) -> Arc<Semaphore> {
		Arc::clone(self.client.client().state().memory_semaphore())
	}

	async fn lock_drive(&self) -> Result<Self::DriveLock, Error> {
		self.client.lock_drive().await
	}

	async fn connected_targets(&self, dir: Uuid) -> Result<ConnectedTargets, Error> {
		self.client.fetch_connected_targets(dir).await
	}

	async fn create_dir(
		&self,
		parent: Uuid,
		uuid: Uuid,
		name: &ValidatedName,
		created: DateTime<Utc>,
	) -> Result<CreatedDir, Error> {
		self.client
			.create_dir_for_copy(parent, uuid, name, created)
			.await
	}

	async fn set_dir_color(
		&self,
		dir: &mut RemoteDirectory,
		color: DirColor<'static>,
	) -> Result<(), Error> {
		self.client.set_dir_color(dir, color).await
	}

	async fn propagate(
		&self,
		targets: &ConnectedTargets,
		item: NonRootItemType<'_, Normal>,
	) -> Vec<Error> {
		self.client
			.propagate_to_targets(targets, std::slice::from_ref(&item))
			.await
	}

	async fn propagate_tree(
		&self,
		targets: &ConnectedTargets,
		item: &NonRootItemType<'static, Normal>,
	) -> Vec<Error> {
		let mut items = vec![item.clone()];
		if let NonRootItemType::Dir(dir) = item {
			match Normal::list_dir_recursive(
				&*self.client,
				&DirType::Dir(Cow::Borrowed(dir.as_ref())),
				None::<&fn(u64, Option<u64>)>,
				(),
			)
			.await
			{
				Ok((dirs, files)) => {
					items.extend(dirs.into_iter().map(NonRootItemType::from));
					items.extend(files.into_iter().map(NonRootItemType::from));
				}
				Err(error) => return vec![error],
			}
		}
		self.client.propagate_to_targets(targets, &items).await
	}

	fn begin_upload(&self, spec: UploadSpec) -> Result<Self::Upload, Error> {
		let mut builder = self
			.client
			.make_file_builder(spec.name.as_ref(), spec.parent)?
			.uuid(spec.uuid)
			.no_exif();
		if let Some(mime) = spec.mime {
			builder = builder.mime(mime);
		}
		Ok(ClientUpload {
			file: builder.build(),
			upload_key: crypto::shared::generate_random_base64_values(32, &mut rand::rng()),
		})
	}

	async fn fetch_chunk(
		&self,
		file: &RemoteFileType<'static>,
		index: u64,
	) -> Result<Vec<u8>, Error> {
		fetch_decrypted_chunk_data(self.client.unauthed(), file, index, None).await
	}

	async fn upload_chunk(
		&self,
		upload: &Self::Upload,
		index: u64,
		data: Vec<u8>,
	) -> Result<RemoteFileInfo, Error> {
		encrypt_and_upload_chunk_data(&self.client, &upload.file, &upload.upload_key, index, data)
			.await
			.map(|(_, info)| info)
	}

	async fn finish_upload(
		&self,
		upload: &Self::Upload,
		completion: UploadCompletion,
		info: RemoteFileInfo,
	) -> Result<RemoteFile, Error> {
		let response =
			complete_upload(&self.client, &upload.file, &upload.upload_key, completion).await?;
		Ok(remote_file_from_upload(
			upload.file.clone(),
			response,
			info,
			completion,
		))
	}
}
