use filen_types::fs::UuidStr;

use crate::{
	ErrorKind, auth::unauth::UnauthClient, consts::random_egest_url, error::Error,
	fs::file::traits::File,
};

pub(crate) async fn download_file_chunk(
	client: &UnauthClient,
	file: &dyn File,
	chunk_idx: u64,
) -> Result<Vec<u8>, Error> {
	download_file_chunk_by_uuid(
		client,
		file.region(),
		file.bucket(),
		*file.uuid(),
		chunk_idx,
	)
	.await
}

pub(crate) async fn download_file_chunk_by_uuid(
	client: &UnauthClient,
	region: &str,
	bucket: &str,
	uuid: UuidStr,
	chunk_idx: u64,
) -> Result<Vec<u8>, Error> {
	let endpoint = random_egest_url();
	let url = format!("{endpoint}/{region}/{bucket}/{uuid}/{chunk_idx}");

	match client.get_raw_bytes(&url, endpoint.into()).await {
		Ok(res) => Ok(res),
		Err(e) => match e.downcast_ref::<reqwest::Error>() {
			Some(reqwest_err) if reqwest_err.status() == Some(reqwest::StatusCode::NOT_FOUND) => {
				Err(Error::custom_with_source(
					ErrorKind::FileChunkNotFound,
					e,
					Some("downloading file chunk"),
				))
			}
			_ => Err(e),
		},
	}
}
