use filen_mobile_native_cache::CacheError;

use super::*;

#[test]
fn a_cache_error_shows_its_kind_alone() {
	let failed = CacheError::remote("Path secret-folder\\secret.txt no longer resolves");
	assert_eq!(error(&failed).to_string(), "the cache failed (remote)");
}
