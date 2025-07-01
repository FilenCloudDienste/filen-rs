use std::borrow::Cow;

use chrono::{DateTime, SubsecRound, Utc};
use filen_types::{
	crypto::Sha512Hash,
	fs::{ObjectType, ParentUuid, UuidStr},
};
use meta::FileMeta;
use traits::{File, HasFileInfo, HasFileMeta, HasRemoteFileInfo, SetFileMeta};

use crate::{
	auth::Client,
	crypto::file::FileKey,
	fs::{SetRemoteInfo, dir::HasUUIDContents},
};

use super::{HasMeta, HasName, HasParent, HasRemoteInfo, HasType, HasUUID};

pub(crate) mod chunk;
pub mod client_impl;
pub mod enums;
pub mod meta;
pub mod read;
pub mod traits;
pub mod write;

pub struct FileBuilder {
	uuid: UuidStr,
	key: FileKey,

	name: String,
	parent: UuidStr,

	mime: Option<String>,
	created: Option<DateTime<Utc>>,
	modified: Option<DateTime<Utc>>,
}

impl FileBuilder {
	pub(crate) fn new(
		name: impl Into<String>,
		parent: &impl HasUUIDContents,
		client: &Client,
	) -> Self {
		Self {
			uuid: UuidStr::new_v4(),
			name: name.into(),
			parent: parent.uuid(),
			key: client.make_file_key(),
			mime: None,
			created: None,
			modified: None,
		}
	}

	pub fn mime(mut self, mime: String) -> Self {
		self.mime = Some(mime);
		self
	}

	pub fn created(mut self, created: DateTime<Utc>) -> Self {
		self.created = Some(created);
		self
	}

	pub fn modified(mut self, modified: DateTime<Utc>) -> Self {
		self.modified = Some(modified);
		self
	}

	pub fn key(mut self, key: FileKey) -> Self {
		self.key = key;
		self
	}

	/// Should not be used outside of testing
	pub fn uuid(mut self, uuid: UuidStr) -> Self {
		self.uuid = uuid;
		self
	}

	pub fn get_uuid(&self) -> UuidStr {
		self.uuid
	}

