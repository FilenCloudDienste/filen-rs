use crate::api::v3::{bulk, notes::trash};

pub const ENDPOINT: &str = "v3/bulk/notes/trash";

pub type Item = trash::Request;

pub type Request<'a> = bulk::Request<'a, Item>;

pub type Response<'a> = bulk::Response<'a, ()>;
