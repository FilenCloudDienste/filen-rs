pub mod rsa;
use std::borrow::{Borrow, Cow};

use base64::prelude::*;
use filen_macros::rkyv_self;
use serde::{Deserialize, Serialize};
use sha2::Digest;
use typenum::{U32, U64};

use crate::{serde::str::SizedHexString, traits::CowHelpers};

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq, CowHelpers)]
#[serde(transparent)]
pub struct DerivedPassword<'a>(pub Cow<'a, str>);

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq, CowHelpers)]
#[serde(transparent)]
pub struct EncodedString<'a>(pub Cow<'a, str>);

impl TryFrom<&EncodedString<'_>> for Vec<u8> {
	type Error = base64::DecodeError;
	fn try_from(value: &EncodedString) -> Result<Self, Self::Error> {
		BASE64_STANDARD.decode(value.0.as_ref())
	}
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq, CowHelpers)]
#[serde(transparent)]
pub struct LinkHashedPassword<'a>(pub Cow<'a, str>);

pub type LinkHashedPasswordStatic = LinkHashedPassword<'static>;

#[cfg(feature = "uniffi")]
uniffi::custom_type!(
	LinkHashedPasswordStatic,
	String, {
		lower: |v: &LinkHashedPasswordStatic| v.0.to_string(),
		try_lift: |v: String| Ok(LinkHashedPassword(Cow::Owned(v)))
	}
);

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq, CowHelpers)]
#[serde(transparent)]
pub struct EncryptedString<'a>(pub Cow<'a, str>);

pub type EncryptedStringStatic = EncryptedString<'static>;
#[cfg(feature = "uniffi")]
uniffi::custom_type!(
	EncryptedStringStatic,
	String, {
		lower: |v: &EncryptedStringStatic| v.0.to_string(),
		try_lift: |v: String| Ok(EncryptedString(Cow::Owned(v)))
	}
);

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
impl<'a> wasm_bindgen::convert::IntoWasmAbi for EncryptedString<'a> {
	type Abi = <&'a str as wasm_bindgen::convert::IntoWasmAbi>::Abi;
	#[inline]
	fn into_abi(self) -> Self::Abi {
		(&self.0).into_abi()
	}
}

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
impl wasm_bindgen::describe::WasmDescribe for EncryptedString<'_> {
	fn describe() {
		<&str as wasm_bindgen::describe::WasmDescribe>::describe()
	}
}

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
impl<'a> wasm_bindgen::convert::FromWasmAbi for EncryptedString<'a> {
	type Abi = <String as wasm_bindgen::convert::FromWasmAbi>::Abi;
	#[inline]
	unsafe fn from_abi(abi: Self::Abi) -> Self {
		Self(Cow::Owned(unsafe {
			<String as wasm_bindgen::convert::FromWasmAbi>::from_abi(abi)
		}))
	}
}

