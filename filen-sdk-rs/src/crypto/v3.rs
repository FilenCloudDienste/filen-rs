use std::{fmt::Debug, str::FromStr};

use aes_gcm::{
	AesGcm, KeyInit, Nonce,
	aead::{Aead, AeadInPlace},
	aes::Aes256,
};
use base64::{Engine, prelude::BASE64_STANDARD};
use filen_types::crypto::{DerivedPassword, EncryptedString};
use generic_array::typenum::{U12, U16};
use rand::Rng;
use serde::{Deserialize, Serialize};

use super::{
	error::ConversionError,
	shared::{CreateRandom, DataCrypter, MetaCrypter},
};

type NonceSize = U12;
const NONCE_SIZE: usize = 12;
type TagSize = U16;
const TAG_SIZE: usize = 16;

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
		f.debug_struct("EncryptionKey")
			.field("bytes", &faster_hex::hex_string(&self.bytes))
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
	fn encrypt_meta_into(
		&self,
		meta: impl AsRef<str>,
		out: &mut EncryptedString,
	) -> Result<(), ConversionError> {
		let meta = meta.as_ref();
		let nonce: [u8; NONCE_SIZE] = rand::random();
		let nonce = Nonce::from_slice(&nonce);
		let out = &mut out.0;
		let base64_len =
			base64::encoded_len(meta.len() + TAG_SIZE, true).expect("meta len too long for base64");
		out.reserve(3 + NONCE_SIZE * 2 + base64_len);
		out.push_str("003");
		{
			// SAFETY: hex::encode_to_slice adds valid UTF8
			let out = unsafe { out.as_mut_vec() };
			out.resize(3 + NONCE_SIZE * 2, 0);
			faster_hex::hex_encode(nonce.as_slice(), &mut out[3..])?;
		}

		// not allocating here is very difficult, so we don't bother
		// the problem is that if we owned the meta String, we could consume it,
		// but it would almost certainly require a reallocation
		// because we would need to extend the buffer to fit the authentication tag
		// before base64 encoding
		let encrypted = self.cipher.encrypt(nonce, meta.as_bytes())?;
		BASE64_STANDARD.encode_string(encrypted, out);
		Ok(())
	}

	fn decrypt_meta_into(
		&self,
		meta: &EncryptedString,
		out: &mut String,
	) -> Result<(), ConversionError> {
		let meta = &meta.0;
		if meta.len() < NONCE_SIZE * 2 + 3 {
			// hex encoded NONCE_SIZE + 3 for version tag
			return Err(ConversionError::InvalidStringLength(
				meta.len(),
				NONCE_SIZE * 2 + 3,
			));
		}
		let tag = &meta[0..3];
		if tag != "003" {
			return Err(ConversionError::InvalidVersion(
				tag.to_string(),
				vec!["003".to_string()],
			));
		}
		let mut nonce = [0u8; NONCE_SIZE];
		faster_hex::hex_decode(
			&meta.as_bytes()[3..3 + NONCE_SIZE * 2],
			nonce.as_mut_slice(),
		)?;
		let nonce = Nonce::from(nonce);
		out.clear();
		{
			// SAFETY: we validate the utf8 status of the vec at the end of this block
			let out = unsafe { out.as_mut_vec() };
			BASE64_STANDARD.decode_vec(&meta[NONCE_SIZE * 2 + 3..], out)?;

			self.cipher.decrypt_in_place(&nonce, &[], out)?;
			if let Err(e) = std::str::from_utf8(out) {
				out.clear();
				return Err(e.into());
			}
		}
		Ok(())
	}
}

impl DataCrypter for EncryptionKey {
	fn encrypt_data(&self, data: &mut Vec<u8>) -> Result<(), ConversionError> {
		let nonce: [u8; NONCE_SIZE] = rand::random();
		let nonce = Nonce::from_slice(&nonce);
		data.reserve_exact(NONCE_SIZE + TAG_SIZE);
		self.cipher.encrypt_in_place(nonce, &[], data)?;
		let original_len = data.len();
		data.extend_from_within(original_len - NONCE_SIZE..);
		data.copy_within(0..original_len - NONCE_SIZE, NONCE_SIZE);
		data[0..NONCE_SIZE].copy_from_slice(nonce.as_slice());
		Ok(())
	}

	fn decrypt_data(&self, data: &mut Vec<u8>) -> Result<(), ConversionError> {
		if data.len() < NONCE_SIZE + TAG_SIZE {
			return Err(ConversionError::InvalidStringLength(
				data.len(),
				NONCE_SIZE + TAG_SIZE,
			));
		}
		let nonce: [u8; NONCE_SIZE] = data[0..NONCE_SIZE].try_into()?;
		let nonce = Nonce::from_slice(&nonce);
		data.copy_within(NONCE_SIZE.., 0);
		data.truncate(data.len() - NONCE_SIZE);
		self.cipher.decrypt_in_place(nonce, &[], data)?;
		Ok(())
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

pub fn derive_password_and_kek(
	pwd: impl AsRef<[u8]>,
	salt: impl AsRef<[u8]>,
) -> Result<(EncryptionKey, DerivedPassword), ConversionError> {
	let mut decoded_salt = [0u8; 256];
	faster_hex::hex_decode(salt.as_ref(), &mut decoded_salt)?;

	let derived_data = derive_password(pwd.as_ref(), &decoded_salt)?;
	let derived_str = faster_hex::hex_string(&derived_data);

	let kek = EncryptionKey::from_str(&derived_str[0..64])?;
	let password = DerivedPassword(derived_str[64..128].to_owned());

	Ok((kek, password))
}
