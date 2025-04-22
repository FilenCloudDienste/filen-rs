use serde::{Deserialize, Serialize};

use crate::crypto::Sha256Hash;

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct Request {
	pub items: Vec<SearchAddItem>,
}

#[derive(Debug, Clone, Copy)]
pub enum SearchAddItemType {
	File,
	Directory,
}

impl Serialize for SearchAddItemType {
	fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: serde::Serializer,
	{
		match self {
			SearchAddItemType::File => serializer.serialize_str("file"),
			SearchAddItemType::Directory => serializer.serialize_str("directory"),
		}
	}
}

impl<'de> Deserialize<'de> for SearchAddItemType {
	fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
	where
		D: serde::Deserializer<'de>,
	{
		let s = String::deserialize(deserializer)?;
		match s.as_str() {
			"file" => Ok(SearchAddItemType::File),
			"directory" => Ok(SearchAddItemType::Directory),
			_ => Err(serde::de::Error::custom(format!("Invalid type: {}", s))),
		}
	}
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct SearchAddItem {
	pub uuid: uuid::Uuid,
	pub hash: Sha256Hash,
	pub r#type: SearchAddItemType,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct Response {
	added: u64,
	skipped: u64,
}
