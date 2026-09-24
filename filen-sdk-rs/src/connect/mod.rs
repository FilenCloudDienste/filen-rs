use std::{borrow::Cow, fmt::Debug};

use filen_macros::js_type;
use filen_types::{
	api::v3::{
		contacts::Contact,
		dir::{
			color::DirColor,
			link::{PublicLinkExpiration, info::LinkPasswordSalt},
		},
		file::link::edit::FileLinkAction,
		item::{linked::ListedPublicLink, shared::SharedUser},
	},
	crypto::{LinkHashedPassword, LinkHashedPasswordStatic},
	fs::{ObjectType, Uuid},
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
		file::{AnonymousRemoteFile, LinkedFile, RemoteFile},
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
	fn salt(&self) -> &LinkPasswordSalt;

	fn get_password_hash(&self) -> Result<LinkHashedPassword<'_>, Error> {
		let password = match self.password() {
			PasswordState::None => None,
			PasswordState::Known(password) => Some(password.as_str()),
			PasswordState::Hashed(password_vec) => {
				return Ok(password_vec.as_borrowed_cow());
			}
		};
		Ok(crate::crypto::connect::derive_password_for_link(
			password,
			self.salt(),
		)?)
	}
}

#[derive(Default, Debug, Clone, PartialEq, Eq)]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(serde::Serialize, serde::Deserialize, tsify::Tsify),
	// internally tagged enums cannot (de)serialize newtype variants wrapping a
	// non-struct (e.g. `Known(String)`), so an explicit content key is required:
	// https://github.com/serde-rs/serde/issues/1307
	serde(tag = "type", content = "data", rename_all = "camelCase"),
)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
pub enum PasswordState {
	Known(String),
	Hashed(LinkHashedPasswordStatic),
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
}

#[derive(Debug, Clone, Eq)]
#[js_type(import, export, no_default)]
pub struct FilePublicLink {
	link_uuid: Uuid,
	password: PasswordState,
	expiration: PublicLinkExpiration,
	downloadable: bool,
	salt: LinkPasswordSalt,
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
					match crate::crypto::connect::derive_password_for_link(Some(a), a_salt) {
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

	pub fn uuid(&self) -> Uuid {
		self.link_uuid
	}

	pub fn expiration(&self) -> PublicLinkExpiration {
		self.expiration
	}

	pub fn downloadable(&self) -> bool {
		self.downloadable
	}

	pub fn set_password(&mut self, password: String) {
		if let PasswordState::Known(ref current) = self.password
			&& &password == current
		{
			return;
		}
		if let PasswordState::Hashed(ref current_hashed) = self.password
			&& let Ok(new_hashed) =
				crate::crypto::connect::derive_password_for_link(Some(&password), &self.salt)
			&& &new_hashed == current_hashed
		{
			return;
		}
		self.password = PasswordState::Known(password);
		self.salt = crate::crypto::connect::new_random_salt();
	}

	pub fn clear_password(&mut self) {
		self.password = PasswordState::None;
		self.salt = crate::crypto::connect::new_random_salt();
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
			link_uuid: Uuid::new_v4(),
			password: PasswordState::None,
			expiration: PublicLinkExpiration::Never,
			downloadable: true,
			salt: LinkPasswordSalt::None,
		}
	}
}

impl MakePasswordSaltAndHash for FilePublicLink {
	fn password(&self) -> &PasswordState {
		&self.password
	}

	fn salt(&self) -> &LinkPasswordSalt {
		&self.salt
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirPublicLink {
	pub(crate) link_uuid: Uuid,
	pub(crate) link_key: MetaKey,
	pub(crate) password: PasswordState,
	pub(crate) enable_download: bool,
	pub(crate) salt: LinkPasswordSalt,
}

impl DirPublicLink {
	pub(crate) fn crypter(&self) -> &impl MetaCrypter {
		&self.link_key
	}

	pub fn uuid(&self) -> &Uuid {
		&self.link_uuid
	}

	pub fn key_string(&self) -> String {
		self.link_key.to_string()
	}

	pub fn set_password(&mut self, password: String) {
		self.password = PasswordState::Known(password);
	}

	pub(crate) fn get_password_hash(&self) -> Result<LinkHashedPassword<'_>, Error> {
		let password = match &self.password {
			PasswordState::None => None,
			PasswordState::Known(password) => Some(password.as_str()),
			PasswordState::Hashed(password_hash) => {
				return Ok(password_hash.as_borrowed_cow());
			}
		};
		Ok(crate::crypto::connect::derive_password_for_link(
			password, &self.salt,
		)?)
	}
}

#[derive(Debug, PartialEq, Eq)]
pub struct DirPublicLinkRW {
	pub(crate) link_uuid: Uuid,
	pub(crate) link_key: Option<MetaKey>,
	pub(crate) password: PasswordState,
	pub(crate) expiration: PublicLinkExpiration,
	pub(crate) enable_download: bool,
	pub(crate) salt: LinkPasswordSalt,
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
			password: value.password,
			enable_download: value.enable_download,
			salt: value.salt,
		})
	}
}

