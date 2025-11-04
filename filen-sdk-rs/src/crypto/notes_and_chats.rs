use std::{borrow::Cow, str::FromStr};

use filen_types::crypto::{EncryptedString, rsa::RSAEncryptedString};
use rsa::{RsaPrivateKey, RsaPublicKey};
use serde::{Deserialize, Serialize};

use crate::{
	Error, ErrorKind,
	crypto::{
		shared::{CreateRandom, DataCrypter, MetaCrypter},
		v2::{MasterKey, V2Key},
		v3::EncryptionKey,
	},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NoteOrChatKey {
	V2(V2Key),
	V3(EncryptionKey),
}

#[cfg(feature = "uniffi")]
uniffi::custom_type!(
	NoteOrChatKey,
	String,
	{
		lower : |key: &NoteOrChatKey| key.to_string(),
		try_lift : |s: String| NoteOrChatKey::from_cow_str(Cow::Owned(s)).map_err(|e| uniffi::deps::anyhow::anyhow!(e))
	}
);

impl NoteOrChatKey {
	pub(crate) fn from_cow_str(s: Cow<str>) -> Result<Self, crate::crypto::error::ConversionError> {
		match s.len() {
			32 => Ok(NoteOrChatKey::V2(MasterKey::try_from(s.into_owned())?.0)),
			64 => Ok(NoteOrChatKey::V3(EncryptionKey::from_str(&s)?)),
			len => Err(crate::crypto::error::ConversionError::InvalidStringLength(
				len, 32,
			)),
		}
	}
}

impl std::fmt::Display for NoteOrChatKey {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			NoteOrChatKey::V2(key) => write!(f, "{}", key.as_ref()),
			NoteOrChatKey::V3(key) => write!(f, "{}", key),
		}
	}
}

impl Serialize for NoteOrChatKey {
	fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: serde::Serializer,
	{
		match self {
			NoteOrChatKey::V2(key) => key.as_ref().serialize(serializer),
			NoteOrChatKey::V3(key) => key.serialize(serializer),
		}
	}
}

impl<'de> Deserialize<'de> for NoteOrChatKey {
	fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
	where
		D: serde::Deserializer<'de>,
	{
		let key = String::deserialize(deserializer)?;
		Self::from_cow_str(Cow::Owned(key)).map_err(serde::de::Error::custom)
	}
}

impl MetaCrypter for NoteOrChatKey {
	fn blocking_encrypt_meta_into(
		&self,
		meta: &str,
		out: String,
	) -> filen_types::crypto::EncryptedString<'static> {
		match self {
			NoteOrChatKey::V2(key) => key.blocking_encrypt_meta_into(meta, out),
			NoteOrChatKey::V3(key) => key.blocking_encrypt_meta_into(meta, out),
		}
	}

	fn blocking_decrypt_meta_into(
		&self,
		meta: &filen_types::crypto::EncryptedString<'_>,
		out: Vec<u8>,
	) -> Result<String, (super::error::ConversionError, Vec<u8>)> {
		match self {
			NoteOrChatKey::V2(key) => key.blocking_decrypt_meta_into(meta, out),
			NoteOrChatKey::V3(key) => key.blocking_decrypt_meta_into(meta, out),
		}
	}
}

impl DataCrypter for NoteOrChatKey {
	fn blocking_encrypt_data(
		&self,
		data: &mut Vec<u8>,
	) -> Result<(), super::error::ConversionError> {
		match self {
			NoteOrChatKey::V2(key) => key.blocking_encrypt_data(data),
			NoteOrChatKey::V3(key) => key.blocking_encrypt_data(data),
		}
	}

	fn blocking_decrypt_data(
		&self,
		data: &mut Vec<u8>,
	) -> Result<(), super::error::ConversionError> {
		match self {
			NoteOrChatKey::V2(key) => key.blocking_decrypt_data(data),
			NoteOrChatKey::V3(key) => key.blocking_decrypt_data(data),
		}
	}
}

impl CreateRandom for NoteOrChatKey {
	fn seeded_generate(rng: &mut rand::prelude::ThreadRng) -> Self {
		Self::V2(MasterKey::seeded_generate(rng).0)
	}
}

pub(crate) trait NoteOrChatCarrierCrypto<T>
where
	T: ToOwned + ?Sized,
	T::Owned: Default,
{
	type WithLifetime<'de>: Serialize + Deserialize<'de>;
	const NAME: &'static str;
	fn into_inner<'a>(item: Self::WithLifetime<'a>) -> T::Owned;
	fn from_inner<'a>(s: &'a T) -> Self::WithLifetime<'a>;
}

