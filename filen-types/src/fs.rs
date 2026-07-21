use std::{borrow::Cow, str::FromStr};

use filen_macros::rkyv_self;
use serde::{Deserialize, Serialize};

use crate::error::ConversionError;

pub use uuid::UuidStr;

pub use ::uuid::Uuid;

#[derive(Debug, Deserialize, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(tsify::Tsify),
	tsify(large_number_types_as_bigints)
)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
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
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[rkyv_self]
#[repr(u8)]
pub enum ParentUuid {
	Uuid(Uuid),
	/// A trashed item's parent. Carries the item's *original* parent so a trashed file/dir
	/// remembers where it came from. By convention this is always a real uuid for an actual
	/// item; the sole exception is the transient list-trash request target, which uses
	/// [`Uuid::nil`] as a placeholder (the payload is dropped by every string/wire encoding —
	/// [`ParentUuid::to_str`] renders `"trash"` regardless).
	Trash(Uuid),
	Recents,
	Favorites,
	Links,
}

/// A stack-allocated string view of a [`ParentUuid`], returned by [`ParentUuid::to_str`].
///
/// `ParentUuid` stores a binary [`Uuid`], so it cannot lend out a `&str` directly; this owns the
/// formatted form (a hyphenated uuid or a `&'static` sentinel word) so callers can `.as_ref()` it.
pub enum ParentUuidStr {
	Uuid(UuidStr),
	Sentinel(&'static str),
}

impl AsRef<str> for ParentUuidStr {
	fn as_ref(&self) -> &str {
		match self {
			ParentUuidStr::Uuid(uuid) => uuid.as_ref(),
			ParentUuidStr::Sentinel(s) => s,
		}
	}
}

impl std::fmt::Display for ParentUuidStr {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.write_str(self.as_ref())
	}
}

impl ParentUuid {
	/// Whether this parent is the trash (ignoring any remembered original-parent payload).
	pub fn is_trash(&self) -> bool {
		matches!(self, ParentUuid::Trash(_))
	}

	/// A trashed item's remembered original parent, if known (`None` for non-trash parents and for
	/// the nil placeholder used by the list-trash request target).
	pub fn original_parent(&self) -> Option<Uuid> {
		match self {
			ParentUuid::Trash(original) if !original.is_nil() => Some(*original),
			_ => None,
		}
	}

	pub fn to_str(&self) -> ParentUuidStr {
		match self {
			ParentUuid::Uuid(uuid) => ParentUuidStr::Uuid(uuid.into()),
			ParentUuid::Trash(_) => ParentUuidStr::Sentinel("trash"),
			ParentUuid::Recents => ParentUuidStr::Sentinel("recents"),
			ParentUuid::Favorites => ParentUuidStr::Sentinel("favorites"),
			ParentUuid::Links => ParentUuidStr::Sentinel("links"),
		}
	}
}
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
#[wasm_bindgen::prelude::wasm_bindgen(typescript_custom_section)]
const TS_PARENT_UUID: &'static str =
	r#"export type ParentUuid = UuidStr | "trash" | "recents" | "favorites" | "links";"#;

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
#[wasm_bindgen::prelude::wasm_bindgen(typescript_custom_section)]
const TS_UUID: &'static str = r#"export type Uuid = UuidStr;"#;

#[cfg(feature = "uniffi")]
uniffi::custom_type!(Uuid, String, {
	remote,
	lower: |uuid: &Uuid| uuid.to_string(),
	try_lift: |s: String| {
		Uuid::from_str(&s).map_err(|_| uniffi::deps::anyhow::anyhow!("invalid UUID string: {}", s))
	},
});

impl Default for ParentUuid {
	fn default() -> Self {
		ParentUuid::Uuid(Uuid::nil())
	}
}

impl From<Uuid> for ParentUuid {
	fn from(uuid: Uuid) -> Self {
		ParentUuid::Uuid(uuid)
	}
}

impl PartialEq<Uuid> for ParentUuid {
	fn eq(&self, other: &Uuid) -> bool {
		match self {
			ParentUuid::Uuid(uuid) => uuid == other,
			_ => false,
		}
	}
}

impl TryFrom<ParentUuid> for Uuid {
	type Error = ConversionError;

	fn try_from(value: ParentUuid) -> Result<Self, Self::Error> {
		match value {
			ParentUuid::Uuid(uuid) => Ok(uuid),
			other => Err(ConversionError::ParentUuidError(format!("{other:?}"))),
		}
	}
}

impl std::fmt::Display for ParentUuid {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.write_str(self.to_str().as_ref())
	}
}

impl FromStr for ParentUuid {
	type Err = ConversionError;

