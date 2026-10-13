//! Drive operations over many items, sent through the `bulk/*` endpoints a chunk at a time and
//! answered per item as their single calls would answer.

use chrono::{DateTime, Utc};

use crate::{
	api::{
		self,
		v3::bulk::{Sent, dir::create},
	},
	auth::Client,
	error::Error,
	fs::dir::{RemoteDirectory, client_impl::NewDirRequest},
	util::MaybeSend,
};

pub(crate) mod create_dirs;
#[cfg(all(test, not(all(target_family = "wasm", target_os = "unknown"))))]
mod tests;

/// The requests the bulk operations send: [`Client`] sends them to the API, tests answer them from
/// a scripted server.
pub(crate) trait Backend {
	/// A new directory named `name`, minted as a single create mints it.
	fn mint_dir(
		&self,
		name: &str,
		created: DateTime<Utc>,
	) -> impl Future<Output = Result<NewDirRequest, Error>> + MaybeSend;
	/// One `bulk/dir/create` request.
	fn bulk_create_dirs<'a>(
		&self,
		items: &'a [create::Item<'a>],
	) -> impl Future<Output = Result<Sent<'a, create::Item<'a>, api::v3::dir::create::Response>, Error>>
	+ MaybeSend;
	/// One `dir/create` request.
	fn create_one_dir(
		&self,
		item: &create::Item<'_>,
	) -> impl Future<Output = Result<api::v3::dir::create::Response, Error>> + MaybeSend;
}

impl Backend for Client {
	async fn mint_dir(&self, name: &str, created: DateTime<Utc>) -> Result<NewDirRequest, Error> {
		let (uuid, meta) = RemoteDirectory::make_parts(name, created)?;
		Ok(self.new_dir_request(uuid, &meta).await)
	}

	async fn bulk_create_dirs<'a>(
		&self,
		items: &'a [create::Item<'a>],
	) -> Result<Sent<'a, create::Item<'a>, api::v3::dir::create::Response>, Error> {
		create::post(self.client(), &create::Request { items }).await
	}

	async fn create_one_dir(
		&self,
		item: &create::Item<'_>,
	) -> Result<api::v3::dir::create::Response, Error> {
		api::v3::dir::create::post(self.client(), item).await
	}
}
