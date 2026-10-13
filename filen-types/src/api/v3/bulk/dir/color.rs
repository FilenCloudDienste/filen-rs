use crate::api::v3::{bulk, dir::color};

pub const ENDPOINT: &str = "v3/bulk/dir/color";

pub type Item<'a> = color::Request<'a>;

pub type Request<'a> = bulk::Request<'a, Item<'a>>;

pub type Response<'a> = bulk::Response<'a, ()>;