#[derive(Debug, PartialEq, Eq, CowHelpers, serde::Serialize, serde::Deserialize)]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(tsify::Tsify),
	tsify(into_wasm_abi, from_wasm_abi)
)]
#[serde(bound(
	serialize = "T: serde::Serialize, T::Owned: serde::Serialize",
	deserialize = "T::Owned: serde::de::DeserializeOwned"
))]
pub enum MaybeEncrypted<'a, T>
where
	T: ToOwned + ?Sized,
	T::Owned: Clone + Borrow<T> + std::fmt::Debug + serde::de::DeserializeOwned,
	Cow<'static, T>: 'static,
{
	Decrypted(Cow<'a, T>),
	Encrypted(EncryptedString<'a>),
}

impl<T> Clone for MaybeEncrypted<'_, T>
where
	T: ToOwned + ?Sized,
	T::Owned: Clone + Borrow<T> + std::fmt::Debug + serde::de::DeserializeOwned,
{
	fn clone(&self) -> Self {
		match self {
			MaybeEncrypted::Decrypted(s) => MaybeEncrypted::Decrypted(s.clone()),
			MaybeEncrypted::Encrypted(e) => MaybeEncrypted::Encrypted(e.clone()),
		}
	}
}

pub type MaybeEncryptedStatic = MaybeEncrypted<'static, str>;

#[cfg(feature = "uniffi")]
#[derive(uniffi::Enum)]
pub enum MaybeEncryptedUniffi {
	Decrypted(String),
	Encrypted(EncryptedString<'static>),
}

#[cfg(feature = "uniffi")]
uniffi::custom_type!(
	MaybeEncryptedStatic,
	MaybeEncryptedUniffi, {
		lower: |v: EncryptedStringStatic| match v {
			MaybeEncrypted::Decrypted(s) => MaybeEncryptedUniffi::Decrypted(s.into_owned()),
			MaybeEncrypted::Encrypted(e) => MaybeEncryptedUniffi::Encrypted(e.into_owned_cow()),
		},
		try_lift: |v: MaybeDecryptedUniffi| match v {
			MaybeEncryptedUniffi::Decrypted(s) => Ok(MaybeEncrypted::Decrypted(Cow::Owned(s))),
			MaybeEncryptedUniffi::Encrypted(e) => Ok(MaybeEncrypted::Encrypted(e)),
		}
	}
);

// claude said to do this to define the type in TS
// without allowing it to be constructed in TS
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
#[wasm_bindgen::prelude::wasm_bindgen(typescript_custom_section)]
const TS_ENCRYPTED_STRING: &'static str = r#"export type EncryptedString = unknown"#;

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq, CowHelpers)]
#[serde(transparent)]
pub struct EncryptedMasterKeys<'a>(pub EncryptedString<'a>);

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq, CowHelpers)]
#[serde(transparent)]
pub struct EncryptedDEK<'a>(pub EncryptedString<'a>);

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq, CowHelpers)]
#[serde(transparent)]
pub struct EncryptedMetaKey<'a>(pub EncryptedString<'a>);

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Debug)]
#[serde(transparent)]
#[rkyv_self]
pub struct Blake3Hash(SizedHexString<U32>);

#[cfg(feature = "uniffi")]
uniffi::custom_type!(
	Blake3Hash,
	Vec<u8>, {
	lower: |v: &Blake3Hash| v.0.as_slice().to_vec(),
	try_lift: |v: Vec<u8>| {
		let slice: [u8; 32] = v.as_slice().try_into().map_err(|_| {
			uniffi::deps::anyhow::anyhow!("expected 32 bytes for Blake3Hash, got {}", v.len())
		})?;
		Ok(Blake3Hash(SizedHexString::from(slice)))
	}}
);

impl Blake3Hash {
	pub fn as_sized_str(&self) -> &SizedHexString<U32> {
		&self.0
	}
}

impl From<blake3::Hash> for Blake3Hash {
	fn from(hash: blake3::Hash) -> Self {
		let hash = <[u8; 32]>::from(hash);

		Self(SizedHexString::<U32>::from(hash))
	}
}

impl From<[u8; 32]> for Blake3Hash {
	fn from(hash: [u8; 32]) -> Self {
		Self(SizedHexString::from(hash))
	}
}

impl From<Blake3Hash> for [u8; 32] {
	fn from(hash: Blake3Hash) -> Self {
		*hash.0.as_ref()
	}
}

impl AsRef<[u8; 32]> for Blake3Hash {
	fn as_ref(&self) -> &[u8; 32] {
		self.0.as_ref()
	}
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Debug)]
#[serde(transparent)]
pub struct Sha256Hash(SizedHexString<U32>);

impl From<digest::Output<sha2::Sha256>> for Sha256Hash {
	fn from(hash: digest::Output<sha2::Sha256>) -> Self {
		Self(SizedHexString::from(<[u8; _]>::from(hash)))
	}
}

impl From<Sha256Hash> for digest::Output<sha2::Sha256> {
	fn from(hash: Sha256Hash) -> Self {
		(*hash.0.as_ref()).into()
	}
}

impl From<Sha256Hash> for [u8; 32] {
	fn from(hash: Sha256Hash) -> Self {
		*hash.0.as_ref()
	}
}

impl AsRef<[u8; 32]> for Sha256Hash {
	fn as_ref(&self) -> &[u8; 32] {
		self.0.as_ref()
	}
}

