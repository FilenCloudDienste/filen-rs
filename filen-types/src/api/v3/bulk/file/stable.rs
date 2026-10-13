use crate::api::v3::{bulk, file::stable};

pub const ENDPOINT: &str = "v3/bulk/file/stable";

pub type Item = stable::Request;

pub type Request<'a> = bulk::Request<'a, Item>;

pub type Response<'a> = bulk::Response<'a, stable::Response<'a>>;
