use filen_types::auth::{FileEncryptionVersion, MetaEncryptionVersion};

pub const GATEWAY_URLS: [&str; 8] = [
	"https://gateway.filen.io",
	"https://gateway.filen.net",
	"https://gateway.filen-1.net",
	"https://gateway.filen-2.net",
	"https://gateway.filen-3.net",
	"https://gateway.filen-4.net",
	"https://gateway.filen-5.net",
	"https://gateway.filen-6.net",
];

pub const EGEST_URLS: [&str; 8] = [
	"https://egest.filen.io",
	"https://egest.filen.net",
	"https://egest.filen-1.net",
	"https://egest.filen-2.net",
	"https://egest.filen-3.net",
	"https://egest.filen-4.net",
	"https://egest.filen-5.net",
	"https://egest.filen-6.net",
];

pub const INGEST_URLS: [&str; 8] = [
	"https://ingest.filen.io",
	"https://ingest.filen.net",
	"https://ingest.filen-1.net",
	"https://ingest.filen-2.net",
	"https://ingest.filen-3.net",
	"https://ingest.filen-4.net",
	"https://ingest.filen-5.net",
	"https://ingest.filen-6.net",
];

pub fn random_gateway_url() -> &'static str {
	GATEWAY_URLS[rand::random_range(0..GATEWAY_URLS.len())]
}

pub fn gateway_url(path: &str) -> String {
	format!("{}/{}", random_gateway_url(), path)
}

pub fn random_egest_url() -> &'static str {
	EGEST_URLS[rand::random_range(0..EGEST_URLS.len())]
}

pub fn random_ingest_url() -> &'static str {
	INGEST_URLS[rand::random_range(0..INGEST_URLS.len())]
}

pub const V2FILE_ENCRYPTION_VERSION: FileEncryptionVersion = FileEncryptionVersion::V2;
pub const V2META_ENCRYPTION_VERSION: MetaEncryptionVersion = MetaEncryptionVersion::V2;

pub const DEFAULT_MAX_DOWNLOAD_THREADS_PER_FILE: u64 = 8;
pub const DEFAULT_MAX_UPLOAD_THREADS_PER_FILE: usize = 8;
pub const CHUNK_SIZE: usize = 1024 * 1024; // 1MiB
pub const CHUNK_SIZE_U64: u64 = CHUNK_SIZE as u64;
pub const FILE_CHUNK_SIZE_EXTRA: usize = 28; // auth tag (16) + nonce (12)
