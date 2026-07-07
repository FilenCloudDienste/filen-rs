use std::borrow::Cow;
use std::str::FromStr;

use aes_gcm::aes::{self};
use base64::{Engine, prelude::BASE64_STANDARD};
use cbc::cipher::block_padding::Pkcs7;
use cbc::cipher::{BlockDecryptMut, KeyIvInit};
use filen_macros::rkyv_self;
use filen_types::crypto::{DerivedPassword, EncryptedString};
use filen_types::serde::str::{SizedHexString, SizedStr};
use md2::{Digest, Md2};
use md4::Md4;
use md5::Md5;
use serde::{Deserialize, Serialize};
use sha1::Sha1;
use sha2::{Sha256, Sha384, Sha512};
use typenum::{U32, U64};

use crate::crypto::error::ConversionError;
use crate::crypto::shared::{DataCrypter, MetaCrypter};
use crate::crypto::v2::V2Key;

use super::v2::{MasterKey, MasterKeys};

const KEY_LEN: usize = 32;
const IV_LEN: usize = 16;

fn evp_bytes_to_key<'a>(
	password: &[u8],
	salt: &[u8],
	iv_len: usize,
	out: &'a mut [u8],
) -> (&'a [u8], &'a [u8]) {
	let mut hasher = Md5::new();
	let step_size = Md5::output_size();

	for i in (0..out.len()).step_by(step_size) {
		hasher.update(password);
		hasher.update(salt);
		let res = hasher.finalize_reset();
		let copy_len = step_size.min(out.len() - i);
		out[i..i + copy_len].copy_from_slice(&res[..copy_len]);
		if out.len() - i > step_size {
			hasher.update(res);
		}
	}

	let key = &out[..out.len() - iv_len];
	let iv = &out[key.len()..];
	(key, iv)
}

fn decrypt(key: &[u8], data: &mut Vec<u8>) -> Result<(), ConversionError> {
	if data.len() < 16 {
		return Err(ConversionError::InvalidStringLength(data.len(), 16));
	}
	let salt = &data[8..16];
	let mut tmp = [0u8; KEY_LEN + IV_LEN];
	let (key_bytes, iv_bytes) = evp_bytes_to_key(key, salt, IV_LEN, &mut tmp);

	let decryptor = cbc::Decryptor::<aes::Aes256>::new_from_slices(key_bytes, iv_bytes)?;

	data.copy_within(16.., 0);
	data.truncate(data.len() - 16);
	decryptor.decrypt_padded_mut::<Pkcs7>(data)?;
	let padding_len = data.last().copied().unwrap_or(0) as usize;
	data.truncate(data.len() - padding_len);
	Ok(())
}

pub(crate) fn decrypt_meta(
	key: &[u8],
	meta: &EncryptedString,
	mut out: Vec<u8>,
) -> Result<String, (ConversionError, Vec<u8>)> {
	out.clear();
	if let Err(e) = BASE64_STANDARD.decode_vec(meta.0.as_ref(), &mut out) {
		return Err((e.into(), out));
	}

	if let Err(e) = decrypt(key, &mut out) {
		return Err((e, out));
	}

	let out = String::from_utf8(out).map_err(|e| {
		let err = e.utf8_error();
		let out = e.into_bytes();
		(err.into(), out)
	})?;
	Ok(out)
}

// V1 data comes in three layouts, distinguished by sniffing (the stored version number
// cannot tell them apart):
//   - raw OpenSSL EVP: "Salted__" + 8-byte salt + AES-256-CBC body, key+IV via EVP_BytesToKey
//   - the same EVP layout as base64 text (starts with "U2FsdGVk"), decoded first
//   - "normal CBC" (no marker): AES-256-CBC over the whole buffer, key used directly,
//     IV = key[0..16], no header to strip
fn decrypt_data(key: &[u8], data: &mut Vec<u8>) -> Result<(), ConversionError> {
	let first_16 = &data[..16.min(data.len())];
	let as_str = String::from_utf8_lossy(first_16);
	let as_b64 = BASE64_STANDARD.encode(first_16);

	let needs_convert = !as_str.starts_with("Salted_") && !as_b64.starts_with("Salted_");
	let is_normal_cbc =
		needs_convert && !as_str.starts_with("U2FsdGVk") && !as_b64.starts_with("U2FsdGVk");

	if needs_convert && !is_normal_cbc {
		*data = BASE64_STANDARD.decode(std::str::from_utf8(data)?)?
	}

	if !is_normal_cbc {
		decrypt(key, data)?;
	} else {
		let cipher = cbc::Decryptor::<aes::Aes256>::new_from_slices(key, &key[..IV_LEN])?;
		cipher.decrypt_padded_mut::<Pkcs7>(data)?;
		let padding_len = data.last().copied().unwrap_or(0) as usize;
		data.truncate(data.len() - padding_len);
	}
	Ok(())
}

