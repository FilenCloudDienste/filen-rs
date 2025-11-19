use std::borrow::Cow;

use base64::{Engine, prelude::BASE64_STANDARD};
use chrono::{DateTime, SubsecRound, Utc};
use filen_macros::CowHelpers;
use filen_types::{
	crypto::{EncryptedString, rsa::RSAEncryptedString},
	traits::CowHelpers,
};
use rsa::RsaPrivateKey;
use serde::{Deserialize, Serialize};

use crate::{
	crypto::{self, shared::MetaCrypter},
	error::{Error, InvalidNameError, MetadataWasNotDecryptedError},
};

#[derive(Debug, PartialEq, Eq, Clone, CowHelpers)]
pub enum DirectoryMeta<'a> {
	Decoded(DecryptedDirectoryMeta<'a>),
	DecryptedRaw(Cow<'a, [u8]>),
	DecryptedUTF8(Cow<'a, str>),
	Encrypted(EncryptedString<'a>),
	RSAEncrypted(RSAEncryptedString<'a>),
}

impl DirectoryMeta<'_> {
	pub fn into_owned(self) -> DirectoryMeta<'static> {
		match self {
			DirectoryMeta::Decoded(meta) => DirectoryMeta::Decoded(meta.into_owned_cow()),
			DirectoryMeta::DecryptedRaw(raw) => {
				DirectoryMeta::DecryptedRaw(Cow::Owned(raw.into_owned()))
			}
			DirectoryMeta::DecryptedUTF8(utf8) => {
				DirectoryMeta::DecryptedUTF8(Cow::Owned(utf8.into_owned()))
			}
			DirectoryMeta::Encrypted(encrypted) => {
				DirectoryMeta::Encrypted(EncryptedString(Cow::Owned(encrypted.0.into_owned())))
			}
			DirectoryMeta::RSAEncrypted(encrypted) => DirectoryMeta::RSAEncrypted(
				RSAEncryptedString(Cow::Owned(encrypted.0.into_owned())),
			),
		}
	}
}

impl<'a> DirectoryMeta<'a> {
	pub(crate) fn blocking_from_encrypted(
		encrypted: EncryptedString<'a>,
		decrypter: &impl MetaCrypter,
	) -> Self {
		let Ok(decrypted) = decrypter.blocking_decrypt_meta(&encrypted) else {
			return Self::Encrypted(encrypted);
		};
		let Ok(meta) = serde_json::from_str(&decrypted) else {
			return Self::DecryptedUTF8(Cow::Owned(decrypted));
		};
		Self::Decoded(meta)
	}

	pub(crate) fn blocking_from_rsa_encrypted(
		encrypted: RSAEncryptedString<'a>,
		decrypter: &RsaPrivateKey,
	) -> Self {
		let Ok(decrypted) = crypto::rsa::blocking_decrypt_with_private_key(decrypter, &encrypted)
		else {
			return Self::RSAEncrypted(encrypted);
		};
		let Ok(meta) = serde_json::from_slice(decrypted.as_ref()) else {
			match String::from_utf8(decrypted) {
				Ok(decrypted) => return Self::DecryptedUTF8(Cow::Owned(decrypted)),
				Err(err) => return Self::DecryptedRaw(Cow::Owned(err.into_bytes())),
			}
		};
		Self::Decoded(meta)
	}
}

impl<'a> DirectoryMeta<'a> {
	pub(crate) fn blocking_encrypt(
		&self,
		encrypter: &impl MetaCrypter,
	) -> Option<EncryptedString<'static>> {
		match self {
			Self::Decoded(meta) => {
				let json = serde_json::to_string(meta).expect("Failed to serialize directory meta");

				Some(encrypter.blocking_encrypt_meta(&json))
			}
			Self::DecryptedRaw(raw) => {
				Some(encrypter.blocking_encrypt_meta(&BASE64_STANDARD.encode(raw)))
			}
			Self::DecryptedUTF8(utf8) => Some(encrypter.blocking_encrypt_meta(utf8)),
			other => {
				log::warn!("Cannot convert {other:?} to encrypted meta");
				None
			}
		}
	}

	pub fn try_to_string(&'a self) -> Option<Cow<'a, str>> {
		match self {
			// SAFETY: serializing a DecryptedDirectoryMeta always succeeds
			// - filen_types::serde::time::optional::serialize cannot fail
			// - serializing a String cannot fail
			// - serde_json::to_string always suceeds if we have string keys and serialization cannot fail
			Self::Decoded(meta) => Some(Cow::Owned(meta.to_json_string())),
			Self::DecryptedUTF8(utf8) => Some(Cow::Borrowed(utf8)),
			Self::DecryptedRaw(_) | Self::Encrypted(_) | Self::RSAEncrypted(_) => None,
		}
	}

	pub fn name(&self) -> Option<&str> {
		match self {
			Self::Decoded(meta) => Some(meta.name()),
			Self::DecryptedRaw(_)
			| Self::DecryptedUTF8(_)
			| Self::Encrypted(_)
			| Self::RSAEncrypted(_) => None,
		}
	}

	pub fn created(&self) -> Option<DateTime<Utc>> {
		match self {
			Self::Decoded(meta) => meta.created(),
			Self::DecryptedRaw(_)
			| Self::DecryptedUTF8(_)
			| Self::Encrypted(_)
			| Self::RSAEncrypted(_) => None,
		}
	}

	pub(crate) fn apply_changes(&mut self, changes: DirectoryMetaChanges) -> Result<(), Error> {
		match self {
			Self::Decoded(meta) => meta.apply_changes(changes),
			Self::DecryptedRaw(_)
			| Self::DecryptedUTF8(_)
			| Self::Encrypted(_)
			| Self::RSAEncrypted(_) => {
				// if all the metadata is being applied, we can convert to Decoded
				*self = Self::Decoded(DecryptedDirectoryMeta {
					name: changes
						.name
						.map(Cow::Owned)
						.ok_or(MetadataWasNotDecryptedError)?,
					created: changes.created.ok_or(MetadataWasNotDecryptedError)?,
				})
			}
		}
		Ok(())
	}

	pub(crate) fn borrow_with_changes(
		&'a self,
		changes: &'a DirectoryMetaChanges,
	) -> Result<Self, Error> {
		Ok(match self {
			Self::Decoded(meta) => Self::Decoded(meta.borrowed_with_changes(changes)),
			Self::DecryptedRaw(_)
			| Self::DecryptedUTF8(_)
			| Self::Encrypted(_)
			| Self::RSAEncrypted(_) => Self::Decoded(DecryptedDirectoryMeta {
				name: changes
					.name
					.as_deref()
					.map(Cow::Borrowed)
					.ok_or(MetadataWasNotDecryptedError)?,
				created: changes.created.ok_or(MetadataWasNotDecryptedError)?,
			}),
		})
	}
}

#[derive(Debug, PartialEq, Eq, Clone, Serialize, Deserialize, CowHelpers)]
pub struct DecryptedDirectoryMeta<'a> {
	pub name: Cow<'a, str>,
	#[serde(
		with = "filen_types::serde::time::optional",
		rename = "creation",
		default
	)]
	pub created: Option<DateTime<Utc>>,
}

