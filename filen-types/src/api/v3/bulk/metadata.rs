use crate::api::v3::{bulk, dir, file};

pub const ENDPOINT: &str = "v3/bulk/metadata";

pub type Item<'a> = bulk::FileOrDir<file::metadata::Request<'a>, dir::metadata::Request<'a>>;

pub type Request<'a> = bulk::Request<'a, Item<'a>>;

pub type Response<'a> = bulk::Response<'a, ()>;