impl From<Sha256Hash> for SizedHexString<U32> {
	fn from(hash: Sha256Hash) -> Self {
		hash.0
	}
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Debug)]
#[serde(transparent)]
pub struct Sha512Hash(SizedHexString<U64>);

impl Sha512Hash {
	pub fn digest(data: &[u8]) -> Self {
		Self::from(sha2::Sha512::digest(data))
	}
}

impl From<digest::Output<sha2::Sha512>> for Sha512Hash {
	fn from(hash: digest::Output<sha2::Sha512>) -> Self {
		Self(SizedHexString::from(<[u8; _]>::from(hash)))
	}
}

impl From<Sha512Hash> for digest::Output<sha2::Sha512> {
	fn from(hash: Sha512Hash) -> Self {
		(*hash.0.as_ref()).into()
	}
}

impl From<Sha512Hash> for [u8; 64] {
	fn from(hash: Sha512Hash) -> Self {
		*hash.0.as_ref()
	}
}

impl AsRef<[u8; 64]> for Sha512Hash {
	fn as_ref(&self) -> &[u8; 64] {
		self.0.as_ref()
	}
}

impl From<Sha512Hash> for SizedHexString<U64> {
	fn from(hash: Sha512Hash) -> Self {
		hash.0
	}
}

#[cfg(test)]
mod tests {
	use super::Blake3Hash;
	use crate::serde::str::SizedHexString;
	use filen_macros::rkyv_self;
	use generic_array::ArrayLength;
	use rkyv::rancor::Error;
	use typenum::U32;

	#[test]
	fn blake3_hash_rkyv_round_trip() {
		let original = Blake3Hash::from([0xABu8; 32]);
		let bytes = rkyv::to_bytes::<Error>(&original).unwrap();
		let decoded = rkyv::from_bytes::<Blake3Hash, Error>(&bytes).unwrap();
		assert_eq!(decoded, original);
	}

	#[test]
	fn blake3_hash_archived_form_is_identical_bytes() {
		// `#[rkyv_self]` pins the archived form to the type itself (`as = Self`) over a
		// `#[repr(transparent)]` wrapper, so the buffer must be exactly the 32 raw bytes
		// with no rkyv header or padding.
		let raw = [0x12u8; 32];
		let hash = Blake3Hash::from(raw);
		let bytes = rkyv::to_bytes::<Error>(&hash).unwrap();
		assert_eq!(bytes.len(), 32);
		assert_eq!(bytes.as_slice(), &raw);
	}

	#[test]
	fn blake3_hash_validated_access_round_trips() {
		// `rkyv::access` runs the generated `CheckBytes` impl (which delegates to the
		// inner `SizedHexString`), exercising validation rather than unchecked access.
		let original = Blake3Hash::from([0x7Fu8; 32]);
		let bytes = rkyv::to_bytes::<Error>(&original).unwrap();
		let archived = rkyv::access::<Blake3Hash, Error>(&bytes).unwrap();
		assert_eq!(*archived, original);
	}

	// Exercises the generic codegen path of `#[rkyv_self]`, which `Blake3Hash` (being
	// non-generic) does not cover. `SizedHexString<N>` is `Portable`, archives `as =
	// Self`, and implements `CheckBytes`, satisfying the bounds the macro generates.
	#[rkyv_self]
	struct GenericWrapper<N: ArrayLength>(SizedHexString<N>);

	#[test]
	fn rkyv_self_generic_wrapper_round_trips() {
		let original = GenericWrapper::<U32>(SizedHexString::<U32>::from([0x5Au8; 32]));
		let bytes = rkyv::to_bytes::<Error>(&original).unwrap();
		let decoded = rkyv::from_bytes::<GenericWrapper<U32>, Error>(&bytes).unwrap();
		assert_eq!(decoded.0, original.0);
	}

	// Exercises the path where `#[repr(transparent)]` is written explicitly: the
	// macro must accept it as-is rather than adding a duplicate or erroring.
	#[rkyv_self]
	#[repr(transparent)]
	struct ExplicitReprWrapper(SizedHexString<U32>);

