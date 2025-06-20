use std::fmt::{Debug, Display};

use serde::{Deserialize, Serialize};
use serde_repr::{Deserialize_repr, Serialize_repr};

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
pub struct APIKey(pub String);

impl Display for APIKey {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "{}", self.0)
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize_repr, Deserialize_repr)]
#[repr(u8)]
pub enum AuthVersion {
	V1 = 1,
	V2 = 2,
	V3 = 3,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize_repr, Deserialize_repr)]
#[repr(u8)]
pub enum FileEncryptionVersion {
	V1 = 1,
	V2 = 2,
	V3 = 3,
}

impl From<u8> for FileEncryptionVersion {
	fn from(value: u8) -> Self {
		match value {
			1 => FileEncryptionVersion::V1,
			2 => FileEncryptionVersion::V2,
			3 => FileEncryptionVersion::V3,
			o => panic!("Invalid FileEncryptionVersion value {}", o),
		}
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize_repr, Deserialize_repr)]
#[repr(u8)]
pub enum MetaEncryptionVersion {
	V1 = 1,
	V2 = 2,
	V3 = 3,
}
