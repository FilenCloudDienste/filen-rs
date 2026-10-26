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
	/// The system has no mount backend: WinFsp on Windows, FUSE 3 on Linux. macOS always has one.
	#[error("no FUSE is installed")]
	FuseMissing,
	/// Something is mounted at the mount point already.
	#[error("the mount point is in use")]
	MountPointInUse,
	/// The mount point directory is not empty.
	#[error("the mount point is not empty")]
	MountPointNotEmpty,
	/// The mount point is not an existing directory by an absolute path that is not a symlink, or
	/// on Windows not a drive letter.
	#[error("the mount point is not a usable directory or drive letter")]
	MountPointInvalid,
	/// The drive letter is taken by another drive.
	#[error("the drive letter is in use")]
	DriveLetterInUse,
	/// Another process listens on the serve's port.
	#[error("port {port} is in use")]
	PortInUse { port: u16 },
	/// The serve's address is not one of this machine's.
	#[error("the address is not one of this machine's")]
	AddressNotLocal,
	/// The serve's host is not an IP address or `localhost` (an empty one would listen on every
	/// interface), or its port is 0.
	#[error("the address must name an IP address or localhost and a port")]
	ImplicitAddress,
	/// The protocol needs credentials of its kind (access keys for s3, a user and password for the
	/// others but nfs) and was given none or the other kind; or the serve answered a request that
	/// carried none, and was stopped.
	#[error("the serve requires authentication")]
	AuthRequired,
	/// A credential is empty, too long or holds a character it may not: any control character, a
	/// `:` in a user name, a `,` or `"` in an s3 secret; an s3 access key ID is not 16 to 128
	/// letters and digits; or credentials were given to nfs, which takes none.
	#[error("the serve's credentials are not valid")]
	InvalidCredentials,
	/// An unauthenticated serve (nfs) may listen on loopback only.
	#[error("an unauthenticated serve may listen on loopback only")]
	UnauthenticatedOffLoopback,
	/// The protocol is not served on this platform or by this rclone.
	#[error("the protocol is not supported here")]
	UnsupportedProtocol,
	/// The protocol does not serve over TLS: only webdav, s3, http and restic do.
	#[error("the protocol does not serve over TLS")]
	TlsUnsupported,
}

impl RcloneRefusal {
	/// The error kind a refusal is returned as: [`ErrorKind::IO`] for what is wrong on this
	/// machine, [`ErrorKind::InvalidState`] for a serve's options the daemon does not run.
	pub fn kind(&self) -> ErrorKind {
		match self {
			Self::BinaryMissing
			| Self::BinaryChecksum
			| Self::UnsupportedVersion { .. }
			| Self::WatchdogUnavailable
			| Self::FuseMissing
			| Self::MountPointInUse
			| Self::MountPointNotEmpty
			| Self::MountPointInvalid
			| Self::DriveLetterInUse
			| Self::PortInUse { .. }
			| Self::AddressNotLocal
			| Self::UnsupportedProtocol => ErrorKind::IO,
			Self::ImplicitAddress
			| Self::AuthRequired
			| Self::InvalidCredentials
			| Self::UnauthenticatedOffLoopback
			| Self::TlsUnsupported => ErrorKind::InvalidState,
		}
	}
}

impl From<RcloneRefusal> for Error {
	fn from(refusal: RcloneRefusal) -> Self {
		Self::custom_with_source(refusal.kind(), refusal, None::<&str>)
	}
}
