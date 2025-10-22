use std::{borrow::Cow, str::FromStr};

use serde::{Deserialize, Serialize};

use crate::error::ConversionError;

pub use uuid::UuidStr;

#[derive(Debug, Deserialize, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(tsify::Tsify),
	tsify(large_number_types_as_bigints)
)]
pub enum ObjectType {
	#[serde(rename = "file")]
	File,
	#[serde(rename = "folder")]
	Dir,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectType2 {
	#[serde(rename = "file")]
	File,
	#[serde(rename = "directory")]
	Dir,
}

impl From<ObjectType> for ObjectType2 {
	fn from(object_type: ObjectType) -> Self {
		match object_type {
			ObjectType::File => ObjectType2::File,
			ObjectType::Dir => ObjectType2::Dir,
		}
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParentUuid {
	Uuid(UuidStr),
	Trash,
	Recents,
	Favorites,
	Links,
}
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
#[wasm_bindgen::prelude::wasm_bindgen(typescript_custom_section)]
const TS_PARENT_UUID: &'static str =
	r#"export type ParentUuid = UuidStr | "trash" | "recents" | "favorites" | "links";"#;

impl Default for ParentUuid {
	fn default() -> Self {
		ParentUuid::Uuid(UuidStr::nil())
	}
}

impl From<UuidStr> for ParentUuid {
	fn from(uuid: UuidStr) -> Self {
		ParentUuid::Uuid(uuid)
	}
}

impl PartialEq<UuidStr> for ParentUuid {
	fn eq(&self, other: &UuidStr) -> bool {
		match self {
			ParentUuid::Uuid(uuid) => uuid == other,
			_ => false,
		}
	}
}

impl TryFrom<ParentUuid> for UuidStr {
	type Error = ConversionError;

	fn try_from(value: ParentUuid) -> Result<Self, Self::Error> {
		match value {
			ParentUuid::Uuid(uuid) => Ok(uuid),
			other => Err(ConversionError::ParentUuidError(format!("{other:?}"))),
		}
	}
}

impl AsRef<str> for ParentUuid {
	fn as_ref(&self) -> &str {
		match self {
			ParentUuid::Uuid(uuid) => uuid.as_ref(),
			ParentUuid::Trash => "trash",
			ParentUuid::Recents => "recents",
			ParentUuid::Favorites => "favorites",
			ParentUuid::Links => "links",
		}
	}
}

impl std::fmt::Display for ParentUuid {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.write_str(self.as_ref())
	}
}

impl FromStr for ParentUuid {
	type Err = ConversionError;

	fn from_str(s: &str) -> Result<Self, Self::Err> {
		match s {
			"trash" => Ok(ParentUuid::Trash),
			"recents" => Ok(ParentUuid::Recents),
			"favorites" => Ok(ParentUuid::Favorites),
			"links" => Ok(ParentUuid::Links),
			_ => {
				Ok(ParentUuid::Uuid(UuidStr::from_str(s).map_err(|_| {
					ConversionError::ParentUuidError(s.to_string())
				})?))
			}
		}
	}
}

impl Serialize for ParentUuid {
	fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: serde::Serializer,
	{
		serializer.serialize_str(self.as_ref())
	}
}

impl<'de> Deserialize<'de> for ParentUuid {
	fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
	where
		D: serde::Deserializer<'de>,
	{
		let s = Cow::<'de, str>::deserialize(deserializer)?;
		ParentUuid::from_str(&s).map_err(serde::de::Error::custom)
	}
}

mod uuid {
	use std::{borrow::Cow, str::FromStr};

	use serde::{Deserialize, Serialize};
	use uuid::{Uuid, fmt::Hyphenated};
	#[cfg(all(target_family = "wasm", target_os = "unknown"))]
	use wasm_bindgen::{
		convert::{FromWasmAbi, RefFromWasmAbi},
		describe::WasmDescribe,
		prelude::wasm_bindgen,
	};

	#[derive(Clone, Copy, PartialEq, Eq)]
	pub struct UuidStr([u8; Hyphenated::LENGTH]);

	#[cfg(all(target_family = "wasm", target_os = "unknown"))]
	#[wasm_bindgen(typescript_custom_section)]
	const TS_PARENT_UUID: &'static str =
		r#"export type UuidStr = `${string}-${string}-${string}-${string}`;"#;

	#[cfg(all(target_family = "wasm", target_os = "unknown"))]
	impl WasmDescribe for UuidStr {
		fn describe() {
			<str as WasmDescribe>::describe();
		}
	}

	#[cfg(all(target_family = "wasm", target_os = "unknown"))]
	impl FromWasmAbi for UuidStr {
		type Abi = <str as RefFromWasmAbi>::Abi;

		unsafe fn from_abi(abi: Self::Abi) -> Self {
			let s = unsafe { <str>::ref_from_abi(abi) };
			UuidStr::from_str(&s).expect("Invalid UUID string passed from JS")
		}
	}

	impl UuidStr {
		pub const LENGTH: usize = Hyphenated::LENGTH;

		pub fn new_v4() -> Self {
			Uuid::new_v4().into()
		}

