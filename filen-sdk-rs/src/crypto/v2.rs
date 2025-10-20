use std::{borrow::Cow, str::FromStr};

use aes_gcm::{
	AesGcm, Nonce,
	aead::{Aead, AeadInPlace},
	aes::Aes256,
};
use base64::{Engine, prelude::BASE64_STANDARD};
use filen_types::crypto::{DerivedPassword, EncryptedMasterKeys, EncryptedString};
use pbkdf2::{hmac::Hmac, pbkdf2};
use rand::distr::Distribution;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha512};

use crate::crypto::shared::{NONCE_SIZE, NonceSize, TAG_SIZE, TagSize};

use super::{
	error::ConversionError,
	shared::{CreateRandom, DataCrypter, MetaCrypter},
};

const NONCE_VALUES: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";

struct BadNonce([u8; NONCE_SIZE]);

impl CreateRandom for BadNonce {
	fn seeded_generate(rng: &mut rand::prelude::ThreadRng) -> Self {
		let mut nonce = [0u8; NONCE_SIZE];
		let sampler =
			rand::distr::Uniform::new(0, NONCE_VALUES.len()).expect("Uniform should be valid");
		for byte in nonce.iter_mut() {
			*byte = NONCE_VALUES[sampler.sample(rng)];
		}
		Self(nonce)
	}
}

impl From<BadNonce> for Nonce<NonceSize> {
	fn from(val: BadNonce) -> Self {
		val.0.into()
	}
}

impl AsRef<str> for BadNonce {
	fn as_ref(&self) -> &str {
		// SAFETY: The nonce is generated from a fixed set of valid chars
		unsafe { std::str::from_utf8_unchecked(&self.0) }
	}
}

#[derive(Clone)]
pub struct V2Key {
	key: String,
	cipher: Box<AesGcm<Aes256, NonceSize, TagSize>>,
}

impl PartialEq for V2Key {
	fn eq(&self, other: &Self) -> bool {
		self.key == other.key
	}
}
impl Eq for V2Key {}

impl AsRef<str> for V2Key {
	fn as_ref(&self) -> &str {
		&self.key
	}
}

impl V2Key {
	fn decrypt_meta_into_v2(
		&self,
		meta: &EncryptedString,
		mut out: Vec<u8>,
	) -> Result<String, (ConversionError, Vec<u8>)> {
		let meta = &meta.0;
		let nonce = &meta[3..NONCE_SIZE + 3];
		let nonce = Nonce::from_slice(nonce.as_bytes());
		out.clear();
		if let Err(e) = BASE64_STANDARD.decode_vec(&meta[NONCE_SIZE + 3..], &mut out) {
			return Err((e.into(), out));
		}
		if let Err(e) = self.cipher.decrypt_in_place(nonce, &[], &mut out) {
			return Err((e.into(), out));
		}

		String::from_utf8(out).map_err(|e| {
			let err = e.utf8_error();
			let out = e.into_bytes();
			(err.into(), out)
		})
	}
}

impl MetaCrypter for V2Key {
	fn blocking_encrypt_meta_into(&self, meta: &str, mut out: String) -> EncryptedString<'static> {
		let nonce = BadNonce::generate();
		out.clear();
		let base64_len =
			base64::encoded_len(meta.len() + TAG_SIZE, true).expect("meta len too long for base64");
		out.reserve(3 + NONCE_SIZE + base64_len);
		out.push_str("002");
		out.push_str(nonce.as_ref());

		// not allocating here is very difficult, so we don't bother
		// the problem is that if we owned the meta String, we could consume it,
		// but it would almost certainly require a reallocation
		// because we would need to extend the buffer to fit the authentication tag
		// before base64 encoding

		// SAFETY: This cannot fail unless we encrypt more than 64GiB of metadata at a time, which we will never do
		// we also don't have AAD
		let encrypted = self.cipher.encrypt(&nonce.into(), meta.as_bytes()).unwrap();

