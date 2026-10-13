use crate::api::v3::{bulk, item::shared::rename};

pub const ENDPOINT: &str = "v3/bulk/shared/rename";

pub type Item<'a> = rename::Request<'a>;

pub type Request<'a> = bulk::Request<'a, Item<'a>>;

pub type Response<'a> = bulk::Response<'a, ()>;