		pub fn nil() -> Self {
			Uuid::nil().into()
		}
	}

	impl FromStr for UuidStr {
		type Err = <Uuid as FromStr>::Err;

		fn from_str(s: &str) -> Result<Self, Self::Err> {
			Ok(Uuid::from_str(s)?.into())
		}
	}

	impl AsRef<str> for UuidStr {
		fn as_ref(&self) -> &str {
			// SAFETY: The string is guaranteed to be valid UTF-8 because it is a UUID string
			unsafe { std::str::from_utf8_unchecked(&self.0) }
		}
	}

	impl std::fmt::Display for UuidStr {
		fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
			f.write_str(self.as_ref())
		}
	}

	impl From<UuidStr> for Uuid {
		fn from(uuid_string: UuidStr) -> Self {
			// SAFETY: The string is guaranteed to be a valid Hyphenated UUID string
			unsafe {
				Hyphenated::from_str(uuid_string.as_ref())
					.unwrap_unchecked()
					.into_uuid()
			}
		}
	}

	impl From<Uuid> for UuidStr {
		fn from(uuid: Uuid) -> Self {
			let hyphenated = uuid.hyphenated();
			let mut bytes = [0u8; Hyphenated::LENGTH];
			hyphenated.encode_lower(&mut bytes);
			UuidStr(bytes)
		}
	}

	impl Serialize for UuidStr {
		fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
		where
			S: serde::Serializer,
		{
			serializer.serialize_str(self.as_ref())
		}
	}

	impl<'de> Deserialize<'de> for UuidStr {
		fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
		where
			D: serde::Deserializer<'de>,
		{
			let s = Cow::<'de, str>::deserialize(deserializer)?;
			UuidStr::from_str(&s).map_err(serde::de::Error::custom)
		}
	}

	impl Default for UuidStr {
		fn default() -> Self {
			UuidStr::nil()
		}
	}

	impl std::fmt::Debug for UuidStr {
		fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
			f.write_str(self.as_ref())
		}
	}

	#[cfg(feature = "rusqlite")]
	mod sqlite {
		use rusqlite::{
			Error, ToSql,
			types::{FromSql, FromSqlError, FromSqlResult, ToSqlOutput, ValueRef},
		};

		use super::*;

		impl ToSql for UuidStr {
			fn to_sql(&self) -> Result<ToSqlOutput<'_>, Error> {
				Ok(ToSqlOutput::Borrowed(ValueRef::Text(
					self.as_ref().as_bytes(),
				)))
			}
		}

		impl FromSql for UuidStr {
			fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
				match value {
					ValueRef::Text(s) => UuidStr::from_str(
						std::str::from_utf8(s).map_err(|_| FromSqlError::InvalidType)?,
					)
					.map_err(|e| FromSqlError::Other(Box::new(e))),
					_ => Err(FromSqlError::InvalidType),
				}
			}
		}
	}

	#[cfg(target_family = "wasm")]
	mod wasm {
		use wasm_bindgen::JsValue;

		use super::*;

		impl From<UuidStr> for JsValue {
			fn from(uuid: UuidStr) -> Self {
				JsValue::from(uuid.as_ref())
			}
		}
	}
}

#[cfg(feature = "rusqlite")]
mod sqlite {
	use std::str::FromStr;

	use crate::fs::UuidStr;

	use super::ParentUuid;
	use rusqlite::{
		Error, ToSql,
		types::{FromSql, FromSqlError, FromSqlResult, ToSqlOutput, ValueRef},
	};
	use uuid::fmt::Hyphenated;

	impl ToSql for ParentUuid {
		fn to_sql(&self) -> Result<ToSqlOutput<'_>, Error> {
			Ok(ToSqlOutput::Borrowed(self.as_ref().into()))
		}
	}

	impl FromSql for ParentUuid {
		fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
			match value {
				ValueRef::Text(s) => {
					if s.len() == Hyphenated::LENGTH {
						// If the string is exactly 36 characters, it is likely a UUID
						UuidStr::column_result(ValueRef::Text(s)).map(ParentUuid::Uuid)
					} else {
						// Otherwise, treat it as a special parent type
						match std::str::from_utf8(s) {
							Ok(s) => ParentUuid::from_str(s)
								.map_err(|e| FromSqlError::Other(Box::new(e))),
							Err(e) => Err(FromSqlError::Other(Box::new(e))),
						}
					}
				}
				_ => Err(FromSqlError::InvalidType),
			}
		}
	}
}

#[cfg(target_family = "wasm")]
mod wasm {
	use wasm_bindgen::JsValue;

	use super::*;

	impl From<ParentUuid> for JsValue {
		fn from(parent: ParentUuid) -> Self {
			JsValue::from(parent.as_ref())
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn test_parent_uuid_stringification() {
		let uuid = UuidStr::new_v4();
		let parent_uuid = ParentUuid::Uuid(uuid);
		assert_eq!(parent_uuid.to_string(), uuid.to_string());
		assert_eq!(
			ParentUuid::from_str(parent_uuid.as_ref()).unwrap(),
			parent_uuid
		);
	}
}
