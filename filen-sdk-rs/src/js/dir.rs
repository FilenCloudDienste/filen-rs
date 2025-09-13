use std::borrow::Cow;

use chrono::{DateTime, Utc};
use filen_types::{
	crypto::{EncryptedString, rsa::RSAEncryptedString},
	fs::{ParentUuid, UuidStr},
};
use serde::{Deserialize, Serialize};

use crate::{
	connect::fs::SharingRole,
	fs::{
		HasUUID,
		dir::{
			DecryptedDirectoryMeta as SDKDecryptedDirMeta, DirectoryMetaType, DirectoryType,
			DirectoryTypeWithShareInfo, RemoteDirectory, RootDirectory, RootDirectoryWithMeta,
			UnsharedDirectoryType, meta::DirectoryMeta,
		},
	},
	js::{AsEncodedOrDecoded, EncodedOrDecoded},
};

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
use tsify::Tsify;

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq)]
#[cfg_attr(all(target_arch = "wasm32", target_os = "unknown"), derive(Tsify))]
#[cfg_attr(
	all(target_arch = "wasm32", target_os = "unknown"),
	tsify(large_number_types_as_bigints)
)]
#[cfg_attr(test, derive(Debug))]
pub struct DecryptedDirMeta {
	pub name: String,
	#[cfg_attr(
		all(target_arch = "wasm32", target_os = "unknown"),
		tsify(type = "bigint")
	)]
	#[serde(
		with = "filen_types::serde::time::optional",
		skip_serializing_if = "Option::is_none",
		default
	)]
	pub created: Option<DateTime<Utc>>,
}

impl From<SDKDecryptedDirMeta<'_>> for DecryptedDirMeta {
	fn from(meta: SDKDecryptedDirMeta) -> Self {
		DecryptedDirMeta {
			name: meta.name.into_owned(),
			created: meta.created,
		}
	}
}

impl From<DecryptedDirMeta> for SDKDecryptedDirMeta<'static> {
	fn from(meta: DecryptedDirMeta) -> Self {
		SDKDecryptedDirMeta {
			name: Cow::Owned(meta.name),
			created: meta.created,
		}
	}
}

#[derive(Clone, PartialEq, Eq)]
#[cfg_attr(test, derive(Debug))]
pub enum DirMeta {
	Decoded(DecryptedDirMeta),
	DecryptedRaw(Vec<u8>),
	DecryptedUTF8(String),
	Encrypted(String),
	RSAEncrypted(String),
}

#[derive(Serialize, Deserialize)]
enum DirMetaEncoded<'a> {
	DecryptedRaw(Cow<'a, [u8]>),
	DecryptedUTF8(Cow<'a, str>),
	Encrypted(EncryptedString<'a>),
	RSAEncrypted(RSAEncryptedString<'a>),
}

impl From<DirectoryMeta<'_>> for DirMeta {
	fn from(meta: DirectoryMeta) -> Self {
		match meta {
			DirectoryMeta::Decoded(meta) => DirMeta::Decoded(meta.into()),
			DirectoryMeta::DecryptedRaw(meta) => DirMeta::DecryptedRaw(meta.into_owned()),
			DirectoryMeta::DecryptedUTF8(meta) => DirMeta::DecryptedUTF8(meta.into_owned()),
			DirectoryMeta::Encrypted(meta) => DirMeta::Encrypted(meta.0.into_owned()),
			DirectoryMeta::RSAEncrypted(meta) => DirMeta::RSAEncrypted(meta.0.into_owned()),
		}
	}
}

impl From<DirMeta> for DirectoryMeta<'static> {
	fn from(meta: DirMeta) -> Self {
		match meta {
			DirMeta::Decoded(meta) => DirectoryMeta::Decoded(meta.into()),
			DirMeta::DecryptedRaw(meta) => DirectoryMeta::DecryptedRaw(Cow::Owned(meta)),
			DirMeta::DecryptedUTF8(meta) => DirectoryMeta::DecryptedUTF8(Cow::Owned(meta)),
			DirMeta::Encrypted(meta) => DirectoryMeta::Encrypted(EncryptedString(Cow::Owned(meta))),
			DirMeta::RSAEncrypted(meta) => {
				DirectoryMeta::RSAEncrypted(RSAEncryptedString(Cow::Owned(meta)))
			}
		}
	}
}

#[derive(Serialize, Deserialize)]
#[cfg_attr(all(target_arch = "wasm32", target_os = "unknown"), derive(Tsify))]
#[cfg_attr(
	all(target_arch = "wasm32", target_os = "unknown"),
	tsify(into_wasm_abi, from_wasm_abi)
)]
#[cfg_attr(test, derive(Debug, PartialEq, Eq, Clone))]
pub struct Root {
	pub uuid: UuidStr,
}

