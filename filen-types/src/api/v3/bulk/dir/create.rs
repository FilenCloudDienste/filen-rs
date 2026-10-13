use crate::api::v3::{bulk, dir::create};

pub const ENDPOINT: &str = "v3/bulk/dir/create";

/// Send parents before children: a later item whose `parent` is an earlier item's requested
/// `uuid` runs in the folder that item answered.
pub type Item<'a> = create::Request<'a>;

pub type Request<'a> = bulk::Request<'a, Item<'a>>;

/// `data.uuid` can differ from the requested uuid (a same-named folder already existed).
pub type Response<'a> = bulk::Response<'a, create::Response>;
