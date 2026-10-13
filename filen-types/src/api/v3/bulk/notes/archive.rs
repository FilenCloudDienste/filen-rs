use crate::api::v3::{bulk, notes::archive};

pub const ENDPOINT: &str = "v3/bulk/notes/archive";

pub type Item = archive::Request;

pub type Request<'a> = bulk::Request<'a, Item>;

pub type Response<'a> = bulk::Response<'a, ()>;
