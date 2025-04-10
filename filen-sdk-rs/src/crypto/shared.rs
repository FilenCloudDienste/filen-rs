use filen_types::crypto::EncryptedString;
use rand::rngs::ThreadRng;
use serde::{Deserialize, Serialize};
use sha2::Sha512;

use super::error::ConversionError;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Sha512Hash(#[serde(with = "faster_hex")] digest::Output<Sha512>);

impl From<digest::Output<Sha512>> for Sha512Hash {
	fn from(hash: digest::Output<Sha512>) -> Self {
		Self(hash)
	}
}

impl From<Sha512Hash> for digest::Output<Sha512> {
	fn from(hash: Sha512Hash) -> Self {
		hash.0
	}
}

impl From<Sha512Hash> for [u8; 64] {
	fn from(hash: Sha512Hash) -> Self {
		hash.0.into()
	}
}

pub(crate) trait MetaCrypter {
	fn encrypt_meta_into(
		&self,
		meta: &str,
		out: &mut EncryptedString,
	) -> Result<(), ConversionError>;
	fn encrypt_meta(&self, meta: &str) -> Result<EncryptedString, ConversionError> {
		let mut out = EncryptedString(String::new());
		self.encrypt_meta_into(meta, &mut out)?;
		Ok(out)
	}
	fn decrypt_meta_into(
		&self,
		meta: &EncryptedString,
		out: &mut String,
	) -> Result<(), ConversionError>;
	fn decrypt_meta(&self, meta: &EncryptedString) -> Result<String, ConversionError> {
		let mut out = String::new();
		self.decrypt_meta_into(meta, &mut out)?;
		Ok(out)
	}
}

pub(crate) trait DataCrypter {
	fn encrypt_data(&self, data: &mut Vec<u8>) -> Result<(), ConversionError>;
	fn decrypt_data(&self, data: &mut Vec<u8>) -> Result<(), ConversionError>;
}

pub(crate) trait CreateRandom: Sized {
	fn seeded_generate(rng: ThreadRng) -> Self;
	fn generate() -> Self {
		Self::seeded_generate(rand::rng())
	}
}