	fn from_str(s: &str) -> Result<Self, Self::Err> {
		match s {
			"trash" => Ok(ParentUuid::Trash(Uuid::nil())),
			"recents" => Ok(ParentUuid::Recents),
			"favorites" => Ok(ParentUuid::Favorites),
			"links" => Ok(ParentUuid::Links),
			_ => {
				Ok(ParentUuid::Uuid(Uuid::from_str(s).map_err(|_| {
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
		serializer.serialize_str(self.to_str().as_ref())
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

	use filen_macros::rkyv_self;
	use serde::{Deserialize, Serialize};
	use uuid::{Uuid, fmt::Hyphenated};
	#[cfg(all(target_family = "wasm", target_os = "unknown"))]
	use wasm_bindgen::{
		convert::{FromWasmAbi, IntoWasmAbi, RefFromWasmAbi},
		describe::WasmDescribe,
		throw_str,
	};

	#[derive(Clone, Copy, PartialEq, Eq)]
	#[rkyv_self]
	pub struct UuidStr([u8; Hyphenated::LENGTH]);

	#[cfg(feature = "uniffi")]
	uniffi::custom_type!(UuidStr, String, {
		remote,
		lower: |uuid: &UuidStr| uuid.as_ref().to_string(),
		try_lift: |s: String| {
			UuidStr::from_str(&s).map_err(|_| uniffi::deps::anyhow::anyhow!("invalid UUID string: {}", s))
		},
	});

	#[cfg(all(target_family = "wasm", target_os = "unknown"))]
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen::prelude::wasm_bindgen(typescript_custom_section)
	)]
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
			// throw_str instead of panicking: a panic traps and poisons the
			// whole wasm instance, so every subsequent call would fail.
			//
			// This does NOT make the error catchable by the JS caller: every
			// exported fn taking UuidStr by value in this workspace is async,
			// and wasm-bindgen-futures runs argument conversion inside
			// future_to_promise on the first poll, so the throw surfaces as an
			// uncaught microtask error and the returned Promise never settles
			// (caller try/catch does not fire). That is parity with the old
			// trap's error surface — the gain is that the instance stays
			// usable.
			//
			// Deliberate trade-off: wasm-bindgen documents that destructors
			// are skipped at the throw, so each hostile call leaks the
			// receiver RcRef anchor (leaving the exported class
			// un-free()-able), the Box<str> anchor `s`, and the message
			// String — bounded per call, identical to the old trap path.
			//
			// The complete fix is a fallible boundary (accept String and
			// parse inside the fn, returning Result); that requires exported
			// signature changes and is deferred as a follow-up.
			UuidStr::from_str(&s)
				.unwrap_or_else(|_| throw_str(&format!("invalid UUID string passed from JS: {s}")))
		}
	}

	#[cfg(all(target_family = "wasm", target_os = "unknown"))]
	impl IntoWasmAbi for UuidStr {
		type Abi = <String as IntoWasmAbi>::Abi;

		fn into_abi(self) -> Self::Abi {
			self.as_ref().to_string().into_abi()
		}
	}

	impl From<Uuid> for UuidStr {
		fn from(uuid: Uuid) -> Self {
			(&uuid).into()
		}
	}

	impl From<UuidStr> for Uuid {
		fn from(uuid: UuidStr) -> Self {
			(&uuid).into()
		}
	}

	impl UuidStr {
		pub const LENGTH: usize = Hyphenated::LENGTH;
	}

	impl FromStr for UuidStr {
		type Err = <Uuid as FromStr>::Err;

		fn from_str(s: &str) -> Result<Self, Self::Err> {
			Ok((&Uuid::from_str(s)?).into())
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

	impl From<&UuidStr> for Uuid {
		fn from(uuid_string: &UuidStr) -> Self {
			// SAFETY: The string is guaranteed to be a valid Hyphenated UUID string
			unsafe {
				Hyphenated::from_str(uuid_string.as_ref())
					.unwrap_unchecked()
					.into_uuid()
			}
		}
	}

	impl From<&Uuid> for UuidStr {
		fn from(uuid: &Uuid) -> Self {
			let hyphenated = uuid.as_hyphenated();
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

	impl std::fmt::Debug for UuidStr {
		fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
			f.write_str(self.as_ref())
		}
	}
}

#[cfg(target_family = "wasm")]
mod wasm {
	use wasm_bindgen::JsValue;

	use super::*;

	impl From<ParentUuid> for JsValue {
		fn from(parent: ParentUuid) -> Self {
			JsValue::from(parent.to_str().as_ref())
		}
	}

	impl From<UuidStr> for JsValue {
		fn from(uuid: UuidStr) -> Self {
			JsValue::from(uuid.as_ref())
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn test_parent_uuid_stringification() {
		let uuid = Uuid::new_v4();
		let parent_uuid = ParentUuid::Uuid(uuid);
		assert_eq!(parent_uuid.to_string(), uuid.to_string());
		assert_eq!(
			ParentUuid::from_str(parent_uuid.to_str().as_ref()).unwrap(),
			parent_uuid
		);
	}
}
