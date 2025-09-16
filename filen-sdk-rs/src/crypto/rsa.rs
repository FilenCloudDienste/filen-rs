use std::borrow::Cow;

use base64::{Engine, prelude::BASE64_STANDARD};
use digest::Digest;
use filen_types::crypto::{
	Sha256Hash,
	rsa::{EncryptedPrivateKey, RSAEncryptedString},
};
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use rsa::{Oaep, RsaPrivateKey, RsaPublicKey, pkcs8::DecodePrivateKey, traits::PrivateKeyParts};
use sha2::{Sha256, Sha512};

use super::{error::ConversionError, shared::MetaCrypter};

const INFO: &[u8] = b"hmac-sha256-key";

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct HMACKey([u8; 32]);

impl std::fmt::Debug for HMACKey {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		let hmac_hash_str = faster_hex::hex_string(&sha2::Sha512::digest(self.0));
		write!(f, "HMACKey({hmac_hash_str})")
	}
}

impl HMACKey {
	pub(crate) fn new(key: &RsaPrivateKey) -> Self {
		let mut okm = [0u8; 32];
		let hmac = Hkdf::<Sha256>::new(None, &key.d().to_bytes_be());
		hmac.expand(INFO, okm.as_mut())
			.expect("Failed to expand HMAC key");
		Self(okm)
	}
}

impl HMACKey {
	pub(crate) fn hash(&self, data: &[u8]) -> Sha256Hash {
		let mut hmac = Hmac::<Sha256>::new_from_slice(&self.0).expect("HMAC key should be valid");
		hmac.update(data);
		hmac.finalize().into_bytes().into()
	}

	pub(crate) fn hash_to_string(&self, data: &[u8]) -> String {
		faster_hex::hex_string(self.hash(data).as_ref())
	}
}

pub(crate) fn get_key_pair(
	public_key: RsaPublicKey,
	private_key: &EncryptedPrivateKey,
	meta_crypter: &impl MetaCrypter,
) -> Result<(RsaPrivateKey, RsaPublicKey, HMACKey), ConversionError> {
	let private_key_str = meta_crypter.decrypt_meta(&private_key.0)?;
	let private_key = RsaPrivateKey::from_pkcs8_der(&BASE64_STANDARD.decode(&private_key_str)?)?;

	if *private_key.as_ref() != public_key {
		return Err(ConversionError::InvalidKeyPair);
	}

	let hmac = HMACKey::new(&private_key);
	Ok((private_key, public_key, hmac))
}

pub(crate) fn encrypt_with_public_key(
	public_key: &RsaPublicKey,
	data: &[u8],
) -> Result<RSAEncryptedString<'static>, rsa::Error> {
	let mut rng = old_rng::thread_rng();
	let encrypted_data = public_key.encrypt(&mut rng, Oaep::new::<Sha512>(), data.as_ref())?;

	Ok(RSAEncryptedString(Cow::Owned(
		BASE64_STANDARD.encode(encrypted_data),
	)))
}

pub fn decrypt_with_private_key(
	private_key: &RsaPrivateKey,
	data: &RSAEncryptedString,
) -> Result<Vec<u8>, ConversionError> {
	let encrypted_data = BASE64_STANDARD.decode(data.0.as_ref())?;
	let decrypted_data = private_key.decrypt(Oaep::new::<Sha512>(), &encrypted_data)?;

	Ok(decrypted_data)
}
