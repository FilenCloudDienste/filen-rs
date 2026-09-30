//! The drive operations a job made of many items needs (locking, creating directories,
//! transferring chunks, registering files, sharing new items with the destination's links and
//! shares), as a trait so engines can run against a fake in tests, and its implementation on a
//! logged-in client.

#[cfg(feature = "archive")]
use std::borrow::Cow;
use std::{future::Future, sync::Arc};

use chrono::{DateTime, Utc};
use filen_types::{api::v3::dir::color::DirColor, fs::Uuid, traits::CowHelpers};
use tokio::sync::Semaphore;

use crate::{
	Error,
	auth::Client,
	connect::ConnectedTargets,
	crypto,
	fs::{
		HasName,
		categories::{DirType, NonRootItemType, Normal, fs::CategoryFS},
		dir::{RemoteDirectory, client_impl::CreateDirOutcome},
		file::{
			BaseFile, FileBuilder, RemoteFile,
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
	util::{MaybeSend, MaybeSendSync},
};

/// Outcome of creating a directory.
#[derive(Debug)]
pub(crate) enum CreatedDir {
	Created(RemoteDirectory),
	/// The server already had a directory with that name there and returned it instead.
	Merged,
}

/// The names a directory holds.
#[derive(Debug, Clone, Default)]
pub(crate) struct ListedNames {
	pub(crate) names: Vec<String>,
	/// Some items' names could not be decrypted, so a name picked to be free among `names` may
	/// still be taken.
	pub(crate) unverified: bool,
}

/// Lists the names of the items in `dir`, a job's destination.
pub(crate) async fn list_dir_names(
	client: &Client,
	dir: &DirType<'_, Normal>,
) -> Result<ListedNames, Error> {
	let (dirs, files) = Normal::list_dir(client, dir, None::<&fn(u64, Option<u64>)>, ()).await?;
	let mut listed = ListedNames::default();
	for name in dirs
		.iter()
		.map(|d| d.name())
		.chain(files.iter().map(|f| f.name()))
	{
		match name {
			Some(name) => listed.names.push(name.to_owned()),
			None => listed.unverified = true,
		}
	}
	Ok(listed)
}

/// What a new file is created as.
#[derive(Debug, Clone)]
pub(crate) struct UploadSpec {
	pub(crate) uuid: Uuid,
	pub(crate) parent: Uuid,
	pub(crate) name: ValidatedName,
	pub(crate) mime: Option<String>,
}

/// The drive operations a job needs. The production implementation is the client; tests use a
/// fake with the real memory semaphore.
pub(crate) trait DriveBackend: MaybeSendSync + 'static {
	type DriveLock: MaybeSendSync + 'static;
	type Upload: MaybeSendSync + 'static;

	/// The client's file-IO memory semaphore, in bytes.
	fn memory(&self) -> Arc<Semaphore>;
	/// Acquires the drive lock, waiting while another client holds it.
	fn acquire_drive_lock(
		&self,
	) -> impl Future<Output = Result<Self::DriveLock, Error>> + MaybeSend;
	fn connected_targets(
		&self,
		dir: Uuid,
	) -> impl Future<Output = Result<ConnectedTargets, Error>> + MaybeSend;
	/// The names of the items in `dir`.
	#[cfg(feature = "archive")]
	fn list_dir_names(
		&self,
		dir: &DirType<'static, Normal>,
	) -> impl Future<Output = Result<ListedNames, Error>> + MaybeSend;
	/// Creates `name` in `parent` under the given `uuid`. The caller holds the drive lock.
	/// Unlike [`Client::create_dir`](crate::auth::Client::create_dir) it does not propagate
	/// the directory, and reports a merge into an existing one instead of returning it.
	fn create_dir_unpropagated(
		&self,
		parent: Uuid,
		uuid: Uuid,
		name: &ValidatedName,
		created: DateTime<Utc>,
	) -> impl Future<Output = Result<CreatedDir, Error>> + MaybeSend;
	fn set_dir_color(
		&self,
		dir: &mut RemoteDirectory,
		color: DirColor<'static>,
	) -> impl Future<Output = Result<(), Error>> + MaybeSend;
	/// Adds a new item to `targets`; returns the operations that failed.
	fn propagate(
		&self,
		targets: &ConnectedTargets,
		item: NonRootItemType<'_, Normal>,
	) -> impl Future<Output = Vec<Error>> + MaybeSend;
	/// Adds a created top-level item, and for a directory everything below it, to `targets`;
	/// returns the operations that failed.
	fn propagate_tree(
		&self,
		targets: &ConnectedTargets,
		item: &NonRootItemType<'static, Normal>,
	) -> impl Future<Output = Vec<Error>> + MaybeSend;
	/// An item of the user's drive, for a job that kept only its uuid: a file a tar's hard link
	/// copies, or a top-level item to propagate again.
	#[cfg(feature = "archive")]
	fn normal_item(
		&self,
		uuid: Uuid,
		is_dir: bool,
	) -> impl Future<Output = Result<NonRootItemType<'static, Normal>, Error>> + MaybeSend;
	fn begin_upload(&self, spec: UploadSpec) -> Self::Upload;
	/// Downloads and decrypts chunk `index` of `file`.
	fn fetch_chunk(
		&self,
		file: &RemoteFileType<'static>,
		index: u64,
	) -> impl Future<Output = Result<Vec<u8>, Error>> + MaybeSend;
	/// Encrypts and uploads the plaintext `data` as chunk `index` of `upload`.
	fn upload_chunk(
		&self,
		upload: &Self::Upload,
		index: u64,
		data: Vec<u8>,
	) -> impl Future<Output = Result<RemoteFileInfo, Error>> + MaybeSend;
	/// Whether a file or directory called `name` exists in `parent`, compared as the server
	/// compares names.
	fn name_exists(
		&self,
		parent: Uuid,
		name: &ValidatedName,
	) -> impl Future<Output = Result<bool, Error>> + MaybeSend;
	/// Registers the uploaded file under `name`. The caller holds the drive lock.
	fn finish_upload(
		&self,
		upload: &Self::Upload,
		name: &ValidatedName,
		completion: UploadCompletion,
		info: RemoteFileInfo,
	) -> impl Future<Output = Result<RemoteFile, Error>> + MaybeSend;
}

pub(crate) struct ClientBackend {
	client: Arc<Client>,
}

impl ClientBackend {
	pub(crate) fn new(client: Arc<Client>) -> Self {
		Self { client }
	}

	#[cfg(feature = "archive")]
	pub(crate) fn client(&self) -> &Client {
		&self.client
	}
}

pub(crate) struct ClientUpload {
	file: BaseFile,
	upload_key: String,
}

impl DriveBackend for ClientBackend {
	type DriveLock = Arc<ResourceLock>;
	type Upload = ClientUpload;

	fn memory(&self) -> Arc<Semaphore> {
		Arc::clone(self.client.client().state().memory_semaphore())
	}

	async fn acquire_drive_lock(&self) -> Result<Self::DriveLock, Error> {
		self.client.lock_drive().await
	}

	async fn connected_targets(&self, dir: Uuid) -> Result<ConnectedTargets, Error> {
		self.client.fetch_connected_targets(dir).await
	}

	#[cfg(feature = "archive")]
	async fn list_dir_names(&self, dir: &DirType<'static, Normal>) -> Result<ListedNames, Error> {
		list_dir_names(&self.client, dir).await
	}

	async fn create_dir_unpropagated(
		&self,
		parent: Uuid,
		uuid: Uuid,
		name: &ValidatedName,
		created: DateTime<Utc>,
	) -> Result<CreatedDir, Error> {
		// The meta owns its name, and the trait lends the engine's.
		let meta = RemoteDirectory::make_meta(name.clone(), created);
		Ok(
			match self.client.post_create_dir(parent, uuid, meta).await? {
				CreateDirOutcome::Created(dir) => CreatedDir::Created(dir),
				CreateDirOutcome::Merged(_) => CreatedDir::Merged,
			},
		)
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
		let items = match self.client.items_with_subtree(item.as_borrowed_cow()).await {
			Ok(items) => items,
			Err(error) => return vec![error],
		};
		self.client.propagate_to_targets(targets, &items).await
	}

	#[cfg(feature = "archive")]
	async fn normal_item(
		&self,
		uuid: Uuid,
		is_dir: bool,
	) -> Result<NonRootItemType<'static, Normal>, Error> {
		Ok(if is_dir {
			NonRootItemType::Dir(Cow::Owned(self.client.get_dir(uuid).await?))
		} else {
			NonRootItemType::File(Cow::Owned(self.client.get_file(uuid).await?))
		})
	}

	fn begin_upload(&self, spec: UploadSpec) -> Self::Upload {
		let mut builder =
			FileBuilder::new_valid_name(spec.name, spec.uuid, spec.parent, &self.client).no_exif();
		if let Some(mime) = spec.mime {
			builder = builder.mime(mime);
		}
		ClientUpload {
			file: builder.build(),
			upload_key: crypto::shared::generate_random_base64_values(32, &mut rand::rng()),
		}
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

	async fn name_exists(&self, parent: Uuid, name: &ValidatedName) -> Result<bool, Error> {
		let (file, dir) = futures::try_join!(
			self.client.inner_file_exists(name, parent),
			self.client.inner_dir_exists(parent, name),
		)?;
		Ok(file.is_some() || dir.is_some())
	}

	async fn finish_upload(
		&self,
		upload: &Self::Upload,
		name: &ValidatedName,
		completion: UploadCompletion,
		info: RemoteFileInfo,
	) -> Result<RemoteFile, Error> {
		// the upload is shared with its chunk uploads; register a copy that carries the final name
		let mut file = upload.file.clone();
		file.root.name = name.clone();
		let response = complete_upload(&self.client, &file, &upload.upload_key, completion).await?;
		Ok(remote_file_from_upload(file, response, info, completion))
	}
}
