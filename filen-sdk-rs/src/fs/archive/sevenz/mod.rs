//! 7z archives, read and written in-house over the codec crates (lzma-rust2's LZMA, LZMA2 and
//! branch filters, ppmd-rust, bzip2, flate2): 7z crates parse headers with allocations sized by
//! the archive, which a hostile archive turns into an out-of-memory abort, where here every
//! count and size is checked against what is left of the header and the job's budgets first.

pub(crate) mod crypto;
pub(crate) mod header;
pub(crate) mod read;
pub(crate) mod write;

use std::io;

use crate::{Error, ErrorKind};

use super::error::read_failure;

#[derive(Debug, thiserror::Error)]
pub(crate) enum SevenZError {
	#[error("the 7z archive is damaged: {0}")]
	Corrupt(&'static str),
	/// Reading past what a folder decodes to: under a key not yet proven, a sign of the wrong
	/// one.
	#[error("the 7z archive is damaged: a 7z folder ends early")]
	FolderEndsEarly,
	/// AES-CBC data of other than a whole number of blocks, which no key decrypts.
	#[error("the 7z archive is damaged: 7z AES data is not a whole number of blocks")]
	AesPartialBlock,
	/// Data a coder's decoder rejected, which is the source: its message may quote the data, so
	/// it is not part of this one.
	#[error(
		"the 7z archive is damaged: a 7z coder's data {}",
		if .0.kind() == io::ErrorKind::UnexpectedEof { "ends early" } else { "does not decode" }
	)]
	Decode(#[source] io::Error),
	#[error("the 7z archive uses a feature that isn't supported: {0}")]
	Unsupported(&'static str),
	#[error("the 7z archive is too large to read: {0}")]
	TooLarge(&'static str),
	#[error("the 7z archive is encrypted and needs a password")]
	PasswordRequired,
	#[error("the password is wrong")]
	WrongPassword,
	#[error(transparent)]
	Read(#[from] io::Error),
}

impl From<SevenZError> for Error {
	fn from(error: SevenZError) -> Self {
		let kind = match error {
			SevenZError::Read(error) => return read_failure(error),
			SevenZError::Corrupt(_)
			| SevenZError::FolderEndsEarly
			| SevenZError::AesPartialBlock
			| SevenZError::Decode(_) => ErrorKind::ArchiveCorrupt,
			SevenZError::Unsupported(_) => ErrorKind::ArchiveUnsupported,
			SevenZError::TooLarge(_) => ErrorKind::ArchiveTooLarge,
			SevenZError::PasswordRequired => ErrorKind::ArchivePasswordRequired,
			SevenZError::WrongPassword => ErrorKind::ArchiveWrongPassword,
		};
		Error::custom_with_source(kind, error, None::<&str>)
	}
}

impl SevenZError {
	/// Whether the archive's data is damaged, which under a key not yet proven is likelier the
	/// wrong key.
	pub(crate) fn is_damage(&self) -> bool {
		matches!(
			self,
			Self::Corrupt(_) | Self::FolderEndsEarly | Self::AesPartialBlock | Self::Decode(_)
		)
	}
}
