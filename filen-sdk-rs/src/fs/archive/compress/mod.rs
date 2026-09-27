//! Compressing drive items into an archive in the drive. See
//! [`Client::compress_items`](crate::auth::Client::compress_items).

mod client_impl;
pub(crate) mod codec;
mod engine;
mod report;

pub use client_impl::CompressConfig;
pub use report::{
	CompressCallback, CompressCounts, CompressEvent, CompressFailed, CompressPhase, CompressReport,
	CompressUpdate, RunState,
};

use crate::{Error, ErrorKind};

pub use super::encode::Compression;

use super::format::{ExtensionFormat, StreamCodec, match_extension};

/// What an archive is written as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompressFormat {
	/// A tar of the items, bare or compressed as a whole.
	Tar { compression: Option<Compression> },
	/// One file, compressed on its own.
	Single { compression: Compression },
}

impl StreamCodec {
	/// The file-name extension of a file compressed with the codec, dot included.
	pub fn extension(self) -> &'static str {
		match self {
			Self::Gzip => ".gz",
			Self::Bzip2 => ".bz2",
			Self::Xz => ".xz",
			Self::Lzma => ".lzma",
			Self::Lzip => ".lz",
			Self::Lz4 => ".lz4",
			Self::Brotli => ".br",
		}
	}
}

impl CompressFormat {
	/// The file-name extension an archive in this format carries, dot included: `.tar`,
	/// `.tar.gz`, `.gz`, ...
	pub fn extension(self) -> String {
		match self {
			Self::Tar { compression: None } => ".tar".to_owned(),
			Self::Tar {
				compression: Some(compression),
			} => format!(".tar{}", compression.codec.extension()),
			Self::Single { compression } => compression.codec.extension().to_owned(),
		}
	}

	/// Memory the format's encoder needs, in bytes (0 for a bare tar).
	pub fn encoder_memory(self) -> Result<u64, Error> {
		match self {
			Self::Tar { compression: None } => Ok(0),
			Self::Tar {
				compression: Some(compression),
			}
			| Self::Single { compression } => compression.encoder_memory(),
		}
	}

	/// The length of the extension `name` ends in, which has to be one this format goes by
	/// (`.tgz` is a `.tar.gz`): readers tell brotli and LZMA streams by their extension alone.
	pub(crate) fn check_name(self, name: &str) -> Result<usize, Error> {
		let expected = match self {
			Self::Tar { compression: None } => ExtensionFormat::Tar,
			Self::Tar {
				compression: Some(compression),
			} => ExtensionFormat::CompressedTar(compression.codec),
			Self::Single { compression } => ExtensionFormat::Stream(compression.codec),
		};
		match match_extension(name) {
			Some((extension, format)) if format == expected => Ok(extension.len()),
			_ => Err(Error::custom(
				ErrorKind::InvalidName,
				format!("an archive in this format is named *{}", self.extension()),
			)),
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn compression(codec: StreamCodec) -> Compression {
		Compression { codec, level: None }
	}

	#[test]
	fn names_must_carry_the_formats_extension() {
		let tgz = CompressFormat::Tar {
			compression: Some(compression(StreamCodec::Gzip)),
		};
		assert_eq!(tgz.extension(), ".tar.gz");
		assert_eq!(tgz.check_name("photos.tar.gz").unwrap(), 7);
		assert_eq!(tgz.check_name("photos.TGZ").unwrap(), 4);
		for wrong in ["photos.gz", "photos.tar", "photos.tar.xz", "photos"] {
			assert_eq!(
				tgz.check_name(wrong).unwrap_err().kind(),
				ErrorKind::InvalidName,
				"{wrong}"
			);
		}
		let single = CompressFormat::Single {
			compression: compression(StreamCodec::Brotli),
		};
		assert_eq!(single.check_name("notes.txt.br").unwrap(), 3);
		assert!(single.check_name("notes.tar.br").is_err());
		assert_eq!(
			CompressFormat::Tar { compression: None }
				.check_name("a.tar")
				.unwrap(),
			4
		);
	}
}
