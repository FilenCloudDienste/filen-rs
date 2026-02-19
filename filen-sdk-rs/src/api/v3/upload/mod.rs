use bytes::Bytes;
pub use filen_types::api::v3::upload::{ENDPOINT, Response};
use log::debug;
use sha2::{Digest, Sha512};

use crate::{auth::http::AuthClient, consts::random_ingest_url, error::Error, fs::file::BaseFile};

pub(crate) mod done;
pub(crate) mod empty;

pub(crate) async fn upload_file_chunk(
	client: &AuthClient,
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

	let ingest_url = random_ingest_url();

	let data_hash = Sha512::digest(&chunk);
	let url = format!(
		"{}/{}?uuid={}&index={}&parent={}&uploadKey={}&hash={}",
		ingest_url,
		ENDPOINT,
		file.uuid(),
		chunk_idx,
		file.parent(),
		upload_key,
		faster_hex::hex_string(data_hash.as_slice()),
	);

	client
		.post_raw_bytes_auth(chunk, &url, ingest_url.into())
		.await
}
