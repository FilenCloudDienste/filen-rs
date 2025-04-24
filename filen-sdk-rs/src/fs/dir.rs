use std::borrow::Cow;

use chrono::{DateTime, SubsecRound, Utc};
use filen_types::crypto::EncryptedString;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{api, auth::Client, crypto::shared::MetaCrypter, error::Error};

use super::{FSObjectType, HasContents, HasUUID, NonRootObject, file::RemoteFile};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RootDirectory {
	uuid: Uuid,
}

impl RootDirectory {
	pub(crate) fn new(uuid: Uuid) -> Self {
		Self { uuid }
	}
}

impl HasUUID for RootDirectory {
	fn uuid(&self) -> uuid::Uuid {
		self.uuid
	}
}
impl HasContents for RootDirectory {}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Directory {
	uuid: Uuid,
	name: String,
	parent: Uuid,

	color: Option<String>, // todo use Color struct
	created: Option<DateTime<Utc>>,
	favorited: bool,
}

impl Directory {
	pub fn from_encrypted(
		uuid: Uuid,
		parent: Uuid,
		color: Option<String>,
		favorited: bool,
		meta: &EncryptedString,
		decrypter: &impl MetaCrypter,
	) -> Result<Self, crate::error::Error> {
		let meta = DirectoryMeta::from_encrypted(meta, decrypter)?;
		Ok(Self {
			name: meta.name.into_owned(),
			uuid,
			parent,
			color,
			created: meta.created,
			favorited,
		})
	}

	pub fn new(name: String, parent: Uuid, created: DateTime<Utc>) -> Self {
		Self {
			uuid: Uuid::new_v4(),
			name,
			parent,
			color: None,
			created: Some(created.round_subsecs(3)),
			favorited: false,
		}
	}

	pub fn name(&self) -> &str {
		&self.name
	}

	pub(crate) fn set_uuid(&mut self, uuid: Uuid) {
		self.uuid = uuid;
	}

	pub(crate) fn set_parent(&mut self, parent: Uuid) {
		self.parent = parent;
	}

	pub fn created(&self) -> Option<DateTime<Utc>> {
		self.created
	}

	pub fn borrow_meta(&self) -> DirectoryMeta<'_> {
		DirectoryMeta {
			name: Cow::Borrowed(&self.name),
			created: self.created,
		}
	}

	pub fn get_meta(&self) -> DirectoryMeta<'static> {
		DirectoryMeta {
			name: Cow::Owned(self.name.clone()),
			created: self.created,
		}
	}

	pub fn set_meta(&mut self, meta: DirectoryMeta<'_>) {
		self.name = meta.name.into_owned();
		self.created = meta.created;
	}
}

impl HasUUID for Directory {
	fn uuid(&self) -> uuid::Uuid {
		self.uuid
	}
}
impl HasContents for Directory {}

impl NonRootObject for Directory {
	fn name(&self) -> &str {
		&self.name
	}

	fn get_meta_string(&self) -> String {
		// SAFETY if this fails, I want it to panic
		// as this is a logic error
		serde_json::to_string(&DirectoryMeta {
			name: Cow::Borrowed(&self.name),
			created: self.created,
		})
		.unwrap()
	}

	fn parent(&self) -> uuid::Uuid {
		self.parent
	}
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DirectoryType<'a> {
	Root(Cow<'a, RootDirectory>),
	Dir(Cow<'a, Directory>),
}

impl HasUUID for DirectoryType<'_> {
	fn uuid(&self) -> uuid::Uuid {
		match self {
			DirectoryType::Root(dir) => dir.uuid(),
			DirectoryType::Dir(dir) => dir.uuid(),
		}
	}
}
impl HasContents for DirectoryType<'_> {}

mod dir_meta_serde {
	use chrono::{DateTime, Utc};
	use serde::de::Visitor;

	pub(super) fn deserialize<'de, D>(deserializer: D) -> Result<Option<DateTime<Utc>>, D::Error>
	where
		D: serde::Deserializer<'de>,
	{
		struct OptionalDateTimeVisitor;
		impl<'de> Visitor<'de> for OptionalDateTimeVisitor {
			type Value = Option<DateTime<Utc>>;

			fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
				formatter.write_str("an optional timestamp in milliseconds")
			}

			fn visit_none<E>(self) -> Result<Self::Value, E>
			where
				E: serde::de::Error,
			{
				Ok(None)
			}

			fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
			where
				D: serde::Deserializer<'de>,
			{
				deserializer.deserialize_i64(self)
			}

			fn visit_i64<E>(self, v: i64) -> Result<Self::Value, E>
			where
				E: serde::de::Error,
			{
				Ok(Some(
					chrono::DateTime::<Utc>::from_timestamp_millis(v)
						.ok_or_else(|| serde::de::Error::custom("Invalid timestamp"))?,
				))
			}

			fn visit_u64<E>(self, v: u64) -> Result<Self::Value, E>
			where
				E: serde::de::Error,
			{
				self.visit_i64(v.try_into().map_err(|_| {
					serde::de::Error::custom("Invalid timestamp: cannot convert u64 to i64")
				})?)
			}
		}
		deserializer.deserialize_option(OptionalDateTimeVisitor)
	}

