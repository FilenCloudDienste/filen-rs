use crate::api::v3::{bulk, dir, file};

pub const ENDPOINT: &str = "v3/bulk/restore";

pub type Item = bulk::FileOrDir<file::restore::Request, dir::restore::Request>;

pub type Request<'a> = bulk::Request<'a, Item>;

pub type Response<'a> = bulk::Response<'a, ()>;
