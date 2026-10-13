use crate::api::v3::{bulk, notes::delete};

pub const ENDPOINT: &str = "v3/bulk/notes/delete";

pub type Item = delete::Request;

pub type Request<'a> = bulk::Request<'a, Item>;

pub type Response<'a> = bulk::Response<'a, ()>;
