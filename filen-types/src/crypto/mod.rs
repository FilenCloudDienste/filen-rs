pub mod rsa;
use std::{
	borrow::{Borrow, Cow},
	fmt::Formatter,
};

use base64::prelude::*;
use serde::{Deserialize, Serialize};

use crate::traits::CowHelpers;

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

#[derive(Debug, PartialEq, Eq, Serialize, CowHelpers)]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(tsify::Tsify),
	tsify(into_wasm_abi)
)]
pub enum MaybeEncrypted<'a, T>
where
	T: ToOwned + ?Sized,
	T::Owned: Clone + Borrow<T> + std::fmt::Debug,
	Cow<'static, T>: 'static,
{
	Decrypted(Cow<'a, T>),
	Encrypted(EncryptedString<'a>),
}

impl<T> Clone for MaybeEncrypted<'_, T>
where
	T: ToOwned + ?Sized,
	T::Owned: Clone + Borrow<T> + std::fmt::Debug,
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

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Blake3Hash(#[serde(with = "crate::serde::hex::const_size")] [u8; 32]);

#[cfg(feature = "uniffi")]
uniffi::custom_type!(
	Blake3Hash,
	Vec<u8>, {
	lower: |v: &Blake3Hash| v.0.to_vec(),
	try_lift: |v: Vec<u8>| {
		let slice: [u8; 32] = v.as_slice().try_into().map_err(|_| {
			uniffi::deps::anyhow::anyhow!("expected 32 bytes for Blake3Hash, got {}", v.len())
		})?;
		Ok(Blake3Hash(slice))
	}}
);

impl std::fmt::Debug for Blake3Hash {
	fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
		write!(f, "Blake3Hash({})", faster_hex::hex_string(&self.0))
	}
}

impl From<blake3::Hash> for Blake3Hash {
	fn from(hash: blake3::Hash) -> Self {
		Self(hash.into())
	}
}

impl From<[u8; 32]> for Blake3Hash {
	fn from(hash: [u8; 32]) -> Self {
		Self(hash)
	}
}

impl From<Blake3Hash> for [u8; 32] {
	fn from(hash: Blake3Hash) -> Self {
		hash.0
	}
}

impl AsRef<[u8]> for Blake3Hash {
	fn as_ref(&self) -> &[u8] {
		&self.0
	}
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Sha256Hash(#[serde(with = "crate::serde::hex::const_size")] [u8; 32]);

impl From<digest::Output<sha2::Sha256>> for Sha256Hash {
	fn from(hash: digest::Output<sha2::Sha256>) -> Self {
		Self(hash.into())
	}
}

impl std::fmt::Debug for Sha256Hash {
	fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
		write!(f, "Sha256Hash({})", faster_hex::hex_string(&self.0))
	}
}

impl From<Sha256Hash> for digest::Output<sha2::Sha256> {
	fn from(hash: Sha256Hash) -> Self {
		hash.0.into()
	}
}

impl From<Sha256Hash> for [u8; 32] {
	fn from(hash: Sha256Hash) -> Self {
		hash.0
	}
}

impl AsRef<[u8]> for Sha256Hash {
	fn as_ref(&self) -> &[u8] {
		&self.0
	}
}
