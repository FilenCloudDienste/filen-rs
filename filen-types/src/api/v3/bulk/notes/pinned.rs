use crate::api::v3::{bulk, notes::pinned};

pub const ENDPOINT: &str = "v3/bulk/notes/pinned";

pub type Item = pinned::Request;

pub type Request<'a> = bulk::Request<'a, Item>;

pub type Response<'a> = bulk::Response<'a, ()>;
