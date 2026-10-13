use crate::api::v3::{bulk, notes::tag};

pub const ENDPOINT: &str = "v3/bulk/notes/tag";

pub type Item = tag::Request;

pub type Request<'a> = bulk::Request<'a, Item>;

/// An already tagged note answers `note_tagged` without data.
pub type Response<'a> = bulk::Response<'a, tag::Response>;