	pub(super) fn serialize<S>(
		value: &Option<DateTime<Utc>>,
		serializer: S,
	) -> Result<S::Ok, S::Error>
	where
		S: serde::Serializer,
	{
		match value {
			Some(dt) => serializer.serialize_i64(dt.timestamp_millis()),
			None => serializer.serialize_none(),
		}
	}
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DirectoryMeta<'a> {
	name: Cow<'a, str>,
	#[serde(with = "dir_meta_serde")]
	#[serde(rename = "creation")]
	#[serde(default)]
	created: Option<DateTime<Utc>>,
}

impl DirectoryMeta<'static> {
	pub fn from_encrypted(
		encrypted: &EncryptedString,
		decrypter: &impl MetaCrypter,
	) -> Result<Self, crate::error::Error> {
		let decrypted = decrypter.decrypt_meta(encrypted)?;
		let meta = serde_json::from_str(&decrypted)?;
		Ok(meta)
	}
}

impl<'a> DirectoryMeta<'a> {
	pub fn name(&self) -> &str {
		&self.name
	}

	pub fn created(&self) -> Option<DateTime<Utc>> {
		self.created
	}

	pub fn set_name(&mut self, name: impl Into<Cow<'a, str>>) {
		self.name = name.into();
	}

	pub fn set_created(&mut self, created: DateTime<Utc>) {
		self.created = Some(created.round_subsecs(3));
	}
}

impl Client {
	pub async fn create_dir(
		&self,
		parent: &impl HasContents,
		name: impl Into<String>,
	) -> Result<Directory, Error> {
		let mut dir = Directory::new(name.into(), parent.uuid(), chrono::Utc::now());

		let response = api::v3::dir::create::post(
			self.client(),
			&api::v3::dir::create::Request {
				uuid: dir.uuid(),
				parent: dir.parent(),
				name_hashed: self.hash_name(dir.name()),
				meta: dir.get_encrypted_meta(self.crypter())?,
			},
		)
		.await?;
		if dir.uuid() != response.uuid {
			dir.set_uuid(response.uuid);
		}
		self.update_search_hashes_for_item(&dir).await?;
		Ok(dir)
	}

	pub async fn get_dir(&self, uuid: Uuid) -> Result<Directory, Error> {
		let response = api::v3::dir::post(self.client(), &api::v3::dir::Request { uuid }).await?;

		Directory::from_encrypted(
			uuid,
			response.parent,
			response.color,
			response.favorited,
			&response.metadata,
			self.crypter(),
		)
	}

	pub async fn dir_exists(
		&self,
		parent: &impl HasContents,
		name: impl AsRef<str>,
	) -> Result<Option<uuid::Uuid>, Error> {
		Ok(api::v3::dir::exists::post(
			self.client(),
			&api::v3::dir::exists::Request {
				parent: parent.uuid(),
				name_hashed: self.hash_name(name.as_ref()),
			},
		)
		.await
		.map(|r| r.0)?)
	}

	pub async fn list_dir(
		&self,
		dir: &impl HasContents,
	) -> Result<(Vec<Directory>, Vec<RemoteFile>), Error> {
		let response = api::v3::dir::content::post(
			self.client(),
			&api::v3::dir::content::Request { uuid: dir.uuid() },
		)
		.await?;

		let dirs = response
			.dirs
			.into_iter()
			.map(|d| {
				Directory::from_encrypted(
					d.uuid,
					d.parent,
					d.color,
					d.favorited,
					&d.meta,
					self.crypter(),
				)
			})
			.collect::<Result<Vec<_>, _>>()?;

		let files = response
			.files
			.into_iter()
			.map(|f| {
				RemoteFile::from_encrypted(
					f.uuid,
					f.parent,
					f.size,
					f.chunks,
					f.region,
					f.bucket,
					f.favorited,
					&f.metadata,
					self.crypter(),
				)
			})
			.collect::<Result<Vec<_>, _>>()?;
		Ok((dirs, files))
	}

