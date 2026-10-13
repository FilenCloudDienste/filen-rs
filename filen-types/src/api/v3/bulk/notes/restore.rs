use crate::api::v3::{bulk, notes::restore};

pub const ENDPOINT: &str = "v3/bulk/notes/restore";

pub type Item = restore::Request;

pub type Request<'a> = bulk::Request<'a, Item>;

pub type Response<'a> = bulk::Response<'a, ()>;
