//! Files only the current user may read.

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::{io::Write as _, path::Path};

/// Write `contents` to `path`, creating or truncating it with owner-only permissions
/// (mode `0o600` on Unix). The files written with it hold secrets (the auth config's master keys,
/// private key and API key; the rc password's hash), so they must never be left group- or
/// world-readable for other users on the machine.
pub(crate) fn write_private_file(path: &Path, contents: &str) -> std::io::Result<()> {
	let mut options = std::fs::OpenOptions::new();
	options.write(true).create(true).truncate(true);
	#[cfg(unix)]
	options.mode(0o600);
	let mut file = options.open(path)?;
	// A pre-existing file keeps its previous mode through create+truncate, so tighten it
	// explicitly while it is still empty, before any secret bytes are written.
	#[cfg(unix)]
	file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
	file.write_all(contents.as_bytes())
}
// copied from filen-cli/src/auth.rs
