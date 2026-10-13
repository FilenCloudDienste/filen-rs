use std::borrow::Cow;

use serde::Serialize;

use crate::{
	api::v3::{bulk, file},
	fs::Uuid,
};

pub const ENDPOINT: &str = "v3/bulk/exists";

/// Every item is looked up in the one `parent`.
#[derive(Serialize, Debug, Clone)]
pub struct Request<'a> {
	pub parent: Uuid,
	pub items: &'a [bulk::FileOrDir<Item<'a>, Item<'a>>],
}

#[derive(Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Item<'a> {
	pub name_hashed: Cow<'a, str>,
}

/// File and dir items answer the shared single-route shape.
pub type Response<'a> = bulk::Response<'a, file::exists::Response>;
