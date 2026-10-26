//! Which rclone binary runs: one the app ships, or (`rclone-managed`) a pinned release downloaded
//! into a directory.

use std::{io, path::PathBuf};

use sha2::{Digest as _, Sha256};

use crate::{Error, ErrorKind, error::ResultExt as _, rclone::RcloneRefusal};

#[cfg(feature = "rclone-managed")]
pub(crate) mod managed;

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
}

/// `path` once it is known to be a file and, with `sha256`, to hash to it. Takes the path by value
/// to move it into the blocking task that hashes the file, and to return it.
async fn resolve_bundled(path: PathBuf, sha256: Option<[u8; 32]>) -> Result<PathBuf, Error> {
	tokio::task::spawn_blocking(move || {
		let mut file = match std::fs::File::open(&path) {
			Ok(file) => file,
			Err(e) if e.kind() == io::ErrorKind::NotFound => {
				return Err(RcloneRefusal::BinaryMissing.into());
			}
			Err(e) => return Err(Error::from(e)),
		};
		if !file.metadata()?.is_file() {
			return Err(RcloneRefusal::BinaryMissing.into());
		}
		if let Some(expected) = sha256 {
			// Streamed: the binary is tens of megabytes.
			let mut hasher = Sha256::new();
			io::copy(&mut file, &mut hasher)?;
			if hasher.finalize().as_slice() != expected {
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

#[cfg(test)]
mod tests;
