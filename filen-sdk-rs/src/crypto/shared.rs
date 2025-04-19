use filen_types::crypto::EncryptedString;
use rand::{Rng, rngs::ThreadRng};

use super::error::ConversionError;

pub trait MetaCrypter {
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

const BASE64_ALPHABET: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

pub fn generate_random_base64_values(len: usize) -> String {
	let mut rng = rand::rng();
	let mut values = String::with_capacity(len);
	for _ in 0..len {
		values.push(
			BASE64_ALPHABET
				.chars()
				.nth(rng.random_range(0..64))
				.unwrap(), // SAFETY: The range is valid and the alphabet is 64 characters long.
		);
	}
	values
}