	pub async fn list_dir_recursive(
		&self,
		dir: &impl HasContents,
	) -> Result<(Vec<Directory>, Vec<RemoteFile>), Error> {
		let response = api::v3::dir::download::post(
			self.client(),
			&api::v3::dir::download::Request {
				uuid: dir.uuid(),
				skip_cache: false,
			},
		)
		.await?;

		let dirs = response
			.dirs
			.into_iter()
			.filter_map(|response_dir| {
				Some(Directory::from_encrypted(
					response_dir.uuid,
					match response_dir.parent {
						// the request returns the base dir for the request as one of its dirs, we filter it out here
						None => return None,
						Some(parent) => parent,
					},
					response_dir.color,
					response_dir.favorited,
					&response_dir.meta,
					self.crypter(),
				))
			})
			.collect::<Result<Vec<_>, _>>()?;

		let files = response
			.files
			.into_iter()
			.map(|f| {
				RemoteFile::from_encrypted(
					f.uuid,
					f.parent,
					f.size,
					f.chunks,
					f.region,
					f.bucket,
					f.favorited,
					&f.metadata,
					self.crypter(),
				)
			})
			.collect::<Result<Vec<_>, _>>()?;
		Ok((dirs, files))
	}

	pub async fn trash_dir(&self, dir: &Directory) -> Result<(), Error> {
		api::v3::dir::trash::post(
			self.client(),
			&api::v3::dir::trash::Request { uuid: dir.uuid() },
		)
		.await?;
		Ok(())
	}

	pub async fn update_dir_metadata(
		&self,
		dir: &mut Directory,
		new_meta: DirectoryMeta<'_>,
	) -> Result<(), Error> {
		api::v3::dir::metadata::post(
			self.client(),
			&api::v3::dir::metadata::Request {
				uuid: dir.uuid(),
				name_hashed: self.hash_name(&new_meta.name),
				metadata: self
					.crypter()
					.encrypt_meta(&serde_json::to_string(&new_meta)?)?,
			},
		)
		.await?;

		dir.set_meta(new_meta);
		Ok(())
	}

	pub async fn find_item_in_dir(
		&self,
		dir: &impl HasContents,
		name: impl AsRef<str>,
	) -> Result<Option<FSObjectType<'static>>, Error> {
		let (dirs, files) = self.list_dir(dir).await?;
		if let Some(dir) = dirs.into_iter().find(|d| d.name() == name.as_ref()) {
			return Ok(Some(FSObjectType::Dir(Cow::Owned(dir))));
		}
		if let Some(file) = files.into_iter().find(|f| f.name() == name.as_ref()) {
			return Ok(Some(FSObjectType::File(Cow::Owned(file))));
		}
		Ok(None)
	}

	pub async fn find_or_create_dir(
		&self,
		path: impl AsRef<str>,
	) -> Result<DirectoryType<'_>, Error> {
		let mut curr_dir = DirectoryType::Root(Cow::Borrowed(self.root()));
		let mut curr_path = String::with_capacity(path.as_ref().len());
		for component in path.as_ref().split('/') {
			if component.is_empty() {
				continue;
			}
			let (dirs, files) = self.list_dir(&curr_dir).await?;
			if let Some(dir) = dirs.into_iter().find(|d| d.name() == component) {
				curr_dir = DirectoryType::Dir(Cow::Owned(dir));
				curr_path.push_str(component);
				curr_path.push('/');
				continue;
			}

			if files.iter().any(|f| f.name() == component) {
				return Err(Error::Custom(format!(
					"find_or_create_dir path {}/{} is a file when trying to create dir {}",
					curr_path,
					component,
					path.as_ref()
				)));
			}

			let new_dir = self.create_dir(&curr_dir, component).await?;
			curr_dir = DirectoryType::Dir(Cow::Owned(new_dir));
			curr_path.push_str(component);
			curr_path.push('/');
		}
		Ok(curr_dir)
	}

	// todo add overwriting
	// I want to add this in tandem with a locking mechanism so that I avoid race conditions
	pub async fn move_dir(
		&self,
		dir: &mut Directory,
		new_parent: &impl HasContents,
	) -> Result<(), Error> {
		api::v3::dir::r#move::post(
			self.client(),
			&api::v3::dir::r#move::Request {
				uuid: dir.uuid(),
				to: new_parent.uuid(),
			},
		)
		.await?;
		dir.set_parent(new_parent.uuid());
		Ok(())
	}

	pub async fn get_dir_size(
		&self,
		dir: &impl HasContents,
		trash: bool,
	) -> Result<api::v3::dir::size::Response, Error> {
		Ok(api::v3::dir::size::post(
			self.client(),
			&api::v3::dir::size::Request {
				uuid: dir.uuid(),
				sharer_id: None,
				receiver_id: None,
				trash,
			},
		)
		.await?)
	}
}
