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

#[derive(Clone, Copy, PartialEq, Eq, Serialize_repr, Deserialize_repr)]
#[repr(u8)]
pub enum AuthVersion {
	V1 = 1,
	V2 = 2,
	V3 = 3,
}

impl Debug for AuthVersion {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "AuthVersion({})", serde_json::to_string(self).unwrap())
	}
}