		BASE64_STANDARD.encode_string(encrypted, &mut out);
		EncryptedString(Cow::Owned(out))
	}

	fn blocking_decrypt_meta_into(
		&self,
		meta: &EncryptedString<'_>,
		out: Vec<u8>,
	) -> Result<String, (ConversionError, Vec<u8>)> {
		if meta.0.len() < NONCE_SIZE + 3 {
			return Err((
				ConversionError::InvalidStringLength(meta.0.len(), NONCE_SIZE + 3),
				out,
			));
		}

		let v1_tag = &meta.0[0..8];
		if v1_tag == "U2FsdGVk" {
			return self.decrypt_meta_into_v1(meta, out);
		}

		let tag = &meta.0[0..3];
		if tag == "002" {
			return self.decrypt_meta_into_v2(meta, out);
		}
		Err((
			ConversionError::InvalidVersion(tag.to_string(), vec!["002".to_string()]),
			out,
		))
	}
}

impl DataCrypter for V2Key {
	fn blocking_encrypt_data(&self, data: &mut Vec<u8>) -> Result<(), ConversionError> {
		super::shared::encrypt_data(&self.cipher, data)
	}

	fn blocking_decrypt_data(&self, data: &mut Vec<u8>) -> Result<(), ConversionError> {
		super::shared::decrypt_data(&self.cipher, data)
	}
}

impl std::fmt::Debug for V2Key {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		let hash_key_str =
			faster_hex::hex_string(sha2::Sha512::digest(self.key.as_bytes()).as_ref());
		f.debug_struct("V2Key")
			.field("key (hashed)", &hash_key_str)
			.finish()
	}
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MasterKey(pub(crate) V2Key);

impl Serialize for MasterKey {
	fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: serde::Serializer,
	{
		serializer.serialize_str(&self.0.key)
	}
}

impl<'de> Deserialize<'de> for MasterKey {
	fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
	where
		D: serde::Deserializer<'de>,
	{
		let key = String::deserialize(deserializer)?;
		MasterKey::try_from(key).map_err(serde::de::Error::custom)
	}
}

impl FromStr for MasterKey {
	type Err = <MasterKey as TryFrom<String>>::Error;
	fn from_str(key: &str) -> Result<Self, Self::Err> {
		Self::try_from(key.to_string())
	}
}

impl AsRef<str> for MasterKey {
	fn as_ref(&self) -> &str {
		self.0.as_ref()
	}
}

impl MetaCrypter for MasterKey {
	fn blocking_encrypt_meta_into(&self, meta: &str, out: String) -> EncryptedString<'static> {
		self.0.blocking_encrypt_meta_into(meta, out)
	}

	fn blocking_decrypt_meta_into(
		&self,
		meta: &EncryptedString<'_>,
		out: Vec<u8>,
	) -> Result<String, (ConversionError, Vec<u8>)> {
		self.0.blocking_decrypt_meta_into(meta, out)
	}
}

