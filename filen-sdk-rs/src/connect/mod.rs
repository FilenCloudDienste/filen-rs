use std::{borrow::Cow, fmt::Debug, pin::Pin, sync::Arc};

use chrono::{DateTime, Utc};
use filen_types::{
	api::v3::{
		contacts::Contact,
		dir::link::PublicLinkExpiration,
		file::link::edit::FileLinkAction,
		item::{linked::ListedPublicLink, shared::SharedUser},
	},
	auth::FileEncryptionVersion,
	crypto::rsa::{EncodedPublicKey, RSAEncryptedString},
	fs::ObjectType,
};
use fs::{SharedDirectory, SharedFile};
use futures::{
	future::BoxFuture,
	stream::{self, FuturesUnordered, StreamExt},
};
use rsa::RsaPublicKey;
use uuid::Uuid;

use crate::{
	api,
	auth::{Client, MetaKey},
	consts::MAX_SMALL_PARALLEL_REQUESTS,
	crypto::{error::ConversionError, shared::MetaCrypter},
	error::Error,
	fs::{
		HasMeta, HasMetaExt, HasParent, HasType, HasUUID, NonRootFSObject,
		dir::{HasContents, RemoteDirectory},
		file::{RemoteFile, meta::FileMeta},
	},
};

pub mod contacts;
pub mod fs;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct User {
	email: String,
	public_key: RsaPublicKey,
	id: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkedFileInfo {
	pub uuid: Uuid,
	pub name: String,
	pub mime: String,
	pub hashed_password: Option<Vec<u8>>,
	pub chunks: u64,
	pub size: u64,
	pub region: String,
	pub bucket: String,
	pub timestamp: DateTime<Utc>,
	pub version: FileEncryptionVersion,
}

impl TryFrom<SharedUser<'_>> for User {
	type Error = ConversionError;
	fn try_from(shared_user: SharedUser) -> Result<Self, Self::Error> {
		Ok(Self {
			email: shared_user.email.into_owned(),
			public_key: RsaPublicKey::try_from(shared_user.public_key.as_ref())?,
			id: shared_user.id,
		})
	}
}

impl User {
	pub fn new(
		email: String,
		public_key: &EncodedPublicKey,
		id: u64,
	) -> Result<Self, ConversionError> {
		Ok(Self {
			email,
			public_key: RsaPublicKey::try_from(public_key)?,
			id,
		})
	}

	pub fn email(&self) -> &str {
		&self.email
	}

	pub fn public_encrypt(&self, data: &[u8]) -> Result<RSAEncryptedString, rsa::Error> {
		crate::crypto::rsa::encrypt_with_public_key(&self.public_key, data)
	}
}

trait MakePasswordSaltAndHash {
	fn password(&self) -> &PasswordState;
	fn salt(&self) -> &[u8];

