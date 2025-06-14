use std::str::FromStr;

use aes_gcm::{
	AesGcm, Nonce,
	aead::{Aead, AeadInPlace},
	aes::Aes256,
};
use base64::{Engine, prelude::BASE64_STANDARD};
use filen_types::crypto::{DerivedPassword, EncryptedMasterKeys, EncryptedString};
use generic_array::typenum::{U12, U16};
use pbkdf2::{hmac::Hmac, pbkdf2};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha512};

use super::{
	error::ConversionError,
	shared::{CreateRandom, DataCrypter, MetaCrypter},
};

type NonceSize = U12;
const NONCE_SIZE: usize = 12;
type TagSize = U16;
const TAG_SIZE: usize = 16;

const NONCE_VALUES: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
pub(crate) fn generate_bad_nonce() -> Nonce<NonceSize> {
	let mut nonce: [u8; 12] = rand::random();
	nonce
		.iter_mut()
		.for_each(|b| *b = NONCE_VALUES[*b as usize % NONCE_VALUES.len()]);
	nonce.into()
}

#[derive(Clone)]
pub struct MasterKey {
	pub key: String,
	pub cipher: Box<AesGcm<Aes256, NonceSize, TagSize>>,
}

impl std::fmt::Debug for MasterKey {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		let hash_key_str =
			faster_hex::hex_string(sha2::Sha512::digest(self.key.as_bytes()).as_ref());
		f.debug_struct("MasterKey")
			.field("key (hashed)", &hash_key_str)
			.finish()
	}
}

impl PartialEq for MasterKey {
	fn eq(&self, other: &Self) -> bool {
		self.key == other.key
	}
}
impl Eq for MasterKey {}

impl FromStr for MasterKey {
	type Err = ConversionError;
	fn from_str(key: &str) -> Result<Self, ConversionError> {
		let mut derived_key = [0u8; 32];
		pbkdf2::pbkdf2::<Hmac<Sha512>>(key.as_bytes(), key.as_bytes(), 1, &mut derived_key)?;

		let cipher = <AesGcm<Aes256, NonceSize> as digest::KeyInit>::new(&derived_key.into());
		Ok(Self {
			key: key.to_string(),
			cipher: Box::new(cipher),
		})
	}
}

impl AsRef<str> for MasterKey {
	fn as_ref(&self) -> &str {
		&self.key
	}
}

impl MetaCrypter for MasterKey {
	fn encrypt_meta_into(
		&self,
		meta: impl AsRef<str>,
		out: &mut EncryptedString,
	) -> Result<(), ConversionError> {
		let meta = meta.as_ref();
		let nonce = generate_bad_nonce();
		let out = &mut out.0;
		out.clear();
		let base64_len =
			base64::encoded_len(meta.len() + TAG_SIZE, true).expect("meta len too long for base64");
		out.reserve(3 + NONCE_SIZE + base64_len);
		out.push_str("002");
		out.push_str(std::str::from_utf8(&nonce)?); // can be changed to unsafe for max perf

		// not allocating here is very difficult, so we don't bother
		// the problem is that if we owned the meta String, we could consume it,
		// but it would almost certainly require a reallocation
		// because we would need to extend the buffer to fit the authentication tag
		// before base64 encoding
		let encrypted = self.cipher.encrypt(&nonce, meta.as_bytes())?;
		BASE64_STANDARD.encode_string(encrypted, out);
		Ok(())
	}

	fn decrypt_meta_into(
		&self,
		meta: &EncryptedString,
		out: &mut String,
	) -> Result<(), ConversionError> {
		let meta = &meta.0;
		if meta.len() < NONCE_SIZE + 3 {
			return Err(ConversionError::InvalidStringLength(
				meta.len(),
				NONCE_SIZE + 3,
			));
		}
		let tag = &meta[0..3];
		if tag != "002" {
			return Err(ConversionError::InvalidVersion(
				tag.to_string(),
				vec!["002".to_string()],
			));
		}
		let nonce = &meta[3..NONCE_SIZE + 3];
		let nonce = Nonce::from_slice(nonce.as_bytes());
		out.clear();
		{
			// SAFETY: we validate the utf8 status of the vec at the end of this block
			let out = unsafe { out.as_mut_vec() };
			BASE64_STANDARD.decode_vec(&meta[NONCE_SIZE + 3..], out)?;

			self.cipher.decrypt_in_place(nonce, &[], out)?;
			if let Err(e) = std::str::from_utf8(out) {
				out.clear();
				return Err(e.into());
			}
		}
		Ok(())
	}
}

