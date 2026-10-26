use crate::{Error, ErrorKind, rclone::binary::MIN_RCLONE};

/// What the rclone module refuses with that a caller can tell apart without reading the message.
///
/// Carried as the source of the returned [`Error`], of the kind [`RcloneRefusal::kind`] names,
/// so a caller recovers it with [`Error::downcast_ref`]. The error's context says which path,
/// address or item it was about.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RcloneRefusal {
	/// The rclone binary is not where it was expected, or is not a file.
	#[error("the rclone binary is missing")]
	BinaryMissing,
	/// The bundled rclone binary's SHA-256 is not the one it was shipped with.
	#[error("the rclone binary does not match its expected checksum")]
	BinaryChecksum,
	/// The rclone binary is older than the oldest one the daemon runs, or its version cannot be
	/// read. `found` is the version it reported.
	#[error(
		"rclone {found} is not supported: the daemon needs rclone v{}.{}.{} or later",
		MIN_RCLONE.0,
		MIN_RCLONE.1,
		MIN_RCLONE.2
	)]
	UnsupportedVersion { found: String },
	/// The watchdog that stops rclone when its host dies could not be started, so rclone is not
	/// run at all.
	#[error("the rclone watchdog could not be started")]
	WatchdogUnavailable,
}

impl RcloneRefusal {
	/// The error kind a refusal is returned as: [`ErrorKind::IO`] for what is wrong on this machine.
	pub fn kind(&self) -> ErrorKind {
		match self {
			Self::BinaryMissing
			| Self::BinaryChecksum
			| Self::UnsupportedVersion { .. }
			| Self::WatchdogUnavailable => ErrorKind::IO,
		}
	}
}

impl From<RcloneRefusal> for Error {
	fn from(refusal: RcloneRefusal) -> Self {
		Self::custom_with_source(refusal.kind(), refusal, None::<&str>)
	}
}