impl<'a> DecryptedDirectoryMeta<'a> {
	pub fn name(&self) -> &str {
		&self.name
	}

	pub fn created(&self) -> Option<DateTime<Utc>> {
		self.created
	}

	fn apply_changes(&mut self, changes: DirectoryMetaChanges) {
		if let Some(name) = changes.name {
			// don't need to check for empty name here,
			// because it was already checked in DirectoryMetaChanger::set_name
			self.name = Cow::Owned(name);
		}
		if let Some(created) = changes.created {
			self.created = created;
		}
	}

	pub fn borrowed_with_changes(&'a self, changes: &'a DirectoryMetaChanges) -> Self {
		Self {
			name: Cow::Borrowed(changes.name.as_deref().unwrap_or(&self.name)),
			created: changes.created.unwrap_or(self.created),
		}
	}

	pub(crate) fn to_json_string(&self) -> String {
		// SAFETY: serializing a DecryptedDirectoryMeta always succeeds
		// - filen_types::serde::time::optional::serialize cannot fail
		// - serializing a String cannot fail
		// - serde_json::to_string always suceeds if we have string keys and serialization cannot fail
		serde_json::to_string(self)
			.expect("Failed to serialize directory meta (should be impossible)")
	}
}

#[derive(Debug, PartialEq, Eq, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "wasm-full", derive(tsify::Tsify), tsify(from_wasm_abi))]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct DirectoryMetaChanges {
	#[serde(default)]
	#[cfg_attr(feature = "wasm-full", tsify(type = "string"))]
	#[cfg_attr(feature = "uniffi", uniffi(default = None))]
	name: Option<String>,
	// double option because we need to distinguish between
	// "not set" and "set to None"
	#[cfg_attr(feature = "wasm-full", tsify(type = "bigint | null"))]
	#[serde(
		default,
		deserialize_with = "crate::serde::deserialize_double_option_timestamp"
	)]
	#[cfg_attr(feature = "uniffi", uniffi(default = None))]
	created: Option<Option<DateTime<Utc>>>,
}

impl DirectoryMetaChanges {
	pub fn name(mut self, name: String) -> Result<Self, Error> {
		if name.is_empty() {
			return Err(InvalidNameError(name).into());
		}
		self.name = Some(name);
		Ok(self)
	}

	pub fn created(mut self, created: Option<DateTime<Utc>>) -> Self {
		self.created = Some(created.map(|t| t.round_subsecs(3)));
		self
	}
}
