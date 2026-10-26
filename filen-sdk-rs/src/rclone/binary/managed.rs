//! The managed rclone: a pinned release downloaded from rclone's GitHub releases into a directory
//! and checked against the release's checksums.

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};

use hex_literal::hex;
use sha2::{Digest as _, Sha256};
use tokio::{fs, io::AsyncWriteExt as _};
use tracing::{debug, info, warn};

use crate::{Error, ErrorKind, error::ResultExt as _};

const RCLONE_VERSION: &str = "1.74.4";
const RCLONE_CHECKSUM_LINUX_AMD64: [u8; 32] =
	hex!("fe435e0c36228e7c2f116a8701f01127bb1f694005fc11d1f27186c8bca4115d");
const RCLONE_CHECKSUM_LINUX_ARM64: [u8; 32] =
	hex!("97685285c9ad6a0cf17d5844115d2a67245af6444db672187074bd9c358de419");
const RCLONE_CHECKSUM_OSX_AMD64: [u8; 32] =
	hex!("4188aa84043d7a6240912923f47639a9d2da21f3b40a521c065c8d92e66563f6");
const RCLONE_CHECKSUM_OSX_ARM64: [u8; 32] =
	hex!("c2100e2d4a4b3be04c55cd45380cafe7647e1ad772bb055f52f00876ed701167");
const RCLONE_CHECKSUM_WINDOWS_AMD64: [u8; 32] =
	hex!("ef097ef9de37a57feb7d9f9c7afb34148ad3c65be8025f1d8f7f521554a701ea");
const RCLONE_CHECKSUM_WINDOWS_ARM64: [u8; 32] =
	hex!("72194ad0aaf210d7a55808801191fecc7e175444dab7be7491b7a63074521f3a");
// info: when updating the rclone version here, also update the expected SHA256 checksums by copying them from the supplied file

/// Returns the path to the managed rclone binary in `config_dir`, downloading it first if it is
/// not there and `allow_download` is set. Without it, a missing binary is the error "Rclone not
/// found".
pub(crate) async fn ensure(config_dir: &Path, allow_download: bool) -> Result<PathBuf, Error> {
	// determine download url
	let platform_str = match std::env::consts::OS {
		"windows" => Some("windows"),
		"linux" => Some("linux"),
		"macos" => Some("osx"),
		_ => None,
	};
	let arch_str = match std::env::consts::ARCH {
		"x86_64" => Some("amd64"),
		"aarch64" => Some("arm64"),
		_ => None,
	};
	let rclone_zip_name = match (platform_str, arch_str) {
		(Some(platform), Some(arch)) => format!("rclone-v{}-{}-{}", RCLONE_VERSION, platform, arch),
		_ => {
			return Err(Error::custom(
				ErrorKind::IO,
				format!(
					"Unsupported platform/architecture: {} {}",
					std::env::consts::OS,
					std::env::consts::ARCH
				),
			));
		}
	};
	let rclone_zip_download_url = format!(
		"https://github.com/rclone/rclone/releases/download/v{}/{}.zip",
		RCLONE_VERSION, rclone_zip_name
	);
	let file_ending = match std::env::consts::OS {
		"windows" => ".exe",
		_ => "",
	};

	let rclone_binary_path = config_dir.join(format!("{}{}", rclone_zip_name, file_ending));
	debug!("Rclone binary path: {}", rclone_binary_path.display());

	// download binary if it doesn't exist
	if rclone_binary_path.exists() {
		info!("Rclone binary already exists, skipping download");
	} else {
		if !allow_download {
			return Err(Error::custom(ErrorKind::IO, "Rclone not found"));
		}

		info!(
			"Downloading Rclone binary from {}...",
			rclone_zip_download_url
		);
		let expected_zip_checksum = match (std::env::consts::OS, std::env::consts::ARCH) {
			("windows", "x86_64") => RCLONE_CHECKSUM_WINDOWS_AMD64,
			("windows", "aarch64") => RCLONE_CHECKSUM_WINDOWS_ARM64,
			("linux", "x86_64") => RCLONE_CHECKSUM_LINUX_AMD64,
			("linux", "aarch64") => RCLONE_CHECKSUM_LINUX_ARM64,
			("macos", "x86_64") => RCLONE_CHECKSUM_OSX_AMD64,
			("macos", "aarch64") => RCLONE_CHECKSUM_OSX_ARM64,
			_ => unreachable!(),
		};
		let zip_part_path = config_dir.join(format!("{}.zip{}", rclone_zip_name, PART_SUFFIX));
		let binary_part_path = part_path(&rclone_binary_path);
		let downloaded = async {
			download_verified(
				&rclone_zip_download_url,
				&expected_zip_checksum,
				&zip_part_path,
			)
			.await?;
			extract_rclone(
				&zip_part_path,
				format!("{}/rclone{}", rclone_zip_name, file_ending),
				&binary_part_path,
			)
			.await
		}
		.await;
		// The zip is only needed to extract from, and a failed attempt's parts are never reused.
		remove_if_present(&zip_part_path).await;
		if let Err(e) = downloaded {
			remove_if_present(&binary_part_path).await;
			return Err(e);
		}
		fs::rename(&binary_part_path, &rclone_binary_path)
			.await
			.context("Failed to move the Rclone binary into place")?;
		prune_other_versions(config_dir, &rclone_binary_path).await;
	}

	debug!("Using Rclone binary at {}", rclone_binary_path.display());
	Ok(rclone_binary_path)
}

