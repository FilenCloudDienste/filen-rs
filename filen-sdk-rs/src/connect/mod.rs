use std::{borrow::Cow, fmt::Debug, sync::Arc};

use filen_macros::js_type;
use filen_types::{
	api::v3::{
		contacts::Contact,
		dir::{color::DirColor, link::PublicLinkExpiration},
		file::link::edit::FileLinkAction,
		item::{linked::ListedPublicLink, shared::SharedUser},
	},
	fs::{ObjectType, UuidStr},
	traits::CowHelpers,
};
use fs::{SharedDirectory, SharedRootFile};
use futures::stream::{FuturesUnordered, StreamExt};

use crate::{
	ErrorKind, api,
	auth::{Client, MetaKey, shared_client::SharedClient},
	connect::fs::{SharedRootDirectory, SharingRole},
	crypto::{file::FileKey, shared::MetaCrypter},
	error::{Error, MetadataWasNotDecryptedError},
	fs::{
		HasMeta, HasMetaExt, HasParent, HasType, HasUUID,
		categories::{
			DirType, Linked, NonRootItemType, Normal, RootItemType, Shared,
			fs::CategoryFS,
			shared::{list_all_in_shared, list_all_out_shared},
		},
		dir::{LinkedDirectory, RemoteDirectory, RootDirectoryWithMeta, meta::DirectoryMeta},
		file::{LinkedFile, RemoteFile},
	},
	runtime::do_cpu_intensive,
	util::MaybeSendBoxFuture,
};

pub mod contacts;
pub mod fs;
#[cfg(any(feature = "wasm-full", feature = "uniffi"))]
pub mod js_impls;

pub(crate) trait MakePasswordSaltAndHash {
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
			crate::crypto::connect::derive_password_for_link(password, Some(self.salt()))?,
		))
	}
}

#[derive(Default)]
#[js_type(tagged, wasm_all)]
pub enum PasswordState {
	Known(String),
	#[cfg_attr(feature = "wasm-full", serde(with = "serde_bytes"))]
	Hashed(Vec<u8>),
	#[default]
	None,
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

#[derive(Debug, Clone, Eq)]
#[js_type(import, export, no_default)]
pub struct FilePublicLink {
	link_uuid: UuidStr,
	password: PasswordState,
	expiration: PublicLinkExpiration,
	downloadable: bool,
	#[cfg_attr(feature = "wasm-full", serde(with = "serde_bytes"))]
	salt: Option<Vec<u8>>,
}

impl PartialEq for FilePublicLink {
	fn eq(&self, other: &Self) -> bool {
		let password_match = match (&self.password, &self.salt, &other.password, &other.salt) {
			(PasswordState::Known(a), _, PasswordState::Known(b), _) => a == b,
			(PasswordState::Hashed(a), _, PasswordState::Hashed(b), _) => a == b,
			(PasswordState::None, _, PasswordState::None, _) => true,
			(PasswordState::Known(a), a_salt, PasswordState::Hashed(b), b_salt)
			| (PasswordState::Hashed(b), b_salt, PasswordState::Known(a), a_salt) => {
				if a_salt != b_salt {
					return false;
				} else {
					match crate::crypto::connect::derive_password_for_link(
						Some(a),
						a_salt.as_deref(),
					) {
						Ok(hash) => b == &hash,
						Err(_) => false,
					}
				}
			}
			_ => false,
		};

		self.link_uuid == other.link_uuid
			&& password_match
			&& self.expiration == other.expiration
			&& self.downloadable == other.downloadable
			&& self.salt == other.salt
	}
}

impl FilePublicLink {
	pub fn password(&self) -> &PasswordState {
		&self.password
	}

	pub fn uuid(&self) -> UuidStr {
		self.link_uuid
	}

	pub fn set_password(&mut self, password: String) {
		if let PasswordState::Known(ref current) = self.password
			&& &password == current
		{
			return;
		}
		if let PasswordState::Hashed(ref current_hashed) = self.password
			&& let Ok(new_hashed) = crate::crypto::connect::derive_password_for_link(
				Some(&password),
				self.salt.as_deref(),
			) && &new_hashed == current_hashed
		{
			return;
		}
		self.password = PasswordState::Known(password);
		self.salt = Some(rand::random::<[u8; 256]>().to_vec());
	}