impl From<RootDirectory> for Root {
	fn from(dir: RootDirectory) -> Self {
		Root { uuid: *dir.uuid() }
	}
}

impl From<Root> for RootDirectory {
	fn from(root: Root) -> Self {
		RootDirectory::new(root.uuid)
	}
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DirColor {
	Default,
	Blue,
	Green,
	Purple,
	Red,
	Gray,
	#[serde(untagged)]
	Custom(String),
}

// tsify does not support untagged variants yet: https://github.com/madonoharu/tsify/issues/52
#[wasm_bindgen::prelude::wasm_bindgen(typescript_custom_section)]
const TS_DIR_COLOR: &'static str =
	r#"export type DirColor = "default" | "blue" | "green" | "purple" | "red" | "gray" | string;"#;

impl From<filen_types::api::v3::dir::color::DirColor<'_>> for DirColor {
	fn from(color: filen_types::api::v3::dir::color::DirColor) -> Self {
		match color {
			filen_types::api::v3::dir::color::DirColor::Default => DirColor::Default,
			filen_types::api::v3::dir::color::DirColor::Blue => DirColor::Blue,
			filen_types::api::v3::dir::color::DirColor::Green => DirColor::Green,
			filen_types::api::v3::dir::color::DirColor::Purple => DirColor::Purple,
			filen_types::api::v3::dir::color::DirColor::Red => DirColor::Red,
			filen_types::api::v3::dir::color::DirColor::Gray => DirColor::Gray,
			filen_types::api::v3::dir::color::DirColor::Custom(c) => {
				DirColor::Custom(c.into_owned())
			}
		}
	}
}

impl From<DirColor> for filen_types::api::v3::dir::color::DirColor<'static> {
	fn from(color: DirColor) -> Self {
		match color {
			DirColor::Default => filen_types::api::v3::dir::color::DirColor::Default,
			DirColor::Blue => filen_types::api::v3::dir::color::DirColor::Blue,
			DirColor::Green => filen_types::api::v3::dir::color::DirColor::Green,
			DirColor::Purple => filen_types::api::v3::dir::color::DirColor::Purple,
			DirColor::Red => filen_types::api::v3::dir::color::DirColor::Red,
			DirColor::Gray => filen_types::api::v3::dir::color::DirColor::Gray,
			DirColor::Custom(c) => {
				filen_types::api::v3::dir::color::DirColor::Custom(Cow::Owned(c))
			}
		}
	}
}

#[derive(Clone)]
#[cfg_attr(all(target_arch = "wasm32", target_os = "unknown"), derive(Tsify))]
#[cfg_attr(
	all(target_arch = "wasm32", target_os = "unknown"),
	tsify(into_wasm_abi, from_wasm_abi)
)]
#[cfg_attr(test, derive(Debug, PartialEq, Eq))]
pub struct Dir {
	pub uuid: UuidStr,
	pub parent: ParentUuid,
	pub color: DirColor,
	pub favorited: bool,
	#[cfg_attr(
		all(target_arch = "wasm32", target_os = "unknown"),
		tsify(optional, type = "DecryptedDirMeta")
	)]
	pub meta: DirMeta,
}

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
impl wasm_bindgen::__rt::VectorIntoJsValue for Dir {
	fn vector_into_jsvalue(
		vector: wasm_bindgen::__rt::std::boxed::Box<[Self]>,
	) -> wasm_bindgen::JsValue {
		wasm_bindgen::__rt::js_value_vector_into_jsvalue(vector)
	}
}