/// What a file being written is called until it is complete. Nothing reads a path with it: a
/// download or extraction cut short by a crash leaves only such a file, never a truncated binary
/// at the path that is taken as installed.
const PART_SUFFIX: &str = ".part";

fn part_path(path: &Path) -> PathBuf {
	let mut name = path.as_os_str().to_os_string();
	name.push(PART_SUFFIX);
	PathBuf::from(name)
}

/// Streams `url` into `path`, checking it against `expected_sha256` as it is written, without
/// holding the release in memory. A mismatch is an error, and the file is left for the caller to
/// remove.
async fn download_verified(
	url: &str,
	expected_sha256: &[u8; 32],
	path: &Path,
) -> Result<(), Error> {
	let mut response = reqwest::get(url)
		.await
		.context("Failed to download Rclone binary")?
		.error_for_status()
		.context("Failed to download Rclone binary")?;
	let mut file = fs::File::create(path)
		.await
		.context("Failed to create the Rclone download file")?;
	let mut hasher = Sha256::new();
	while let Some(chunk) = response
		.chunk()
		.await
		.context("Failed to read Rclone binary response")?
	{
		hasher.update(&chunk);
		file.write_all(&chunk)
			.await
			.context("Failed to write the Rclone download")?;
	}
	file.flush()
		.await
		.context("Failed to write the Rclone download")?;
	if hasher.finalize().as_slice() != expected_sha256 {
		return Err(Error::custom(
			ErrorKind::Response,
			"Downloaded Rclone zip's checksum doesn't match!",
		));
	}
	Ok(())
}

/// Extracts the zip entry `entry` of the zip at `zip_path` to `binary_path`, executable on Unix.
async fn extract_rclone(zip_path: &Path, entry: String, binary_path: &Path) -> Result<(), Error> {
	let (zip_path, binary_path) = (zip_path.to_path_buf(), binary_path.to_path_buf());
	tokio::task::spawn_blocking(move || -> Result<(), Error> {
		let zip_file =
			std::fs::File::open(&zip_path).context("Failed to open the downloaded Rclone zip")?;
		let mut zip = zip::ZipArchive::new(zip_file).map_err(|e| {
			Error::custom_with_source(ErrorKind::Response, e, Some("Failed to read Rclone zip"))
		})?;
		let mut rclone_file = zip.by_name(&entry).map_err(|e| {
			Error::custom_with_source(
				ErrorKind::Response,
				e,
				Some("Failed to find Rclone file in zip"),
			)
		})?;
		let mut options = std::fs::OpenOptions::new();
		options.write(true).create(true).truncate(true);
		#[cfg(unix)]
		options.mode(0o755);
		let mut binary = options
			.open(&binary_path)
			.context("Failed to create the Rclone binary")?;
		std::io::copy(&mut rclone_file, &mut binary).context("Failed to read Rclone file")?;
		Ok(())
	})
	.await
	.map_err(|e| {
		Error::custom_with_source(
			ErrorKind::Internal,
			e,
			Some("The Rclone extraction task failed"),
		)
	})?
}

