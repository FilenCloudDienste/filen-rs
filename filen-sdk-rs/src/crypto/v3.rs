use std::{borrow::Cow, fmt::Debug, str::FromStr};

use aes_gcm::{
	AesGcm, Nonce,
	aead::{Aead, AeadInPlace},
	aes::Aes256,
};
use base64::{Engine, prelude::BASE64_STANDARD};
use filen_types::{
	api::v3::dir::link::info::LinkPasswordSalt,
	crypto::{DerivedPassword, EncryptedString},
	serde::str::{SizedHexString, StackSizedString},
};
use rand::Rng;
use serde::{Deserialize, Serialize};
use sha2::Digest;
use typenum::{U32, U64, U256};

use crate::crypto::shared::{NONCE_SIZE, NonceSize, TAG_SIZE, TagSize};

use super::{
	error::ConversionError,
	shared::{CreateRandom, DataCrypter, MetaCrypter},
};

pub const ARGON2_PARAMS: argon2::Params = match argon2::Params::new(65536, 3, 4, Some(64)) {
	Ok(params) => params,
	Err(_) => panic!("Failed to create Argon2 params"),
};

#[derive(Clone, PartialEq, Eq)]
pub struct EncryptionKey {
	hex_string: SizedHexString<U32>,
}

impl EncryptionKey {
	fn as_bytes(&self) -> &[u8; 32] {
		self.hex_string.as_ref()
	}

	fn cipher(&self) -> AesGcm<Aes256, NonceSize, TagSize> {
		<AesGcm<Aes256, NonceSize> as aes_gcm::KeyInit>::new(self.as_bytes().into())
	}
}

#[cfg(feature = "uniffi")]
uniffi::custom_type!(EncryptionKey, String, {
	lower : |key: &EncryptionKey| key.to_string(),
	try_lift : |s: String| EncryptionKey::from_str(&s).map_err(|e| uniffi::deps::anyhow::anyhow!(e))
});

impl EncryptionKey {
	pub fn new(key: [u8; 32]) -> Self {
		Self {
			hex_string: SizedHexString::from(key),
		}
	}

	pub fn to_str(&self) -> StackSizedString<U64> {
		self.hex_string.to_str()
	}
}

impl FromStr for EncryptionKey {
	type Err = ConversionError;
	fn from_str(key: &str) -> Result<Self, ConversionError> {
		Ok(Self {
			hex_string: SizedHexString::new_from_hex_str(key)?,
		})
	}
}

impl std::fmt::Display for EncryptionKey {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "{}", self.hex_string)
	}
}

impl Serialize for EncryptionKey {
	fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: serde::Serializer,
	{
		self.hex_string.serialize(serializer)
	}
}

impl<'de> Deserialize<'de> for EncryptionKey {
	fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
	where
		D: serde::Deserializer<'de>,
	{
		Ok(Self {
			hex_string: SizedHexString::deserialize(deserializer)?,
		})
	}
}

impl Debug for EncryptionKey {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		let key_hash_str = hex::encode(sha2::Sha512::digest(self.hex_string.as_slice()));
		f.debug_struct("EncryptionKey")
			.field("bytes (hashed)", &key_hash_str)
			.finish()
	}
}

impl MetaCrypter for EncryptionKey {
	fn blocking_encrypt_meta_into(&self, meta: &str, mut out: String) -> EncryptedString<'static> {
		let nonce: [u8; NONCE_SIZE] = rand::random();
		let nonce = Nonce::from_slice(&nonce);
		out.clear();
		let base64_len =
			base64::encoded_len(meta.len() + TAG_SIZE, true).expect("meta len too long for base64");
		out.reserve(3 + NONCE_SIZE * 2 + base64_len);
		out.push_str("003");
		// SAFETY: the nonce is NONCE_SIZE bytes long, and we reserve NONCE_SIZE * 2 bytes for the hex representation
		// and the hex representation is always valid UTF-8.
		// ideally faster_hex would provide a way to write directly into a String,
		unsafe {
			let out = out.as_mut_vec();
			let out_len = out.len();
			out.resize(out_len + nonce.len() * 2, 0);
			hex::encode_to_slice(nonce.as_slice(), &mut out[out_len..]).unwrap_unchecked();
		}