impl From<RemoteDirectory> for Dir {
	fn from(dir: RemoteDirectory) -> Self {
		Dir {
			uuid: dir.uuid,
			parent: dir.parent,
			color: dir.color.into(),
			favorited: dir.favorited,
			meta: dir.meta.into(),
		}
	}
}

impl From<Dir> for RemoteDirectory {
	fn from(dir: Dir) -> Self {
		RemoteDirectory::from_meta(
			dir.uuid,
			dir.parent,
			dir.color.into(),
			dir.favorited,
			dir.meta.into(),
		)
	}
}

#[derive(Deserialize)]
#[cfg_attr(all(target_arch = "wasm32", target_os = "unknown"), derive(Tsify))]
#[cfg_attr(
	all(target_arch = "wasm32", target_os = "unknown"),
	tsify(from_wasm_abi)
)]
#[serde(untagged)]
#[cfg_attr(test, derive(Debug, PartialEq, Eq))]
pub enum DirEnum {
	Dir(Dir),
	Root(Root),
}

impl From<RemoteDirectory> for DirEnum {
	fn from(dir: RemoteDirectory) -> Self {
		DirEnum::Dir(Dir::from(dir))
	}
}

impl From<UnsharedDirectoryType<'_>> for DirEnum {
	fn from(dir: UnsharedDirectoryType<'_>) -> Self {
		match dir {
			UnsharedDirectoryType::Root(root) => DirEnum::Root(Root::from(root.into_owned())),
			UnsharedDirectoryType::Dir(dir) => {
				let dir = dir.into_owned();
				DirEnum::Dir(Dir::from(dir))
			}
		}
	}
}

impl From<DirEnum> for UnsharedDirectoryType<'static> {
	fn from(dir: DirEnum) -> Self {
		match dir {
			DirEnum::Root(root) => UnsharedDirectoryType::Root(Cow::Owned(root.into())),
			DirEnum::Dir(dir) => UnsharedDirectoryType::Dir(Cow::Owned(dir.into())),
		}
	}
}

impl<'a>
	AsEncodedOrDecoded<
		'a,
		DirMetaEncoded<'a>,
		&'a DecryptedDirMeta,
		DirMetaEncoded<'static>,
		DecryptedDirMeta,
	> for DirMeta
{
	fn as_encoded_or_decoded(
		&'a self,
	) -> EncodedOrDecoded<DirMetaEncoded<'a>, &'a DecryptedDirMeta> {
		match self {
			DirMeta::Decoded(meta) => EncodedOrDecoded::Decoded(meta),
			DirMeta::DecryptedRaw(data) => {
				EncodedOrDecoded::Encoded(DirMetaEncoded::DecryptedRaw(Cow::Borrowed(data)))
			}
			DirMeta::DecryptedUTF8(data) => {
				EncodedOrDecoded::Encoded(DirMetaEncoded::DecryptedUTF8(Cow::Borrowed(data)))
			}
			DirMeta::Encrypted(data) => EncodedOrDecoded::Encoded(DirMetaEncoded::Encrypted(
				EncryptedString(Cow::Borrowed(data)),
			)),
			DirMeta::RSAEncrypted(data) => EncodedOrDecoded::Encoded(DirMetaEncoded::RSAEncrypted(
				RSAEncryptedString(Cow::Borrowed(data)),
			)),
		}
	}

	fn from_encoded(encoded: DirMetaEncoded<'static>) -> Self {
		match encoded {
			DirMetaEncoded::DecryptedRaw(data) => DirMeta::DecryptedRaw(data.into_owned()),
			DirMetaEncoded::DecryptedUTF8(data) => DirMeta::DecryptedUTF8(data.into_owned()),
			DirMetaEncoded::Encrypted(data) => DirMeta::Encrypted(data.0.into_owned()),
			DirMetaEncoded::RSAEncrypted(data) => DirMeta::RSAEncrypted(data.0.into_owned()),
		}
	}

	fn from_decoded(decoded: DecryptedDirMeta) -> Self {
		DirMeta::Decoded(decoded)
	}
}