	fn get_password_hash(&self) -> Result<Cow<'_, Vec<u8>>, Error> {
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PasswordState {
	None,
	Known(String),
	Hashed(Vec<u8>),
}

impl PasswordState {
	fn is_known(&self) -> bool {
		match self {
			PasswordState::None => false,
			PasswordState::Hashed(_) => true,
			PasswordState::Known(_) => true,
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilePublicLink {
	link_uuid: Uuid,
	password: PasswordState,
	expiration: PublicLinkExpiration,
	downloadable: bool,
	salt: Vec<u8>,
}

impl FilePublicLink {
	pub(crate) fn new() -> Self {
		Self {
			link_uuid: Uuid::new_v4(),
			password: PasswordState::None,
			expiration: PublicLinkExpiration::Never,
			downloadable: true,
			salt: rand::random::<[u8; 256]>().to_vec(),
		}
	}

	pub fn uuid(&self) -> Uuid {
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirPublicLink {
	link_uuid: Uuid,
	link_key: MetaKey,
	password: PasswordState,
	expiration: PublicLinkExpiration,
	enable_download: bool,
	salt: Option<Vec<u8>>,
}

impl DirPublicLink {
	pub(crate) fn new(link_key: MetaKey) -> Self {
		Self {
			link_uuid: Uuid::new_v4(),
			link_key,
			password: PasswordState::None,
			expiration: PublicLinkExpiration::Never,
			enable_download: true,
			salt: None,
		}
	}

	pub fn uuid(&self) -> Uuid {
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

	pub(crate) fn crypter(&self) -> &impl MetaCrypter {
		&self.link_key
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
	async fn update_shared_item_meta<I>(&self, item: &I, user: &User) -> Result<(), Error>
	where
		I: HasMeta + HasUUID + Debug,
	{
		api::v3::item::shared::rename::post(
			self.client(),
			&api::v3::item::shared::rename::Request {
				uuid: item.uuid(),
				receiver_id: user.id,
				metadata: Cow::Borrowed(
					&item
						.get_rsa_encrypted_meta(&user.public_key)
						.map_err(|e| Error::Custom(e.to_string()))?,
				),
			},
		)
		.await
	}

	async fn update_linked_item_meta<I>(
		&self,
		item: &I,
		link_uuid: Uuid,
		crypter: &impl MetaCrypter,
	) -> Result<(), Error>
	where
		I: HasMeta + HasUUID,
	{
		api::v3::item::linked::rename::post(
			self.client(),
			&api::v3::item::linked::rename::Request {
				uuid: item.uuid(),
				link_uuid,
				metadata: Cow::Borrowed(&item.get_encrypted_meta(crypter)?),
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
					&api::v3::item::linked::Request { uuid: item.uuid() },
				)
				.await
			},
			async {
				api::v3::item::shared::post(
					self.client(),
					&api::v3::item::shared::Request { uuid: item.uuid() },
				)
				.await
			},
		)?;

		let futures = FuturesUnordered::new();
		for link in linked.links {
			futures.push(Box::pin(async move {
				let crypter = self.decrypt_meta_key(&link.link_key)?;
				self.update_linked_item_meta(item, link.link_uuid, &crypter)
					.await
			})
				as Pin<
					Box<dyn std::future::Future<Output = Result<(), Error>> + Send>,
				>);
		}
		for user in shared.users {
			futures.push(Box::pin(async move {
				let user = user.try_into()?;
				self.update_shared_item_meta(item, &user).await
			})
				as Pin<
					Box<dyn std::future::Future<Output = Result<(), Error>> + Send>,
				>);
		}

		let mut stream = futures
			.map(|v| async move { v }) // type coercion
			.buffer_unordered(MAX_SMALL_PARALLEL_REQUESTS);

		while let Some(result) = stream.next().await {
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
		let uuid = item.parent();

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
					let (dirs, files) = self.list_dir_recursive(dir.as_ref()).await?;
					Ok(std::iter::once(NonRootFSObject::Dir(dir))
						.chain(dirs.into_iter().map(Into::into))
						.chain(files.into_iter().map(Into::into))
						.collect::<Vec<_>>())
				} else {
					Ok(vec![item])
				}
			}
		)?;

		let futures = FuturesUnordered::new();

		for link in linked.links {
			let link = Arc::new(link);
			let crypter = Arc::new(self.decrypt_meta_key(&link.link_key)?);
			for item in &items_to_process {
				let link = link.clone();
				let crypter = crypter.clone();
				futures.push(Box::pin(async move {
					self.add_item_to_directory_link(item, link.as_ref(), crypter.as_ref())
						.await
				}) as BoxFuture<'_, Result<(), Error>>);
			}
		}

		for user in shared.users {
			let user: Arc<User> = Arc::new(user.try_into()?);
			for item in &items_to_process {
				let user = user.clone();
				futures.push(Box::pin(
					async move { self.inner_share_item(item, user.as_ref()).await },
				) as BoxFuture<'_, Result<(), Error>>);
			}
		}

		let mut stream = futures
			.map(|v| async move { v }) // type coercion
			.buffer_unordered(MAX_SMALL_PARALLEL_REQUESTS);

		while let Some(result) = stream.next().await {
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
				uuid: item.uuid(),
				parent: Some(item.parent()),
				link_uuid: link.link_uuid,
				r#type: item.object_type(),
				metadata: Cow::Borrowed(&item.get_encrypted_meta(link_crypter)?),
				key: Cow::Borrowed(&link.link_key),
				expiration: PublicLinkExpiration::Never,
			},
		)
		.await?;
		Ok(())
	}

	pub async fn public_link_dir(&self, dir: &RemoteDirectory) -> Result<DirPublicLink, Error> {
		let public_link = DirPublicLink::new(self.make_meta_key());
		let (dirs, files) = self.list_dir_recursive(dir).await?;
		let link = ListedPublicLink {
			link_uuid: public_link.link_uuid,
			link_key: Cow::Owned(self.encrypt_meta_key(&public_link.link_key)?),
		};

		// link main dir
		let future = {
			let link = &link;
			let key = &public_link.link_key;
			Box::pin(async move {
				api::v3::dir::link::add::post(
					self.client(),
					&api::v3::dir::link::add::Request {
						uuid: dir.uuid(),
						parent: None,
						link_uuid: public_link.link_uuid,
						r#type: ObjectType::Dir,
						metadata: Cow::Borrowed(&dir.get_encrypted_meta(key)?),
						key: Cow::Borrowed(&link.link_key),
						expiration: PublicLinkExpiration::Never,
					},
				)
				.await
			}) as Pin<Box<dyn std::future::Future<Output = Result<(), Error>> + Send>>
		};

		// link descendants
		let mut stream = stream::iter(
			std::iter::once(future).chain(
				dirs.into_iter()
					.map(|dir| {
						let link = &link;
						let key = &public_link.link_key;
						Box::pin(
							async move { self.add_item_to_directory_link(&dir, link, key).await },
						)
							as Pin<Box<dyn std::future::Future<Output = Result<(), Error>> + Send>>
					})
					.chain(files.into_iter().map(|f| {
						let link = &link;
						let key = &public_link.link_key;
						Box::pin(
							async move { self.add_item_to_directory_link(&f, link, key).await },
						)
							as Pin<Box<dyn std::future::Future<Output = Result<(), Error>> + Send>>
					})),
			),
		)
		.buffer_unordered(MAX_SMALL_PARALLEL_REQUESTS);

		while let Some(result) = stream.next().await {
			match result {
				Ok(_) => continue,
				Err(e) => return Err(e),
			}
		}

		std::mem::drop(stream);
		Ok(public_link)
	}

	pub async fn public_link_file(&self, file: &RemoteFile) -> Result<FilePublicLink, Error> {
		let file_link = FilePublicLink::new();

		api::v3::file::link::edit::post(
			self.client(),
			&api::v3::file::link::edit::Request {
				uuid: file_link.link_uuid,
				file_uuid: file.uuid(),
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
				uuid: dir.uuid(),
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
				file_uuid: file.uuid(),
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
			&api::v3::file::link::status::Request { uuid: file.uuid() },
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
	pub async fn get_linked_file(&self, link: &FilePublicLink) -> Result<LinkedFileInfo, Error> {
		let response = api::v3::file::link::info::post(
			self.client(),
			&api::v3::file::link::info::Request {
				uuid: link.link_uuid,
				password: Cow::Borrowed(&link.get_password_hash()?),
			},
		)
		.await?;

		let size_str = self.crypter().decrypt_meta(&response.size)?;
		let size = size_str
			.parse::<u64>()
			.map_err(|_| Error::Custom(format!("Failed to parse size: {}", size_str)))?;

		let file_info = LinkedFileInfo {
			uuid: response.uuid,
			name: self.crypter().decrypt_meta(&response.name)?,
			mime: self.crypter().decrypt_meta(&response.mime)?,
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
			&api::v3::dir::link::status::Request { uuid: dir.uuid() },
		)
		.await?;

		let link_status = match response.0 {
			None => {
				return Ok(None);
			}
			Some(link_status) => link_status,
		};

		let info_response = api::v3::dir::link::info::post(
			self.client(),
			&api::v3::dir::link::info::Request {
				uuid: link_status.uuid,
			},
		)
		.await?;
		let password = match link_status.password {
			Some(password) => PasswordState::Hashed(password.into_owned()),
			None => PasswordState::None,
		};
		Ok(Some(DirPublicLink {
			link_uuid: link_status.uuid,
			link_key: self.decrypt_meta_key(&link_status.key)?,
			password,
			expiration: link_status.expiration_text,
			enable_download: link_status.download_btn,
			salt: info_response.salt.map(|v| v.into_owned()),
		}))
	}

	pub async fn list_linked_dir(
		&self,
		dir: &dyn HasContents,
		link: &DirPublicLink,
	) -> Result<(Vec<RemoteDirectory>, Vec<RemoteFile>), Error> {
		let response = api::v3::dir::link::content::post(
			self.client(),
			&api::v3::dir::link::content::Request {
				uuid: link.link_uuid,
				password: Cow::Borrowed(&link.get_password_hash()?),
				parent: dir.uuid(),
			},
		)
		.await?;

		let dirs = response
			.dirs
			.into_iter()
			.map(|d| {
				RemoteDirectory::from_encrypted(
					d.uuid,
					d.parent,
					d.color.map(|c| c.into_owned()),
					false,
					&d.metadata,
					link.crypter(),
				)
			})
			.collect::<Result<Vec<_>, _>>()?;

		let files: Vec<RemoteFile> = response
			.files
			.into_iter()
			.map(|f| {
				let meta = FileMeta::from_encrypted(&f.metadata, link.crypter())?;
				Ok::<RemoteFile, Error>(RemoteFile::from_meta(
					f.uuid, f.parent, f.size, f.chunks, f.region, f.bucket, false, meta,
				))
			})
			.collect::<Result<Vec<_>, Error>>()?;
		Ok((dirs, files))
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

	async fn inner_share_item<I>(&self, item: &I, user: &User) -> Result<(), Error>
	where
		I: HasParent + HasMeta + HasUUID + HasType,
	{
		api::v3::item::share::post(
			self.client(),
			&api::v3::item::share::Request {
				uuid: item.uuid(),
				parent: Some(item.parent()),
				email: Cow::Borrowed(user.email()),
				r#type: item.object_type(),
				metadata: Cow::Owned(
					item.get_rsa_encrypted_meta(&user.public_key)
						.map_err(|e| Error::Custom(e.to_string()))?,
				),
			},
		)
		.await?;
		Ok(())
	}

	pub async fn share_dir(&self, dir: &RemoteDirectory, user: &User) -> Result<(), Error> {
		let (dirs, files) = self.list_dir_recursive(dir).await?;
		let futures = FuturesUnordered::new();

		futures.push(Box::pin(async move {
			api::v3::item::share::post(
				self.client(),
				&api::v3::item::share::Request {
					uuid: dir.uuid(),
					parent: None,
					email: Cow::Borrowed(user.email()),
					r#type: ObjectType::Dir,
					metadata: Cow::Owned(
						dir.get_rsa_encrypted_meta(&user.public_key)
							.map_err(|e| Error::Custom(e.to_string()))?,
					),
				},
			)
			.await
		}) as BoxFuture<'_, Result<(), Error>>);

		for dir in dirs {
			futures.push(
				Box::pin(async move { self.inner_share_item(&dir, user).await })
					as BoxFuture<'_, Result<(), Error>>,
			);
		}

		for file in files {
			futures.push(
				Box::pin(async move { self.inner_share_item(&file, user).await })
					as BoxFuture<'_, Result<(), Error>>,
			);
		}

		let mut stream = futures
			.map(|v| async move { v }) // type coercion
			.buffer_unordered(MAX_SMALL_PARALLEL_REQUESTS);
		while let Some(result) = stream.next().await {
			match result {
				Ok(_) => continue,
				Err(e) => return Err(e),
			}
		}
		Ok(())
	}

	pub async fn share_file(&self, file: &RemoteFile, user: &User) -> Result<(), Error> {
		api::v3::item::share::post(
			self.client(),
			&api::v3::item::share::Request {
				uuid: file.uuid(),
				parent: None,
				email: Cow::Borrowed(user.email()),
				r#type: ObjectType::File,
				metadata: Cow::Borrowed(
					&file
						.get_rsa_encrypted_meta(&user.public_key)
						.map_err(|e| Error::Custom(e.to_string()))?,
				),
			},
		)
		.await
	}

	pub async fn make_user_from_contact(&self, contact: &Contact<'_>) -> Result<User, Error> {
		let response = api::v3::user::public_key::post(
			self.client(),
			&api::v3::user::public_key::Request {
				email: Cow::Borrowed(&*contact.email),
			},
		)
		.await?;
		Ok(User::new(
			contact.email.to_string(),
			&response.public_key,
			contact.user_id,
		)?)
	}

	async fn inner_list_out_shared(
		&self,
		dir: Option<&impl HasContents>,
		user: Option<&User>,
	) -> Result<(Vec<SharedDirectory>, Vec<SharedFile>), Error> {
		let response = api::v3::shared::out::post(
			self.client(),
			&api::v3::shared::out::Request {
				uuid: dir.map(|d| d.uuid()),
				receiver_id: user.map(|u| u.id),
			},
		)
		.await?;

		let dirs = response
			.dirs
			.into_iter()
			.map(|d| SharedDirectory::from_shared_out(d, self.crypter()))
			.collect::<Result<Vec<_>, _>>()?;

		let files = response
			.files
			.into_iter()
			.map(|f| SharedFile::from_shared_out(f, self.crypter()))
			.collect::<Result<Vec<_>, _>>()?;
		Ok((dirs, files))
	}

	pub async fn list_out_shared(
		&self,
		user: Option<&User>,
	) -> Result<(Vec<SharedDirectory>, Vec<SharedFile>), Error> {
		self.inner_list_out_shared(None::<&RemoteDirectory>, user)
			.await
	}

	pub async fn list_out_shared_dir(
		&self,
		dir: &impl HasContents,
		user: &User,
	) -> Result<(Vec<SharedDirectory>, Vec<SharedFile>), Error> {
		self.inner_list_out_shared(Some(dir), Some(user)).await
	}

	async fn inner_list_in_shared(
		&self,
		dir: Option<&impl HasContents>,
	) -> Result<(Vec<SharedDirectory>, Vec<SharedFile>), Error> {
		let response = api::v3::shared::r#in::post(
			self.client(),
			&api::v3::shared::r#in::Request {
				uuid: dir.map(|d| d.uuid()),
			},
		)
		.await?;
		let dirs = response
			.dirs
			.into_iter()
			.map(|d| SharedDirectory::from_shared_in(d, self.private_key()))
			.collect::<Result<Vec<_>, _>>()?;

		let files = response
			.files
			.into_iter()
			.map(|f| SharedFile::from_shared_in(f, self.private_key()))
			.collect::<Result<Vec<_>, _>>()?;
		Ok((dirs, files))
	}

	pub async fn list_in_shared(&self) -> Result<(Vec<SharedDirectory>, Vec<SharedFile>), Error> {
		self.inner_list_in_shared(None::<&RemoteDirectory>).await
	}

	pub async fn list_in_shared_dir(
		&self,
		dir: &impl HasContents,
	) -> Result<(Vec<SharedDirectory>, Vec<SharedFile>), Error> {
		self.inner_list_in_shared(Some(dir)).await
	}
}
