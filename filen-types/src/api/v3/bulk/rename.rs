use crate::api::v3::{bulk, dir, file};

pub const ENDPOINT: &str = "v3/bulk/rename";

/// The single rename routes take the same fields as the metadata routes (`metadata` on file items
/// only), so their request types are reused.
pub type Item<'a> = bulk::FileOrDir<file::metadata::Request<'a>, dir::metadata::Request<'a>>;

pub type Request<'a> = bulk::Request<'a, Item<'a>>;

pub type Response<'a> = bulk::Response<'a, ()>;
