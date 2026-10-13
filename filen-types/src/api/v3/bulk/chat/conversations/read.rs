use crate::api::v3::{bulk, chat::conversations::read};

pub const ENDPOINT: &str = "v3/bulk/chat/conversations/read";

pub type Item = read::Request;

pub type Request<'a> = bulk::Request<'a, Item>;

pub type Response<'a> = bulk::Response<'a, ()>;
