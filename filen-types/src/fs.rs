use std::str::FromStr;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::ConversionError;

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectType {
	#[serde(rename = "file")]
	File,
	#[serde(rename = "folder")]
	Dir,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectType2 {
	#[serde(rename = "file")]
	File,
	#[serde(rename = "directory")]
	Dir,
}

impl From<ObjectType> for ObjectType2 {
	fn from(object_type: ObjectType) -> Self {
		match object_type {
			ObjectType::File => ObjectType2::File,
			ObjectType::Dir => ObjectType2::Dir,
		}
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParentUuid {
	Uuid(Uuid),
	Trash,
	Recents,
	Favorites,
	Links,
}

impl Default for ParentUuid {
	fn default() -> Self {
		ParentUuid::Uuid(Uuid::nil())
	}
}

impl From<Uuid> for ParentUuid {
	fn from(uuid: Uuid) -> Self {
		ParentUuid::Uuid(uuid)
	}
}

impl PartialEq<Uuid> for ParentUuid {
	fn eq(&self, other: &Uuid) -> bool {
		match self {
			ParentUuid::Uuid(uuid) => uuid == other,
			_ => false,
		}
	}
}

impl TryFrom<ParentUuid> for Uuid {
	type Error = ConversionError;

	fn try_from(value: ParentUuid) -> Result<Self, Self::Error> {
		match value {
			ParentUuid::Uuid(uuid) => Ok(uuid),
			other => Err(ConversionError::ParentUuidError(format!("{:?}", other))),
		}
	}
}

impl std::fmt::Display for ParentUuid {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			ParentUuid::Uuid(uuid) => write!(f, "{}", uuid),
			ParentUuid::Trash => write!(f, "trash"),
			ParentUuid::Recents => write!(f, "recents"),
			ParentUuid::Favorites => write!(f, "favorites"),
			ParentUuid::Links => write!(f, "links"),
		}
	}
}

impl FromStr for ParentUuid {
	type Err = ConversionError;

	fn from_str(s: &str) -> Result<Self, Self::Err> {
		match s {
			"trash" => Ok(ParentUuid::Trash),
			"recents" => Ok(ParentUuid::Recents),
			"favorites" => Ok(ParentUuid::Favorites),
			"links" => Ok(ParentUuid::Links),
			_ => {
				Ok(ParentUuid::Uuid(Uuid::parse_str(s).map_err(|_| {
					ConversionError::ParentUuidError(s.to_string())
				})?))
			}
		}
	}
}

impl Serialize for ParentUuid {
	fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: serde::Serializer,
	{
		serializer.serialize_str(&self.to_string())
	}
}

impl<'de> Deserialize<'de> for ParentUuid {
	fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
	where
		D: serde::Deserializer<'de>,
	{
		let s = <&str>::deserialize(deserializer)?;
		ParentUuid::from_str(s).map_err(serde::de::Error::custom)
	}
}

#[cfg(feature = "rusqlite")]
mod sqlite {
	use std::str::FromStr;

	use super::ParentUuid;
	use rusqlite::{
		Error, ToSql,
		types::{FromSql, FromSqlError, FromSqlResult, ToSqlOutput, ValueRef},
	};
	use uuid::Uuid;

	impl ToSql for ParentUuid {
		fn to_sql(&self) -> Result<ToSqlOutput<'_>, Error> {
			match self {
				ParentUuid::Uuid(uuid) => uuid.to_sql(),
				_ => Ok(ToSqlOutput::Owned(self.to_string().into())),
			}
		}
	}

	impl FromSql for ParentUuid {
		fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
			match value {
				ValueRef::Text(s) => {
					if s.len() == 36 {
						// If the string is exactly 36 characters, it is likely a UUID
						Uuid::column_result(ValueRef::Text(s)).map(ParentUuid::Uuid)
					} else {
						// Otherwise, treat it as a special parent type
						match std::str::from_utf8(s) {
							Ok(s) => ParentUuid::from_str(s)
								.map_err(|e| FromSqlError::Other(Box::new(e))),
							Err(e) => Err(FromSqlError::Other(Box::new(e))),
						}
					}
				}
				ValueRef::Blob(b) => {
					if b.len() == 16 {
						// If the blob is exactly 16 bytes, it is likely a UUID
						Uuid::column_result(ValueRef::Blob(b)).map(ParentUuid::Uuid)
					} else {
						// Otherwise, treat it as a special parent type
						match std::str::from_utf8(b) {
							Ok(s) => ParentUuid::from_str(s)
								.map_err(|e| FromSqlError::Other(Box::new(e))),
							Err(e) => Err(FromSqlError::Other(Box::new(e))),
						}
					}
				}
				_ => Err(FromSqlError::InvalidType),
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn test_parent_uuid_stringification() {
		let uuid = Uuid::new_v4();
		let parent_uuid = ParentUuid::Uuid(uuid);
		assert_eq!(parent_uuid.to_string(), uuid.to_string());
		assert_eq!(
			ParentUuid::from_str(&parent_uuid.to_string()).unwrap(),
			parent_uuid
		);
	}
}
