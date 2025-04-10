use crate::auth::{FileEncryptionVersion, MetaEncryptionVersion};

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

pub fn random_gateway_url() -> &'static str {
	GATEWAY_URLS[rand::random_range(0..GATEWAY_URLS.len())]
}

pub fn gateway_url(path: &str) -> String {
	format!("{}/{}", random_gateway_url(), path)
}

pub const V2FILE_ENCRYPTION_VERSION: FileEncryptionVersion = FileEncryptionVersion::V2;
pub const V2META_ENCRYPTION_VERSION: MetaEncryptionVersion = MetaEncryptionVersion::V2;
