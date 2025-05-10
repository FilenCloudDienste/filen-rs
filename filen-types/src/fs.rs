use serde::{Deserialize, Serialize};

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
