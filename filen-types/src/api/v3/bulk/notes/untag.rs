use crate::api::v3::{bulk, notes::untag};

pub const ENDPOINT: &str = "v3/bulk/notes/untag";

pub type Item = untag::Request;

pub type Request<'a> = bulk::Request<'a, Item>;

pub type Response<'a> = bulk::Response<'a, ()>;