	pub fn clear_password(&mut self) {
		self.password = PasswordState::None;
		self.salt = None;
	}

	pub fn set_expiration(&mut self, expiration: PublicLinkExpiration) {
		self.expiration = expiration;
	}

	pub fn set_downloadable(&mut self, enable_download: bool) {
		self.downloadable = enable_download;
	}
}

impl FilePublicLink {
	pub(crate) fn new() -> Self {
		Self {
			link_uuid: UuidStr::new_v4(),
			password: PasswordState::None,
			expiration: PublicLinkExpiration::Never,
			downloadable: true,
			salt: None,
		}
	}
}

impl MakePasswordSaltAndHash for FilePublicLink {
	fn password(&self) -> &PasswordState {
		&self.password
	}

	fn salt(&self) -> &[u8] {
		self.salt.as_deref().unwrap_or(&[])
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirPublicLink {
	pub(crate) link_uuid: UuidStr,
	pub(crate) link_key: MetaKey,
	pub(crate) password: Option<String>,
	pub(crate) enable_download: bool,
	pub(crate) salt: Option<Vec<u8>>,
}

impl DirPublicLink {
	pub(crate) fn crypter(&self) -> &impl MetaCrypter {
		&self.link_key
	}

	pub(crate) fn uuid(&self) -> &UuidStr {
		&self.link_uuid
	}

	pub(crate) fn get_password_hash(&self) -> Result<Cow<'_, [u8]>, Error> {
		Ok(Cow::Owned(
			crate::crypto::connect::derive_password_for_link(
				self.password.as_deref(),
				self.salt.as_deref(),
			)?,
		))
	}
}

pub struct DirPublicLinkRW {
	pub(crate) link_uuid: UuidStr,
	pub(crate) link_key: Option<MetaKey>,
	pub(crate) password: PasswordState,
	pub(crate) expiration: PublicLinkExpiration,
	pub(crate) enable_download: bool,
	pub(crate) salt: Option<Vec<u8>>,
}

impl TryFrom<DirPublicLinkRW> for DirPublicLink {
	type Error = Error;

	fn try_from(value: DirPublicLinkRW) -> Result<Self, Self::Error> {
		Ok(Self {
			link_uuid: value.link_uuid,
			link_key: value.link_key.ok_or_else(|| {
				Error::custom(
					ErrorKind::MetadataWasNotDecrypted,
					"Cannot convert DirPublicLinkRW without decrypted link key to DirPublicLink",
				)
			})?,
			password: match value.password {
				PasswordState::Known(password) => Some(password),
				PasswordState::Hashed(_) => None,
				PasswordState::None => None,
			},
			enable_download: value.enable_download,
			salt: value.salt,
		})
	}
}

impl DirPublicLinkRW {
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

	pub(crate) fn crypter(&self) -> Option<&impl MetaCrypter> {
		self.link_key.as_ref()
	}
}

impl DirPublicLinkRW {
	pub fn uuid(&self) -> UuidStr {
		self.link_uuid
	}

