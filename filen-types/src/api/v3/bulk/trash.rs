use crate::api::v3::{bulk, dir, file};

pub const ENDPOINT: &str = "v3/bulk/trash";

pub type Item = bulk::FileOrDir<file::trash::Request, dir::trash::Request>;

pub type Request<'a> = bulk::Request<'a, Item>;

pub type Response<'a> = bulk::Response<'a, ()>;
