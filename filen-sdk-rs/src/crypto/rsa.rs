use base64::{Engine, prelude::BASE64_STANDARD};
use filen_types::crypto::{
	Sha256Hash,
	rsa::{EncodedPublicKey, EncryptedPrivateKey, RSAEncryptedString},
};
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use rsa::{Oaep, RsaPrivateKey, RsaPublicKey, pkcs8::DecodePrivateKey, traits::PrivateKeyParts};
use sha1::Sha1;
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

pub fn encrypt_with_public_key(
	public_key: &RsaPublicKey,
	data: impl AsRef<[u8]>,
) -> Result<RSAEncryptedString, rsa::Error> {
	let mut rng = old_rng::thread_rng();
	// this is RSA_PKCS1_OAEP_PADDING according to
	// https://github.com/RustCrypto/RSA/issues/435
	let encrypted_data = public_key.encrypt(
		&mut rng,
		Oaep::new_with_mgf_hash::<Sha256, Sha1>(),
		data.as_ref(),
	)?;

	Ok(RSAEncryptedString(BASE64_STANDARD.encode(encrypted_data)))
}
