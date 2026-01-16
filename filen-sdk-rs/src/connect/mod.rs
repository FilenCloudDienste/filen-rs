use std::{borrow::Cow, fmt::Debug, sync::Arc};

use chrono::{DateTime, Utc};
use filen_types::{
	api::v3::{
		contacts::Contact,
		dir::link::PublicLinkExpiration,
		file::link::edit::FileLinkAction,
		item::{linked::ListedPublicLink, shared::SharedUser},
	},
	auth::FileEncryptionVersion,
	fs::{ObjectType, UuidStr},
	traits::CowHelpers,
};
use fs::{SharedDirectory, SharedFile};
use futures::stream::{FuturesUnordered, StreamExt};
#[cfg(feature = "multi-threaded-crypto")]
use rayon::iter::ParallelIterator;
use serde::{Deserialize, Serialize};

use crate::{
	api,
	auth::{Client, MetaKey},
	crypto::{file::FileKey, shared::MetaCrypter},
	error::{Error, ErrorKind, MetadataWasNotDecryptedError},
	fs::{
		HasMeta, HasMetaExt, HasParent, HasType, HasUUID, NonRootFSObject,
		dir::{HasUUIDContents, RemoteDirectory},
		file::{RemoteFile, meta::FileMeta},
	},
	runtime::{blocking_join, do_cpu_intensive},
	util::{IntoMaybeParallelIterator, MaybeSendBoxFuture},
};

pub mod contacts;
pub mod fs;
#[cfg(any(feature = "wasm-full", feature = "uniffi"))]
pub mod js_impls;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkedFileInfo {
	pub uuid: UuidStr,
	pub name: Option<String>,
	pub mime: Option<String>,
	pub hashed_password: Option<Vec<u8>>,
	pub chunks: u64,
	pub size: u64,
	pub region: String,
	pub bucket: String,
	pub timestamp: DateTime<Utc>,
	pub version: FileEncryptionVersion,
}

trait MakePasswordSaltAndHash {
	fn password(&self) -> &PasswordState;
	fn salt(&self) -> &[u8];

	fn get_password_hash(&self) -> Result<Cow<'_, [u8]>, Error> {
		let password = match self.password() {
			PasswordState::None => None,
			PasswordState::Known(password) => Some(password.as_str()),
			PasswordState::Hashed(password_vec) => {
				return Ok(Cow::Borrowed(password_vec));
			}
		};
		Ok(Cow::Owned(
			crate::crypto::connect::derive_password_for_link(password, self.salt())?,
		))
	}
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(untagged)]
pub enum PasswordState {
	Known(String),
	#[serde(with = "serde_bytes")]
	Hashed(Vec<u8>),
	None,
}

impl Default for PasswordState {
	fn default() -> Self {
		Self::None
	}
}

impl PasswordState {
	fn is_known(&self) -> bool {
		match self {
			PasswordState::None => false,
			PasswordState::Hashed(_) => true,
			PasswordState::Known(_) => true,
		}
	}

	fn is_none(&self) -> bool {
		matches!(self, PasswordState::None)
	}
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(
	feature = "wasm-full",
	derive(tsify::Tsify),
	tsify(into_wasm_abi, from_wasm_abi)
)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[serde(rename_all = "camelCase")]
pub struct FilePublicLink {
	link_uuid: UuidStr,
	#[serde(default, skip_serializing_if = "PasswordState::is_none")]
	#[cfg_attr(feature = "wasm-full", tsify(type = "string | Uint8Array"))]
	password: PasswordState,
	expiration: PublicLinkExpiration,
	downloadable: bool,
	#[serde(with = "serde_bytes")]
	#[cfg_attr(feature = "wasm-full", tsify(type = "Uint8Array"))]
	salt: Vec<u8>,
}

impl FilePublicLink {
	pub(crate) fn new() -> Self {
		Self {
			link_uuid: UuidStr::new_v4(),
			password: PasswordState::None,
			expiration: PublicLinkExpiration::Never,
			downloadable: true,
			salt: rand::random::<[u8; 256]>().to_vec(),
		}
	}

