use std::{num::NonZeroU32, time::Duration};

use filen_types::auth::{AuthVersion, FileEncryptionVersion, MetaEncryptionVersion};

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
const GATEWAY_URLS: [&str; 8] = [
	"https://gateway.filen.io",
	"https://gateway.filen.net",
	"https://gateway.filen-1.net",
	"https://gateway.filen-2.net",
	"https://gateway.filen-3.net",
	"https://gateway.filen-4.net",
	"https://gateway.filen-5.net",
	"https://gateway.filen-6.net",
];

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
const EGEST_URLS: [&str; 8] = [
	"https://egest.filen.io",
	"https://egest.filen.net",
	"https://egest.filen-1.net",
	"https://egest.filen-2.net",
	"https://egest.filen-3.net",
	"https://egest.filen-4.net",
	"https://egest.filen-5.net",
	"https://egest.filen-6.net",
];

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
const INGEST_URLS: [&str; 8] = [
	"https://ingest.filen.io",
	"https://ingest.filen.net",
	"https://ingest.filen-1.net",
	"https://ingest.filen-2.net",
	"https://ingest.filen-3.net",
	"https://ingest.filen-4.net",
	"https://ingest.filen-5.net",
	"https://ingest.filen-6.net",
];

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
pub fn random_gateway_url() -> &'static str {
	GATEWAY_URLS[rand::random_range(0..GATEWAY_URLS.len())]
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
pub fn random_gateway_url() -> &'static str {
	"https://gateway.filen.io"
}

pub fn gateway_url(path: &str) -> String {
	format!("{}/{}", random_gateway_url(), path)
}

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
pub fn random_egest_url() -> &'static str {
	EGEST_URLS[rand::random_range(0..EGEST_URLS.len())]
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
pub fn random_egest_url() -> &'static str {
	"https://egest.filen.io"
}

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
pub fn random_ingest_url() -> &'static str {
	INGEST_URLS[rand::random_range(0..INGEST_URLS.len())]
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
pub fn random_ingest_url() -> &'static str {
	"https://ingest.filen.io"
}

pub const V2FILE_ENCRYPTION_VERSION: FileEncryptionVersion = FileEncryptionVersion::V2;
pub const V2META_ENCRYPTION_VERSION: MetaEncryptionVersion = MetaEncryptionVersion::V2;
pub const NEW_ACCOUNT_AUTH_VERSION: AuthVersion = AuthVersion::V2;

pub const CHUNK_SIZE: usize = 1024 * 1024; // 1MiB
pub const CHUNK_SIZE_U64: u64 = CHUNK_SIZE as u64;
pub const FILE_CHUNK_SIZE: NonZeroU32 = NonZeroU32::new(1024 * 1024).unwrap(); // 1 MiB
pub const FILE_CHUNK_SIZE_EXTRA: NonZeroU32 = NonZeroU32::new(28).unwrap(); // auth tag (16) + nonce (12)
pub const FILE_CHUNK_SIZE_EXTRA_USIZE: usize = FILE_CHUNK_SIZE_EXTRA.get() as usize;

pub(crate) const MAX_SMALL_PARALLEL_REQUESTS: usize = 64;
#[cfg(not(target_os = "ios"))]
pub(crate) const MAX_DEFAULT_MEMORY_USAGE_TARGET: usize =
	(CHUNK_SIZE + FILE_CHUNK_SIZE_EXTRA_USIZE) * 4; // 4 full Chunks
#[cfg(target_os = "ios")]
pub(crate) const MAX_DEFAULT_MEMORY_USAGE_TARGET: usize =
	(CHUNK_SIZE + FILE_CHUNK_SIZE_EXTRA_USIZE) * 2; // 4 full Chunks
pub(crate) const MAX_OPEN_FILES: usize = 64;

pub const RSA_KEY_SIZE: usize = 4096;

/// How often callbacks should be called during long operations
pub(crate) const CALLBACK_INTERVAL: Duration = Duration::from_millis(200);
