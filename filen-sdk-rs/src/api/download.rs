use filen_types::fs::UuidStr;

use crate::{
	ErrorKind, auth::unauth::UnauthClient, consts::random_egest_url, error::Error,
	fs::file::traits::File, util::MaybeSendSync,
};

/// A 404 from the egest server means the chunk genuinely does not exist, surfaced as
/// [`ErrorKind::FileChunkNotFound`]; every other error passes through unchanged.
fn map_chunk_download_error(error: Error) -> Error {
	match error.downcast_ref::<reqwest::Error>() {
		Some(reqwest_err) if reqwest_err.status() == Some(reqwest::StatusCode::NOT_FOUND) => {
			Error::custom_with_source(
				ErrorKind::FileChunkNotFound,
				error,
				Some("downloading file chunk"),
			)
		}
		_ => error,
	}
}

/// Downloads one encrypted file chunk, invoking `callback(bytes_so_far, content_length)` as the
/// (still-encrypted) body streams in. The body is fully buffered before return (decryption needs
/// the whole chunk); the callback only reports arrival progress.
pub(crate) async fn download_file_chunk<F>(
	client: &UnauthClient,
	file: &dyn File,
	chunk_idx: u64,
	callback: Option<&F>,
) -> Result<Vec<u8>, Error>
where
	F: Fn(u64, Option<u64>) + MaybeSendSync,
{
	download_file_chunk_by_uuid(
		client,
		file.region(),
		file.bucket(),
		*file.uuid(),
		chunk_idx,
		callback,
	)
	.await
}

pub(crate) async fn download_file_chunk_by_uuid<F>(
	client: &UnauthClient,
	region: &str,
	bucket: &str,
	uuid: UuidStr,
	chunk_idx: u64,
	callback: Option<&F>,
) -> Result<Vec<u8>, Error>
where
	F: Fn(u64, Option<u64>) + MaybeSendSync,
{
	let endpoint = random_egest_url();
	let url = format!("{endpoint}/{region}/{bucket}/{uuid}/{chunk_idx}");
	client
		.get_raw_bytes_with_callback(&url, endpoint.into(), callback)
		.await
		.map_err(map_chunk_download_error)
}
