//! What a resuming upload asks of the server, behind a trait so its policy runs against a fake.

use std::{borrow::Cow, sync::Arc};

use chrono::{DateTime, Utc};
use filen_types::fs::Uuid;

use crate::{
	Error, ErrorKind,
	auth::{Client, http::SharedClientState},
	connect::ParentTargets,
	crypto::{file::FileKey, shared::MetaCrypter},
	fs::{
		categories::NonRootItemType,
		file::{
			BaseFile, RemoteFile,
			write::{
				RemoteFileInfo, UploadCompletion, encrypt_and_upload_chunk_data,
				register_upload_under_lock, remote_file_from_upload,
			},
		},
	},
};

/// The server side of a resuming upload.
pub(crate) trait ResumeBackend: Sync {
	/// The client state whose memory budget the chunks are read into.
	fn state(&self) -> &SharedClientState;

	/// The account's metadata crypter, which journals are sealed with.
	fn crypter(&self) -> Arc<impl MetaCrypter + 'static>;

	/// A new file key, for an upload started over.
	fn make_file_key(&self) -> FileKey;

	fn now(&self) -> DateTime<Utc>;

	/// Encrypts `data` and uploads it as chunk `idx` of `file`, handing back the buffer, cleared,
	/// and where the server stored it.
	fn upload_chunk(
		&self,
		file: &BaseFile,
		upload_key: &str,
		idx: u64,
		data: Vec<u8>,
	) -> impl Future<Output = Result<(Vec<u8>, RemoteFileInfo), Error>> + Send;

	/// The file `uuid`, if the server has it.
	fn find_file(
		&self,
		uuid: Uuid,
	) -> impl Future<Output = Result<Option<RemoteFile>, Error>> + Send;

	/// Registers the upload of `file`, whose chunks are all on the server, and adds it to the links
	/// and shares of its parent, under the drive lock.
	fn register(
		&self,
		file: &BaseFile,
		upload_key: &str,
		completion: UploadCompletion,
		info: RemoteFileInfo,
	) -> impl Future<Output = Result<RemoteFile, Error>> + Send;

	/// Adds `file`, registered by an earlier upload whose answer was lost, to the links and shares
	/// of its parent it is not in yet, under the drive lock.
	fn propagate_existing(
		&self,
		file: &RemoteFile,
	) -> impl Future<Output = Result<(), Error>> + Send;
}

/// [`ResumeBackend`] over a logged-in client.
pub(crate) struct ClientResumeBackend<'a> {
	pub(crate) client: &'a Client,
}

impl ResumeBackend for ClientResumeBackend<'_> {
	fn state(&self) -> &SharedClientState {
		self.client.client().state()
	}

	fn crypter(&self) -> Arc<impl MetaCrypter + 'static> {
		self.client.crypter()
	}

	fn make_file_key(&self) -> FileKey {
		self.client.make_file_key()
	}

	fn now(&self) -> DateTime<Utc> {
		Utc::now()
	}

	async fn upload_chunk(
		&self,
		file: &BaseFile,
		upload_key: &str,
		idx: u64,
		data: Vec<u8>,
	) -> Result<(Vec<u8>, RemoteFileInfo), Error> {
		encrypt_and_upload_chunk_data(self.client, file, upload_key, idx, data).await
	}

	async fn find_file(&self, uuid: Uuid) -> Result<Option<RemoteFile>, Error> {
		match self.client.get_file(uuid).await {
			Ok(file) => Ok(Some(file)),
			Err(e) if e.kind() == ErrorKind::FileNotFound => Ok(None),
			Err(e) => Err(e),
		}
	}

	async fn register(
		&self,
		file: &BaseFile,
		upload_key: &str,
		completion: UploadCompletion,
		info: RemoteFileInfo,
	) -> Result<RemoteFile, Error> {
		let lock = self.client.lock_drive().await?;
		let (response, lock) =
			register_upload_under_lock(self.client, lock, file, upload_key, completion).await?;
		// The caller keeps `file` to start the upload over if registering fails.
		let remote = remote_file_from_upload(file.clone(), response, info, completion);
		// `lock` is held until the file is added to its parent's links and shares.
		self.client
			.update_item_with_maybe_connected_parent(
				&lock,
				NonRootItemType::File(Cow::Borrowed(&remote)),
				ParentTargets::All,
			)
			.await?;
		Ok(remote)
	}

	async fn propagate_existing(&self, file: &RemoteFile) -> Result<(), Error> {
		let lock = self.client.lock_drive().await?;
		self.client
			.update_item_with_maybe_connected_parent(
				&lock,
				NonRootItemType::File(Cow::Borrowed(file)),
				ParentTargets::Missing,
			)
			.await
	}
}