impl MasterKeys {
	pub fn decrypt_meta_into_v1(
		&self,
		meta: &EncryptedString,
		mut out: Vec<u8>,
	) -> Result<String, (ConversionError, Vec<u8>)> {
		let mut errs = Vec::new();
		for key in &self.0 {
			match key.0.decrypt_meta_into_v1(meta, out) {
				Ok(string) => return Ok(string),
				Err((e, out_err)) => {
					errs.push(e);
					out = out_err;
				}
			}
		}
		Err((ConversionError::MultipleErrors(errs), out))
	}
}

impl V2Key {
	pub(crate) fn decrypt_meta_into_v1(
		&self,
		meta: &EncryptedString,
		out: Vec<u8>,
	) -> Result<String, (ConversionError, Vec<u8>)> {
		decrypt_meta(self.as_ref().as_bytes(), meta, out)
	}
}

#[derive(Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
#[rkyv_self]
pub struct FileKey {
	key: SizedStr<U32>,
}

// Redacting Debug: the raw key must never reach logs. Mirrors
// V2Key's hashed Debug rather than the derived one that prints SizedStr's bytes.
impl std::fmt::Debug for FileKey {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		let hashed = SizedHexString::<U64>::from(<[u8; 64]>::from(sha2::Sha512::digest(
			self.key.as_bytes(),
		)))
		.to_str();
		f.debug_struct("FileKey")
			.field("key (hashed)", &hashed)
			.finish()
	}
}

impl AsRef<str> for FileKey {
	fn as_ref(&self) -> &str {
		self.key.as_ref()
	}
}

impl core::fmt::Display for FileKey {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "{}", self.as_ref())
	}
}

impl FromStr for FileKey {
	type Err = ConversionError;

	fn from_str(key: &str) -> Result<Self, Self::Err> {
		Ok(Self {
			key: *SizedStr::ref_from_str(key)?,
		})
	}
}

impl DataCrypter for FileKey {
	fn blocking_encrypt_data(&self, _data: &mut Vec<u8>) -> Result<(), ConversionError> {
		// this is intentional, do not write code for this, this does not need to be supported
		unimplemented!("Data encryption for V1 is not supported");
	}

	fn blocking_decrypt_data(&self, data: &mut Vec<u8>) -> Result<(), ConversionError> {
		decrypt_data(self.key.as_bytes(), data)
	}
}

impl MetaCrypter for FileKey {
	fn blocking_encrypt_meta_into(&self, _meta: &str, _out: String) -> EncryptedString<'static> {
		unimplemented!("Meta encryption for V1 is not supported");
	}

	fn blocking_decrypt_meta_into(
		&self,
		meta: &EncryptedString,
		out: Vec<u8>,
	) -> Result<String, (ConversionError, Vec<u8>)> {
		decrypt_meta(self.key.as_bytes(), meta, out)
	}
}

fn hash_password(password: &[u8]) -> DerivedPassword<'static> {
	let mut out = vec![0u8; 256];

	// SAFETY: The output buffer is guaranteed to be large enough for all hashes
	// since the size of the output is fixed and we allocate 256 bytes.
	// additionally, since the output is hex-encoded, it will always be valid UTF-8.
	let pass = unsafe {
		let sha1 = hex::encode_to_str(Sha1::digest(password), &mut out[0..40]).unwrap_unchecked();
		let sha256 = hex::encode_to_str(Sha256::digest(sha1), &mut out[0..64]).unwrap_unchecked();
		let sha384 = hex::encode_to_str(Sha384::digest(sha256), &mut out[0..96]).unwrap_unchecked();
		hex::encode_to_str(Sha512::digest(sha384), &mut out[0..128]).unwrap_unchecked();

		let md2 = hex::encode_to_str(Md2::digest(password), &mut out[128..160]).unwrap_unchecked();
		let md4 = hex::encode_to_str(Md4::digest(md2), &mut out[128..160]).unwrap_unchecked();
		let md5 = hex::encode_to_str(Md5::digest(md4), &mut out[128..160]).unwrap_unchecked();
		hex::encode_to_str(Sha512::digest(md5), &mut out[128..256]).unwrap_unchecked();
		String::from_utf8_unchecked(out)
	};

	DerivedPassword(Cow::Owned(pass))
}

