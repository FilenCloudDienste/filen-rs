use futures::StreamExt;
use log::debug;

use crate::{
	auth::http::AuthorizedClient,
	consts::random_egest_url,
	error::{ChunkTooLargeError, Error},
	fs::file::traits::File,
};
pub(crate) async fn download_file_chunk(
	client: impl AuthorizedClient,
	file: &dyn File,
	chunk_idx: u64,
	out_chunk: &mut Vec<u8>,
) -> Result<(), Error> {
	out_chunk.clear();
	let url = format!(
		"{}/{}/{}/{}/{}",
		random_egest_url(),
		file.region(),
		file.bucket(),
		file.uuid(),
		chunk_idx
	);

	let _permit = client.get_semaphore_permit().await;
	let response = client.get_auth_request(url).send().await?;

	let mut bytes_stream = response.bytes_stream();
	let mut i = 0;

	while let Some(chunk) = bytes_stream.next().await {
		let chunk = chunk?;
		if chunk.len() + i > out_chunk.capacity() {
			return Err(ChunkTooLargeError {
				expected: out_chunk.capacity(),
				actual: chunk.len() + i,
			}
			.into());
		}
		out_chunk.extend_from_slice(&chunk);
		i += chunk.len();
	}

	debug!(
		"Downloaded chunk {chunk_idx} of file {}, size: {}",
		file.uuid(),
		out_chunk.len()
	);

	Ok(())
}
