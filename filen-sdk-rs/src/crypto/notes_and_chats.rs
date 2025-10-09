use std::{borrow::Cow, ops::Deref, str::FromStr};

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
		match key.len() {
			32 => Ok(NoteOrChatKey::V2(
				MasterKey::try_from(key)
					.map_err(serde::de::Error::custom)?
					.0,
			)),
			64 => Ok(NoteOrChatKey::V3(
				EncryptionKey::from_str(&key).map_err(serde::de::Error::custom)?,
			)),
			len => Err(serde::de::Error::custom(format!(
				"Invalid key length: {len}"
			))),
		}
	}
}

impl MetaCrypter for NoteOrChatKey {
	fn encrypt_meta_into(
		&self,
		meta: &str,
		out: String,
	) -> filen_types::crypto::EncryptedString<'static> {
		match self {
			NoteOrChatKey::V2(key) => key.encrypt_meta_into(meta, out),
			NoteOrChatKey::V3(key) => key.encrypt_meta_into(meta, out),
		}
	}

	fn decrypt_meta_into(
		&self,
		meta: &filen_types::crypto::EncryptedString<'_>,
		out: Vec<u8>,
	) -> Result<String, (super::error::ConversionError, Vec<u8>)> {
		match self {
			NoteOrChatKey::V2(key) => key.decrypt_meta_into(meta, out),
			NoteOrChatKey::V3(key) => key.decrypt_meta_into(meta, out),
		}
	}
}

impl DataCrypter for NoteOrChatKey {
	fn encrypt_data(&self, data: &mut Vec<u8>) -> Result<(), super::error::ConversionError> {
		match self {
			NoteOrChatKey::V2(key) => key.encrypt_data(data),
			NoteOrChatKey::V3(key) => key.encrypt_data(data),
		}
	}

	fn decrypt_data(&self, data: &mut Vec<u8>) -> Result<(), super::error::ConversionError> {
		match self {
			NoteOrChatKey::V2(key) => key.decrypt_data(data),
			NoteOrChatKey::V3(key) => key.decrypt_data(data),
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
	fn try_decrypt<MC>(
		crypter: impl Deref<Target = MC>,
		encrypted: &EncryptedString<'_>,
		outer_tmp_vec: &mut Vec<u8>,
	) -> Result<T::Owned, Error>
	where
		MC: MetaCrypter,
	{
		if encrypted.0.is_empty() {
			return Ok(T::Owned::default());
		}

		let tmp_vec = std::mem::take(outer_tmp_vec);
		let decrypted = crypter
			.deref()
			.decrypt_meta_into(encrypted, tmp_vec)
			.map_err(|(e, tmp_vec)| {
				*outer_tmp_vec = tmp_vec;
				Error::custom_with_source(ErrorKind::Response, e, Some("decrypt note title"))
			})?;

		let carrier: Self::WithLifetime<'_> = serde_json::from_str(&decrypted)?;
		let out_string = Self::into_inner(carrier);
		*outer_tmp_vec = decrypted.into_bytes();
		Ok(out_string)
	}

	fn encrypt<MC>(crypter: impl Deref<Target = MC>, inner: &T) -> EncryptedString<'static>
	where
		MC: MetaCrypter,
	{
		let struct_ = Self::from_inner(inner);
		let struct_string =
			serde_json::to_string(&struct_).expect("Failed to serialize note title");
		crypter.deref().encrypt_meta(&struct_string)
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
	pub(crate) fn try_decrypt_rsa(
		rsa_key: &RsaPrivateKey,
		encrypted_key: &RSAEncryptedString<'_>,
	) -> Result<NoteOrChatKey, Error> {
		let key =
			crate::crypto::rsa::decrypt_with_private_key(rsa_key, encrypted_key).map_err(|e| {
				Error::custom_with_source(ErrorKind::Response, e, Some("decrypt note key"))
			})?;
		let key_str = str::from_utf8(&key)
			.map_err(|_| Error::custom(ErrorKind::Response, "Failed to parse note key as UTF-8"))?;
		let key_struct: NoteOrChatKeyStruct = serde_json::from_str(key_str)?;
		Ok(key_struct.key.into_owned())
	}

	pub(crate) fn try_encrypt_rsa(
		rsa_key: &RsaPublicKey,
		note_key: &NoteOrChatKey,
	) -> Result<RSAEncryptedString<'static>, Error> {
		let key_struct = NoteOrChatKeyStruct {
			key: Cow::Borrowed(note_key),
		};
		let key_string = serde_json::to_string(&key_struct)?;
		let encrypted_key =
			crate::crypto::rsa::encrypt_with_public_key(rsa_key, key_string.as_bytes()).map_err(
				|e| Error::custom_with_source(ErrorKind::Conversion, e, Some("encrypt note key")),
			)?;
		Ok(encrypted_key)
	}

	pub(crate) fn encrypt_symmetric<MC>(
		crypter: impl Deref<Target = MC>,
		note_key: &NoteOrChatKey,
	) -> EncryptedString<'static>
	where
		MC: MetaCrypter,
	{
		let key_struct = NoteOrChatKeyStruct {
			key: Cow::Borrowed(note_key),
		};
		let key_string = serde_json::to_string(&key_struct).expect("Failed to serialize note key");
		crypter.deref().encrypt_meta(&key_string)
	}
}
