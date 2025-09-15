pub(crate) mod archive;
pub(crate) mod content;
pub(crate) mod create;
pub(crate) mod delete;
pub(crate) mod favorite;
pub(crate) mod history;
pub(crate) mod participants;
pub(crate) mod pinned;
pub(crate) mod restore;
pub(crate) mod tag;
pub(crate) mod tags;
pub(crate) mod title;
pub(crate) mod trash;
pub(crate) mod r#type;
pub(crate) mod untag;

pub use filen_types::api::v3::notes::{ENDPOINT, Response};

use crate::{api::get_auth_request, auth::http::AuthorizedClient, error::Error};

pub(crate) async fn get(client: impl AuthorizedClient) -> Result<Response<'static>, Error> {
	get_auth_request(client, ENDPOINT).await
}
