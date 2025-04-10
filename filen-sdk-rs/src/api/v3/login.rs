pub use filen_types::api::v3::login::{Request, Response};
use filen_types::{api::response::FilenResponse, error::ResponseError};

use crate::{auth::http::UnauthorizedClient, consts::gateway_url};

pub(crate) async fn post(
	client: impl UnauthorizedClient,
	request: Request<'_>,
) -> Result<Response, ResponseError> {
	println!("login request: {:?}", request);
	let response = client
		.post_request_json(gateway_url("v3/login"))
		.body(request)
		.send()
		.await?;
	println!("login response: {:?}", response);

	let text = response.text().await?;
	let deserializer = &mut serde_json::Deserializer::from_str(&text);
	let filen_response: FilenResponse<Response> =
		serde_path_to_error::deserialize(deserializer).unwrap();

	// let filen_response: FilenResponse<Response> = serde_json::from_str(&text).unwrap();
	// panic!();
	// let filen_response = response.json::<FilenResponse<Response>>().await?;
	println!("login filen_response: {:?}", filen_response);
	filen_response.into_data()
}
