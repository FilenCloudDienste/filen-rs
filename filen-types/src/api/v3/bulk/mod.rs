//! `POST /v3/bulk/*`: each endpoint runs one single route for many items and answers, per item,
//! exactly the envelope that single call would have answered.
//!
//! A request either fails as a whole with an ordinary error envelope (nothing was applied, e.g.
//! `invalid_params`, or `invalid_endpoint` from a server without the endpoint) or answers
//! `bulk_processed` with one result per item, in item order.

use serde::{Deserialize, Serialize};

use crate::api::response::FilenResponse;

pub mod exists;
pub mod file;
pub mod linked;
pub mod present;
pub mod shared;

#[cfg(test)]
mod tests;

/// Most items one request to a read endpoint (`exists`, `present`, `file/stable`, `linked`,
/// `shared`) may carry.
pub const MAX_READ_ITEMS: usize = 1000;
/// Most items one request to any other (write) endpoint may carry.
pub const MAX_WRITE_ITEMS: usize = 250;

/// Body of an endpoint with no top-level fields: `{"items": [...]}`.
#[derive(Serialize, Debug, Clone)]
pub struct Request<'a, T> {
	pub items: &'a [T],
}

/// `data` of the `bulk_processed` answer: `results[i]` is the single route's envelope for
/// `items[i]`. The outer `status: true` says nothing about the items, and some single routes
/// answer `status: true` for a no-op or not-found case, so branch on each result's `code`.
#[derive(Deserialize, Debug)]
#[serde(bound = "T: Deserialize<'de>")]
pub struct Response<'a, T> {
	pub results: Vec<FilenResponse<'a, T>>,
}

/// An item of an endpoint that mirrors a file route and a dir route: the single route's request
/// fields plus `"type": "file"` or `"type": "dir"`.
#[derive(Serialize, Debug, Clone)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum FileOrDir<F, D> {
	File(F),
	Dir(D),
}
