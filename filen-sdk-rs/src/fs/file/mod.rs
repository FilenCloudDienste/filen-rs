use std::borrow::Cow;

use chrono::{DateTime, SubsecRound, Utc};
use filen_types::{
	crypto::Sha512Hash,
	fs::{ObjectType, ParentUuid, UuidStr},
};
use meta::DecryptedFileMeta;
use traits::{File, HasFileInfo, HasFileMeta, HasRemoteFileInfo, UpdateFileMeta};

use crate::{
	auth::Client,
	crypto::file::FileKey,
	error::{Error, MetadataWasNotDecryptedError},
	fs::{
		SetRemoteInfo,
		dir::HasUUIDContents,
		file::meta::{FileMeta, FileMetaChanges},
	},
};

use super::{HasMeta, HasName, HasParent, HasRemoteInfo, HasType, HasUUID};

pub(crate) mod chunk;
pub mod client_impl;
pub mod enums;
// #[cfg(any(feature = "node", all(target_arch = "wasm32", target_os = "unknown")))]
#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
pub mod js_impl;
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
			parent: *parent.uuid(),
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
				mime: make_mime(&self.name, self.mime),
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

	pub fn parent(&self) -> UuidStr {
		self.parent
	}
}

impl TryFrom<RemoteFile> for BaseFile {
	type Error = crate::error::Error;
	fn try_from(file: RemoteFile) -> Result<Self, Self::Error> {
		let meta = match file.meta {
			FileMeta::Decoded(decrypted_file_meta) => decrypted_file_meta,
			_ => {
				return Err(MetadataWasNotDecryptedError.into());
			}
		};
		Ok(Self {
			root: RootFile {
				uuid: file.uuid,
				name: meta.name.into_owned(),
				mime: meta.mime.into_owned(),
				key: meta.key.into_owned(),
				created: meta.created.unwrap_or_default(),
				modified: meta.last_modified,
			},
			parent: file.parent.try_into()?,
		})
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteFile {
	pub uuid: UuidStr,
	pub meta: FileMeta<'static>,

	pub parent: ParentUuid,
	pub size: u64,
	pub favorited: bool,
	pub region: String,
	pub bucket: String,
	pub chunks: u64,
}

impl PartialEq<BaseFile> for RemoteFile {
	fn eq(&self, other: &BaseFile) -> bool {
		self.uuid == other.uuid()
			&& self.parent == other.parent
			&& self.name() == Some(other.name())
			&& self.mime() == Some(other.mime())
			&& self.key() == Some(other.key())
			&& self.created() == Some(other.created())
			&& self.last_modified() == Some(other.last_modified())
	}
}

impl RemoteFile {
	#[allow(clippy::too_many_arguments)]
	pub fn from_meta(
		uuid: UuidStr,
		parent: ParentUuid,
		fallback_size: u64,
		chunks: u64,
		region: impl Into<String>,
		bucket: impl Into<String>,
		favorited: bool,
		meta: FileMeta<'static>,
	) -> Self {
		let size = match &meta {
			FileMeta::Decoded(decrypted) => decrypted.size,
			_ => fallback_size,
		};
		Self {
			uuid,
			meta,
			parent,
			size,
			favorited,
			region: region.into(),
			bucket: bucket.into(),
			chunks,
		}
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
			uuid: file.uuid,
			parent: file.parent,
			size: file.size,
			favorited: file.favorited,
			region: file.region,
			bucket: file.bucket,
			chunks: file.chunks,
			meta: FileMeta::Decoded(DecryptedFileMeta {
				size: file.size,
				name: Cow::Owned(file.name),
				mime: Cow::Owned(file.mime),
				key: Cow::Owned(file.key),
				created: Some(file.created.round_subsecs(3)),
				last_modified: file.modified.round_subsecs(3),
				hash: file.hash,
			}),
		}
	}
}

impl HasUUID for RemoteFile {
	fn uuid(&self) -> &UuidStr {
		&self.uuid
	}
}

impl HasParent for RemoteFile {
	fn parent(&self) -> &ParentUuid {
		&self.parent
	}
}

impl HasName for RemoteFile {
	fn name(&self) -> Option<&str> {
		self.meta.name()
	}
}

impl HasFileMeta for RemoteFile {
	fn get_meta(&self) -> &FileMeta<'_> {
		&self.meta
	}
}

