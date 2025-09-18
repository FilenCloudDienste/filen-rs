use std::str::FromStr;

use filen_types::crypto::EncryptedString;
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
pub enum NoteKey {
	V2(V2Key),
	V3(EncryptionKey),
}

impl Serialize for NoteKey {
	fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: serde::Serializer,
	{
		match self {
			NoteKey::V2(key) => key.as_ref().serialize(serializer),
			NoteKey::V3(key) => key.serialize(serializer),
		}
	}
}

impl<'de> Deserialize<'de> for NoteKey {
	fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
	where
		D: serde::Deserializer<'de>,
	{
		let key = String::deserialize(deserializer)?;
		match key.len() {
			32 => Ok(NoteKey::V2(
				MasterKey::try_from(key)
					.map_err(serde::de::Error::custom)?
					.0,
			)),
			64 => Ok(NoteKey::V3(
				EncryptionKey::from_str(&key).map_err(serde::de::Error::custom)?,
			)),
			len => Err(serde::de::Error::custom(format!(
				"Invalid key length: {len}"
			))),
		}
	}
}

impl MetaCrypter for NoteKey {
	fn encrypt_meta_into(
		&self,
		meta: &str,
		out: String,
	) -> filen_types::crypto::EncryptedString<'static> {
		match self {
			NoteKey::V2(key) => key.encrypt_meta_into(meta, out),
			NoteKey::V3(key) => key.encrypt_meta_into(meta, out),
		}
	}

	fn decrypt_meta_into(
		&self,
		meta: &filen_types::crypto::EncryptedString<'_>,
		out: Vec<u8>,
	) -> Result<String, (super::error::ConversionError, Vec<u8>)> {
		match self {
			NoteKey::V2(key) => key.decrypt_meta_into(meta, out),
			NoteKey::V3(key) => key.decrypt_meta_into(meta, out),
		}
	}
}

impl DataCrypter for NoteKey {
	fn encrypt_data(&self, data: &mut Vec<u8>) -> Result<(), super::error::ConversionError> {
		match self {
			NoteKey::V2(key) => key.encrypt_data(data),
			NoteKey::V3(key) => key.encrypt_data(data),
		}
	}

	fn decrypt_data(&self, data: &mut Vec<u8>) -> Result<(), super::error::ConversionError> {
		match self {
			NoteKey::V2(key) => key.decrypt_data(data),
			NoteKey::V3(key) => key.decrypt_data(data),
		}
	}
}

impl CreateRandom for NoteKey {
	fn seeded_generate(rng: &mut rand::prelude::ThreadRng) -> Self {
		Self::V2(MasterKey::seeded_generate(rng).0)
	}
}

pub(crate) trait NoteOrChatCarrierCrypto<T>
where
	T: ToOwned + ?Sized,
{
	type WithLifetime<'de>: Serialize + Deserialize<'de>;
	const NAME: &'static str;
	fn into_inner<'a>(item: Self::WithLifetime<'a>) -> T::Owned;
	fn from_inner<'a>(s: &'a T) -> Self::WithLifetime<'a>;
}

pub(crate) trait NoteOrChatCarrierCryptoExt<T>: NoteOrChatCarrierCrypto<T>
where
	T: ToOwned + ?Sized,
{
	fn try_decrypt(
		crypter: &impl MetaCrypter,
		encrypted: &EncryptedString<'_>,
		outer_tmp_vec: &mut Vec<u8>,
	) -> Result<T::Owned, Error> {
		let tmp_vec = std::mem::take(outer_tmp_vec);
		let decrypted = crypter
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

	fn encrypt(crypter: &impl MetaCrypter, inner: &T) -> EncryptedString<'static> {
		let struct_ = Self::from_inner(inner);
		let struct_string =
			serde_json::to_string(&struct_).expect("Failed to serialize note title");
		crypter.encrypt_meta(&struct_string)
	}
}

impl<T: NoteOrChatCarrierCrypto<U>, U> NoteOrChatCarrierCryptoExt<U> for T where U: ToOwned + ?Sized {}

macro_rules! impl_note_or_chat_carrier_crypto {
	($struct_name:ident, $field_name:ident, $debug_name:literal, $inner_type_name:ident) => {
		impl NoteOrChatCarrierCrypto<$inner_type_name> for $struct_name<'_> {
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
