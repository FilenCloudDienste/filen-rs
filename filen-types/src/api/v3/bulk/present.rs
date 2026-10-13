use serde::{Deserialize, Serialize};

use crate::{
	api::v3::bulk,
	fs::{StableUuid, Uuid},
};

pub const ENDPOINT: &str = "v3/bulk/present";

#[derive(Serialize, Debug, Clone)]
pub struct Item {
	pub uuid: Uuid,
}

pub type Request<'a> = bulk::Request<'a, bulk::FileOrDir<Item, Item>>;

pub type Response<'a> = bulk::Response<'a, Data>;

/// What `/v3/file/present` or `/v3/dir/present` found for the uuid. Trashed items, archived
/// versions and unfinished uploads count as present.
#[derive(Deserialize, Debug, Clone)]
#[serde(try_from = "RawData")]
pub enum Data {
	Missing,
	File {
		/// The uuid is an archived version; `stable_uuid` is then its current head's.
		versioned: bool,
		trash: bool,
		/// The uuid as sent.
		uuid: Uuid,
		stable_uuid: StableUuid,
	},
	Dir {
		trash: bool,
	},
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawData {
	present: bool,
	versioned: Option<bool>,
	trash: Option<bool>,
	uuid: Option<Uuid>,
	#[serde(rename = "stableUUID")]
	stable_uuid: Option<StableUuid>,
}

impl TryFrom<RawData> for Data {
	type Error = &'static str;

	fn try_from(raw: RawData) -> Result<Self, Self::Error> {
		match raw {
			RawData { present: false, .. } => Ok(Self::Missing),
			RawData {
				versioned: Some(versioned),
				trash: Some(trash),
				uuid: Some(uuid),
				stable_uuid: Some(stable_uuid),
				..
			} => Ok(Self::File {
				versioned,
				trash,
				uuid,
				stable_uuid,
			}),
			RawData {
				versioned: None,
				trash: Some(trash),
				uuid: None,
				stable_uuid: None,
				..
			} => Ok(Self::Dir { trash }),
			_ => Err("present data matches neither the file nor the folder shape"),
		}
	}
}
