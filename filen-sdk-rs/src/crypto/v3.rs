use std::{borrow::Cow, fmt::Debug, str::FromStr};

use aes_gcm::{
	AesGcm, KeyInit, Nonce,
	aead::{Aead, AeadInPlace},
	aes::Aes256,
};
use base64::{Engine, prelude::BASE64_STANDARD};
use filen_types::crypto::{DerivedPassword, EncryptedString};
use rand::Rng;
use serde::{Deserialize, Serialize};
use sha2::Digest;

use crate::crypto::shared::{NONCE_SIZE, NonceSize, TAG_SIZE, TagSize};

use super::{
	error::ConversionError,
	shared::{CreateRandom, DataCrypter, MetaCrypter},
};

pub const ARGON2_PARAMS: argon2::Params = match argon2::Params::new(65536, 3, 4, Some(64)) {
	Ok(params) => params,
	Err(_) => panic!("Failed to create Argon2 params"),
};

#[derive(Clone)]
pub struct EncryptionKey {
	pub bytes: [u8; 32],
	pub cipher: Box<AesGcm<Aes256, NonceSize, TagSize>>,
}

impl EncryptionKey {
	pub fn new(key: [u8; 32]) -> Self {
		let cipher = AesGcm::new(&key.into());
		Self {
			bytes: key,
			cipher: Box::new(cipher),
		}
	}
}

impl FromStr for EncryptionKey {
	type Err = ConversionError;
	fn from_str(key: &str) -> Result<Self, ConversionError> {
		if key.len() != 64 {
			return Err(ConversionError::InvalidStringLength(key.len(), 64));
		}
		let mut array = [0u8; 32];
		faster_hex::hex_decode(key.as_bytes(), &mut array).expect("Invalid hex string");
		Ok(Self::new(array))
	}
}

impl std::fmt::Display for EncryptionKey {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "{}", faster_hex::hex_string(&self.bytes))
	}
}

impl Serialize for EncryptionKey {
	fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: serde::Serializer,
	{
		let hex = faster_hex::hex_string(&self.bytes);
		serializer.serialize_str(&hex)
	}
}

impl<'de> Deserialize<'de> for EncryptionKey {
	fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
	where
		D: serde::Deserializer<'de>,
	{
		let hex = String::deserialize(deserializer)?;
		EncryptionKey::from_str(&hex).map_err(serde::de::Error::custom)
	}
}

impl Debug for EncryptionKey {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		let key_hash_str = faster_hex::hex_string(&sha2::Sha512::digest(self.bytes));
		f.debug_struct("EncryptionKey")
			.field("bytes (hashed)", &key_hash_str)
			.finish()
	}
}

impl PartialEq for EncryptionKey {
	fn eq(&self, other: &Self) -> bool {
		self.bytes == other.bytes
	}
}

impl Eq for EncryptionKey {}

impl MetaCrypter for EncryptionKey {
	fn encrypt_meta_into(&self, meta: &str, mut out: String) -> EncryptedString<'static> {
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
			faster_hex::hex_encode(nonce.as_slice(), &mut out[out_len..]).unwrap_unchecked();
		}

		// not allocating here is very difficult, so we don't bother
		// the problem is that if we owned the meta String, we could consume it,
		// but it would almost certainly require a reallocation
		// because we would need to extend the buffer to fit the authentication tag
		// before base64 encoding

		// SAFETY: This cannot fail unless we encrypt more than 64GiB of metadata at a time
		// which we will never do we also don't have AAD which could cause issues
		let encrypted = self.cipher.encrypt(nonce, meta.as_bytes()).unwrap();
		BASE64_STANDARD.encode_string(encrypted, &mut out);
		EncryptedString(Cow::Owned(out))
	}

	fn decrypt_meta_into(
		&self,
		meta: &EncryptedString,
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
		if let Err(e) = faster_hex::hex_decode(
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
		if let Err(e) = self.cipher.decrypt_in_place(&nonce, &[], &mut out) {
			return Err((e.into(), out));
		}
		String::from_utf8(out).map_err(|e| (e.utf8_error().into(), e.into_bytes()))
	}
}

impl DataCrypter for EncryptionKey {
	fn encrypt_data(&self, data: &mut Vec<u8>) -> Result<(), ConversionError> {
		super::shared::encrypt_data(&self.cipher, data)
	}

	fn decrypt_data(&self, data: &mut Vec<u8>) -> Result<(), ConversionError> {
		super::shared::decrypt_data(&self.cipher, data)
	}
}

impl CreateRandom for EncryptionKey {
	fn seeded_generate(mut rng: rand::prelude::ThreadRng) -> Self {
		Self::new(rng.random())
	}
}

pub(crate) fn derive_password(password: &[u8], salt: &[u8]) -> Result<[u8; 64], ConversionError> {
	let argon2 = argon2::Argon2::new(
		argon2::Algorithm::Argon2id,
		argon2::Version::V0x13,
		ARGON2_PARAMS,
	);
	let mut derived_data = [0u8; 64];
	argon2.hash_password_into(password, salt, &mut derived_data)?;
	Ok(derived_data)
}

pub(crate) fn derive_password_and_kek(
	pwd: &[u8],
	salt: &[u8],
) -> Result<(EncryptionKey, DerivedPassword<'static>), ConversionError> {
	let mut decoded_salt = [0u8; 256];
	faster_hex::hex_decode(salt, &mut decoded_salt)?;

	let derived_data = derive_password(pwd, &decoded_salt)?;
	let derived_str = faster_hex::hex_string(&derived_data);

	let kek = EncryptionKey::from_str(&derived_str[0..64])?;
	let password = DerivedPassword(Cow::Owned(derived_str[64..128].to_string()));

	Ok((kek, password))
}
