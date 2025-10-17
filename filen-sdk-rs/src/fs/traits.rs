use std::borrow::Cow;

use filen_types::{
	crypto::{EncryptedString, rsa::RSAEncryptedString},
	fs::{ObjectType, ParentUuid, UuidStr},
};
use rsa::RsaPublicKey;

use crate::{crypto::shared::MetaCrypter, runtime::do_cpu_intensive};

pub trait HasParent {
	fn parent(&self) -> &ParentUuid;
}

pub trait HasRemoteInfo {
	fn favorited(&self) -> bool;
}

pub trait SetRemoteInfo {
	fn set_favorited(&mut self, value: bool);
}

pub trait HasUUID: Send + Sync {
	fn uuid(&self) -> &UuidStr;
}

impl HasUUID for UuidStr {
	fn uuid(&self) -> &UuidStr {
		self
	}
}

pub trait HasType {
	fn object_type(&self) -> ObjectType;
}

pub trait HasName {
	fn name(&self) -> Option<&str>;
}

pub trait HasMeta {
	fn get_meta_string(&self) -> Option<Cow<'_, str>>;
}

pub trait HasMetaExt: Send + Sync {
	fn blocking_get_encrypted_meta(
		&self,
		crypter: &impl MetaCrypter,
	) -> Option<EncryptedString<'static>>;

	fn get_encrypted_meta<'a>(
		&'a self,
		crypter: &'a impl MetaCrypter,
	) -> impl Future<Output = Option<EncryptedString<'static>>> + Send + 'a {
		do_cpu_intensive(|| self.blocking_get_encrypted_meta(crypter))
	}

	fn blocking_get_rsa_encrypted_meta(
		&self,
		public_key: &RsaPublicKey,
	) -> Option<RSAEncryptedString<'static>>;

	fn get_rsa_encrypted_meta<'a>(
		&'a self,
		public_key: &'a RsaPublicKey,
	) -> impl Future<Output = Option<RSAEncryptedString<'static>>> + Send + 'a {
		do_cpu_intensive(|| self.blocking_get_rsa_encrypted_meta(public_key))
	}
}

impl<T> HasMetaExt for T
where
	T: HasMeta + ?Sized + Send + Sync,
{
	fn blocking_get_encrypted_meta(
		&self,
		crypter: &impl MetaCrypter,
	) -> Option<EncryptedString<'static>> {
		Some(crypter.blocking_encrypt_meta(&self.get_meta_string()?))
	}

	fn blocking_get_rsa_encrypted_meta(
		&self,
		public_key: &RsaPublicKey,
	) -> Option<RSAEncryptedString<'static>> {
		let meta = self.get_meta_string()?;
		match crate::crypto::rsa::blocking_encrypt_with_public_key(public_key, meta.as_bytes()) {
			Ok(encrypted) => Some(encrypted),
			Err(_) => {
				log::error!(
					"Failed to encrypt metadata with RSA public key metadata len: {}",
					meta.len()
				);
				None
			}
		}
	}
}
