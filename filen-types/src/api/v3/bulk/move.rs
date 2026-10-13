use crate::api::v3::{bulk, dir, file};

pub const ENDPOINT: &str = "v3/bulk/move";

pub type Item = bulk::FileOrDir<file::r#move::Request, dir::r#move::Request>;

pub type Request<'a> = bulk::Request<'a, Item>;

pub type Response<'a> = bulk::Response<'a, ()>;
