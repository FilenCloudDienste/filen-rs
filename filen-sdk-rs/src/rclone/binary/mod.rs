//! Which rclone binary runs: one the app ships, or (`rclone-managed`) a pinned release downloaded
//! into a directory.

use std::{io, path::PathBuf, process::Stdio, time::Duration};

use filen_types::crypto::DigestWriter;
use sha2::{Digest as _, Sha256};
use tokio::process::Command;

#[cfg(windows)]
use crate::rclone::system::CREATE_NO_WINDOW;
use crate::{
	Error, ErrorKind,
	error::ResultExt as _,
	rclone::{RcloneRefusal, system::spawn_rclone},
};

#[cfg(feature = "rclone-managed")]
pub(crate) mod managed;

/// How long `rclone version` may take to answer.
const VERSION_TIMEOUT: Duration = Duration::from_secs(30);

/// Where the rclone binary comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RcloneBinary {
	/// A binary the app ships at `path`. When `sha256` is given, the file must hash to it on every
	/// resolve; where the platform's code signing rewrites the binary after it is built (macOS,
	/// Windows), the app checks the signature itself and passes `None`.
	Bundled {
		path: PathBuf,
		sha256: Option<[u8; 32]>,
	},
	/// The pinned release, downloaded into `dir` the first time it is resolved.
	#[cfg(feature = "rclone-managed")]
	Managed { dir: PathBuf },
}

impl RcloneBinary {
	/// The path of a binary that can be run. A missing bundled binary is refused with
	/// [`RcloneRefusal::BinaryMissing`], one with another SHA-256 than expected with
	/// [`RcloneRefusal::BinaryChecksum`]; a managed one is downloaded if it is not there yet.
	pub async fn resolve(&self) -> Result<PathBuf, Error> {
		match self {
			Self::Bundled { path, sha256 } => resolve_bundled(path.clone(), *sha256)
				.await
				.context(format!("rclone binary {}", path.display())),
			#[cfg(feature = "rclone-managed")]
			Self::Managed { dir } => managed::ensure(dir, true).await,
		}
	}

	/// The version the binary reports, such as `v1.74.4`, once it is resolved as
	/// [`RcloneBinary::resolve`] resolves it. It is not checked against the oldest version the
	/// daemon runs, which [`crate::rclone::RcloneDaemon::start`] refuses with
	/// [`RcloneRefusal::UnsupportedVersion`]. Fails when `rclone version` fails, takes longer than
	/// 30 s, or prints no version.
	pub async fn version(&self) -> Result<String, Error> {
		let path = self.resolve().await?;
		let mut command = Command::new(&path);
		command
			// An in-memory config: the user's own is neither read nor created.
			.args(["--config", "", "version"])
			.stdin(Stdio::null())
			.stdout(Stdio::piped())
			.stderr(Stdio::null())
			.kill_on_drop(true);
		#[cfg(windows)]
		command.creation_flags(CREATE_NO_WINDOW);
		let run = async {
			spawn_rclone(&mut command)
				.await
				.context("failed to start rclone version")?
				.wait_with_output()
				.await
				.context("failed to wait for rclone version")
		};
		let output = tokio::time::timeout(VERSION_TIMEOUT, run)
			.await
			.map_err(|_| {
				Error::custom(
					ErrorKind::IO,
					format!("rclone version did not answer within {VERSION_TIMEOUT:?}"),
				)
			})??;
		if !output.status.success() {
			return Err(Error::custom(
				ErrorKind::IO,
				format!("rclone version failed: {}", output.status),
			));
		}
		reported_version(&String::from_utf8_lossy(&output.stdout))
			.ok_or_else(|| Error::custom(ErrorKind::IO, "rclone version printed no version"))
	}
}

/// The version `rclone version` prints on its first line, `rclone v1.74.4`, as `v1.74.4`, if it
/// is one [`parse_version`] reads.
fn reported_version(output: &str) -> Option<String> {
	let line = output.lines().next()?.trim();
	let version = line.strip_prefix("rclone ")?;
	parse_version(version)?;
	Some(version.to_owned())
}

#[cfg(feature = "rclone-managed")]
impl RcloneBinary {
	/// Whether [`RcloneBinary::resolve`] finds the binary without downloading it: a bundled binary
	/// is a file at its path, a managed one was downloaded before.
	pub async fn is_installed(&self) -> bool {
		match self {
			Self::Bundled { path, .. } => tokio::fs::metadata(path)
				.await
				.is_ok_and(|metadata| metadata.is_file()),
			Self::Managed { dir } => managed::ensure(dir, false).await.is_ok(),
		}
	}
}

/// `path` once it is known to be a file and, with `sha256`, to hash to it. Takes the path by value
/// to move it into the blocking task that hashes the file, and to return it.
async fn resolve_bundled(path: PathBuf, sha256: Option<[u8; 32]>) -> Result<PathBuf, Error> {
	tokio::task::spawn_blocking(move || {
		let mut file = match std::fs::File::open(&path) {
			Ok(file) if file.metadata()?.is_file() => file,
			Ok(_) => return Err(RcloneRefusal::BinaryMissing.into()),
			// Windows refuses to open a directory as a file, with access denied.
			Err(e) if e.kind() == io::ErrorKind::NotFound || path.is_dir() => {
				return Err(RcloneRefusal::BinaryMissing.into());
			}
			Err(e) => return Err(Error::from(e)),
		};
		if let Some(expected) = sha256 {
			// Streamed: the binary is tens of megabytes.
			let mut hasher = DigestWriter::new(Sha256::new());
			io::copy(&mut file, &mut hasher)?;
			if hasher.into_inner().finalize().as_slice() != expected {
				return Err(RcloneRefusal::BinaryChecksum.into());
			}
		}
		Ok(path)
	})
	.await
	.map_err(|e| {
		Error::custom_with_source(
			ErrorKind::Internal,
			e,
			Some("the rclone binary check task failed"),
		)
	})?
}

/// The oldest rclone the daemon runs: the first release whose rc has everything it relies on
/// (`serve/start`, `mount/mount` with `vfsOpt`/`mountOpt`, `config/create` into an in-memory
/// config) and its `filen` backend the options it is configured with.
pub(crate) const MIN_RCLONE: (u32, u32, u32) = (1, 74, 3);

/// The `(major, minor, patch)` of an rclone version string: `rc core/version`'s `v1.74.3`, or the
/// first line of `rclone version`'s output, `rclone v1.74.3`. A suffix after the patch number
/// (`-beta.8836.abcdef`, `-DEV`) is ignored.
pub(crate) fn parse_version(text: &str) -> Option<(u32, u32, u32)> {
	let line = text.lines().next()?.trim();
	let version = line
		.strip_prefix("rclone ")
		.unwrap_or(line)
		.strip_prefix('v')?;
	let mut parts = version.splitn(3, '.');
	let major = parts.next()?.parse().ok()?;
	let minor = parts.next()?.parse().ok()?;
	let rest = parts.next()?;
	let patch_end = rest
		.find(|c: char| !c.is_ascii_digit())
		.unwrap_or(rest.len());
	let patch = rest.get(..patch_end)?.parse().ok()?;
	Some((major, minor, patch))
}

#[cfg(test)]
mod tests;