	pub fn build(self) -> BaseFile {
		BaseFile {
			root: RootFile {
				uuid: self.uuid,
				mime: self.mime.unwrap_or_else(|| {
					mime_guess::from_ext(self.name.rsplit('.').next().unwrap_or_else(|| &self.name))
						.first_or_octet_stream()
						.to_string()
				}),
				name: self.name,
				key: self.key,
				created: self.created.unwrap_or_else(Utc::now).round_subsecs(3),
				modified: self.modified.unwrap_or_else(Utc::now).round_subsecs(3),
			},
			parent: self.parent,
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootFile {
	pub uuid: UuidStr,
	pub name: String,
	pub mime: String,
	pub key: FileKey,
	pub created: DateTime<Utc>,
	pub modified: DateTime<Utc>,
}

impl RootFile {
	fn from_meta(uuid: UuidStr, meta: FileMeta<'_>) -> Self {
		Self {
			uuid,
			name: meta.name.into_owned(),
			mime: meta.mime.into_owned(),
			key: meta.key.into_owned(),
			created: meta.created.unwrap_or_default(),
			modified: meta.last_modified,
		}
	}

	pub fn uuid(&self) -> UuidStr {
		self.uuid
	}

	pub fn name(&self) -> &str {
		&self.name
	}

	pub fn mime(&self) -> &str {
		&self.mime
	}

	pub fn key(&self) -> &FileKey {
		&self.key
	}

	pub fn created(&self) -> DateTime<Utc> {
		self.created
	}

	pub fn last_modified(&self) -> DateTime<Utc> {
		self.modified
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseFile {
	pub root: RootFile,
	pub parent: UuidStr,
}

impl BaseFile {
	pub fn uuid(&self) -> UuidStr {
		self.root.uuid()
	}

	pub fn name(&self) -> &str {
		self.root.name()
	}

	pub fn mime(&self) -> &str {
		self.root.mime()
	}

	pub fn key(&self) -> &FileKey {
		self.root.key()
	}

	pub fn created(&self) -> DateTime<Utc> {
		self.root.created()
	}

	pub fn last_modified(&self) -> DateTime<Utc> {
		self.root.last_modified()
	}

	pub fn set_modified_now(&mut self) {
		self.root.modified = Utc::now().round_subsecs(3);
	}

	fn from_meta(uuid: UuidStr, parent: UuidStr, meta: FileMeta<'_>) -> Self {
		Self {
			root: RootFile::from_meta(uuid, meta),
			parent,
		}
	}

	pub fn parent(&self) -> UuidStr {
		self.parent
	}
}

impl TryFrom<RemoteFile> for BaseFile {
	type Error = filen_types::error::ConversionError;
	fn try_from(file: RemoteFile) -> Result<Self, Self::Error> {
		Ok(Self {
			root: file.file,
			parent: file.parent.try_into()?,
		})
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteFile {
	pub file: RootFile,
	pub parent: ParentUuid,
	pub size: u64,
	pub favorited: bool,
	pub region: String,
	pub bucket: String,
	pub chunks: u64,
	pub hash: Option<Sha512Hash>,
}

impl RemoteFile {
	#[allow(clippy::too_many_arguments)]
	pub fn from_meta(
		uuid: UuidStr,
		parent: ParentUuid,
		size: u64,
		chunks: u64,
		region: impl Into<String>,
		bucket: impl Into<String>,
		favorited: bool,
		meta: FileMeta<'_>,
	) -> Self {
		Self {
			hash: meta.hash,
			parent,
			file: RootFile::from_meta(uuid, meta),
			size,
			favorited,
			region: region.into(),
			bucket: bucket.into(),
			chunks,
		}
	}
	pub fn inner_file(&self) -> &RootFile {
		&self.file
	}
}

pub struct FlatRemoteFile {
	pub uuid: UuidStr,
	pub parent: ParentUuid,
	pub name: String,
	pub mime: String,
	pub key: FileKey,
	pub created: DateTime<Utc>,
	pub modified: DateTime<Utc>,
	pub size: u64,
	pub chunks: u64,
	pub favorited: bool,
	pub region: String,
	pub bucket: String,
	pub hash: Option<Sha512Hash>,
}

impl From<FlatRemoteFile> for RemoteFile {
	fn from(file: FlatRemoteFile) -> Self {
		Self {
			file: RootFile {
				uuid: file.uuid,
				name: file.name,
				mime: file.mime,
				key: file.key,
				created: file.created,
				modified: file.modified,
			},
			parent: file.parent,
			size: file.size,
			favorited: file.favorited,
			region: file.region,
			bucket: file.bucket,
			chunks: file.chunks,
			hash: file.hash,
		}
	}
}

impl HasUUID for RemoteFile {
	fn uuid(&self) -> UuidStr {
		self.file.uuid()
	}
}

impl HasParent for RemoteFile {
	fn parent(&self) -> ParentUuid {
		self.parent
	}
}

impl HasName for RemoteFile {
	fn name(&self) -> &str {
		self.file.name()
	}
}

impl HasFileMeta for RemoteFile {
	fn borrow_meta(&self) -> FileMeta<'_> {
		FileMeta {
			name: Cow::Borrowed(self.name()),
			size: self.size,
			mime: Cow::Borrowed(self.mime()),
			key: Cow::Borrowed(self.key()),
			created: Some(self.created()),
			last_modified: self.last_modified(),
			hash: self.hash,
		}
	}
	fn get_meta(&self) -> FileMeta<'static> {
		FileMeta {
			name: Cow::Owned(self.name().to_owned()),
			size: self.size,
			mime: Cow::Owned(self.mime().to_owned()),
			key: Cow::Owned(self.key().clone()),
			created: Some(self.created()),
			last_modified: self.last_modified(),
			hash: self.hash,
		}
	}
}

impl SetFileMeta for RemoteFile {
	fn set_meta(&mut self, meta: FileMeta<'_>) {
		self.file.name = meta.name.into_owned();
		self.file.mime = meta.mime.into_owned();
		self.file.key = meta.key.into_owned();
		self.file.modified = meta.last_modified;
		self.file.created = meta.created.unwrap_or_default();
	}
}

impl HasMeta for RemoteFile {
	fn get_meta_string(&self) -> String {
		// If this fails, I want it to panic
		// as this is a logic error
		serde_json::to_string(&self.borrow_meta()).unwrap()
	}
}

impl HasType for RemoteFile {
	fn object_type(&self) -> ObjectType {
		ObjectType::File
	}
}

impl HasFileInfo for RemoteFile {
	fn mime(&self) -> &str {
		self.file.mime()
	}

	fn created(&self) -> DateTime<Utc> {
		self.file.created()
	}

	fn last_modified(&self) -> DateTime<Utc> {
		self.file.last_modified()
	}

	fn size(&self) -> u64 {
		self.size
	}

	fn chunks(&self) -> u64 {
		self.chunks
	}

	fn key(&self) -> &FileKey {
		self.file.key()
	}
}

impl HasRemoteInfo for RemoteFile {
	fn favorited(&self) -> bool {
		self.favorited
	}
}

impl SetRemoteInfo for RemoteFile {
	fn set_favorited(&mut self, value: bool) {
		self.favorited = value;
	}
}

impl HasRemoteFileInfo for RemoteFile {
	fn region(&self) -> &str {
		&self.region
	}

	fn bucket(&self) -> &str {
		&self.bucket
	}

	fn hash(&self) -> Option<Sha512Hash> {
		self.hash
	}
}

impl PartialEq<RemoteRootFile> for RemoteFile {
	fn eq(&self, other: &RemoteRootFile) -> bool {
		self.file == other.file
			&& self.size == other.size
			&& self.region == other.region
			&& self.bucket == other.bucket
			&& self.chunks == other.chunks
			&& self.hash == other.hash
	}
}

impl File for RemoteFile {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteRootFile {
	file: RootFile,
	size: u64,
	region: String,
	bucket: String,
	chunks: u64,
	hash: Option<Sha512Hash>,
}

impl RemoteRootFile {
	pub fn from_meta(
		uuid: UuidStr,
		size: u64,
		chunks: u64,
		region: impl Into<String>,
		bucket: impl Into<String>,
		meta: FileMeta<'_>,
	) -> Self {
		Self {
			hash: meta.hash,
			file: RootFile::from_meta(uuid, meta),
			size,
			region: region.into(),
			bucket: bucket.into(),
			chunks,
		}
	}
	pub fn inner_file(&self) -> &RootFile {
		&self.file
	}
}

impl HasUUID for RemoteRootFile {
	fn uuid(&self) -> UuidStr {
		self.file.uuid
	}
}

impl HasName for RemoteRootFile {
	fn name(&self) -> &str {
		self.file.name()
	}
}

impl HasFileMeta for RemoteRootFile {
	fn borrow_meta(&self) -> FileMeta<'_> {
		FileMeta {
			name: Cow::Borrowed(self.name()),
			size: self.size,
			mime: Cow::Borrowed(self.mime()),
			key: Cow::Borrowed(self.key()),
			created: Some(self.created()),
			last_modified: self.last_modified(),
			hash: self.hash,
		}
	}
	fn get_meta(&self) -> FileMeta<'static> {
		FileMeta {
			name: Cow::Owned(self.name().to_owned()),
			size: self.size,
			mime: Cow::Owned(self.mime().to_owned()),
			key: Cow::Owned(self.key().clone()),
			created: Some(self.created()),
			last_modified: self.last_modified(),
			hash: self.hash,
		}
	}
}

impl SetFileMeta for RemoteRootFile {
	fn set_meta(&mut self, meta: FileMeta<'_>) {
		self.file.name = meta.name.into_owned();
		self.file.mime = meta.mime.into_owned();
		self.file.key = meta.key.into_owned();
		self.file.modified = meta.last_modified;
		self.file.created = meta.created.unwrap_or_default();
	}
}

impl HasMeta for RemoteRootFile {
	fn get_meta_string(&self) -> String {
		// If this fails, I want it to panic
		// as this is a logic error
		serde_json::to_string(&self.borrow_meta()).unwrap()
	}
}

impl HasType for RemoteRootFile {
	fn object_type(&self) -> ObjectType {
		ObjectType::File
	}
}

impl HasFileInfo for RemoteRootFile {
	fn mime(&self) -> &str {
		self.file.mime()
	}

	fn created(&self) -> DateTime<Utc> {
		self.file.created()
	}

	fn last_modified(&self) -> DateTime<Utc> {
		self.file.last_modified()
	}

	fn size(&self) -> u64 {
		self.size
	}

	fn chunks(&self) -> u64 {
		self.chunks
	}

	fn key(&self) -> &FileKey {
		self.file.key()
	}
}

impl HasRemoteInfo for RemoteRootFile {
	fn favorited(&self) -> bool {
		false
	}
}

impl HasRemoteFileInfo for RemoteRootFile {
	fn region(&self) -> &str {
		&self.region
	}

	fn bucket(&self) -> &str {
		&self.bucket
	}

	fn hash(&self) -> Option<Sha512Hash> {
		self.hash
	}
}

impl PartialEq<RemoteFile> for RemoteRootFile {
	fn eq(&self, other: &RemoteFile) -> bool {
		self.file == other.file
			&& self.size == other.size
			&& self.region == other.region
			&& self.bucket == other.bucket
			&& self.chunks == other.chunks
			&& self.hash == other.hash
	}
}

impl File for RemoteRootFile {}
