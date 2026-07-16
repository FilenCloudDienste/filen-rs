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
	fs::meta_recovery,
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

		let decrypted = match crypter.blocking_decrypt_meta_into(encrypted, Vec::new()) {
			Ok(decrypted) => decrypted,
			// non-UTF-8 plaintext is Latin-1 from clients that skip UTF-8
			// encoding; the mapping is lossless (see fs::meta_recovery)
			Err((super::error::ConversionError::ToStrError(_), bytes)) => {
				meta_recovery::latin1_to_string(&bytes)
			}
			Err((e, _)) => {
				return Err(Error::custom_with_source(
					ErrorKind::Response,
					e,
					Some("decrypt note title"),
				));
			}
		};

		match serde_json::from_str::<Self::WithLifetime<'_>>(&decrypted) {
			Ok(carrier) => Ok(Self::into_inner(carrier)),
			Err(e) => {
				// JS clients JSON.stringify unpaired surrogates as lone \udXXX
				// escapes, which JSON.parse accepts but serde_json rejects
				if let Some(sanitized) =
					meta_recovery::replace_unpaired_surrogate_escapes(&decrypted)
					&& let Ok(carrier) = serde_json::from_str::<Self::WithLifetime<'_>>(&sanitized)
				{
					return Ok(Self::into_inner(carrier));
				}
				Err(e.into())
			}
		}
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

	pub(crate) fn blocking_try_decrypt_symmetric(
		crypter: &impl MetaCrypter,
		encrypted_key: &EncryptedString<'_>,
	) -> Result<NoteOrChatKey, Error> {
		let key_string = crypter.blocking_decrypt_meta(encrypted_key).map_err(|e| {
			Error::custom_with_source(ErrorKind::Response, e, Some("decrypt note key"))
		})?;
		let key_struct: NoteOrChatKeyStruct = serde_json::from_str(&key_string)?;
		Ok(key_struct.key.into_owned())
	}
}

#[cfg(test)]
mod tests {
	use aes_gcm::{Aes256Gcm, KeyInit, Nonce, aead::Aead};
	use base64::{Engine, prelude::BASE64_STANDARD};
	use hmac::Hmac;
	use sha2::Sha512;

	use super::*;
	use crate::{
		chats::crypto::ChatMessage,
		notes::crypto::{NoteContent, NotePreview, NoteTitle},
	};

	const V2_KEY: &str = "abcdefghijklmnopqrstuvwxyzABCDEF";

	fn v2_key() -> NoteOrChatKey {
		NoteOrChatKey::from_cow_str(Cow::Borrowed(V2_KEY)).unwrap()
	}

	#[test]
	fn lone_surrogate_note_preview_recovers_to_replacement_char() {
		let key = v2_key();
		let encrypted = key.blocking_encrypt_meta(r#"{"preview":"a\ud83d"}"#);
		assert_eq!(
			NotePreview::blocking_try_decrypt(&key, &encrypted).unwrap(),
			"a\u{FFFD}"
		);
	}

	#[test]
	fn paired_surrogate_note_preview_decrypts_losslessly() {
		let key = v2_key();
		let encrypted = key.blocking_encrypt_meta(r#"{"preview":"a😀"}"#);
		assert_eq!(
			NotePreview::blocking_try_decrypt(&key, &encrypted).unwrap(),
			"a😀"
		);
	}

	#[test]
	fn lone_surrogate_chat_message_recovers_to_replacement_char() {
		let key = v2_key();
		let encrypted = key.blocking_encrypt_meta(r#"{"message":"hi \udc00"}"#);
		assert_eq!(
			ChatMessage::blocking_try_decrypt(&key, &encrypted).unwrap(),
			"hi \u{FFFD}"
		);
	}

	#[test]
	fn lone_surrogate_note_title_recovers_with_v3_key() {
		let key = NoteOrChatKey::from_cow_str(Cow::Borrowed(
			"00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff",
		))
		.unwrap();
		let encrypted = key.blocking_encrypt_meta(r#"{"title":"\ud800"}"#);
		assert_eq!(
			NoteTitle::blocking_try_decrypt(&key, &encrypted).unwrap(),
			"\u{FFFD}"
		);
	}

	#[test]
	fn latin1_note_content_recovers_original_text() {
		// encrypts `{"content":"café"}` truncated to one byte per code point,
		// the way clients that skip UTF-8 encoding produce it
		let mut derived_key = [0u8; 32];
		pbkdf2::pbkdf2::<Hmac<Sha512>>(V2_KEY.as_bytes(), V2_KEY.as_bytes(), 1, &mut derived_key)
			.unwrap();
		let nonce = b"012345678901";
		let plaintext = b"{\"content\":\"caf\xe9\"}";
		let ciphertext = Aes256Gcm::new_from_slice(&derived_key)
			.unwrap()
			.encrypt(Nonce::from_slice(nonce), plaintext.as_slice())
			.unwrap();
		let meta = EncryptedString(Cow::Owned(format!(
			"002{}{}",
			std::str::from_utf8(nonce).unwrap(),
			BASE64_STANDARD.encode(&ciphertext)
		)));
		assert_eq!(
			NoteContent::blocking_try_decrypt(&v2_key(), &meta).unwrap(),
			"café"
		);
	}

	#[test]
	fn invalid_json_still_errors() {
		let key = v2_key();
		let encrypted = key.blocking_encrypt_meta("not json");
		assert!(NotePreview::blocking_try_decrypt(&key, &encrypted).is_err());
	}

	#[test]
	fn escaped_surrogate_pair_decrypts_losslessly() {
		let key = v2_key();
		let encrypted = key.blocking_encrypt_meta(r#"{"preview":"a\ud83d\ude00"}"#);
		assert_eq!(
			NotePreview::blocking_try_decrypt(&key, &encrypted).unwrap(),
			"a😀"
		);
	}

	#[test]
	fn empty_ciphertext_decrypts_to_default() {
		assert_eq!(
			NotePreview::blocking_try_decrypt(&v2_key(), &EncryptedString(Cow::Borrowed("")))
				.unwrap(),
			""
		);
	}

	#[test]
	fn tampered_ciphertext_errors_instead_of_recovering() {
		let key = v2_key();
		let encrypted = key.blocking_encrypt_meta(r#"{"preview":"a"}"#);
		let mut tampered = encrypted.0.into_owned();
		let flipped = if tampered.ends_with('A') { 'B' } else { 'A' };
		tampered.pop();
		tampered.push(flipped);
		assert!(
			NotePreview::blocking_try_decrypt(&key, &EncryptedString(Cow::Owned(tampered)))
				.is_err()
		);
	}
}