		// not allocating here is very difficult, so we don't bother
		// the problem is that if we owned the meta String, we could consume it,
		// but it would almost certainly require a reallocation
		// because we would need to extend the buffer to fit the authentication tag
		// before base64 encoding

		// SAFETY: This cannot fail unless we encrypt more than 64GiB of metadata at a time
		// which we will never do we also don't have AAD which could cause issues
		let encrypted = self.cipher().encrypt(nonce, meta.as_bytes()).unwrap();
		BASE64_STANDARD.encode_string(encrypted, &mut out);
		EncryptedString(Cow::Owned(out))
	}

	fn blocking_decrypt_meta_into(
		&self,
		meta: &EncryptedString<'_>,
		mut out: Vec<u8>,
	) -> Result<String, (ConversionError, Vec<u8>)> {
		let meta = &meta.0;
		if meta.len() < NONCE_SIZE * 2 + 3 {
			// hex encoded NONCE_SIZE + 3 for version tag
			return Err((
				ConversionError::InvalidStringLength(meta.len(), NONCE_SIZE * 2 + 3),
				out,
			));
		}
		let tag = &meta[0..3];
		if tag != "003" {
			return Err((
				ConversionError::InvalidVersion(tag.to_string(), vec!["003".to_string()]),
				out,
			));
		}
		let mut nonce = [0u8; NONCE_SIZE];
		if let Err(e) = hex::decode_to_slice(
			&meta.as_bytes()[3..3 + NONCE_SIZE * 2],
			nonce.as_mut_slice(),
		) {
			return Err((e.into(), out));
		}
		let nonce = Nonce::from(nonce);
		out.clear();
		if let Err(e) = BASE64_STANDARD.decode_vec(&meta[NONCE_SIZE * 2 + 3..], &mut out) {
			return Err((e.into(), out));
		}
		if let Err(e) = self.cipher().decrypt_in_place(&nonce, &[], &mut out) {
			return Err((e.into(), out));
		}
		String::from_utf8(out).map_err(|e| (e.utf8_error().into(), e.into_bytes()))
	}
}

impl DataCrypter for EncryptionKey {
	fn blocking_encrypt_data(&self, data: &mut Vec<u8>) -> Result<(), ConversionError> {
		super::shared::encrypt_data(&self.cipher(), data)
	}

	fn blocking_decrypt_data(&self, data: &mut Vec<u8>) -> Result<(), ConversionError> {
		super::shared::decrypt_data(&self.cipher(), data)
	}
}

impl CreateRandom for EncryptionKey {
	fn seeded_generate(rng: &mut rand::prelude::ThreadRng) -> Self {
		Self::new(rng.random())
	}
}

pub(crate) fn derive_password(
	password: &[u8],
	salt: &SizedHexString<U256>,
) -> Result<[u8; 64], ConversionError> {
	let argon2 = argon2::Argon2::new(
		argon2::Algorithm::Argon2id,
		argon2::Version::V0x13,
		ARGON2_PARAMS,
	);
	let mut derived_data = [0u8; 64];

	argon2.hash_password_into(password, salt.as_slice(), &mut derived_data)?;
	Ok(derived_data)
}

pub(crate) fn derive_password_and_kek(
	pwd: &[u8],
	salt: &SizedHexString<U256>,
) -> Result<(EncryptionKey, DerivedPassword<'static>), ConversionError> {
	let derived_data = SizedHexString::<U64>::from(derive_password(pwd, salt)?);
	let derived_str = derived_data.to_str();

	let kek = EncryptionKey::from_str(&derived_str[0..64])?;
	let password = DerivedPassword(Cow::Owned(derived_str[64..128].to_string()));

	Ok((kek, password))
}

pub(crate) fn make_link_salt() -> LinkPasswordSalt<'static> {
	LinkPasswordSalt::V3(Box::new(SizedHexString::<U256>::from(rand::random::<
		[u8; 256],
	>())))
}
