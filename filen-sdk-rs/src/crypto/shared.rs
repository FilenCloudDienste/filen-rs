use aes_gcm::{AesGcm, Nonce, aead::AeadInPlace, aes::Aes256};
use digest::consts::{U12, U16};
use filen_types::crypto::EncryptedString;
use rand::{Rng, rngs::ThreadRng};

use crate::runtime;

use super::error::ConversionError;

pub trait MetaCrypter: Send + Sync {
	fn blocking_encrypt_meta_into(&self, meta: &str, out: String) -> EncryptedString<'static>;
	fn blocking_encrypt_meta(&self, meta: &str) -> EncryptedString<'static> {
		self.blocking_encrypt_meta_into(meta, String::new())
	}

	fn encrypt_meta_into<'a>(
		&'a self,
		meta: &'a str,
		out: String,
	) -> impl Future<Output = EncryptedString<'static>> + Send + 'a {
		runtime::do_cpu_intensive(move || self.blocking_encrypt_meta_into(meta, out))
	}

	fn encrypt_meta<'a>(
		&'a self,
		meta: &'a str,
	) -> impl Future<Output = EncryptedString<'static>> + Send + 'a {
		runtime::do_cpu_intensive(move || self.blocking_encrypt_meta(meta))
	}

	fn blocking_decrypt_meta_into(
		&self,
		meta: &EncryptedString,
		out: Vec<u8>,
	) -> Result<String, (ConversionError, Vec<u8>)>;

	fn blocking_decrypt_meta(&self, meta: &EncryptedString) -> Result<String, ConversionError> {
		self.blocking_decrypt_meta_into(meta, Vec::new())
			.map_err(|(e, _)| e)
	}

	fn decrypt_meta_into<'a>(
		&'a self,
		meta: &'a EncryptedString<'a>,
		out: Vec<u8>,
	) -> impl Future<Output = Result<String, (ConversionError, Vec<u8>)>> + Send + 'a {
		runtime::do_cpu_intensive(|| self.blocking_decrypt_meta_into(meta, out))
	}

	fn decrypt_meta<'a>(
		&'a self,
		meta: &'a EncryptedString<'a>,
	) -> impl Future<Output = Result<String, ConversionError>> + Send + 'a {
		runtime::do_cpu_intensive(|| self.blocking_decrypt_meta(meta))
	}
}

pub(crate) trait DataCrypter: Send + Sync {
	fn blocking_encrypt_data(&self, data: &mut Vec<u8>) -> Result<(), ConversionError>;
	fn blocking_decrypt_data(&self, data: &mut Vec<u8>) -> Result<(), ConversionError>;

	fn encrypt_data<'a>(
		&'a self,
		data: &'a mut Vec<u8>,
	) -> impl Future<Output = Result<(), ConversionError>> + Send + 'a {
		runtime::do_cpu_intensive(|| self.blocking_encrypt_data(data))
	}

	fn decrypt_data<'a>(
		&'a self,
		data: &'a mut Vec<u8>,
	) -> impl Future<Output = Result<(), ConversionError>> + Send + 'a {
		runtime::do_cpu_intensive(|| self.blocking_decrypt_data(data))
	}
}

pub(crate) trait CreateRandom: Sized {
	fn seeded_generate(rng: &mut ThreadRng) -> Self;
	fn generate() -> Self {
		Self::seeded_generate(&mut rand::rng())
	}
}

const BASE64_ALPHABET: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

pub fn generate_random_base64_values(len: usize, rng: &mut rand::prelude::ThreadRng) -> String {
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

pub(crate) type NonceSize = U12;
pub(crate) const NONCE_SIZE: usize = 12;
pub(crate) type TagSize = U16;
pub(crate) const TAG_SIZE: usize = 16;

pub(crate) fn encrypt_data(
	cipher: &AesGcm<Aes256, NonceSize, TagSize>,
	data: &mut Vec<u8>,
) -> Result<(), ConversionError> {
	let nonce: [u8; NONCE_SIZE] = rand::random();
	let nonce = Nonce::from_slice(&nonce);
	data.reserve_exact(NONCE_SIZE + TAG_SIZE);
	cipher.encrypt_in_place(nonce, &[], data)?;
	let original_len = data.len();
	data.extend_from_within(original_len - NONCE_SIZE..);
	data.copy_within(0..original_len - NONCE_SIZE, NONCE_SIZE);
	data[0..NONCE_SIZE].copy_from_slice(nonce.as_slice());
	Ok(())
}

pub(crate) fn decrypt_data(
	cipher: &AesGcm<Aes256, NonceSize, TagSize>,
	data: &mut Vec<u8>,
) -> Result<(), ConversionError> {
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
	cipher.decrypt_in_place(nonce, &[], data)?;
	Ok(())
}