/// Removes every other managed release from `dir`: older versions' binaries, and the parts a
/// cut-short download left. Only what a managed download names `rclone-v*` is touched; a file
/// that cannot be removed is logged and left.
async fn prune_other_versions(dir: &Path, keep: &Path) {
	let mut entries = match fs::read_dir(dir).await {
		Ok(entries) => entries,
		Err(e) => {
			warn!(
				"Failed to list {} to prune old Rclone versions: {e}",
				dir.display()
			);
			return;
		}
	};
	loop {
		let entry = match entries.next_entry().await {
			Ok(Some(entry)) => entry,
			Ok(None) => return,
			Err(e) => {
				warn!(
					"Failed to list {} to prune old Rclone versions: {e}",
					dir.display()
				);
				return;
			}
		};
		let path = entry.path();
		if path == keep || !entry.file_name().to_string_lossy().starts_with("rclone-v") {
			continue;
		}
		if let Err(e) = fs::remove_file(&path).await {
			warn!(
				"Failed to remove the old Rclone file {}: {e}",
				path.display()
			);
		}
	}
}

async fn remove_if_present(path: &Path) {
	match fs::remove_file(path).await {
		Ok(()) => {}
		Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
		Err(e) => warn!("Failed to remove {}: {e}", path.display()),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// The managed binary's name on every platform, so a test can plant its leftovers here.
	fn every_platform_binary_name() -> Vec<String> {
		let mut names = Vec::new();
		for (os, ending) in [("linux", ""), ("osx", ""), ("windows", ".exe")] {
			for arch in ["amd64", "arm64"] {
				names.push(format!("rclone-v{RCLONE_VERSION}-{os}-{arch}{ending}"));
			}
		}
		names
	}

	#[tokio::test]
	async fn a_part_file_left_by_a_crash_is_never_taken_as_the_binary() {
		let dir = tempfile::tempdir().unwrap();
		for name in every_platform_binary_name() {
			std::fs::write(
				dir.path().join(format!("{name}{PART_SUFFIX}")),
				b"truncated",
			)
			.unwrap();
		}

		let error = ensure(dir.path(), false).await.unwrap_err();

		assert_eq!(error.kind(), ErrorKind::IO);
		assert_eq!(
			error.to_string(),
			"Error of kind IO: context: Rclone not found"
		);
	}

	#[tokio::test]
	async fn pruning_keeps_the_current_binary_and_what_is_not_a_managed_release() {
		let dir = tempfile::tempdir().unwrap();
		let keep = dir
			.path()
			.join(format!("rclone-v{RCLONE_VERSION}-osx-arm64"));
		for name in [
			format!("rclone-v{RCLONE_VERSION}-osx-arm64"),
			format!("rclone-v{RCLONE_VERSION}-osx-arm64{PART_SUFFIX}"),
			format!("rclone-v{RCLONE_VERSION}-osx-arm64.zip{PART_SUFFIX}"),
			"rclone-v1.0.0-osx-arm64".to_string(),
			"rclone.conf".to_string(),
		] {
			std::fs::write(dir.path().join(name), b"x").unwrap();
		}

		prune_other_versions(dir.path(), &keep).await;

		let mut left = std::fs::read_dir(dir.path())
			.unwrap()
			.map(|entry| entry.unwrap().file_name().into_string().unwrap())
			.collect::<Vec<_>>();
		left.sort();
		assert_eq!(
			left,
			[
				format!("rclone-v{RCLONE_VERSION}-osx-arm64"),
				"rclone.conf".to_string()
			]
		);
	}
}
