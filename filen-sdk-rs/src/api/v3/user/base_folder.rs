use filen_types::{
	api::{response::FilenResponse, v3::user::base_folder::Response},
	error::ResponseError,
};

use crate::{auth::http::AuthorizedClient, consts::gateway_url};

pub(crate) async fn get(client: impl AuthorizedClient) -> Result<Response, ResponseError> {
	client
		.get_auth_request(gateway_url("v3/user/baseFolder"))
		.send()
		.await?
		.json::<FilenResponse<Response>>()
		.await?
		.into_data()
}
