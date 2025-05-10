pub use filen_types::api::v3::upload::{ENDPOINT, Response};
use filen_types::{api::response::FilenResponse, error::ResponseError};
use sha2::{Digest, Sha512};

use crate::{auth::http::AuthorizedClient, consts::random_ingest_url, fs::file::BaseFile};

pub(crate) mod done;
pub(crate) mod empty;

pub(crate) async fn upload_file_chunk(
	client: impl AuthorizedClient,
	file: &BaseFile,
	upload_key: &str,
	chunk_idx: u64,
	chunk: Vec<u8>,
) -> Result<Response<'static>, ResponseError> {
	let data_hash = Sha512::digest(&chunk);
	let url = format!(
		"{}/{}?uuid={}&index={}&parent={}&uploadKey={}&hash={}",
		random_ingest_url(),
		ENDPOINT,
		file.uuid(),
		chunk_idx,
		file.parent(),
		upload_key,
		faster_hex::hex_string(data_hash.as_slice()),
	);

	client
		.post_auth_request(url)
		.body(chunk)
		.send()
		.await?
		.json::<FilenResponse<Response>>()
		.await?
		.into_data()
}