impl DirPublicLinkRW {
	pub(crate) fn new(link_key: MetaKey) -> Self {
		Self {
			link_uuid: Uuid::new_v4(),
			link_key: Some(link_key),
			password: PasswordState::None,
			expiration: PublicLinkExpiration::Never,
			enable_download: true,
			salt: LinkPasswordSalt::None,
		}
	}
}

impl DirPublicLinkRW {
	pub fn uuid(&self) -> Uuid {
		self.link_uuid
	}

	pub fn key_string(&self) -> Option<String> {
		self.link_key.as_ref().map(|k| k.to_string())
	}

	pub fn password(&self) -> &PasswordState {
		&self.password
	}

	pub fn expiration(&self) -> PublicLinkExpiration {
		self.expiration
	}

	pub fn download_enabled(&self) -> bool {
		self.enable_download
	}

	pub fn set_password(&mut self, password: String) {
		self.password = PasswordState::Known(password);
		self.salt = crate::crypto::connect::new_random_salt()
	}

	pub fn clear_password(&mut self) {
		self.password = PasswordState::None;
		self.salt = LinkPasswordSalt::None;
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

	fn salt(&self) -> &LinkPasswordSalt {
		&self.salt
	}
}

/// Drives every future to completion, collecting the errors from any that fail instead of
/// aborting on the first one.
///
/// Used when propagating a metadata update to all connected links and shared receivers: the
/// primary operation (create/upload/rename) has already been committed server-side, so a
/// failure to update one connected copy must not cancel the updates to the others, nor make
/// the already-succeeded primary operation report failure to the caller.
async fn drain_collecting_errors(
	mut futures: FuturesUnordered<MaybeSendBoxFuture<'_, Result<(), Error>>>,
) -> Vec<Error> {
	let mut errors = Vec::new();
	while let Some(result) = futures.next().await {
		if let Err(e) = result {
			errors.push(e);
		}
	}
	errors
}

/// A public link a directory belongs to, with its key decrypted.
#[derive(Clone)]
pub(crate) struct ConnectedLink {
	link: ListedPublicLink<'static>,
	crypter: MetaKey,
}

/// The public links and shared users that items created inside a directory must be
/// propagated to. New items inherit their parent's links and shares, so one snapshot of the
/// destination covers every item created below it.
#[derive(Clone, Default)]
pub(crate) struct ConnectedTargets {
	links: Vec<ConnectedLink>,
	users: Vec<SharedUser<'static>>,
}

impl ConnectedTargets {
	/// True when the directory has no usable link and no share, so nothing needs propagating.
	pub(crate) fn is_empty(&self) -> bool {
		self.links.is_empty() && self.users.is_empty()
	}

	/// The links and users in `self` but not in `other`.
	pub(crate) fn without(&self, other: &Self) -> Self {
		Self {
			links: self
				.links
				.iter()
				.filter(|link| {
					!other
						.links
						.iter()
						.any(|o| o.link.link_uuid == link.link.link_uuid)
				})
				.cloned()
				.collect(),
			users: self
				.users
				.iter()
				.filter(|user| !other.users.iter().any(|o| o.id == user.id))
				.cloned()
				.collect(),
		}
	}

	/// Targets with `users` shared users (ids `1..=users`) and no links.
	#[cfg(test)]
	pub(crate) fn with_test_users(users: u64) -> Self {
		let key = rsa::RsaPrivateKey::new(&mut old_rng::thread_rng(), 512)
			.unwrap()
			.to_public_key();
		Self {
			links: Vec::new(),
			users: (1..=users)
				.map(|id| SharedUser {
					id,
					email: Cow::Owned(format!("user{id}@example.com")),
					public_key: key.clone(),
				})
				.collect(),
		}
	}

