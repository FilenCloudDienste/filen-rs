use crate::api::v3::{bulk, item::linked};

pub mod rename;

pub const ENDPOINT: &str = "v3/bulk/linked";

pub type Item = linked::Request;

pub type Request<'a> = bulk::Request<'a, Item>;

pub type Response<'a> = bulk::Response<'a, linked::Response<'a>>;
