use std::borrow::Cow;

use chrono::{DateTime, SubsecRound, Utc};
use filen_types::{
	auth::FileEncryptionVersion,
	crypto::{Blake3Hash, MaybeEncrypted},
	fs::{ObjectType, ParentUuid, UuidStr},
	traits::CowHelpers,
};
use meta::DecryptedFileMeta;
use traits::{File, HasFileInfo, HasFileMeta, HasRemoteFileInfo, UpdateFileMeta};

use crate::{
	auth::Client,
	crypto::{file::FileKey, shared::MetaCrypter},
	error::Error,
	fs::{
		SetRemoteInfo,
		file::meta::{FileMeta, FileMetaChanges},
		name::{EntryNameError, ValidatedName},
	},
	runtime::blocking_join,
};

use super::{HasMeta, HasName, HasParent, HasRemoteInfo, HasType, HasUUID};

#[cfg(feature = "cache")]
pub mod cache;
pub(crate) mod chunk;
pub mod client_impl;
pub mod enums;
#[cfg(any(feature = "wasm-full", feature = "uniffi"))]
pub mod js_impl;
pub mod meta;
pub mod read;
pub(crate) mod service_worker;
pub mod traits;
pub mod write;

pub struct FileBuilder {
	uuid: UuidStr,
	key: FileKey,

	name: ValidatedName,
	parent: UuidStr,

	mime: Option<String>,
	created: Option<DateTime<Utc>>,
	modified: Option<DateTime<Utc>>,
}

impl FileBuilder {
	pub(crate) fn new(
		name: &str,
		parent_uuid: UuidStr,
		client: &Client,
	) -> Result<Self, EntryNameError> {
		Ok(Self::new_valid_name(
			ValidatedName::try_from(name)?,
			UuidStr::new_v4(),
			parent_uuid,
			client,
		))
	}