pub fn derive_password_and_mk(
	password: &[u8],
) -> Result<(MasterKey, DerivedPassword<'static>), ConversionError> {
	let master_key_str = SizedHexString::from(super::v2::hash(password)).to_str();

	Ok((
		MasterKey::from_str(&master_key_str)?,
		hash_password(password),
	))
}

#[cfg(test)]
mod tests {
	use super::*;

	const LEGACY_KEY: &str = "0123456789abcdefghijklmnopqrstuv";
	const LEGACY_PLAINTEXT: &[u8] =
		b"Filen v1 legacy data decryption compat test vector 0123456789";

	fn decrypt_legacy(mut data: Vec<u8>) -> Result<Vec<u8>, ConversionError> {
		FileKey::from_str(LEGACY_KEY)
			.unwrap()
			.blocking_decrypt_data(&mut data)?;
		Ok(data)
	}

	// Raw binary AES-256-CBC with the file key used directly as key and IV = key[0..16],
	// no marker. The "v1.5" upload scheme; stored as version 1 (integer version column).
	#[test]
	fn test_decrypt_data_normal_cbc() {
		let data = hex::decode(
			"e7c2d9faf2f03f148a5d47721e90e9ae61cc933a47bb9ef856e48a52664146be12abb5495b649e22637f39a7b1af4b3a4a157252091eea2c47e0a770d437b7da",
		)
		.unwrap();
		assert_eq!(decrypt_legacy(data).unwrap(), LEGACY_PLAINTEXT);
	}

	// OpenSSL EVP layout: "Salted__" + 8-byte salt + AES-256-CBC body,
	// key+IV derived via EVP_BytesToKey (MD5, one iteration).
	#[test]
	fn test_decrypt_data_openssl_salted() {
		let data = hex::decode(
			"53616c7465645f5f010203040506070828c247abdf130c4127c95d6c2d23d23e177aa05b0b8382e3c9775f5a579fe993f9e24b0f2226e2b4c934f1a0d2522edfa371dfb993376771447f2c29db23290b",
		)
		.unwrap();
		assert_eq!(decrypt_legacy(data).unwrap(), LEGACY_PLAINTEXT);
	}

	// The same OpenSSL EVP layout uploaded as base64 text (CryptoJS .toString() output).
	#[test]
	fn test_decrypt_data_openssl_salted_base64() {
		let data =
			b"U2FsdGVkX18BAgMEBQYHCCjCR6vfEwxBJ8ldbC0j0j4XeqBbC4OC48l3X1pXn+mT+eJLDyIm4rTJNPGg0lIu36Nx37mTN2dxRH8sKdsjKQs=".to_vec();
		assert_eq!(decrypt_legacy(data).unwrap(), LEGACY_PLAINTEXT);
	}

	#[test]
	fn test_hash_password() {
		assert_eq!(
			"7465e95234c0f7fed7608be0039f95b3570dc56cdd825ea61bc103c35828e054e2c063ab054b3341d11efd171c68d58971f34aa630387b50c2ad2cbcdd226dbcd42138444bf07a71f21e00a72a3cf09d3f80855d3fdf447765cd31df70d3bb6a7e2c680359d0ca717681a809129f936c411b88ae114fefe86d39678bb7376e91",
			hash_password(b"password123").0
		);
	}

	#[test]
	fn test_evp_bytes_to_key() {
		let out = &mut [0u8; 48];
		let (key, iv) = evp_bytes_to_key(b"password123", b"salt1234", 16, out);
		assert_eq!(
			hex::encode(key),
			"989181c1bf686a99c71c6f61d905f649dcc916e96ed05a9c7c67828a0ceda50f"
		);
		assert_eq!(hex::encode(iv), "cc43215aabc1e94258b228c01401d0d0");

		let out = &mut [0u8; 47];
		let (key, iv) = evp_bytes_to_key(b"password123", b"salt1234", 16, out);
		assert_eq!(
			hex::encode(key),
			"989181c1bf686a99c71c6f61d905f649dcc916e96ed05a9c7c67828a0ceda5"
		);
		assert_eq!(hex::encode(iv), "0fcc43215aabc1e94258b228c01401d0");

		let out = &mut [0u8; 49];
		let (key, iv) = evp_bytes_to_key(b"password123", b"salt1234", 16, out);
		assert_eq!(
			hex::encode(key),
			"989181c1bf686a99c71c6f61d905f649dcc916e96ed05a9c7c67828a0ceda50fcc"
		);
		assert_eq!(hex::encode(iv), "43215aabc1e94258b228c01401d0d098");
	}
}