#[derive(Clone, PartialEq, Eq, Tsify)]
#[tsify(from_wasm_abi, into_wasm_abi)]
pub struct RootWithMeta {
	pub uuid: UuidStr,
	pub color: DirColor,
	#[tsify(optional, type = "DecryptedDirMeta")]
	pub meta: DirMeta,
}

impl From<RootWithMeta> for RootDirectoryWithMeta {
	fn from(dir: RootWithMeta) -> Self {
		RootDirectoryWithMeta::from_meta(dir.uuid, dir.color.into(), dir.meta.into())
	}
}

impl From<RootDirectoryWithMeta> for RootWithMeta {
	fn from(dir: RootDirectoryWithMeta) -> Self {
		RootWithMeta {
			uuid: *dir.uuid(),
			color: dir.color.into(),
			meta: dir.meta.into(),
		}
	}
}

#[derive(Serialize, Deserialize, Tsify)]
#[tsify(into_wasm_abi, from_wasm_abi)]
#[serde(untagged)]
pub enum DirWithMetaEnum {
	Dir(Dir),
	Root(RootWithMeta),
}

impl From<DirWithMetaEnum> for DirectoryMetaType<'static> {
	fn from(value: DirWithMetaEnum) -> Self {
		match value {
			DirWithMetaEnum::Dir(dir) => DirectoryMetaType::Dir(Cow::Owned(dir.into())),
			DirWithMetaEnum::Root(root) => DirectoryMetaType::Root(Cow::Owned(root.into())),
		}
	}
}

impl From<DirectoryMetaType<'_>> for DirWithMetaEnum {
	fn from(value: DirectoryMetaType<'_>) -> Self {
		match value {
			DirectoryMetaType::Dir(dir) => DirWithMetaEnum::Dir(dir.into_owned().into()),
			DirectoryMetaType::Root(dir) => DirWithMetaEnum::Root(dir.into_owned().into()),
		}
	}
}

#[derive(Serialize, Deserialize, Tsify)]
#[tsify(from_wasm_abi, into_wasm_abi)]
#[serde(rename_all = "camelCase")]
pub struct SharedDir {
	pub dir: DirWithMetaEnum,
	pub sharing_role: SharingRole,
	pub write_access: bool,
}

impl From<SharedDir> for crate::connect::fs::SharedDirectory {
	fn from(shared: SharedDir) -> Self {
		Self {
			dir: shared.dir.into(),
			sharing_role: shared.sharing_role,
			write_access: shared.write_access,
		}
	}
}

impl From<crate::connect::fs::SharedDirectory> for SharedDir {
	fn from(shared: crate::connect::fs::SharedDirectory) -> Self {
		Self {
			dir: shared.dir.into(),
			sharing_role: shared.sharing_role,
			write_access: shared.write_access,
		}
	}
}

impl wasm_bindgen::__rt::VectorIntoJsValue for SharedDir {
	fn vector_into_jsvalue(
		vector: wasm_bindgen::__rt::std::boxed::Box<[Self]>,
	) -> wasm_bindgen::JsValue {
		wasm_bindgen::__rt::js_value_vector_into_jsvalue(vector)
	}
}

#[derive(Serialize, Deserialize, Tsify)]
#[tsify(into_wasm_abi, from_wasm_abi)]
#[serde(untagged)]
pub enum AnyDirEnum {
	Dir(Dir),
	RootWithMeta(RootWithMeta),
	Root(Root),
}

impl From<AnyDirEnum> for DirectoryType<'static> {
	fn from(value: AnyDirEnum) -> Self {
		match value {
			AnyDirEnum::Dir(dir) => DirectoryType::Dir(Cow::Owned(dir.into())),
			AnyDirEnum::RootWithMeta(root) => DirectoryType::RootWithMeta(Cow::Owned(root.into())),
			AnyDirEnum::Root(root) => DirectoryType::Root(Cow::Owned(root.into())),
		}
	}
}

#[derive(Deserialize, Tsify)]
#[tsify(from_wasm_abi)]
#[serde(untagged)]
pub enum AnyDirEnumWithShareInfo {
	Dir(Dir),
	SharedDir(SharedDir),
	Root(Root),
}