impl UpdateFileMeta for RemoteFile {
	fn update_meta(&mut self, changes: FileMetaChanges) -> Result<(), Error> {
		self.meta.apply_changes(changes)
	}
}

impl HasMeta for RemoteFile {
	fn get_meta_string(&self) -> Option<Cow<'_, str>> {
		self.meta.try_to_string()
	}
}

impl HasType for RemoteFile {
	fn object_type(&self) -> ObjectType {
		ObjectType::File
	}
}

impl HasFileInfo for RemoteFile {
	fn mime(&self) -> Option<&str> {
		self.meta.mime()
	}

	fn created(&self) -> Option<DateTime<Utc>> {
		self.meta.created()
	}

	fn last_modified(&self) -> Option<DateTime<Utc>> {
		self.meta.last_modified()
	}

	fn size(&self) -> u64 {
		self.size
	}

	fn chunks(&self) -> u64 {
		self.chunks
	}

	fn key(&self) -> Option<&FileKey> {
		self.meta.key()
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
		self.meta.hash()
	}
}

impl PartialEq<RemoteRootFile> for RemoteFile {
	fn eq(&self, other: &RemoteRootFile) -> bool {
		self.meta == other.meta
			&& self.uuid == other.uuid
			&& self.size == other.size
			&& self.region == other.region
			&& self.bucket == other.bucket
			&& self.chunks == other.chunks
	}
}

impl File for RemoteFile {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteRootFile {
	uuid: UuidStr,
	size: u64,
	region: String,
	bucket: String,
	chunks: u64,
	meta: FileMeta<'static>,
}

impl RemoteRootFile {
	pub fn from_meta(
		uuid: UuidStr,
		size: u64,
		chunks: u64,
		region: impl Into<String>,
		bucket: impl Into<String>,
		meta: FileMeta<'static>,
	) -> Self {
		Self {
			uuid,
			meta,
			size,
			region: region.into(),
			bucket: bucket.into(),
			chunks,
		}
	}
}

impl HasUUID for RemoteRootFile {
	fn uuid(&self) -> &UuidStr {
		&self.uuid
	}
}

impl HasName for RemoteRootFile {
	fn name(&self) -> Option<&str> {
		self.meta.name()
	}
}

impl HasFileMeta for RemoteRootFile {
	fn get_meta(&self) -> &FileMeta<'_> {
		&self.meta
	}
}

impl UpdateFileMeta for RemoteRootFile {
	fn update_meta(&mut self, changes: FileMetaChanges) -> Result<(), Error> {
		self.meta.apply_changes(changes)
	}
}

impl HasMeta for RemoteRootFile {
	fn get_meta_string(&self) -> Option<Cow<'_, str>> {
		// If this fails, I want it to panic
		// as this is a logic error
		self.meta.try_to_string()
	}
}

impl HasType for RemoteRootFile {
	fn object_type(&self) -> ObjectType {
		ObjectType::File
	}
}

impl HasFileInfo for RemoteRootFile {
	fn mime(&self) -> Option<&str> {
		self.meta.mime()
	}

	fn created(&self) -> Option<DateTime<Utc>> {
		self.meta.created()
	}

	fn last_modified(&self) -> Option<DateTime<Utc>> {
		self.meta.last_modified()
	}

	fn size(&self) -> u64 {
		self.size
	}

	fn chunks(&self) -> u64 {
		self.chunks
	}

	fn key(&self) -> Option<&FileKey> {
		self.meta.key()
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
		self.meta.hash()
	}
}

impl PartialEq<RemoteFile> for RemoteRootFile {
	fn eq(&self, other: &RemoteFile) -> bool {
		self.meta == other.meta
			&& self.uuid == other.uuid
			&& self.size == other.size
			&& self.region == other.region
			&& self.bucket == other.bucket
			&& self.chunks == other.chunks
	}
}

impl File for RemoteRootFile {}

pub(crate) fn make_mime(name: &str, mime: Option<String>) -> String {
	mime.unwrap_or(
		mime_guess::from_path(name)
			.first_or_octet_stream()
			.to_string(),
	)
}
