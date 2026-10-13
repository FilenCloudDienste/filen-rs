use crate::api::v3::{bulk, item::favorite};

pub const ENDPOINT: &str = "v3/bulk/favorite";

pub type Item = favorite::Request;

pub type Request<'a> = bulk::Request<'a, Item>;

pub type Response<'a> = bulk::Response<'a, favorite::Response>;