impl CreateRandom for MasterKey {
	fn seeded_generate(rng: &mut rand::prelude::ThreadRng) -> Self {
		Self::try_from(super::shared::generate_random_base64_values(32, rng))
			.expect("Failed to generate Master Key key")
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MasterKeys(pub Vec<MasterKey>);

impl MasterKeys {
	pub async fn new(
		encrypted: EncryptedMasterKeys<'_>,
		key: MasterKey,
	) -> Result<Self, ConversionError> {
		let key_str = key.decrypt_meta(&encrypted.0).await?;
		let mut keys = Self::from_decrypted_string(&key_str)?;
		keys.0.retain(|v| *v != key);
		keys.0.insert(0, key);

		Ok(keys)
	}

	pub fn new_from_key(key: MasterKey) -> Self {
		Self(vec![key])
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

	pub async fn to_encrypted(&self) -> EncryptedMasterKeys<'static> {
		let decrypted = self.to_decrypted_string();
		EncryptedMasterKeys(self.0[0].encrypt_meta(&decrypted).await)
	}
}

impl MetaCrypter for MasterKeys {
	fn blocking_encrypt_meta_into(&self, meta: &str, out: String) -> EncryptedString<'static> {
		self.0[0].blocking_encrypt_meta_into(meta, out)
	}

	fn blocking_decrypt_meta_into(
		&self,
		meta: &EncryptedString<'_>,
		mut out: Vec<u8>,
	) -> Result<String, (ConversionError, Vec<u8>)> {
		let mut errs = Vec::new();
		for key in &self.0 {
			match key.blocking_decrypt_meta_into(meta, out) {
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

impl TryFrom<String> for MasterKey {
	type Error = ConversionError;
	fn try_from(key: String) -> Result<Self, Self::Error> {
		let mut derived_key = [0u8; 32];
		pbkdf2::pbkdf2::<Hmac<Sha512>>(key.as_bytes(), key.as_bytes(), 1, &mut derived_key)?;

		let cipher = <AesGcm<Aes256, NonceSize> as digest::KeyInit>::new(&derived_key.into());
		Ok(Self(V2Key {
			key,
			cipher: Box::new(cipher),
		}))
	}
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileKey(pub(crate) V2Key);

impl TryFrom<String> for FileKey {
	type Error = ConversionError;
	fn try_from(key: String) -> Result<Self, Self::Error> {
		if key.len() != 32 {
			return Err(ConversionError::InvalidStringLength(key.len(), 32));
		}
		let cipher = <AesGcm<Aes256, NonceSize> as aes_gcm::KeyInit>::new(key.as_bytes().into());
		Ok(Self(V2Key {
			key,
			cipher: Box::new(cipher),
		}))
	}
}

impl AsRef<str> for FileKey {
	fn as_ref(&self) -> &str {
		self.0.as_ref()
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
		FileKey::try_from(key).map_err(serde::de::Error::custom)
	}
}

impl DataCrypter for FileKey {
	fn blocking_encrypt_data(&self, data: &mut Vec<u8>) -> Result<(), ConversionError> {
		self.0.blocking_encrypt_data(data)
	}

	fn blocking_decrypt_data(&self, data: &mut Vec<u8>) -> Result<(), ConversionError> {
		self.0.blocking_decrypt_data(data)
	}
}

impl CreateRandom for FileKey {
	fn seeded_generate(rng: &mut rand::prelude::ThreadRng) -> Self {
		Self::try_from(super::shared::generate_random_base64_values(32, rng))
			.expect("Failed to generate V2 key")
	}
}
pub(crate) fn hash(name: &[u8]) -> [u8; 20] {
	let mut temp = [0u8; 128];
	// SAFETY: The length of hashed_named must be 2x the length of a Sha512 hash, which is 128 bytes
	let sha2 = unsafe {
		faster_hex::hex_encode(&sha2::Sha512::digest(name), &mut temp).unwrap_unchecked()
	};
	sha1::Sha1::digest(sha2).into()
}

pub(crate) fn derive_password(password: &[u8], salt: &[u8]) -> Result<[u8; 64], ConversionError> {
	let mut derived_data = [0u8; 64];
	pbkdf2::<Hmac<Sha512>>(password, salt, 200_000, &mut derived_data)?;
	Ok(derived_data)
}

pub(crate) fn derive_password_and_mk(
	password: &[u8],
	salt: &[u8],
) -> Result<(MasterKey, DerivedPassword<'static>), ConversionError> {
	let derived_data = derive_password(password, salt)?;
	let derived_str = faster_hex::hex_string(&derived_data);
	let (master_key_str, derived_password_str) = derived_str.split_at(64);

	let master_key = MasterKey::from_str(master_key_str)?;

	let mut hasher = Sha512::new();
	hasher.update(derived_password_str);
	let derived_password = DerivedPassword(Cow::Owned(faster_hex::hex_string(&hasher.finalize())));

	Ok((master_key, derived_password))
}