	pub fn key_string(&self) -> Option<String> {
		self.link_key.as_ref().map(|k| k.to_string())
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
}

impl MakePasswordSaltAndHash for DirPublicLinkRW {
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
		item: NonRootItemType<'_, Normal>,
	) -> Result<(), Error> {
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
				if let NonRootItemType::Dir(dir) = item {
					let (dirs, files) = Normal::list_dir_recursive(
						self,
						&DirType::Dir(Cow::Borrowed(dir.as_ref())),
						None::<&fn(u64, Option<u64>)>,
						(),
					)
					.await?;

					// not using the closure here causes a borrow checker error
					#[allow(clippy::redundant_closure)]
					Ok(std::iter::once(NonRootItemType::<Normal>::Dir(dir))
						.chain(dirs.into_iter().map(|d| NonRootItemType::from(d)))
						.chain(files.into_iter().map(|f| NonRootItemType::from(f)))
						.collect::<Vec<NonRootItemType<'_, Normal>>>())
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

	pub(crate) async fn add_item_to_directory_link(
		&self,
		item: &NonRootItemType<'_, Normal>,
		link: &ListedPublicLink<'_>,
		link_crypter: &impl MetaCrypter,
	) -> Result<(), Error> {
		let meta = match item {
			NonRootItemType::Dir(cow) => cow.get_encrypted_meta(link_crypter).await,
			NonRootItemType::File(cow) => cow.get_encrypted_meta(link_crypter).await,
		};

		api::v3::dir::link::add::post(
			self.client(),
			&api::v3::dir::link::add::Request {
				uuid: *item.uuid(),
				parent: Some((*item.parent()).try_into()?),
				link_uuid: link.link_uuid,
				r#type: item.object_type(),
				metadata: meta.ok_or(MetadataWasNotDecryptedError)?,
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
		progress_callback: &F,
	) -> Result<DirPublicLinkRW, Error>
	where
		F: Fn(u64, Option<u64>) + Send + Sync,
	{
		let public_link = DirPublicLinkRW::new(self.make_meta_key());
		let (dirs, files) = self
			.list_dir_recursive::<Normal, _>(&DirType::Dir(Cow::Borrowed(dir)), progress_callback)
			.await?;
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
			futures.push(Box::pin(async move {
				self.add_item_to_directory_link(&(&dir).into(), link, key)
					.await
			}) as MaybeSendBoxFuture<'_, Result<(), Error>>);
		}
		for file in files {
			futures.push(Box::pin(async move {
				self.add_item_to_directory_link(&(&file).into(), link, key)
					.await
			}) as MaybeSendBoxFuture<'_, Result<(), Error>>);
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
		// why does this just hash_name empty? Who knows,
		// we should fix this with the v4 api
		let tmp_salt = rand::random::<[u8; 256]>();
		let password_hash =
			do_cpu_intensive(|| crate::crypto::connect::derive_password_for_link(None, None))
				.await?;

		api::v3::file::link::edit::post(
			self.client(),
			&api::v3::file::link::edit::Request {
				uuid: file_link.link_uuid,
				file_uuid: *file.uuid(),
				expiration: PublicLinkExpiration::Never,
				password: false,
				password_hashed: Cow::Borrowed(&password_hash),
				salt: Cow::Borrowed(&tmp_salt),
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
		link: &DirPublicLinkRW,
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
		let (password, password_hashed, salt) = if link.password().is_known() {
			(
				true,
				do_cpu_intensive(|| link.get_password_hash()).await?,
				Cow::Borrowed(link.salt()),
			)
		} else {
			// why does this just hash_name empty? Who knows,
			// we should fix this with the v4 api
			let tmp_salt = rand::random::<[u8; 256]>().to_vec();
			(
				false,
				Cow::Owned(
					do_cpu_intensive(|| {
						crate::crypto::connect::derive_password_for_link(None, None)
					})
					.await?,
				),
				Cow::Owned(tmp_salt),
			)
		};

		api::v3::file::link::edit::post(
			self.client(),
			&api::v3::file::link::edit::Request {
				uuid: link.link_uuid,
				file_uuid: *file.uuid(),
				expiration: link.expiration,
				password,
				password_hashed,
				salt,
				download_btn: link.downloadable,
				r#type: FileLinkAction::Enable,
			},
		)
		.await?;
		Ok(())
	}

	pub async fn remove_file_link(
		&self,
		file: &RemoteFile,
		link: FilePublicLink,
	) -> Result<(), Error> {
		let tmp_salt = rand::random::<[u8; 256]>();
		let password_hash =
			do_cpu_intensive(|| crate::crypto::connect::derive_password_for_link(None, None))
				.await?;

		api::v3::file::link::edit::post(
			self.client(),
			&api::v3::file::link::edit::Request {
				uuid: link.link_uuid,
				file_uuid: *file.uuid(),
				expiration: PublicLinkExpiration::Never,
				password: false,
				password_hashed: Cow::Borrowed(&password_hash),
				salt: Cow::Borrowed(&tmp_salt),
				download_btn: false,
				r#type: FileLinkAction::Disable,
			},
		)
		.await
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
			self.unauthed(),
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
			salt: if password_response.salt.is_empty() {
				None
			} else {
				Some(password_response.salt.into_owned())
			},
		}))
	}

	// doesn't require auth, should be moved to a different module in the future

	pub async fn get_dir_link_rw(
		&self,
		dir: &RemoteDirectory,
	) -> Result<Option<DirPublicLinkRW>, Error> {
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
					self.unauthed(),
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
		Ok(Some(DirPublicLinkRW {
			link_uuid: link_status.uuid,
			link_key: decrypted_link_key.ok(),
			password,
			expiration: link_status.expiration_text,
			enable_download: link_status.download_btn,
			salt: info_response.salt.map(|v| v.into_owned()),
		}))
	}

	pub async fn remove_dir_link(&self, link: DirPublicLinkRW) -> Result<(), Error> {
		api::v3::dir::link::remove::post(
			self.client(),
			&api::v3::dir::link::remove::Request {
				uuid: link.link_uuid,
			},
		)
		.await?;
		Ok(())
	}

	async fn inner_share_item(
		&self,
		item: &NonRootItemType<'_, Normal>,
		user: &SharedUser<'_>,
	) -> Result<(), Error> {
		let meta = match item {
			NonRootItemType::Dir(cow) => cow.get_rsa_encrypted_meta(&user.public_key).await,
			NonRootItemType::File(cow) => cow.get_rsa_encrypted_meta(&user.public_key).await,
		};
		api::v3::item::share::post(
			self.client(),
			&api::v3::item::share::Request {
				uuid: *item.uuid(),
				parent: Some((*item.parent()).try_into()?),
				email: user.email.as_borrowed_cow(),
				r#type: item.object_type(),
				metadata: meta.ok_or(MetadataWasNotDecryptedError)?,
			},
		)
		.await?;
		Ok(())
	}

	pub async fn share_dir<F>(
		&self,
		dir: &RemoteDirectory,
		client: &Contact<'_>,
		progress_callback: &F,
	) -> Result<(), Error>
	where
		F: Fn(u64, Option<u64>) + Send + Sync,
	{
		let (dirs, files) = self
			.list_dir_recursive::<Normal, _>(&DirType::Dir(Cow::Borrowed(dir)), progress_callback)
			.await?;

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
			futures.push(Box::pin(async move {
				self.inner_share_item(&(&dir).into(), shared_user).await
			}) as MaybeSendBoxFuture<'_, Result<(), Error>>);
		}

		for file in files {
			futures.push(Box::pin(async move {
				self.inner_share_item(&(&file).into(), shared_user).await
			}) as MaybeSendBoxFuture<'_, Result<(), Error>>);
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

	pub async fn list_out_shared<F>(
		&self,
		contact: Option<&Contact<'_>>,
		callback: Option<&F>,
	) -> Result<(Vec<SharedRootDirectory>, Vec<SharedRootFile>), Error>
	where
		F: Fn(u64, Option<u64>) + Send + Sync,
	{
		list_all_out_shared(self, contact.map(|c| c.user_id), callback).await
	}

	pub async fn list_shared_dir<F>(
		&self,
		dir: &DirType<'_, Shared>,
		sharer_info: &SharingRole,
		callback: Option<&F>,
	) -> Result<(Vec<SharedDirectory>, Vec<RemoteFile>), Error>
	where
		F: Fn(u64, Option<u64>) + Send + Sync,
	{
		Shared::list_dir(self, dir, callback, sharer_info).await
	}

	pub async fn list_in_shared_root<F>(
		&self,
		callback: Option<&F>,
	) -> Result<(Vec<SharedRootDirectory>, Vec<SharedRootFile>), Error>
	where
		F: Fn(u64, Option<u64>) + Send + Sync,
	{
		list_all_in_shared(self, callback).await
	}

	pub async fn remove_shared_item(&self, item: &RootItemType<'_, Shared>) -> Result<(), Error> {
		let share_role = match item {
			RootItemType::Dir(dir) => &dir.info.sharing_role,
			RootItemType::File(file) => &file.sharing_role,
		};
		match share_role {
			fs::SharingRole::Sharer(_) => {
				api::v3::item::shared::r#in::remove::post(
					self.client(),
					&api::v3::item::shared::r#in::remove::Request { uuid: *item.uuid() },
				)
				.await
			}
			fs::SharingRole::Receiver(share_info) => {
				api::v3::item::shared::out::remove::post(
					self.client(),
					&api::v3::item::shared::out::remove::Request {
						uuid: *item.uuid(),
						receiver_id: share_info.id,
					},
				)
				.await
			}
		}
	}
}

#[allow(private_bounds, async_fn_in_trait)]
pub trait PublicLinkSharedClientExt: SharedClient {
	async fn get_linked_file(
		&self,
		link_uuid: UuidStr,
		file_key: Cow<'_, str>,
		password: Option<&str>,
	) -> Result<LinkedFile, Error>;

	async fn list_linked_dir<F>(
		&self,
		dir: &DirType<'_, Linked>,
		link: &DirPublicLink,
		callback: &F,
	) -> Result<(Vec<LinkedDirectory>, Vec<RemoteFile>), Error>
	where
		F: Fn(u64, Option<u64>) + Send + Sync;

	/// The returned dirs here should not be used as normal RemoteDirectories,
	/// they can only be listed via list_linked_dir again.
	async fn list_linked_dir_recursive<F>(
		&self,
		dir: &DirType<'_, Linked>,
		link: &DirPublicLink,
		callback: &F,
	) -> Result<(Vec<LinkedDirectory>, Vec<RemoteFile>), Error>
	where
		F: Fn(u64, Option<u64>) + Send + Sync;

	async fn get_dir_public_link_info(
		&self,
		link_uuid: UuidStr,
		link_key: &str,
	) -> Result<DirPublicInfo, Error> {
		let resp = api::v3::dir::link::info::post(
			self.get_unauth_client(),
			&api::v3::dir::link::info::Request { uuid: link_uuid },
		)
		.await?;

		let key = MetaKey::from_str_and_meta(link_key, &resp.metadata)?;

		let meta =
			do_cpu_intensive(|| DirectoryMeta::blocking_from_encrypted(resp.metadata, &key)).await;
		let root =
			RootDirectoryWithMeta::from_meta(resp.parent, DirColor::Default, resp.timestamp, meta);

		let link = DirPublicLink {
			link_uuid,
			link_key: key,
			password: None,
			enable_download: resp.download_btn,
			salt: None,
		};

		Ok(DirPublicInfo {
			root,
			link,
			has_password: resp.has_password,
		})
	}
}

pub struct DirPublicInfo {
	pub root: RootDirectoryWithMeta,
	pub link: DirPublicLink,
	pub has_password: bool,
}

impl<T> PublicLinkSharedClientExt for T
where
	T: SharedClient,
{
	async fn get_linked_file(
		&self,
		link_uuid: UuidStr,
		file_key: Cow<'_, str>,
		password: Option<&str>,
	) -> Result<LinkedFile, Error> {
		let (password, salt) = match password {
			None => (None, None),
			Some(password) => {
				let resp = api::v3::file::link::password::post(
					self.get_unauth_client(),
					&api::v3::file::link::password::Request { uuid: link_uuid },
				)
				.await?;
				(Some(password), Some(resp.salt))
			}
		};
		let password_hashed = do_cpu_intensive(|| {
			crate::crypto::connect::derive_password_for_link(password, salt.as_deref())
		})
		.await?;
		let response = api::v3::file::link::info::post(
			self.get_unauth_client(),
			&api::v3::file::link::info::Request {
				uuid: link_uuid,
				password: Cow::Borrowed(&password_hashed),
			},
		)
		.await?;

		let key = FileKey::from_string_and_meta(file_key, &response.mime)?;
		do_cpu_intensive(|| LinkedFile::blocking_from_response(key, response)).await
	}

	async fn list_linked_dir<F>(
		&self,
		dir: &DirType<'_, Linked>,
		link: &DirPublicLink,
		callback: &F,
	) -> Result<(Vec<LinkedDirectory>, Vec<RemoteFile>), Error>
	where
		F: Fn(u64, Option<u64>) + Send + Sync,
	{
		Linked::list_dir(
			self.get_unauth_client(),
			dir,
			Some(callback),
			Cow::Borrowed(link),
		)
		.await
	}

	async fn list_linked_dir_recursive<F>(
		&self,
		dir: &DirType<'_, Linked>,
		link: &DirPublicLink,
		callback: &F,
	) -> Result<(Vec<LinkedDirectory>, Vec<RemoteFile>), Error>
	where
		F: Fn(u64, Option<u64>) + Send + Sync,
	{
		Linked::list_dir_recursive(
			self.get_unauth_client(),
			dir,
			Some(callback),
			Cow::Borrowed(link),
		)
		.await
	}
}
