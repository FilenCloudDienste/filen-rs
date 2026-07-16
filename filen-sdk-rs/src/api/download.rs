use filen_types::fs::Uuid;

use crate::{
	ErrorKind,
	auth::unauth::UnauthClient,
	consts::{CHUNK_SIZE, random_egest_url},
	error::Error,
	fs::file::traits::File,
	util::MaybeSendSync,
};

/// Upper bound on the encrypted body of a single downloaded file chunk, across every supported
/// on-the-wire layout. It exists only to bound one chunk's buffer: the egest server sets the
/// (server-controlled) `X-Cl` length, so without a cap a misbehaving/compromised node could stream
/// a multi-GiB body for one chunk, bypass the file-IO memory budget, and OOM the client.
///
/// It must clear the largest legitimate chunk body, not just AES-GCM (V2/V3), which is one
/// plaintext chunk plus nonce(12) + tag(16) = +28. Legacy V1 uses AES-CBC and can additionally
/// carry an OpenSSL "Salted__" EVP header (16) plus a full PKCS7 padding block (16) = +32, and may
/// arrive base64-encoded (~4/3x). Two chunks comfortably clears the base64-expanded V1 worst case
/// while still bounding a single chunk to a couple of MiB.
const MAX_ENCRYPTED_CHUNK_SIZE: usize = 2 * CHUNK_SIZE;

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
		file.uuid(),
		chunk_idx,
		callback,
	)
	.await
}

pub(crate) async fn download_file_chunk_by_uuid<F>(
	client: &UnauthClient,
	region: &str,
	bucket: &str,
	uuid: Uuid,
	chunk_idx: u64,
	callback: Option<&F>,
) -> Result<Vec<u8>, Error>
where
	F: Fn(u64, Option<u64>) + MaybeSendSync,
{
	let endpoint = random_egest_url();
	let url = format!("{endpoint}/{region}/{bucket}/{uuid}/{chunk_idx}");
	client
		.get_raw_bytes_with_callback(
			&url,
			endpoint.into(),
			Some(MAX_ENCRYPTED_CHUNK_SIZE),
			callback,
		)
		.await
		.map_err(map_chunk_download_error)
}

#[cfg(test)]
mod tests {
	use super::MAX_ENCRYPTED_CHUNK_SIZE;
	use crate::consts::CHUNK_SIZE;

	/// The chunk-body cap must accommodate every supported on-the-wire layout, not just AES-GCM
	/// (+28). Legacy V1 AES-CBC adds an OpenSSL "Salted__" EVP header + a full PKCS7 padding block
	/// (+32) and may arrive base64-encoded (~4/3x), so the cap must clear the base64-expanded CBC
	/// worst case or legitimate V1 chunk downloads would be wrongly rejected.
	#[test]
	fn cap_covers_gcm_and_v1_cbc_base64_worst_case() {
		let cap = MAX_ENCRYPTED_CHUNK_SIZE;
		// AES-GCM (V2/V3): one plaintext chunk + nonce(12) + tag(16).
		assert!(cap >= CHUNK_SIZE + 28);
		// V1 AES-CBC with a "Salted__"(8) + salt(8) header and a full 16-byte PKCS7 padding block.
		let v1_evp = CHUNK_SIZE + 16 + 16;
		assert!(cap >= v1_evp);
		// The same body base64-encoded on the wire (4 output chars per 3 input bytes).
		let v1_evp_base64 = v1_evp.div_ceil(3) * 4;
		assert!(
			cap >= v1_evp_base64,
			"cap {cap} must cover the base64 V1 worst case {v1_evp_base64}"
		);
	}
}
