use std::path::PathBuf;

use sha2::{Digest as _, Sha256};

use super::{MIN_RCLONE, RcloneBinary, parse_version};
use crate::{ErrorKind, rclone::RcloneRefusal};

fn bundled(path: PathBuf, sha256: Option<[u8; 32]>) -> RcloneBinary {
	RcloneBinary::Bundled { path, sha256 }
}

#[tokio::test]
async fn bundled_binary_with_the_expected_sha256_resolves() {
	let dir = tempfile::tempdir().unwrap();
	let path = dir.path().join("rclone");
	std::fs::write(&path, b"rclone binary").unwrap();
	let sha256: [u8; 32] = Sha256::digest(b"rclone binary").into();

	let resolved = bundled(path.clone(), Some(sha256)).resolve().await.unwrap();

	assert_eq!(resolved, path);
}

#[tokio::test]
async fn bundled_binary_with_wrong_sha256_is_refused() {
	let dir = tempfile::tempdir().unwrap();
	let path = dir.path().join("rclone");
	std::fs::write(&path, b"a replaced binary").unwrap();
	let sha256: [u8; 32] = Sha256::digest(b"rclone binary").into();

	let error = bundled(path, Some(sha256)).resolve().await.unwrap_err();

	assert_eq!(error.kind(), ErrorKind::IO);
	assert_eq!(
		error.downcast_ref::<RcloneRefusal>(),
		Some(&RcloneRefusal::BinaryChecksum)
	);
}

#[tokio::test]
async fn bundled_binary_without_a_sha256_is_not_hashed() {
	let dir = tempfile::tempdir().unwrap();
	let path = dir.path().join("rclone");
	std::fs::write(&path, b"a signed binary").unwrap();

	let resolved = bundled(path.clone(), None).resolve().await.unwrap();

	assert_eq!(resolved, path);
}

#[tokio::test]
async fn missing_bundled_binary_is_a_typed_refusal() {
	let dir = tempfile::tempdir().unwrap();

	let error = bundled(dir.path().join("rclone"), None)
		.resolve()
		.await
		.unwrap_err();

	assert_eq!(error.kind(), ErrorKind::IO);
	assert_eq!(
		error.downcast_ref::<RcloneRefusal>(),
		Some(&RcloneRefusal::BinaryMissing)
	);
}

#[tokio::test]
async fn a_directory_in_place_of_the_bundled_binary_is_missing() {
	let dir = tempfile::tempdir().unwrap();

	let error = bundled(dir.path().to_path_buf(), None)
		.resolve()
		.await
		.unwrap_err();

	assert_eq!(
		error.downcast_ref::<RcloneRefusal>(),
		Some(&RcloneRefusal::BinaryMissing)
	);
}

#[test]
fn parses_rclone_version_lines() {
	assert_eq!(parse_version("v1.74.3"), Some((1, 74, 3)));
	assert_eq!(
		parse_version("rclone v1.74.3\n- os/version: darwin 26.5.2 (64 bit)\n"),
		Some((1, 74, 3))
	);
	assert_eq!(
		parse_version("rclone v1.75.0-beta.8836.0a1b2c3d4"),
		Some((1, 75, 0))
	);
	assert_eq!(parse_version("v1.74.10-DEV"), Some((1, 74, 10)));
}

#[test]
fn unparsable_versions_are_none() {
	assert_eq!(parse_version(""), None);
	assert_eq!(parse_version("rclone"), None);
	assert_eq!(parse_version("1.74.3"), None);
	assert_eq!(parse_version("v1.74"), None);
	assert_eq!(parse_version("v1.74.x"), None);
	assert_eq!(parse_version("v1.-74.3"), None);
	assert_eq!(parse_version("v99999999999.0.0"), None);
}

#[test]
fn version_floor_compares_component_wise() {
	assert!(parse_version("v1.74.2").unwrap() < MIN_RCLONE);
	assert!(parse_version("v1.74.3").unwrap() >= MIN_RCLONE);
	assert!(parse_version("v1.100.0").unwrap() >= MIN_RCLONE);
	assert!(parse_version("v0.99.99").unwrap() < MIN_RCLONE);
}