	/// One operation per (link, item) and (user, item) pair: links first, then users.
	fn operations<'t, 'i, 'a>(
		&'t self,
		items: &'i [NonRootItemType<'a, Normal>],
	) -> impl Iterator<Item = PropagationOp<'t, 'i, 'a>> {
		let links = self.links.iter().flat_map(move |link| {
			items
				.iter()
				.map(move |item| PropagationOp::Link { link, item })
		});
		let users = self.users.iter().flat_map(move |user| {
			items
				.iter()
				.map(move |item| PropagationOp::Share { user, item })
		});
		links.chain(users)
	}
}

enum PropagationOp<'t, 'i, 'a> {
	Link {
		link: &'t ConnectedLink,
		item: &'i NonRootItemType<'a, Normal>,
	},
	Share {
		user: &'t SharedUser<'static>,
		item: &'i NonRootItemType<'a, Normal>,
	},
}

impl Client {
	async fn update_shared_item_meta<I>(&self, item: &I, user: &SharedUser<'_>) -> Result<(), Error>
	where
		I: HasMeta + HasUUID + Debug,
	{
		api::v3::item::shared::rename::post(
			self.client(),
			&api::v3::item::shared::rename::Request {
				uuid: item.uuid(),
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

		let errors = drain_collecting_errors(futures).await;
		for error in &errors {
			tracing::warn!(
				"failed to propagate metadata update to a connected link or shared user: {error}"
			);
		}
		Ok(())
	}

	/// Snapshot of the public links and shared users that items created inside `dir` must be
	/// propagated to. A link whose key cannot be decrypted is skipped (with a warning) so it
	/// cannot block propagation to the others.
	pub(crate) async fn fetch_connected_targets(
		&self,
		dir: Uuid,
	) -> Result<ConnectedTargets, Error> {
		let linked_request = api::v3::item::linked::Request { uuid: dir };
		let shared_request = api::v3::item::shared::Request { uuid: dir };
		let (linked, shared) = futures::try_join!(
			api::v3::item::linked::post(self.client(), &linked_request),
			api::v3::item::shared::post(self.client(), &shared_request),
		)?;

		let mut links = Vec::with_capacity(linked.links.len());
		for link in linked.links {
			match self.decrypt_meta_key(&link.link_key).await {
				Ok(crypter) => links.push(ConnectedLink { link, crypter }),
				Err(error) => {
					tracing::warn!(
						"failed to decrypt link key for connected link {}, skipping: {error}",
						link.link_uuid
					);
				}
			}
		}

		Ok(ConnectedTargets {
			links,
			users: shared.users,
		})
	}

	/// Adds every item to every link and shares it with every user in `targets`. Each item's
	/// parent must already be propagated. All operations run to completion; the errors of the
	/// failed ones are returned rather than aborting the rest.
	pub(crate) async fn propagate_to_targets(
		&self,
		targets: &ConnectedTargets,
		items: &[NonRootItemType<'_, Normal>],
	) -> Vec<Error> {
		let futures = targets
			.operations(items)
			.map(|op| match op {
				PropagationOp::Link { link, item } => Box::pin(async move {
					self.add_item_to_directory_link(item, &link.link, &link.crypter)
						.await
				})
					as MaybeSendBoxFuture<'_, Result<(), Error>>,
				PropagationOp::Share { user, item } => {
					Box::pin(async move { self.inner_share_item(item, user).await })
						as MaybeSendBoxFuture<'_, Result<(), Error>>
				}
			})
			.collect::<FuturesUnordered<_>>();
		drain_collecting_errors(futures).await
	}

	pub(crate) async fn update_item_with_maybe_connected_parent(
		&self,
		item: NonRootItemType<'_, Normal>,
	) -> Result<(), Error> {
		let uuid = (*item.parent()).try_into()?;

		let targets = self.fetch_connected_targets(uuid).await?;
		// Without a link or share there is nothing to add the item (or a directory's subtree) to,
		// so skip the recursive listing entirely.
		if targets.is_empty() {
			return Ok(());
		}

		let items_to_process = self.items_with_subtree(item).await?;
		let errors = self.propagate_to_targets(&targets, &items_to_process).await;
		for error in &errors {
			tracing::warn!(
				"failed to propagate a connected-parent update to a link or shared user: {error}"
			);
		}
		Ok(())
	}

	/// `item`, followed for a directory by every directory and then every file below it.
	pub(crate) async fn items_with_subtree<'a>(
		&self,
		item: NonRootItemType<'a, Normal>,
	) -> Result<Vec<NonRootItemType<'a, Normal>>, Error> {
		let NonRootItemType::Dir(dir) = item else {
			return Ok(vec![item]);
		};
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
			.collect())
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
				uuid: item.uuid(),
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
		progress_callback: Option<&F>,
	) -> Result<DirPublicLinkRW, Error>
	where
		F: Fn(u64, Option<u64>) + Send + Sync,
	{
		// Hold the drive lock across the recursive listing and every link post, matching the
		// other mutating flows. Otherwise a concurrent upload between the listing and the posts
		// is neither seen by the lister nor covered by its own connected-parent propagation
		// (the link root row does not exist yet), so it is silently absent from the link.
		let _lock = self.lock_drive().await?;
		let public_link = DirPublicLinkRW::new(self.make_meta_key());
		let (dirs, files) = self
			.list_dir_recursive::<Normal, _>(
				&DirType::Dir(Cow::Borrowed(dir)),
				progress_callback,
				(),
			)
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
					uuid: dir.uuid(),
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
		let password_hashed = do_cpu_intensive(|| {
			crate::crypto::connect::derive_password_for_link(None, &file_link.salt)
		})
		.await?;

		api::v3::file::link::edit::post(
			self.client(),
			&api::v3::file::link::edit::Request {
				uuid: file_link.link_uuid,
				file_uuid: file.uuid(),
				expiration: PublicLinkExpiration::Never,
				password: false,
				password_hashed,
				salt: file_link.salt(),
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
				uuid: dir.uuid(),
				expiration: link.expiration,
				password: link.password().is_known(),
				password_hashed: do_cpu_intensive(|| link.get_password_hash()).await?,
				salt: link.salt(),
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
				password_hashed: do_cpu_intensive(|| link.get_password_hash()).await?,
				salt: link.salt(),
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
		api::v3::file::link::edit::post(
			self.client(),
			&api::v3::file::link::edit::Request {
				uuid: link.link_uuid,
				file_uuid: file.uuid(),
				expiration: PublicLinkExpiration::Never,
				password: false,
				password_hashed: crate::crypto::connect::empty_hash(),
				salt: link.salt(),
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
			self.unauthed(),
			&api::v3::file::link::password::Request {
				uuid: link_status.uuid,
			},
		)
		.await?;

		let password = match link_status.password {
			Some(password) => PasswordState::Hashed(password),
			None => PasswordState::None,
		};

		Ok(Some(FilePublicLink {
			link_uuid: link_status.uuid,
			password,
			expiration: link_status.expiration_text,
			downloadable: link_status.download_btn,
			salt: password_response.salt,
		}))
	}

	// doesn't require auth, should be moved to a different module in the future

	pub async fn get_dir_link_rw(
		&self,
		dir: &RemoteDirectory,
	) -> Result<Option<DirPublicLinkRW>, Error> {
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
			Some(password) => PasswordState::Hashed(password),
			None => PasswordState::None,
		};
		Ok(Some(DirPublicLinkRW {
			link_uuid: link_status.uuid,
			link_key: decrypted_link_key.ok(),
			password,
			expiration: link_status.expiration_text,
			enable_download: link_status.download_btn,
			salt: info_response.salt.unwrap_or_default(),
		}))
	}

	/// Removes the public link from `dir`.
	///
	/// The `v3/dir/link/remove` endpoint identifies the link to remove by the linked directory's
	/// uuid (matching `dir/link/{edit,status}`), not by the link's own uuid.
	pub async fn remove_dir_link(&self, dir: &RemoteDirectory) -> Result<(), Error> {
		api::v3::dir::link::remove::post(
			self.client(),
			&api::v3::dir::link::remove::Request { uuid: dir.uuid() },
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
				uuid: item.uuid(),
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
		progress_callback: Option<&F>,
	) -> Result<(), Error>
	where
		F: Fn(u64, Option<u64>) + Send + Sync,
	{
		// Hold the drive lock across the recursive listing and every share post, matching the
		// other mutating flows. Otherwise a concurrent upload between the listing and the posts
		// is neither seen by the lister nor covered by its own connected-parent propagation
		// (the share root row does not exist yet), so it is silently absent from the share.
		let _lock = self.lock_drive().await?;
		let (dirs, files) =
			Normal::list_dir_recursive(self, &dir.into(), progress_callback, ()).await?;

		let shared_user = client.into();
		let shared_user = &shared_user;

		let mut futures = FuturesUnordered::new();

		futures.push(Box::pin(async move {
			api::v3::item::share::post(
				self.client(),
				&api::v3::item::share::Request {
					uuid: dir.uuid(),
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
				uuid: file.uuid(),
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
	) -> Result<(Vec<SharedDirectory>, Vec<AnonymousRemoteFile>), Error>
	where
		F: Fn(u64, Option<u64>) + Send + Sync,
	{
		Shared::list_dir(self, dir, callback, sharer_info).await
	}

	pub async fn list_shared_dir_recursive<F>(
		&self,
		dir: &DirType<'_, Shared>,
		sharer_info: &SharingRole,
		callback: Option<&F>,
	) -> Result<(Vec<SharedDirectory>, Vec<AnonymousRemoteFile>), Error>
	where
		F: Fn(u64, Option<u64>) + Send + Sync,
	{
		Shared::list_dir_recursive(self, dir, callback, sharer_info).await
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
					&api::v3::item::shared::r#in::remove::Request { uuid: item.uuid() },
				)
				.await
			}
			fs::SharingRole::Receiver(share_info) => {
				api::v3::item::shared::out::remove::post(
					self.client(),
					&api::v3::item::shared::out::remove::Request {
						uuid: item.uuid(),
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
		link_uuid: Uuid,
		file_key: &str,
		password: Option<&str>,
	) -> Result<LinkedFile, Error> {
		let (password, salt) = match password {
			None => (None, LinkPasswordSalt::None),
			Some(password) => {
				let resp = api::v3::file::link::password::post(
					self.get_unauth_client(),
					&api::v3::file::link::password::Request { uuid: link_uuid },
				)
				.await?;
				(Some(password), resp.salt)
			}
		};
		let password_hashed =
			do_cpu_intensive(|| crate::crypto::connect::derive_password_for_link(password, &salt))
				.await?;
		let response = api::v3::file::link::info::post(
			self.get_unauth_client(),
			&api::v3::file::link::info::Request {
				uuid: link_uuid,
				password: password_hashed,
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
		callback: Option<&F>,
	) -> Result<(Vec<LinkedDirectory>, Vec<AnonymousRemoteFile>), Error>
	where
		F: Fn(u64, Option<u64>) + Send + Sync,
	{
		Linked::list_dir(self.get_unauth_client(), dir, callback, Cow::Borrowed(link)).await
	}

	/// The returned dirs here should not be used as normal RemoteDirectories,
	/// they can only be listed via list_linked_dir again.
	async fn list_linked_dir_recursive<F>(
		&self,
		dir: &DirType<'_, Linked>,
		link: &DirPublicLink,
		callback: Option<&F>,
	) -> Result<(Vec<LinkedDirectory>, Vec<AnonymousRemoteFile>), Error>
	where
		F: Fn(u64, Option<u64>) + Send + Sync,
	{
		Linked::list_dir_recursive(self.get_unauth_client(), dir, callback, Cow::Borrowed(link))
			.await
	}

	async fn get_dir_public_link_info(
		&self,
		link_uuid: Uuid,
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
			password: PasswordState::None,
			enable_download: resp.download_btn,
			salt: resp.salt.unwrap_or_default(),
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

impl<T> PublicLinkSharedClientExt for T where T: SharedClient {}

#[cfg(test)]
mod tests {
	use std::borrow::Cow;

	use filen_types::{auth::MetaEncryptionVersion, crypto::LinkHashedPassword};

	use super::*;

	fn test_meta_key() -> MetaKey {
		MetaKey::from_str_and_version(
			"0123456789abcdefghijklmnopqrstuv",
			MetaEncryptionVersion::V2,
		)
		.unwrap()
	}

	// An owner reading a password-protected dir link back from the server holds the accepted
	// password only as a `Hashed` credential. Converting the read-write link into the listing
	// link must keep that hash so `get_password_hash` sends it instead of "empty" (which the
	// server rejects with WrongPassword).
	#[test]
	fn dir_public_link_rw_to_dir_public_link_preserves_hashed_password() {
		let hashed = LinkHashedPassword(Cow::Owned("deadbeefhash".to_string()));
		let rw = DirPublicLinkRW {
			link_uuid: Uuid::new_v4(),
			link_key: Some(test_meta_key()),
			password: PasswordState::Hashed(hashed),
			expiration: PublicLinkExpiration::Never,
			enable_download: true,
			salt: LinkPasswordSalt::None,
		};

		let link = DirPublicLink::try_from(rw).expect("conversion should succeed");

		assert_eq!(
			link.get_password_hash().unwrap().0,
			Cow::Borrowed("deadbeefhash"),
			"hashed credential must survive the conversion instead of deriving to \"empty\""
		);
	}

	fn test_dir(name: &str) -> NonRootItemType<'static, Normal> {
		let now = chrono::Utc::now();
		let (uuid, meta) = RemoteDirectory::make_parts(name, now).unwrap();
		NonRootItemType::Dir(Cow::Owned(RemoteDirectory::new_from_parts(
			uuid,
			meta,
			Uuid::new_v4().into(),
			now,
		)))
	}

	fn test_link() -> ConnectedLink {
		ConnectedLink {
			link: ListedPublicLink {
				link_uuid: Uuid::new_v4(),
				link_key: filen_types::crypto::EncryptedMetaKey(
					filen_types::crypto::EncryptedString(Cow::Borrowed("encrypted")),
				),
			},
			crypter: test_meta_key(),
		}
	}

	#[test]
	fn targets_are_empty_only_without_links_and_users() {
		assert!(ConnectedTargets::default().is_empty());
		let with_link = ConnectedTargets {
			links: vec![test_link()],
			users: Vec::new(),
		};
		assert!(!with_link.is_empty());
		assert!(!ConnectedTargets::with_test_users(1).is_empty());
	}

	#[test]
	fn without_keeps_only_new_links_and_users() {
		let link = test_link();
		let old = ConnectedTargets {
			links: vec![link.clone()],
			users: ConnectedTargets::with_test_users(1).users,
		};
		let new = ConnectedTargets {
			links: vec![link, test_link()],
			users: ConnectedTargets::with_test_users(2).users,
		};
		let added = new.without(&old);
		assert_eq!(added.links.len(), 1);
		assert_eq!(added.links[0].link.link_uuid, new.links[1].link.link_uuid);
		assert_eq!(added.users.iter().map(|u| u.id).collect::<Vec<_>>(), [2]);
		assert!(old.without(&new).is_empty());
	}

	#[test]
	fn no_targets_means_no_propagation_operations() {
		let items = [test_dir("a"), test_dir("b")];
		assert_eq!(ConnectedTargets::default().operations(&items).count(), 0);
	}

	#[test]
	fn operations_cover_every_link_and_user_for_every_item() {
		let items = [test_dir("a"), test_dir("b"), test_dir("c")];
		let [a, b, c] = items.each_ref().map(HasUUID::uuid);
		let targets = ConnectedTargets {
			links: vec![test_link(), test_link()],
			users: ConnectedTargets::with_test_users(1).users,
		};
		let [l0, l1] = [0, 1].map(|i| Some(targets.links[i].link.link_uuid));

		let pairs = targets
			.operations(&items)
			.map(|op| match op {
				PropagationOp::Link { link, item } => {
					(Some(link.link.link_uuid), None, item.uuid())
				}
				PropagationOp::Share { user, item } => (None, Some(user.id), item.uuid()),
			})
			.collect::<Vec<_>>();
		assert_eq!(
			pairs,
			[
				(l0, None, a),
				(l0, None, b),
				(l0, None, c),
				(l1, None, a),
				(l1, None, b),
				(l1, None, c),
				(None, Some(1), a),
				(None, Some(1), b),
				(None, Some(1), c),
			],
			"links first, then users; item order kept"
		);
	}

	// A single failing propagation (e.g. one undecryptable link key) must not abort the
	// updates to the remaining links/receivers: every future is awaited and all errors are
	// collected rather than returning on the first one.
	#[tokio::test]
	async fn drain_collecting_errors_awaits_all_and_aggregates_failures() {
		let futures: FuturesUnordered<MaybeSendBoxFuture<'_, Result<(), Error>>> =
			FuturesUnordered::new();
		futures.push(Box::pin(async { Ok(()) }) as MaybeSendBoxFuture<'_, Result<(), Error>>);
		futures.push(
			Box::pin(async { Err(Error::custom(ErrorKind::Conversion, "first")) })
				as MaybeSendBoxFuture<'_, Result<(), Error>>,
		);
		futures.push(
			Box::pin(async { Err(Error::custom(ErrorKind::Conversion, "second")) })
				as MaybeSendBoxFuture<'_, Result<(), Error>>,
		);

		let errors = drain_collecting_errors(futures).await;

		assert_eq!(
			errors.len(),
			2,
			"both failures must be collected — draining must not abort on the first error"
		);
	}
}
