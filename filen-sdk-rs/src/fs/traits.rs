use filen_types::{
	crypto::{EncryptedString, rsa::RSAEncryptedString},
	fs::{ObjectType, ParentUuid, UuidStr},
};
use rsa::RsaPublicKey;

use crate::crypto::{error::ConversionError, shared::MetaCrypter};

pub trait HasParent {
	fn parent(&self) -> ParentUuid;
}

pub trait HasRemoteInfo {
	fn favorited(&self) -> bool;
}

pub trait SetRemoteInfo {
	fn set_favorited(&mut self, value: bool);
}

pub trait HasUUID: Send + Sync {
	fn uuid(&self) -> UuidStr;
}

impl HasUUID for UuidStr {
	fn uuid(&self) -> UuidStr {
		*self
	}
}

pub trait HasType {
	fn object_type(&self) -> ObjectType;
}

pub trait HasName {
	fn name(&self) -> &str;
}

pub trait HasMeta {
	fn get_meta_string(&self) -> String;
}

pub trait HasMetaExt {
	fn get_encrypted_meta(
		&self,
		crypter: &impl MetaCrypter,
	) -> Result<EncryptedString, ConversionError>;
	fn get_rsa_encrypted_meta(
		&self,
		public_key: &RsaPublicKey,
	) -> Result<RSAEncryptedString, rsa::Error>;
}

impl<T> HasMetaExt for T
where
	T: HasMeta + ?Sized,
{
	fn get_encrypted_meta(
		&self,
		crypter: &impl MetaCrypter,
	) -> Result<EncryptedString, ConversionError> {
		crypter.encrypt_meta(&self.get_meta_string())
	}
	fn get_rsa_encrypted_meta(
		&self,
		public_key: &RsaPublicKey,
	) -> Result<RSAEncryptedString, rsa::Error> {
		let meta = self.get_meta_string();
		crate::crypto::rsa::encrypt_with_public_key(public_key, meta.as_bytes())
	}
}
