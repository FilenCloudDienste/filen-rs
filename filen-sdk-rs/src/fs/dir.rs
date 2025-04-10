use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{FSObject, Metadata, NonRootFSObject};

pub(crate) struct RootDirectory {
	uuid: Uuid,
}

impl RootDirectory {
	pub(crate) fn new(uuid: Uuid) -> Self {
		Self { uuid }
	}
}

impl FSObject for RootDirectory {
	fn name(&self) -> &str {
		""
	}

	fn uuid(&self) -> &uuid::Uuid {
		&self.uuid
	}
}

pub(crate) struct Directory {
	uuid: Uuid,
	name: String,
	parent: Uuid,

	color: String, // todo use Color struct
	created: DateTime<Utc>,
	favorited: bool,
}

impl FSObject for Directory {
	fn name(&self) -> &str {
		&self.name
	}

	fn uuid(&self) -> &uuid::Uuid {
		&self.uuid
	}
}

impl NonRootFSObject for Directory {
	fn parent(&self) -> &uuid::Uuid {
		&self.parent
	}

	fn get_meta(&self) -> impl super::Metadata<'_> {
		DirectoryMeta {
			name: &self.name,
			created: self.created,
		}
	}
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct DirectoryMeta<'a> {
	name: &'a str,
	#[serde(rename = "creation")]
	created: DateTime<Utc>,
}

impl Metadata<'_> for DirectoryMeta<'_> {
	fn make_string(&self) -> String {
		serde_json::to_string(self).unwrap()
	}
}