	pub fn uuid(&self) -> UuidStr {
		self.link_uuid
	}

	pub fn set_password(&mut self, password: String) {
		self.password = PasswordState::Known(password);
	}

	pub fn clear_password(&mut self) {
		self.password = PasswordState::None;
	}
	pub fn password(&self) -> &PasswordState {
		&self.password
	}

	pub fn set_expiration(&mut self, expiration: PublicLinkExpiration) {
		self.expiration = expiration;
	}

	pub fn set_downloadable(&mut self, enable_download: bool) {
		self.downloadable = enable_download;
	}
}

impl MakePasswordSaltAndHash for FilePublicLink {
	fn password(&self) -> &PasswordState {
		&self.password
	}

	fn salt(&self) -> &[u8] {
		&self.salt
	}
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(
	feature = "wasm-full",
	derive(tsify::Tsify),
	tsify(into_wasm_abi, from_wasm_abi)
)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[serde(rename_all = "camelCase")]
pub struct DirPublicLink {
	link_uuid: UuidStr,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	link_key: Option<MetaKey>,
	#[serde(default, skip_serializing_if = "PasswordState::is_none")]
	#[cfg_attr(feature = "wasm-full", tsify(type = "string | Uint8Array"))]
	password: PasswordState,
	expiration: PublicLinkExpiration,
	enable_download: bool,
	#[cfg_attr(feature = "wasm-full", tsify(type = "Uint8Array"))]
	#[serde(with = "serde_bytes", default, skip_serializing_if = "Option::is_none")]
	salt: Option<Vec<u8>>,
}

impl DirPublicLink {
	pub(crate) fn new(link_key: MetaKey) -> Self {
		Self {
			link_uuid: UuidStr::new_v4(),
			link_key: Some(link_key),
			password: PasswordState::None,
			expiration: PublicLinkExpiration::Never,
			enable_download: true,
			salt: None,
		}
	}

	pub fn uuid(&self) -> UuidStr {
		self.link_uuid
	}

	pub fn set_password(&mut self, password: String) {
		match self.salt {
			Some(ref mut salt) => {
				if salt.len() != 256 {
					// migrate links to argon2id salt
					*salt = rand::random::<[u8; 256]>().to_vec()
				}
			}
			ref mut none => {
				*none = Some(rand::random::<[u8; 256]>().to_vec());
			}
		}
		self.password = PasswordState::Known(password);
	}

	pub fn clear_password(&mut self) {
		self.password = PasswordState::None;
	}

	pub fn set_expiration(&mut self, expiration: PublicLinkExpiration) {
		self.expiration = expiration;
	}

	pub fn set_enable_download(&mut self, enable_download: bool) {
		self.enable_download = enable_download;
	}

	pub(crate) fn crypter(&self) -> Option<&impl MetaCrypter> {
		self.link_key.as_ref()
	}
}

impl MakePasswordSaltAndHash for DirPublicLink {
	fn password(&self) -> &PasswordState {
		&self.password
	}