	#[test]
	fn rkyv_self_accepts_explicit_repr_transparent() {
		let original = ExplicitReprWrapper(SizedHexString::<U32>::from([0x01u8; 32]));
		let bytes = rkyv::to_bytes::<Error>(&original).unwrap();
		let decoded = rkyv::from_bytes::<ExplicitReprWrapper, Error>(&bytes).unwrap();
		assert_eq!(decoded.0, original.0);
	}

	// Multi-field struct: the macro adds `#[repr(C)]` and the derived `CheckBytes`
	// validates each field. Both field types archive as themselves (`SizedHexString`
	// via `#[rkyv_self]`, `u8` natively) and are `Portable`.
	#[rkyv_self]
	#[derive(PartialEq, Eq, Debug)]
	struct MultiField {
		hash: SizedHexString<U32>,
		tag: u8,
	}

	#[test]
	fn rkyv_self_multi_field_struct_round_trips() {
		let original = MultiField {
			hash: SizedHexString::<U32>::from([0x33u8; 32]),
			tag: 7,
		};
		let bytes = rkyv::to_bytes::<Error>(&original).unwrap();
		// `rkyv::access` runs the derived per-field `CheckBytes`.
		let archived = rkyv::access::<MultiField, Error>(&bytes).unwrap();
		assert_eq!(archived, &original);
		let decoded = rkyv::from_bytes::<MultiField, Error>(&bytes).unwrap();
		assert_eq!(decoded, original);
	}

	// Enum: requires an explicit primitive `repr`; the derived `CheckBytes`
	// validates the discriminant and the active variant's fields.
	#[rkyv_self]
	#[derive(PartialEq, Eq, Debug)]
	#[repr(u8)]
	enum Tagged {
		Empty,
		Hash(SizedHexString<U32>),
		Byte(u8),
	}

	#[test]
	fn rkyv_self_enum_round_trips() {
		for original in [
			Tagged::Empty,
			Tagged::Hash(SizedHexString::<U32>::from([0x44u8; 32])),
			Tagged::Byte(9),
		] {
			let bytes = rkyv::to_bytes::<Error>(&original).unwrap();
			let archived = rkyv::access::<Tagged, Error>(&bytes).unwrap();
			assert_eq!(archived, &original);
			let decoded = rkyv::from_bytes::<Tagged, Error>(&bytes).unwrap();
			assert_eq!(decoded, original);
		}
	}

	#[test]
	fn rkyv_self_enum_rejects_invalid_discriminant() {
		// `#[repr(u8)]` puts the discriminant in the first byte; this enum has three
		// variants (0..=2), so 99 matches none and the derived `CheckBytes` must reject it.
		let mut bytes = rkyv::to_bytes::<Error>(&Tagged::Byte(5)).unwrap().to_vec();
		bytes[0] = 99;
		assert!(rkyv::access::<Tagged, Error>(&bytes).is_err());
	}

	// Named-field enum variants (`Variant { field: T }`) go through a distinct code
	// path in rkyv's/bytecheck's derives than the tuple/unit variants above, so they
	// get their own round-trip (which also exercises the derived `CheckBytes`).
	#[rkyv_self]
	#[derive(PartialEq, Eq, Debug)]
	#[repr(u8)]
	enum NamedVariant {
		Empty,
		WithHash { hash: SizedHexString<U32> },
		WithByte { tag: u8 },
	}

	#[test]
	fn rkyv_self_enum_named_field_variant_round_trips() {
		for original in [
			NamedVariant::Empty,
			NamedVariant::WithHash {
				hash: SizedHexString::<U32>::from([0x55u8; 32]),
			},
			NamedVariant::WithByte { tag: 42 },
		] {
			let bytes = rkyv::to_bytes::<Error>(&original).unwrap();
			let archived = rkyv::access::<NamedVariant, Error>(&bytes).unwrap();
			assert_eq!(archived, &original);
			let decoded = rkyv::from_bytes::<NamedVariant, Error>(&bytes).unwrap();
			assert_eq!(decoded, original);
		}
	}
}
