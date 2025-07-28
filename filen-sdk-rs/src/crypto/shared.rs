use filen_types::crypto::EncryptedString;
use rand::{Rng, rngs::ThreadRng};

use super::error::ConversionError;

pub trait MetaCrypter {
	fn encrypt_meta_into(&self, meta: &str, out: String) -> EncryptedString;
	fn encrypt_meta(&self, meta: &str) -> EncryptedString {
		self.encrypt_meta_into(meta, String::new())
	}
	fn decrypt_meta_into(
		&self,
		meta: &EncryptedString,
		out: Vec<u8>,
	) -> Result<String, (ConversionError, Vec<u8>)>;
	fn decrypt_meta(&self, meta: &EncryptedString) -> Result<String, ConversionError> {
		self.decrypt_meta_into(meta, Vec::new()).map_err(|(e, _)| e)
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