	fn salt(&self) -> &[u8] {
		self.salt.as_deref().unwrap_or(&[])
	}
}

impl Client {
	async fn update_shared_item_meta<I>(&self, item: &I, user: &SharedUser<'_>) -> Result<(), Error>
	where
		I: HasMeta + HasUUID + Debug,
	{
		api::v3::item::shared::rename::post(
			self.client(),
			&api::v3::item::shared::rename::Request {
				uuid: *item.uuid(),
				receiver_id: user.id,
				metadata: item
					.get_rsa_encrypted_meta(&user.public_key)
					.await
					.ok_or(MetadataWasNotDecryptedError)?,
			},
		)
		.await
	}

	async fn update_linked_item_meta<I>(
		&self,
		item: &I,
		link_uuid: UuidStr,
		crypter: &impl MetaCrypter,
	) -> Result<(), Error>
	where
		I: HasMeta + HasUUID,
	{
		api::v3::item::linked::rename::post(
			self.client(),
			&api::v3::item::linked::rename::Request {
				uuid: *item.uuid(),
				link_uuid,
				metadata: item
					.get_encrypted_meta(crypter)
					.await
					.ok_or(MetadataWasNotDecryptedError)?,
			},
		)
		.await
	}

	pub(crate) async fn update_maybe_connected_item<I>(&self, item: &I) -> Result<(), Error>
	where
		I: HasMeta + HasUUID + Send + Sync + Debug,
	{
		let (linked, shared) = futures::try_join!(
			async {
				api::v3::item::linked::post(
					self.client(),
					&api::v3::item::linked::Request { uuid: *item.uuid() },
				)
				.await
			},
			async {
				api::v3::item::shared::post(
					self.client(),
					&api::v3::item::shared::Request { uuid: *item.uuid() },
				)
				.await
			},
		)?;

		let mut futures = FuturesUnordered::new();
		for link in linked.links {
			futures.push(Box::pin(async move {
				let crypter = self
					.decrypt_meta_key(&link.link_key)
					.await
					.map_err(|_| MetadataWasNotDecryptedError)?;
				self.update_linked_item_meta(item, link.link_uuid, &crypter)
					.await
			}) as MaybeSendBoxFuture<'_, Result<(), Error>>);
		}
		for user in shared.users {
			futures.push(
				Box::pin(async move { self.update_shared_item_meta(item, &user).await })
					as MaybeSendBoxFuture<'_, Result<(), Error>>,
			);
		}

		while let Some(result) = futures.next().await {
			match result {
				Ok(_) => continue,
				Err(e) => return Err(e),
			}
		}
		Ok(())
	}

	pub(crate) async fn update_item_with_maybe_connected_parent(
		&self,
		item: impl Into<NonRootFSObject<'_>>,
	) -> Result<(), Error> {
		let item = item.into();
		let uuid = (*item.parent()).try_into()?;

		let (linked, shared, items_to_process) = futures::try_join!(
			async {
				api::v3::item::linked::post(self.client(), &api::v3::item::linked::Request { uuid })
					.await
			},
			async {
				api::v3::item::shared::post(self.client(), &api::v3::item::shared::Request { uuid })
					.await
			},
			async move {
				if let NonRootFSObject::Dir(dir) = item {
					let (dirs, files) = self.list_dir_recursive_no_callback(dir.as_ref()).await?;
					Ok(std::iter::once(NonRootFSObject::Dir(dir))
						.chain(dirs.into_iter().map(Into::into))
						.chain(files.into_iter().map(Into::into))
						.collect::<Vec<_>>())
				} else {
					Ok(vec![item])
				}
			}
		)?;

		let mut futures = FuturesUnordered::new();

		for link in linked.links {
			let link = Arc::new(link);
			let crypter = Arc::new(
				self.decrypt_meta_key(&link.link_key)
					.await
					.map_err(|_| MetadataWasNotDecryptedError)?,
			);
			for item in &items_to_process {
				let link = link.clone();
				let crypter = crypter.clone();
				futures.push(Box::pin(async move {
					self.add_item_to_directory_link(item, link.as_ref(), crypter.as_ref())
						.await
				}) as MaybeSendBoxFuture<'_, Result<(), Error>>);
			}
		}

		for user in shared.users {
			let user = Arc::new(user);
			for item in &items_to_process {
				let user = user.clone();
				futures.push(Box::pin(
					async move { self.inner_share_item(item, user.as_ref()).await },
				) as MaybeSendBoxFuture<'_, Result<(), Error>>);
			}
		}

		while let Some(result) = futures.next().await {
			match result {
				Ok(_) => continue,
				Err(e) => return Err(e),
			}
		}
		Ok(())
	}

	pub(crate) async fn add_item_to_directory_link<I>(
		&self,
		item: &I,
		link: &ListedPublicLink<'_>,
		link_crypter: &impl MetaCrypter,
	) -> Result<(), Error>
	where
		I: HasParent + HasMeta + HasUUID + HasType + ?Sized,
	{
		api::v3::dir::link::add::post(
			self.client(),
			&api::v3::dir::link::add::Request {
				uuid: *item.uuid(),
				parent: Some((*item.parent()).try_into()?),
				link_uuid: link.link_uuid,
				r#type: item.object_type(),
				metadata: item
					.get_encrypted_meta(link_crypter)
					.await
					.ok_or(MetadataWasNotDecryptedError)?,
				key: link.link_key.as_borrowed_cow(),
				expiration: PublicLinkExpiration::Never,
			},
		)
		.await?;
		Ok(())
	}

	pub async fn public_link_dir<F>(
		&self,
		dir: &RemoteDirectory,
		progress_callback: Arc<F>,
	) -> Result<DirPublicLink, Error>
	where
		F: Fn(u64, Option<u64>) + Send + Sync + 'static,
	{
		let public_link = DirPublicLink::new(self.make_meta_key());
		let (dirs, files) = self.list_dir_recursive(dir, progress_callback).await?;
		let link = ListedPublicLink {
			link_uuid: public_link.link_uuid,
			link_key: self
				.encrypt_meta_key(
					public_link
						.link_key
						.as_ref()
						.ok_or(MetadataWasNotDecryptedError)?,
				)
				.await,
		};

		let mut futures = FuturesUnordered::new();

		// link main dir
		let link = &link;
		let key = public_link.link_key.as_ref();
		let key = key.ok_or(MetadataWasNotDecryptedError)?;
		futures.push(Box::pin(async move {
			api::v3::dir::link::add::post(
				self.client(),
				&api::v3::dir::link::add::Request {
					uuid: *dir.uuid(),
					parent: None,
					link_uuid: public_link.link_uuid,
					r#type: ObjectType::Dir,
					metadata: dir
						.get_encrypted_meta(key)
						.await
						.ok_or(MetadataWasNotDecryptedError)?,
					key: link.link_key.as_borrowed_cow(),
					expiration: PublicLinkExpiration::Never,
				},
			)
			.await
		}) as MaybeSendBoxFuture<'_, Result<(), Error>>);

		// link descendants
		for dir in dirs {
			futures.push(Box::pin(
				async move { self.add_item_to_directory_link(&dir, link, key).await },
			) as MaybeSendBoxFuture<'_, Result<(), Error>>);
		}
		for file in files {
			futures.push(Box::pin(
				async move { self.add_item_to_directory_link(&file, link, key).await },
			) as MaybeSendBoxFuture<'_, Result<(), Error>>);
		}

		while let Some(result) = futures.next().await {
			match result {
				Ok(_) => continue,
				Err(e) => return Err(e),
			}
		}

		std::mem::drop(futures);
		Ok(public_link)
	}

	pub async fn public_link_file(&self, file: &RemoteFile) -> Result<FilePublicLink, Error> {
		let file_link = FilePublicLink::new();

		api::v3::file::link::edit::post(
			self.client(),
			&api::v3::file::link::edit::Request {
				uuid: file_link.link_uuid,
				file_uuid: *file.uuid(),
				expiration: PublicLinkExpiration::Never,
				password: false,
				// why does this just hash_name empty? Who knows,
				// we should fix this with the v4 api
				password_hashed: Cow::Borrowed(&file_link.get_password_hash()?),
				salt: Cow::Borrowed(&file_link.salt),
				download_btn: true,
				r#type: FileLinkAction::Enable,
			},
		)
		.await?;

		Ok(file_link)
	}

	pub async fn update_dir_link(
		&self,
		dir: &RemoteDirectory,
		link: &DirPublicLink,
	) -> Result<(), Error> {
		api::v3::dir::link::edit::post(
			self.client(),
			&api::v3::dir::link::edit::Request {
				uuid: *dir.uuid(),
				expiration: link.expiration,
				password: link.password().is_known(),
				password_hashed: Cow::Borrowed(&link.get_password_hash()?),
				salt: Cow::Borrowed(link.salt()),
				download_btn: link.enable_download,
			},
		)
		.await?;

		Ok(())
	}

	pub async fn update_file_link(
		&self,
		file: &RemoteFile,
		link: &FilePublicLink,
	) -> Result<(), Error> {
		api::v3::file::link::edit::post(
			self.client(),
			&api::v3::file::link::edit::Request {
				uuid: link.link_uuid,
				file_uuid: *file.uuid(),
				expiration: link.expiration,
				password: link.password().is_known(),
				password_hashed: Cow::Borrowed(&link.get_password_hash()?),
				salt: Cow::Borrowed(link.salt()),
				download_btn: link.downloadable,
				r#type: FileLinkAction::Enable,
			},
		)
		.await?;
		Ok(())
	}

	pub async fn get_file_link_status(
		&self,
		file: &RemoteFile,
	) -> Result<Option<FilePublicLink>, Error> {
		let response = api::v3::file::link::status::post(
			self.client(),
			&api::v3::file::link::status::Request { uuid: *file.uuid() },
		)
		.await?;

		let link_status = match response.0 {
			None => {
				return Ok(None);
			}
			Some(link_status) => link_status,
		};

		let password_response = api::v3::file::link::password::post(
			self.client(),
			&api::v3::file::link::password::Request {
				uuid: link_status.uuid,
			},
		)
		.await?;

		let password = match link_status.password {
			Some(password) => PasswordState::Hashed(password.into_owned()),
			None => PasswordState::None,
		};

		Ok(Some(FilePublicLink {
			link_uuid: link_status.uuid,
			password,
			expiration: link_status.expiration_text,
			downloadable: link_status.download_btn,
			salt: password_response.salt.into_owned(),
		}))
	}

	// doesn't require auth, should be moved to a different module in the future
	pub async fn get_linked_file(
		&self,
		link: &FilePublicLink,
		link_key: Cow<'_, str>,
	) -> Result<LinkedFileInfo, Error> {
		let response = api::v3::file::link::info::post(
			self.client(),
			&api::v3::file::link::info::Request {
				uuid: link.link_uuid,
				password: Cow::Borrowed(&link.get_password_hash()?),
			},
		)
		.await?;

		let crypter = FileKey::from_string_and_meta(link_key, &response.mime)?.to_meta_key()?;

		let (decrypted_size, decrypted_name, decrypted_mime) = futures::join!(
			crypter.decrypt_meta(&response.size),
			crypter.decrypt_meta(&response.name),
			crypter.decrypt_meta(&response.mime),
		);

		let decrypted_size = decrypted_size?;
		let size = decrypted_size.parse::<u64>().map_err(|_| {
			Error::custom(
				ErrorKind::Conversion,
				format!("Failed to parse size: {decrypted_size}"),
			)
		})?;

		let file_info = LinkedFileInfo {
			uuid: response.uuid,
			name: decrypted_name.ok(),
			mime: decrypted_mime.ok(),
			hashed_password: response.password.map(|v| v.into_owned()),
			chunks: response.chunks,
			size,
			region: response.region.into_owned(),
			bucket: response.bucket.into_owned(),
			timestamp: response.timestamp,
			version: response.version,
		};
		Ok(file_info)
	}

	pub async fn get_dir_link_status(
		&self,
		dir: &RemoteDirectory,
	) -> Result<Option<DirPublicLink>, Error> {
		let response = api::v3::dir::link::status::post(
			self.client(),
			&api::v3::dir::link::status::Request { uuid: *dir.uuid() },
		)
		.await?;

		let link_status = match response.0 {
			None => {
				return Ok(None);
			}
			Some(link_status) => link_status,
		};

		let (info_response, decrypted_link_key) = futures::join!(
			async {
				api::v3::dir::link::info::post(
					self.client(),
					&api::v3::dir::link::info::Request {
						uuid: link_status.uuid,
					},
				)
				.await
			},
			self.decrypt_meta_key(&link_status.key)
		);

		let info_response = info_response?;
		let password = match link_status.password {
			Some(password) => PasswordState::Hashed(password.into_owned()),
			None => PasswordState::None,
		};
		Ok(Some(DirPublicLink {
			link_uuid: link_status.uuid,
			link_key: decrypted_link_key.ok(),
			password,
			expiration: link_status.expiration_text,
			enable_download: link_status.download_btn,
			salt: info_response.salt.map(|v| v.into_owned()),
		}))
	}

	pub async fn list_linked_dir(
		&self,
		dir: &dyn HasUUIDContents,
		link: &DirPublicLink,
	) -> Result<(Vec<RemoteDirectory>, Vec<RemoteFile>), Error> {
		let response = api::v3::dir::link::content::post(
			self.client(),
			&api::v3::dir::link::content::Request {
				uuid: link.link_uuid,
				password: Cow::Borrowed(&link.get_password_hash()?),
				parent: *dir.uuid(),
			},
		)
		.await?;

		let crypter = link.crypter().ok_or(MetadataWasNotDecryptedError)?;

		do_cpu_intensive(|| {
			let (dirs, files) = blocking_join!(
				|| response
					.dirs
					.into_maybe_par_iter()
					.map(|d| {
						RemoteDirectory::blocking_from_encrypted(
							d.uuid,
							d.parent.into(),
							d.color,
							false,
							d.timestamp,
							d.metadata,
							crypter,
						)
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

	pub async fn remove_dir_link(&self, link: DirPublicLink) -> Result<(), Error> {
		api::v3::dir::link::remove::post(
			self.client(),
			&api::v3::dir::link::remove::Request {
				uuid: link.link_uuid,
			},
		)
		.await?;
		Ok(())
	}

	async fn inner_share_item<I>(&self, item: &I, user: &SharedUser<'_>) -> Result<(), Error>
	where
		I: HasParent + HasMeta + HasUUID + HasType,
	{
		api::v3::item::share::post(
			self.client(),
			&api::v3::item::share::Request {
				uuid: *item.uuid(),
				parent: Some((*item.parent()).try_into()?),
				email: user.email.as_borrowed_cow(),
				r#type: item.object_type(),
				metadata: item
					.get_rsa_encrypted_meta(&user.public_key)
					.await
					.ok_or(MetadataWasNotDecryptedError)?,
			},
		)
		.await?;
		Ok(())
	}

	pub async fn share_dir<F>(
		&self,
		dir: &RemoteDirectory,
		client: &Contact<'_>,
		progress_callback: Arc<F>,
	) -> Result<(), Error>
	where
		F: Fn(u64, Option<u64>) + 'static + Send + Sync,
	{
		let (dirs, files) = self.list_dir_recursive(dir, progress_callback).await?;

		let shared_user = client.into();
		let shared_user = &shared_user;

		let mut futures = FuturesUnordered::new();

		futures.push(Box::pin(async move {
			api::v3::item::share::post(
				self.client(),
				&api::v3::item::share::Request {
					uuid: *dir.uuid(),
					parent: None,
					email: client.email.as_borrowed_cow(),
					r#type: ObjectType::Dir,
					metadata: dir
						.get_rsa_encrypted_meta(&client.public_key)
						.await
						.ok_or(MetadataWasNotDecryptedError)?,
				},
			)
			.await
		}) as MaybeSendBoxFuture<'_, Result<(), Error>>);

		for dir in dirs {
			futures.push(
				Box::pin(async move { self.inner_share_item(&dir, shared_user).await })
					as MaybeSendBoxFuture<'_, Result<(), Error>>,
			);
		}

		for file in files {
			futures.push(
				Box::pin(async move { self.inner_share_item(&file, shared_user).await })
					as MaybeSendBoxFuture<'_, Result<(), Error>>,
			);
		}
		while let Some(result) = futures.next().await {
			match result {
				Ok(_) => continue,
				Err(e) => return Err(e),
			}
		}
		std::mem::drop(futures);
		Ok(())
	}

	pub async fn share_file(&self, file: &RemoteFile, contact: &Contact<'_>) -> Result<(), Error> {
		api::v3::item::share::post(
			self.client(),
			&api::v3::item::share::Request {
				uuid: *file.uuid(),
				parent: None,
				email: contact.email.as_borrowed_cow(),
				r#type: ObjectType::File,
				metadata: file
					.get_rsa_encrypted_meta(&contact.public_key)
					.await
					.ok_or(MetadataWasNotDecryptedError)?,
			},
		)
		.await
	}

	pub(crate) async fn inner_list_out_shared(
		&self,
		dir: Option<&impl HasUUIDContents>,
		contact: Option<&Contact<'_>>,
	) -> Result<(Vec<SharedDirectory>, Vec<SharedFile>), Error> {
		let response = api::v3::shared::out::post(
			self.client(),
			&api::v3::shared::out::Request {
				uuid: dir.map(|d| *d.uuid()),
				receiver_id: contact.map(|u| u.user_id),
			},
		)
		.await?;

		let crypter = self.crypter();

		do_cpu_intensive(|| {
			let (dirs, files) = blocking_join!(
				|| {
					response
						.dirs
						.into_maybe_par_iter()
						.map(|d| SharedDirectory::blocking_from_shared_out(d, &*crypter))
						.collect::<Result<Vec<_>, _>>()
				},
				|| {
					response
						.files
						.into_maybe_par_iter()
						.map(|f| SharedFile::blocking_from_shared_out(f, &*crypter))
						.collect::<Result<Vec<_>, _>>()
				}
			);
			Ok((dirs?, files?))
		})
		.await
	}

	pub async fn list_out_shared(
		&self,
		contact: Option<&Contact<'_>>,
	) -> Result<(Vec<SharedDirectory>, Vec<SharedFile>), Error> {
		self.inner_list_out_shared(None::<&RemoteDirectory>, contact)
			.await
	}

	pub async fn list_out_shared_dir(
		&self,
		dir: &impl HasUUIDContents,
		contact: &Contact<'_>,
	) -> Result<(Vec<SharedDirectory>, Vec<SharedFile>), Error> {
		self.inner_list_out_shared(Some(dir), Some(contact)).await
	}

	pub(crate) async fn inner_list_in_shared(
		&self,
		dir: Option<&impl HasUUIDContents>,
	) -> Result<(Vec<SharedDirectory>, Vec<SharedFile>), Error> {
		let response = api::v3::shared::r#in::post(
			self.client(),
			&api::v3::shared::r#in::Request {
				uuid: dir.map(|d| *d.uuid()),
			},
		)
		.await?;

		let priv_key = self.private_key();

		do_cpu_intensive(|| {
			let (dirs, files) = blocking_join!(
				|| {
					response
						.dirs
						.into_maybe_par_iter()
						.map(|d| SharedDirectory::blocking_from_shared_in(d, priv_key))
						.collect::<Result<Vec<_>, _>>()
				},
				|| {
					response
						.files
						.into_maybe_par_iter()
						.map(|f| SharedFile::blocking_from_shared_in(f, priv_key))
						.collect::<Result<Vec<_>, _>>()
				}
			);
			Ok((dirs?, files?))
		})
		.await
	}

	pub async fn list_in_shared(&self) -> Result<(Vec<SharedDirectory>, Vec<SharedFile>), Error> {
		self.inner_list_in_shared(None::<&RemoteDirectory>).await
	}

	pub async fn list_in_shared_dir(
		&self,
		dir: &impl HasUUIDContents,
	) -> Result<(Vec<SharedDirectory>, Vec<SharedFile>), Error> {
		self.inner_list_in_shared(Some(dir)).await
	}

	pub async fn remove_shared_link_in(&self, uuid: UuidStr) -> Result<(), Error> {
		api::v3::item::shared::r#in::remove::post(
			self.client(),
			&api::v3::item::shared::r#in::remove::Request { uuid },
		)
		.await?;
		Ok(())
	}

	pub async fn remove_shared_link_out(
		&self,
		uuid: UuidStr,
		receiver_id: u64,
	) -> Result<(), Error> {
		api::v3::item::shared::out::remove::post(
			self.client(),
			&api::v3::item::shared::out::remove::Request { uuid, receiver_id },
		)
		.await?;
		Ok(())
	}
}
