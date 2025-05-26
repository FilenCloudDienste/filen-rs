use futures::StreamExt;

use crate::{
	auth::http::AuthorizedClient, consts::random_egest_url, error::Error, fs::file::traits::File,
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

	let response = client.get_auth_request(url).send().await?;

	let mut bytes_stream = response.bytes_stream();
	let mut i = 0;

	while let Some(chunk) = bytes_stream.next().await {
		let chunk = chunk?;
		if chunk.len() + i > out_chunk.capacity() {
			return Err(Error::ChunkTooLarge {
				expected: out_chunk.capacity(),
				actual: chunk.len() + i,
			});
		}
		out_chunk.extend_from_slice(&chunk);
		i += chunk.len();
	}

	Ok(())
}
