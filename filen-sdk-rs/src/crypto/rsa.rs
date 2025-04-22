use base64::{Engine, prelude::BASE64_STANDARD};
use filen_types::crypto::{
	Sha256Hash,
	rsa::{EncodedPublicKey, EncryptedPrivateKey},
};
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use rsa::{RsaPrivateKey, RsaPublicKey, pkcs8::DecodePrivateKey, traits::PrivateKeyParts};
use sha2::Sha256;

use super::{error::ConversionError, shared::MetaCrypter};

const INFO: &[u8] = b"hmac-sha256-key";

#[derive(Clone)]
pub(crate) struct HMACKey([u8; 32]);

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
	pub(crate) fn hash(&self, data: impl AsRef<[u8]>) -> Sha256Hash {
		let mut hmac = Hmac::<Sha256>::new_from_slice(&self.0).expect("HMAC key should be valid");
		hmac.update(data.as_ref());
		hmac.finalize().into_bytes().into()
	}

	pub(crate) fn hash_to_string(&self, data: impl AsRef<[u8]>) -> String {
		faster_hex::hex_string(self.hash(data).as_ref())
	}
}

pub fn get_key_pair(
	public_key: &EncodedPublicKey,
	private_key: &EncryptedPrivateKey,
	meta_crypter: &impl MetaCrypter,
) -> Result<(RsaPrivateKey, RsaPublicKey, HMACKey), ConversionError> {
	let private_key_str = meta_crypter.decrypt_meta(&private_key.0)?;
	let private_key = RsaPrivateKey::from_pkcs8_der(&BASE64_STANDARD.decode(&private_key_str)?)?;
	let public_key = RsaPublicKey::try_from(public_key)?;

	if *private_key.as_ref() != public_key {
		return Err(ConversionError::InvalidKeyPair);
	}

	let hmac = HMACKey::new(&private_key);
	Ok((private_key, public_key, hmac))
}
