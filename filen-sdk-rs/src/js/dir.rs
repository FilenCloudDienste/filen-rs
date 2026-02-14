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
};

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
use tsify::Tsify;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(Tsify),
	tsify(large_number_types_as_bigints)
)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct DecryptedDirMeta {
	pub name: String,
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(Tsify),
	tsify(from_wasm_abi, into_wasm_abi, large_number_types_as_bigints)
)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(tag = "type")]
pub enum DirMeta {
	Decoded(DecryptedDirMeta),
	DecryptedRaw(Vec<u8>),
	DecryptedUTF8(String),
	Encrypted(String),
	RSAEncrypted(String),
}

#[derive(Serialize, Deserialize, Clone)]
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
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(Tsify),
	tsify(into_wasm_abi, from_wasm_abi)
)]
#[cfg_attr(test, derive(Debug, PartialEq, Eq, Clone))]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
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
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
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

impl From<String> for DirColor {
	fn from(s: String) -> Self {
		match s.as_str() {
			"default" => DirColor::Default,
			"blue" => DirColor::Blue,
			"green" => DirColor::Green,
			"purple" => DirColor::Purple,
			"red" => DirColor::Red,
			"gray" => DirColor::Gray,
			_ => DirColor::Custom(s),
		}
	}
}

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
impl wasm_bindgen::describe::WasmDescribe for DirColor {
	fn describe() {
		<String as wasm_bindgen::describe::WasmDescribe>::describe();
	}
}

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
impl wasm_bindgen::convert::FromWasmAbi for DirColor {
	type Abi = <String as wasm_bindgen::convert::FromWasmAbi>::Abi;

	unsafe fn from_abi(abi: Self::Abi) -> Self {
		let s = unsafe { <String as wasm_bindgen::convert::FromWasmAbi>::from_abi(abi) };
		DirColor::from(s)
	}
}

// tsify does not support untagged variants yet: https://github.com/madonoharu/tsify/issues/52
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
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

#[derive(Debug, PartialEq, Eq, Clone, Serialize, Deserialize)]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(Tsify),
	tsify(from_wasm_abi, into_wasm_abi, large_number_types_as_bigints),
	serde(rename_all = "camelCase")
)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct Dir {
	pub uuid: UuidStr,
	pub parent: ParentUuid,
	pub color: DirColor,
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		tsify(type = "bigint"),
		serde(with = "chrono::serde::ts_milliseconds")
	)]
	pub timestamp: DateTime<Utc>,
	pub favorited: bool,
	pub meta: DirMeta,
}

impl From<RemoteDirectory> for Dir {
	fn from(dir: RemoteDirectory) -> Self {
		Dir {
			uuid: dir.uuid,
			parent: dir.parent,
			color: dir.color.into(),
			favorited: dir.favorited,
			timestamp: dir.timestamp,
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
			dir.timestamp,
			dir.meta.into(),
		)
	}
}

#[derive(Deserialize)]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(Tsify),
	tsify(from_wasm_abi)
)]
#[serde(untagged)]
#[cfg_attr(test, derive(Debug, PartialEq, Eq))]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
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

#[derive(Debug, PartialEq, Eq, Clone, Serialize, Deserialize)]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(Tsify),
	tsify(from_wasm_abi, into_wasm_abi, large_number_types_as_bigints),
	serde(rename_all = "camelCase")
)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct RootWithMeta {
	pub uuid: UuidStr,
	pub color: DirColor,
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		tsify(type = "bigint"),
		serde(with = "chrono::serde::ts_milliseconds")
	)]
	pub timestamp: DateTime<Utc>,
	pub meta: DirMeta,
}

impl From<RootWithMeta> for RootDirectoryWithMeta {
	fn from(dir: RootWithMeta) -> Self {
		RootDirectoryWithMeta::from_meta(dir.uuid, dir.color.into(), dir.timestamp, dir.meta.into())
	}
}

impl From<RootDirectoryWithMeta> for RootWithMeta {
	fn from(dir: RootDirectoryWithMeta) -> Self {
		RootWithMeta {
			uuid: *dir.uuid(),
			color: dir.color.into(),
			timestamp: dir.timestamp,
			meta: dir.meta.into(),
		}
	}
}

#[derive(Serialize, Deserialize)]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(Tsify),
	tsify(from_wasm_abi, into_wasm_abi, large_number_types_as_bigints)
)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
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

#[derive(Serialize, Deserialize)]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(Tsify),
	tsify(from_wasm_abi, into_wasm_abi, large_number_types_as_bigints)
)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
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

#[derive(Serialize, Deserialize)]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(Tsify),
	tsify(from_wasm_abi, into_wasm_abi, large_number_types_as_bigints)
)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
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

#[derive(Deserialize)]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(Tsify),
	tsify(from_wasm_abi)
)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
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