impl CreateRandom for MasterKey {
	fn seeded_generate(_rng: rand::prelude::ThreadRng) -> Self {
		Self::from_str(&super::shared::generate_random_base64_values(32))
			.expect("Failed to generate Master Key key")
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MasterKeys(pub Vec<MasterKey>);

impl MasterKeys {
	pub fn new(encrypted: EncryptedMasterKeys, key: MasterKey) -> Result<Self, ConversionError> {
		let key_str = key.decrypt_meta(&encrypted.0)?;
		let mut keys = Self::from_decrypted_string(&key_str)?;
		keys.0.retain(|v| *v != key);
		keys.0.insert(0, key);

		Ok(keys)
	}

	pub fn from_decrypted_string(decrypted: &str) -> Result<Self, ConversionError> {
		let keys = decrypted
			.trim()
			.split('|')
			.map(MasterKey::from_str)
			.collect::<Result<Vec<_>, ConversionError>>()?;
		if keys.is_empty() {
			return Err(ConversionError::InvalidStringLength(decrypted.len(), 1));
		}
		Ok(Self(keys))
	}

	pub fn to_decrypted_string(&self) -> String {
		self.0
			.iter()
			.map(|k| k.as_ref())
			.collect::<Vec<_>>()
			.join("|")
	}
}

impl MetaCrypter for MasterKeys {
	fn encrypt_meta_into(
		&self,
		meta: impl AsRef<str>,
		out: &mut EncryptedString,
	) -> Result<(), ConversionError> {
		self.0[0].encrypt_meta_into(meta, out)
	}

	fn decrypt_meta_into(
		&self,
		meta: &EncryptedString,
		out: &mut String,
	) -> Result<(), ConversionError> {
		let mut errs = Vec::new();
		for key in &self.0 {
			match key.decrypt_meta_into(meta, out) {
				Ok(()) => return Ok(()),
				Err(err) => errs.push(err),
			}
		}
		out.clear();
		Err(ConversionError::MultipleErrors(errs))
	}
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileKey(super::v3::EncryptionKey);

impl FromStr for FileKey {
	type Err = ConversionError;
	fn from_str(key: &str) -> Result<Self, ConversionError> {
		if key.len() != 32 {
			return Err(ConversionError::InvalidStringLength(key.len(), 32));
		}
		let mut bytes = [0u8; 32];
		bytes.copy_from_slice(key.as_bytes());
		let key = super::v3::EncryptionKey::new(bytes);
		Ok(Self(key))
	}
}

impl AsRef<str> for FileKey {
	fn as_ref(&self) -> &str {
		unsafe {
			// SAFETY: The key is guaranteed to be 32 bytes, built from a utf8 string so it can be safely converted to a str.
			std::str::from_utf8_unchecked(&self.0.bytes)
		}
	}
}

impl Serialize for FileKey {
	fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: serde::Serializer,
	{
		serializer.serialize_str(self.as_ref())
	}
}

impl<'de> Deserialize<'de> for FileKey {
	fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
	where
		D: serde::Deserializer<'de>,
	{
		let key = String::deserialize(deserializer)?;
		FileKey::from_str(&key).map_err(serde::de::Error::custom)
	}
}

impl DataCrypter for FileKey {
	fn encrypt_data(&self, data: &mut Vec<u8>) -> Result<(), ConversionError> {
		self.0.encrypt_data(data)
	}

	fn decrypt_data(&self, data: &mut Vec<u8>) -> Result<(), ConversionError> {
		self.0.decrypt_data(data)
	}
}

impl CreateRandom for FileKey {
	fn seeded_generate(_rng: rand::prelude::ThreadRng) -> Self {
		Self::from_str(&super::shared::generate_random_base64_values(32))
			.expect("Failed to generate V2 key")
	}
}

pub(crate) fn hash_to_buffer(name: &[u8]) -> [u8; 20] {
	let mut outer_hasher = sha1::Sha1::new();
	let mut inner_hasher = sha2::Sha512::new();
	inner_hasher.update(name);
	let mut hashed_name = [0u8; 128];
	// SAFETY: The length of hashed_named must be 2x the length of a Sha512 hash, which is 128 bytes
	faster_hex::hex_encode(inner_hasher.finalize().as_slice(), &mut hashed_name).unwrap();
	outer_hasher.update(hashed_name);
	outer_hasher.finalize().into()
}

pub(crate) fn derive_password(password: &[u8], salt: &[u8]) -> Result<[u8; 64], ConversionError> {
	let mut derived_data = [0u8; 64];
	pbkdf2::<Hmac<Sha512>>(password, salt, 200_000, &mut derived_data)?;
	Ok(derived_data)
}

pub fn derive_password_and_mk(
	password: impl AsRef<[u8]>,
	salt: impl AsRef<[u8]>,
) -> Result<(MasterKey, DerivedPassword), ConversionError> {
	let derived_data = derive_password(password.as_ref(), salt.as_ref())?;
	let derived_str = faster_hex::hex_string(&derived_data);
	let (master_key_str, derived_password_str) = derived_str.split_at(64);

	let master_key = MasterKey::from_str(master_key_str)?;

	let mut hasher = Sha512::new();
	hasher.update(derived_password_str);
	let derived_password = DerivedPassword(faster_hex::hex_string(&hasher.finalize()));

	Ok((master_key, derived_password))
}
