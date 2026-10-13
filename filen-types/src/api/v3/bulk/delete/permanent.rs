use crate::api::v3::{bulk, dir, file};

pub const ENDPOINT: &str = "v3/bulk/delete/permanent";

pub type Item = bulk::FileOrDir<file::delete::permanent::Request, dir::delete::permanent::Request>;

pub type Request<'a> = bulk::Request<'a, Item>;

pub type Response<'a> = bulk::Response<'a, ()>;