	fn new_valid_name(
		name: ValidatedName,
		uuid: UuidStr,
		parent_uuid: UuidStr,
		client: &Client,
	) -> Self {
		Self {
			uuid,
			name,
			parent: parent_uuid,
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

	pub fn get_created(&self) -> Option<DateTime<Utc>> {
		self.created
	}

	pub fn get_modified(&self) -> Option<DateTime<Utc>> {
		self.modified
	}

	pub fn get_name(&self) -> &str {
		self.name.as_ref()
	}

	pub fn build(self) -> BaseFile {
		BaseFile {
			root: RootFile {
				uuid: self.uuid,
				mime: make_mime(self.name.as_ref(), self.mime),
				name: self.name,
				key: self.key,
				created: self.created.unwrap_or_else(Utc::now).round_subsecs(3),
				modified: self.modified.unwrap_or_else(Utc::now).round_subsecs(3),
			},
			parent: self.parent,
		}
	}
}

pub struct FileBuilderOptionalName {
	name: Option<ValidatedName>,
	uuid: UuidStr,
	parent: UuidStr,

	mime: Option<String>,
	created: Option<DateTime<Utc>>,
	modified: Option<DateTime<Utc>>,
}

impl FileBuilderOptionalName {
	pub fn new(parent_uuid: UuidStr) -> Self {
		Self {
			name: None,
			uuid: UuidStr::new_v4(),
			parent: parent_uuid,
			mime: None,
			created: None,
			modified: None,
		}
	}

	pub fn name(&mut self, name: &str) -> Result<&mut Self, EntryNameError> {
		self.name = Some(ValidatedName::try_from(name)?);
		Ok(self)
	}

	pub fn mime(&mut self, mime: String) -> &mut Self {
		self.mime = Some(mime);
		self
	}

	pub fn created(&mut self, created: DateTime<Utc>) -> &mut Self {
		self.created = Some(created);
		self
	}

	pub fn modified(&mut self, modified: DateTime<Utc>) -> &mut Self {
		self.modified = Some(modified);
		self
	}

	pub fn into_builder<'a>(
		self,
		name_maker: &'a impl Fn() -> Result<&'a str, Error>,
		client: &Client,
	) -> Result<FileBuilder, Error> {
		let name = match self.name {
			Some(name) => name,
			None => ValidatedName::try_from(name_maker()?)?,
		};
		let mut builder = FileBuilder::new_valid_name(name, self.uuid, self.parent, client);

		if let Some(mime) = self.mime {
			builder = builder.mime(mime);
		}
		if let Some(created) = self.created {
			builder = builder.created(created);
		}
		if let Some(modified) = self.modified {
			builder = builder.modified(modified);
		}

		Ok(builder)
	}

	pub fn get_name(&self) -> Option<&str> {
		self.name.as_ref().map(|n| n.as_ref())
	}

	pub fn get_uuid(&self) -> UuidStr {
		self.uuid
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootFile {
	pub uuid: UuidStr,
	pub name: ValidatedName,
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
		self.name.as_ref()
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

#[cfg_attr(
	feature = "http-provider",
	derive(serde::Serialize, serde::Deserialize),
	serde(rename_all = "camelCase")
)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteFile {
	pub uuid: UuidStr,
	#[cfg_attr(feature = "http-provider", serde(with = "meta::serde_stateless"))]
	pub meta: FileMeta<'static>,

	pub parent: ParentUuid,
	pub size: u64,
	pub favorited: bool,
	pub region: String,
	pub bucket: String,
	pub timestamp: DateTime<Utc>,
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
		timestamp: DateTime<Utc>,
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
			timestamp,
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
	pub timestamp: DateTime<Utc>,
	pub hash: Option<Blake3Hash>,
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
			timestamp: file.timestamp,
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

	fn timestamp(&self) -> DateTime<Utc> {
		self.timestamp
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

	fn hash(&self) -> Option<Blake3Hash> {
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
#[cfg_attr(
	feature = "http-provider",
	derive(serde::Serialize, serde::Deserialize),
	serde(rename_all = "camelCase")
)]
pub struct RemoteRootFile {
	pub(crate) uuid: UuidStr,
	pub(crate) size: u64,
	pub(crate) region: String,
	pub(crate) bucket: String,
	pub(crate) chunks: u64,
	pub(crate) timestamp: DateTime<Utc>,
	#[cfg_attr(feature = "http-provider", serde(with = "meta::serde_stateless"))]
	pub(crate) meta: FileMeta<'static>,
}

impl RemoteRootFile {
	pub fn from_meta(
		uuid: UuidStr,
		size: u64,
		chunks: u64,
		region: impl Into<String>,
		bucket: impl Into<String>,
		timestamp: DateTime<Utc>,
		meta: FileMeta<'static>,
	) -> Self {
		Self {
			uuid,
			meta,
			size,
			region: region.into(),
			bucket: bucket.into(),
			timestamp,
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

	fn timestamp(&self) -> DateTime<Utc> {
		self.timestamp
	}
}

impl HasRemoteFileInfo for RemoteRootFile {
	fn region(&self) -> &str {
		&self.region
	}

	fn bucket(&self) -> &str {
		&self.bucket
	}

	fn hash(&self) -> Option<Blake3Hash> {
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileVersion {
	pub(crate) bucket: String,
	pub(crate) region: String,
	pub(crate) chunks: u64,
	pub(crate) size: u64,
	pub(crate) metadata: FileMeta<'static>,
	pub(crate) timestamp: DateTime<Utc>,
	pub(crate) uuid: UuidStr,
}

impl FileVersion {
	pub fn size(&self) -> u64 {
		self.size
	}

	pub fn timestamp(&self) -> DateTime<Utc> {
		self.timestamp
	}

	pub fn metadata(&self) -> &FileMeta<'_> {
		&self.metadata
	}

	pub(crate) fn blocking_from_response(
		crypter: &impl MetaCrypter,
		response: filen_types::api::v3::file::versions::FileVersion<'_>,
	) -> Self {
		Self {
			bucket: response.bucket.into_owned(),
			region: response.region.into_owned(),
			chunks: response.chunks,
			size: response.size,
			metadata: FileMeta::blocking_from_encrypted(
				response.metadata,
				crypter,
				response.version,
			)
			.into_owned_cow(),
			timestamp: response.timestamp,
			uuid: response.uuid,
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(
	feature = "http-provider",
	derive(serde::Serialize),
	serde(rename_all = "camelCase")
)]
pub struct LinkedFile {
	pub(crate) uuid: UuidStr,
	pub(crate) name: MaybeEncrypted<'static, str>,
	pub(crate) mime: MaybeEncrypted<'static, str>,
	pub(crate) size: u64,
	pub(crate) chunks: u64,
	pub(crate) region: String,
	pub(crate) bucket: String,
	pub(crate) version: FileEncryptionVersion,
	#[cfg_attr(
		feature = "http-provider",
		serde(with = "chrono::serde::ts_milliseconds")
	)]
	pub(crate) timestamp: DateTime<Utc>,
	pub(crate) file_key: FileKey,
}

#[cfg(feature = "http-provider")]
impl<'de> serde::Deserialize<'de> for LinkedFile {
	fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
	where
		D: serde::Deserializer<'de>,
	{
		#[derive(serde::Deserialize)]
		struct LinkedFileHelper<'a> {
			uuid: UuidStr,
			name: MaybeEncrypted<'static, str>,
			mime: MaybeEncrypted<'static, str>,
			size: u64,
			chunks: u64,
			region: String,
			bucket: String,
			version: FileEncryptionVersion,
			#[serde(with = "chrono::serde::ts_milliseconds")]
			timestamp: DateTime<Utc>,
			#[serde(borrow)]
			file_key: Cow<'a, str>,
		}

		let helper = LinkedFileHelper::deserialize(deserializer)?;
		let file_key = FileKey::from_string_with_version(helper.file_key, helper.version)
			.map_err(serde::de::Error::custom)?;

		Ok(Self {
			uuid: helper.uuid,
			name: helper.name,
			mime: helper.mime,
			size: helper.size,
			chunks: helper.chunks,
			region: helper.region,
			bucket: helper.bucket,
			version: helper.version,
			timestamp: helper.timestamp,
			file_key,
		})
	}
}

impl LinkedFile {
	pub(crate) fn blocking_from_response(
		file_key: FileKey,
		response: filen_types::api::v3::file::link::info::Response<'static>,
	) -> Result<Self, Error> {
		let meta_key = file_key.to_meta_key()?;
		let (name, mime, size) = blocking_join!(
			|| match meta_key.blocking_decrypt_meta(&response.name) {
				Ok(decrypted) => MaybeEncrypted::Decrypted(Cow::Owned(decrypted)),
				Err(_) => MaybeEncrypted::Encrypted(response.name.into_owned_cow()),
			},
			|| match meta_key.blocking_decrypt_meta(&response.mime) {
				Ok(decrypted) => MaybeEncrypted::Decrypted(Cow::Owned(decrypted)),
				Err(_) => MaybeEncrypted::Encrypted(response.mime.into_owned_cow()),
			},
			|| meta_key
				.blocking_decrypt_meta(&response.size)
				.map_err(Error::from)
				.and_then(
					|s| str::parse::<u64>(&s).map_err(|e| Error::custom_with_source(
						crate::ErrorKind::Conversion,
						e,
						Some("Failed to parse decrypted size string to u64")
					))
				)
		);

		Ok(Self {
			uuid: response.uuid,
			name,
			mime,
			size: size?,
			chunks: response.chunks,
			region: response.region.into_owned(),
			bucket: response.bucket.into_owned(),
			version: response.version,
			timestamp: response.timestamp,
			file_key,
		})
	}
}

impl HasFileInfo for LinkedFile {
	fn mime(&self) -> Option<&str> {
		match &self.mime {
			MaybeEncrypted::Decrypted(mime) => Some(mime.as_ref()),
			MaybeEncrypted::Encrypted(_) => None,
		}
	}

	fn created(&self) -> Option<DateTime<Utc>> {
		None
	}

	fn last_modified(&self) -> Option<DateTime<Utc>> {
		Some(self.timestamp)
	}

	fn size(&self) -> u64 {
		self.size
	}

	fn chunks(&self) -> u64 {
		self.chunks
	}

	fn key(&self) -> Option<&FileKey> {
		Some(&self.file_key)
	}
}

impl HasUUID for LinkedFile {
	fn uuid(&self) -> &UuidStr {
		&self.uuid
	}
}

impl HasRemoteInfo for LinkedFile {
	fn favorited(&self) -> bool {
		false
	}

	fn timestamp(&self) -> DateTime<Utc> {
		self.timestamp
	}
}

impl HasRemoteFileInfo for LinkedFile {
	fn region(&self) -> &str {
		&self.region
	}

	fn bucket(&self) -> &str {
		&self.bucket
	}

	fn hash(&self) -> Option<Blake3Hash> {
		None
	}
}

impl HasMeta for LinkedFile {
	fn get_meta_string(&self) -> Option<Cow<'_, str>> {
		None
	}
}

impl HasName for LinkedFile {
	fn name(&self) -> Option<&str> {
		match &self.name {
			MaybeEncrypted::Decrypted(name) => Some(name),
			MaybeEncrypted::Encrypted(_) => None,
		}
	}
}

impl File for LinkedFile {}

impl PartialEq<RemoteFile> for LinkedFile {
	fn eq(&self, other: &RemoteFile) -> bool {
		self.uuid == other.uuid
			&& self.name() == other.name()
			&& self.mime() == other.mime()
			&& self.size == other.size()
			&& self.chunks == other.chunks
			&& self.region == other.region
			&& self.bucket == other.bucket
	}
}

// #[cfg_attr(feature = "uniffi", uniffi::export)]
// #[cfg_attr(
// 	all(target_family = "wasm", target_os = "unknown"),
// 	wasm_bindgen::prelude::wasm_bindgen
// )]
// impl LinkedFile {
// 	#[cfg_attr(feature = "uniffi", uniffi::method(name = "uuid"))]
// 	#[cfg_attr(
// 		all(target_family = "wasm", target_os = "unknown"),
// 		wasm_bindgen::prelude::wasm_bindgen(getter, js_name = "uuid")
// 	)]
// 	fn inner_uuid(&self) -> UuidStr {
// 		self.uuid
// 	}

// 	#[cfg_attr(feature = "uniffi", uniffi::method(name = "size"))]
// 	#[cfg_attr(
// 		all(target_family = "wasm", target_os = "unknown"),
// 		wasm_bindgen::prelude::wasm_bindgen(getter, js_name = "size")
// 	)]
// 	fn inner_size(&self) -> u64 {
// 		self.size
// 	}
// }

// #[cfg(any(feature = "uniffi", all(target_family = "wasm", target_os = "unknown")))]
// #[cfg_attr(feature = "uniffi", uniffi::export)]
// #[cfg_attr(
// 	all(target_family = "wasm", target_os = "unknown"),
// 	wasm_bindgen::prelude::wasm_bindgen
// )]
// impl LinkedFile {
// 	#[cfg_attr(feature = "uniffi", uniffi::method(name = "name"))]
// 	#[cfg_attr(
// 		all(target_family = "wasm", target_os = "unknown"),
// 		wasm_bindgen::prelude::wasm_bindgen(getter, js_name = "name")
// 	)]
// 	fn name_clone(&self) -> Option<String> {
// 		self.name().map(|s| s.to_string())
// 	}

// 	#[cfg_attr(feature = "uniffi", uniffi::method(name = "mime"))]
// 	#[cfg_attr(
// 		all(target_family = "wasm", target_os = "unknown"),
// 		wasm_bindgen::prelude::wasm_bindgen(getter, js_name = "mime")
// 	)]
// 	fn mime_clone(&self) -> Option<String> {
// 		self.mime().map(|s| s.to_string())
// 	}
// }
