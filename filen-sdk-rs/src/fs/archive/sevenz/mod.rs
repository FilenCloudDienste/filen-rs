//! 7z archives, read and written in-house over the codec crates (lzma-rust2's LZMA, LZMA2 and
//! branch filters, ppmd-rust, bzip2, flate2): 7z crates parse headers with allocations sized by
//! the archive, which a hostile archive turns into an out-of-memory abort, where here every
//! count and size is checked against what is left of the header and the job's budgets first.

pub(crate) mod crypto;
pub(crate) mod header;
pub(crate) mod read;
pub(crate) mod write;

use std::io;

#[derive(Debug, thiserror::Error)]
pub(crate) enum SevenZError {
	#[error("the 7z archive is damaged: {0}")]
	Corrupt(&'static str),
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
