use crate::{Error, ErrorKind};

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
}

impl RcloneRefusal {
	/// The error kind a refusal is returned as: [`ErrorKind::IO`] for what is wrong on this machine.
	pub fn kind(&self) -> ErrorKind {
		match self {
			Self::BinaryMissing | Self::BinaryChecksum => ErrorKind::IO,
		}
	}
}

impl From<RcloneRefusal> for Error {
	fn from(refusal: RcloneRefusal) -> Self {
		Self::custom_with_source(refusal.kind(), refusal, None::<&str>)
	}
}