impl From<AnyDirEnumWithShareInfo> for DirectoryTypeWithShareInfo<'static> {
	fn from(value: AnyDirEnumWithShareInfo) -> Self {
		match value {
			AnyDirEnumWithShareInfo::Dir(dir) => {
				DirectoryTypeWithShareInfo::Dir(Cow::Owned(dir.into()))
			}
			AnyDirEnumWithShareInfo::SharedDir(shared) => {
				DirectoryTypeWithShareInfo::SharedDir(Cow::Owned(shared.into()))
			}
			AnyDirEnumWithShareInfo::Root(root) => {
				DirectoryTypeWithShareInfo::Root(Cow::Owned(root.into()))
			}
		}
	}
}

mod serde_impls {
	use serde::ser::SerializeStruct;

	use crate::js::HIDDEN_META_KEY;

	use super::*;

	impl Serialize for Dir {
		fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
		where
			S: serde::Serializer,
		{
			let num_fields = 5;
			let mut state = serializer.serialize_struct("Dir", num_fields)?;
			state.serialize_field("uuid", &self.uuid)?;
			state.serialize_field("parent", &self.parent)?;
			state.serialize_field("color", &self.color)?;
			state.serialize_field("favorited", &self.favorited)?;
			match self.meta.as_encoded_or_decoded() {
				EncodedOrDecoded::Encoded(encoded) => {
					state.serialize_field(HIDDEN_META_KEY, &encoded)?
				}
				EncodedOrDecoded::Decoded(decoded) => state.serialize_field("meta", decoded)?,
			}
			state.end()
		}
	}

	#[derive(Deserialize)]
	struct DirIntermediate {
		uuid: UuidStr,
		parent: ParentUuid,
		color: DirColor,
		favorited: bool,
		meta: Option<DecryptedDirMeta>,
		// HIDDEN_META_KEY
		#[serde(rename = "__hiddenMeta")]
		hidden_meta: Option<DirMetaEncoded<'static>>,
	}

	impl<'de> Deserialize<'de> for Dir {
		fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
		where
			D: serde::Deserializer<'de>,
		{
			let intermediate = DirIntermediate::deserialize(deserializer)?;

			Ok(Dir {
				uuid: intermediate.uuid,
				parent: intermediate.parent,
				color: intermediate.color,
				favorited: intermediate.favorited,
				meta: DirMeta::from_encoded_or_decoded(intermediate.hidden_meta, intermediate.meta)
					.ok_or_else(|| {
						serde::de::Error::custom(format!(
							"either 'meta' or '{HIDDEN_META_KEY}' field is required"
						))
					})?,
			})
		}
	}

	impl Serialize for RootWithMeta {
		fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
		where
			S: serde::Serializer,
		{
			let num_fields = 3;
			let mut state = serializer.serialize_struct("RootDirWithMeta", num_fields)?;
			state.serialize_field("uuid", &self.uuid)?;
			state.serialize_field("color", &self.color)?;
			match self.meta.as_encoded_or_decoded() {
				EncodedOrDecoded::Encoded(encoded) => {
					state.serialize_field(HIDDEN_META_KEY, &encoded)?
				}
				EncodedOrDecoded::Decoded(decoded) => state.serialize_field("meta", decoded)?,
			}
			state.end()
		}
	}

	#[derive(Deserialize)]
	struct RootWithMetaIntermediate<'a> {
		uuid: UuidStr,
		color: DirColor,
		meta: Option<DecryptedDirMeta>,
		// HIDDEN_META_KEY
		#[serde(rename = "__hiddenMeta")]
		hidden_meta: Option<DirMetaEncoded<'a>>,
	}

	impl<'de> Deserialize<'de> for RootWithMeta {
		fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
		where
			D: serde::Deserializer<'de>,
		{
			let intermediate = RootWithMetaIntermediate::deserialize(deserializer)?;

			Ok(RootWithMeta {
				uuid: intermediate.uuid,
				color: intermediate.color,
				meta: DirMeta::from_encoded_or_decoded(intermediate.hidden_meta, intermediate.meta)
					.ok_or_else(|| {
						serde::de::Error::custom(format!(
							"either 'meta' or '{HIDDEN_META_KEY}' field is required"
						))
					})?,
			})
		}
	}
}