pub(crate) trait NoteOrChatCarrierCryptoExt<T>: NoteOrChatCarrierCrypto<T>
where
	T: ToOwned + ?Sized,
	T::Owned: Default,
{
	fn blocking_try_decrypt(
		crypter: &impl MetaCrypter,
		encrypted: &EncryptedString<'_>,
	) -> Result<T::Owned, Error> {
		if encrypted.0.is_empty() {
			return Ok(T::Owned::default());
		}

		let decrypted = crypter.blocking_decrypt_meta(encrypted).map_err(|e| {
			Error::custom_with_source(ErrorKind::Response, e, Some("decrypt note title"))
		})?;

		let carrier: Self::WithLifetime<'_> = serde_json::from_str(&decrypted)?;
		let out_string = Self::into_inner(carrier);
		Ok(out_string)
	}

	fn blocking_encrypt(crypter: &impl MetaCrypter, inner: &T) -> EncryptedString<'static> {
		let struct_ = Self::from_inner(inner);
		let struct_string =
			serde_json::to_string(&struct_).expect("Failed to serialize note title");
		crypter.blocking_encrypt_meta(&struct_string)
	}
}

impl<T: NoteOrChatCarrierCrypto<U>, U> NoteOrChatCarrierCryptoExt<U> for T
where
	U: ToOwned + ?Sized,
	U::Owned: Default,
{
}

macro_rules! impl_note_or_chat_carrier_crypto {
	($struct_name:ident, $field_name:ident, $debug_name:literal, $inner_type_name:ident) => {
		impl crate::crypto::notes_and_chats::NoteOrChatCarrierCrypto<$inner_type_name>
			for $struct_name<'_>
		{
			type WithLifetime<'a> = $struct_name<'a>;

			const NAME: &'static str = $debug_name;

			fn into_inner<'a>(
				item: Self::WithLifetime<'a>,
			) -> <$inner_type_name as ToOwned>::Owned {
				item.$field_name.into_owned()
			}

			fn from_inner(s: &$inner_type_name) -> Self::WithLifetime<'_> {
				$struct_name {
					$field_name: Cow::Borrowed(s),
				}
			}
		}
	};
}

pub(crate) use impl_note_or_chat_carrier_crypto;

#[derive(Deserialize, Serialize)]
pub(crate) struct NoteOrChatKeyStruct<'a> {
	key: Cow<'a, NoteOrChatKey>,
}

impl NoteOrChatKeyStruct<'_> {
	pub(crate) fn blocking_try_decrypt_rsa(
		rsa_key: &RsaPrivateKey,
		encrypted_key: &RSAEncryptedString<'_>,
	) -> Result<NoteOrChatKey, Error> {
		let key = crate::crypto::rsa::blocking_decrypt_with_private_key(rsa_key, encrypted_key)
			.map_err(|e| {
				Error::custom_with_source(ErrorKind::Response, e, Some("decrypt note key"))
			})?;
		let key_str = str::from_utf8(&key)
			.map_err(|_| Error::custom(ErrorKind::Response, "Failed to parse note key as UTF-8"))?;
		let key_struct: NoteOrChatKeyStruct = serde_json::from_str(key_str)?;
		Ok(key_struct.key.into_owned())
	}

	pub(crate) fn blocking_try_encrypt_rsa(
		rsa_key: &RsaPublicKey,
		note_key: &NoteOrChatKey,
	) -> Result<RSAEncryptedString<'static>, Error> {
		let key_struct = NoteOrChatKeyStruct {
			key: Cow::Borrowed(note_key),
		};
		let key_string = serde_json::to_string(&key_struct)?;
		let encrypted_key =
			crate::crypto::rsa::blocking_encrypt_with_public_key(rsa_key, key_string.as_bytes())
				.map_err(|e| {
					Error::custom_with_source(ErrorKind::Conversion, e, Some("encrypt note key"))
				})?;
		Ok(encrypted_key)
	}

	pub(crate) fn blocking_encrypt_symmetric(
		crypter: &impl MetaCrypter,
		note_key: &NoteOrChatKey,
	) -> EncryptedString<'static> {
		let key_struct = NoteOrChatKeyStruct {
			key: Cow::Borrowed(note_key),
		};
		let key_string = serde_json::to_string(&key_struct).expect("Failed to serialize note key");
		crypter.blocking_encrypt_meta(&key_string)
	}
}
