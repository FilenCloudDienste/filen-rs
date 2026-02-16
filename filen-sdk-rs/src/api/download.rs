use crate::{
	auth::http::UnauthorizedClient, consts::random_egest_url, error::Error, fs::file::traits::File,
};

pub(crate) async fn download_file_chunk(
	client: &impl UnauthorizedClient,
	file: &dyn File,
	chunk_idx: u64,
) -> Result<Vec<u8>, Error> {
	let endpoint = random_egest_url();
	let url = format!(
		"{}/{}/{}/{}/{}",
		endpoint,
		file.region(),
		file.bucket(),
		file.uuid(),
		chunk_idx
	);

	client.get_raw_bytes(&url, endpoint.into()).await
}
