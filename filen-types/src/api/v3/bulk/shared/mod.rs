use crate::api::v3::{bulk, item::shared};

pub mod rename;

pub const ENDPOINT: &str = "v3/bulk/shared";

pub type Item = shared::Request;

pub type Request<'a> = bulk::Request<'a, Item>;

pub type Response<'a> = bulk::Response<'a, shared::Response<'a>>;
