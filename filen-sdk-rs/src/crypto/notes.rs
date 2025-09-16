use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::crypto::{
	shared::{CreateRandom, DataCrypter, MetaCrypter},
	v2::{MasterKey, V2Key},
	v3::EncryptionKey,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NoteKey {
	V2(V2Key),
	V3(EncryptionKey),
}

impl Serialize for NoteKey {
	fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: serde::Serializer,
	{
		match self {
			NoteKey::V2(key) => key.as_ref().serialize(serializer),
			NoteKey::V3(key) => key.serialize(serializer),
		}
	}
}

impl<'de> Deserialize<'de> for NoteKey {
	fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
	where
		D: serde::Deserializer<'de>,
	{
		let key = String::deserialize(deserializer)?;
		match key.len() {
			32 => Ok(NoteKey::V2(
				MasterKey::try_from(key)
					.map_err(serde::de::Error::custom)?
					.0,
			)),
			64 => Ok(NoteKey::V3(
				EncryptionKey::from_str(&key).map_err(serde::de::Error::custom)?,
			)),
			len => Err(serde::de::Error::custom(format!(
				"Invalid key length: {len}"
			))),
		}
	}
}

impl MetaCrypter for NoteKey {
	fn encrypt_meta_into(
		&self,
		meta: &str,
		out: String,
	) -> filen_types::crypto::EncryptedString<'static> {
		match self {
			NoteKey::V2(key) => key.encrypt_meta_into(meta, out),
			NoteKey::V3(key) => key.encrypt_meta_into(meta, out),
		}
	}

	fn decrypt_meta_into(
		&self,
		meta: &filen_types::crypto::EncryptedString<'_>,
		out: Vec<u8>,
	) -> Result<String, (super::error::ConversionError, Vec<u8>)> {
		match self {
			NoteKey::V2(key) => key.decrypt_meta_into(meta, out),
			NoteKey::V3(key) => key.decrypt_meta_into(meta, out),
		}
	}
}

impl DataCrypter for NoteKey {
	fn encrypt_data(&self, data: &mut Vec<u8>) -> Result<(), super::error::ConversionError> {
		match self {
			NoteKey::V2(key) => key.encrypt_data(data),
			NoteKey::V3(key) => key.encrypt_data(data),
		}
	}

	fn decrypt_data(&self, data: &mut Vec<u8>) -> Result<(), super::error::ConversionError> {
		match self {
			NoteKey::V2(key) => key.decrypt_data(data),
			NoteKey::V3(key) => key.decrypt_data(data),
		}
	}
}

impl CreateRandom for NoteKey {
	fn seeded_generate(rng: rand::prelude::ThreadRng) -> Self {
		Self::V2(MasterKey::seeded_generate(rng).0)
	}
}
