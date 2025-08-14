use bytes::Bytes;
use filen_types::api::response::FilenResponse;
pub use filen_types::api::v3::upload::{ENDPOINT, Response};
use log::debug;
use sha2::{Digest, Sha512};

use crate::{
	api::retry_wrap, auth::http::AuthorizedClient, consts::random_ingest_url, error::Error,
	fs::file::BaseFile,
};

pub(crate) mod done;
pub(crate) mod empty;

pub(crate) async fn upload_file_chunk(
	client: impl AuthorizedClient,
	file: &BaseFile,
	upload_key: &str,
	chunk_idx: u64,
	chunk: Bytes,
) -> Result<Response<'static>, Error> {
	debug!(
		"Uploading chunk {chunk_idx} of file {} size: {}",
		file.uuid(),
		chunk.len()
	);

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

	let _permit = client.get_semaphore_permit().await;

	retry_wrap(
		chunk,
		|| client.post_auth_request(&url),
		url.clone(),
		async |response| {
			response
				.json::<FilenResponse<Response>>()
				.await
				.map(|resp| resp.into_data())
				.map_err(|e| crate::api::RetryError::NoRetry(e.into()))
		},
	)
	.await?
	.map_err(Into::into)
}
