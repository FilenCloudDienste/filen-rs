use crate::api::v3::{bulk, item::linked::rename};

pub const ENDPOINT: &str = "v3/bulk/linked/rename";

pub type Item<'a> = rename::Request<'a>;

pub type Request<'a> = bulk::Request<'a, Item<'a>>;

pub type Response<'a> = bulk::Response<'a, ()>;
