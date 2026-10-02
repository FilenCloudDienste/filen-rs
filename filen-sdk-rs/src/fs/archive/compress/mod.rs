//! Compressing drive items into an archive in the drive. See
//! [`Client::compress_items`](crate::auth::Client::compress_items).
//!
//! A compression is an archive job: up to [`ArchiveConfig::job_concurrency`] run at once, a later
//! one waiting in [`CompressPhase::WaitingForWorker`]. A job paused while it runs keeps its slot,
//! since its codec's state stays resident; one paused before it got a slot waits the pause out
//! without taking one, so it never keeps a later job from running.
//!
//! [`ArchiveConfig::job_concurrency`]: super::ArchiveConfig::job_concurrency

mod client_impl;
pub(crate) mod codec;
mod engine;
mod report;

pub use super::dispose::{DisposalOutcome, KeptReason, SourceDisposal, SourceDisposition};
pub use client_impl::{CompressConfig, CompressRequest};
pub use report::{
	CompressCallback, CompressCounts, CompressEvent, CompressFailed, CompressPhase, CompressReport,
	CompressUpdate, HashMismatch, RunState,
};

use filen_macros::js_type;

use crate::{
	Error, ErrorKind,
	fs::categories::{NonRootItemType, Normal},
};

pub use super::{
	encode::Compression,
	format::StreamCodec,
	password::ArchivePassword,
	sevenz::write::{SevenZEncryption, SevenZMethod},
	zip::{crypto::AesStrength, write::ZipMethod},
};
pub use crate::fs::drive_job::listing::{ItemSource, ItemSourceDir};

use super::format::{ExtensionFormat, match_extension};

/// What to compress, and whether to remove it afterwards.
#[derive(Debug, Clone)]
pub enum CompressSources {
	/// Any items the client can read: the user's own, shared, or in a link.
	Keep(Vec<ItemSource>),
	/// Items of the user's own drive, removed once the archive is verified: completed with
	/// nothing skipped, every source's data matching the hash in its metadata, the archive
	/// confirmed with the server, and each source still exactly as it was read. Otherwise they
	/// are kept and the report says why. The archive cannot be written into one of them.
	///
	/// The archive is not read back first. With an encrypted format, have the user confirm the
	/// password before removing anything for good: a mistyped one leaves an archive nobody can
	/// open.
	///
	/// An item given twice, or inside another given folder, shares that one's outcome, its bytes
	/// counted there; a file of it that the folder's permanent removal deleted before stopping
	/// is reported removed.
	Dispose {
		/// How the sources are removed.
		how: SourceDisposal,
		/// The items to compress and then remove.
		items: Vec<NonRootItemType<'static, Normal>>,
	},
}

/// What an archive is written as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[js_type(import, export, tagged, camel_case_fields, no_default)]
pub enum CompressFormat {
	/// A tar of the items, bare or compressed as a whole.
	Tar {
		/// The stream the tar is compressed into; `None` for a bare tar.
		#[cfg_attr(feature = "wasm-full", serde(default), tsify(optional))]
		compression: Option<Compression>,
	},
	/// One file, compressed on its own.
	Single {
		/// How the file is compressed.
		compression: Compression,
	},
	/// A zip of the items, each entry compressed on its own, and encrypted with WinZip AES when
	/// `encryption` is set (names stay readable: zip encrypts data only).
	Zip {
		/// How each entry's data is compressed.
		method: ZipMethod,
		/// The AES key size the entries are encrypted with; `None` for no encryption.
		#[cfg_attr(feature = "wasm-full", serde(default), tsify(optional))]
		encryption: Option<AesStrength>,
	},
	/// A 7z of the items: each file compressed on its own, or `solid` (files together in blocks
	/// of up to 2 GiB, which compresses better but reads slower when one file is wanted), and
	/// encrypted with AES-256 when `encryption` is set.
	SevenZ {
		/// How the files are compressed.
		method: SevenZMethod,
		/// Whether the files are compressed together in blocks rather than each on its own.
		solid: bool,
		/// What is encrypted; `None` for no encryption.
		#[cfg_attr(feature = "wasm-full", serde(default), tsify(optional))]
		encryption: Option<SevenZEncryption>,
	},
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
			Self::Zip { .. } => ".zip".to_owned(),
			Self::SevenZ { .. } => ".7z".to_owned(),
		}
	}

	/// Checks the format's levels, and that a password comes exactly with encryption.
	pub(crate) fn check(self, has_password: bool) -> Result<(), Error> {
		let encrypted = match self {
			Self::Zip { encryption, .. } => encryption.is_some(),
			Self::SevenZ { encryption, .. } => encryption.is_some(),
			Self::Tar { .. } | Self::Single { .. } => false,
		};
		match (encrypted, has_password) {
			(true, false) => Err(Error::custom(
				ErrorKind::ArchivePasswordRequired,
				"an encrypted archive needs a password",
			)),
			(false, true) => Err(Error::custom(
				ErrorKind::InvalidState,
				"a password was given for an archive that is not encrypted",
			)),
			_ => self.encoder_memory().map(drop),
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
			Self::Zip { method, .. } => {
				method
					.check()
					.map_err(|message| Error::custom(ErrorKind::InvalidState, message))?;
				Ok(match method {
					ZipMethod::Stored => 0,
					ZipMethod::Deflate { .. } => Compression {
						codec: StreamCodec::Gzip,
						level: None,
					}
					.encoder_memory()?,
					ZipMethod::Bzip2 { level } => Compression {
						codec: StreamCodec::Bzip2,
						level: Some(level),
					}
					.encoder_memory()?,
				})
			}
			Self::SevenZ {
				method, encryption, ..
			} => {
				method
					.check()
					.map_err(|message| Error::custom(ErrorKind::InvalidState, message))?;
				// an encrypted header is compressed with LZMA2 on its own, once the data is done
				Ok(match encryption {
					Some(SevenZEncryption::EntriesAndHeaders) => method
						.encoder_memory()
						.max(super::sevenz::write::header_encoder_memory()),
					_ => method.encoder_memory(),
				})
			}
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
			Self::Zip { .. } => ExtensionFormat::Zip,
			Self::SevenZ { .. } => ExtensionFormat::SevenZ,
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

	#[test]
	fn zip_levels_and_passwords_are_checked_up_front() {
		let zip = |method, encryption| CompressFormat::Zip { method, encryption };
		let aes = Some(AesStrength::Aes256);
		assert_eq!(zip(ZipMethod::Stored, None).extension(), ".zip");
		assert_eq!(zip(ZipMethod::Stored, None).check_name("a.ZIP").unwrap(), 4);
		assert!(zip(ZipMethod::Stored, None).check_name("a.7z").is_err());
		zip(ZipMethod::Stored, None).check(false).unwrap();
		zip(ZipMethod::Deflate { level: 9 }, aes)
			.check(true)
			.unwrap();
		zip(ZipMethod::Bzip2 { level: 1 }, None)
			.check(false)
			.unwrap();
		for level in [0, 10] {
			for method in [ZipMethod::Deflate { level }, ZipMethod::Bzip2 { level }] {
				assert_eq!(
					zip(method, None).check(false).unwrap_err().kind(),
					ErrorKind::InvalidState,
					"{method:?}"
				);
			}
		}
		assert_eq!(
			zip(ZipMethod::Stored, aes).check(false).unwrap_err().kind(),
			ErrorKind::ArchivePasswordRequired
		);
		assert_eq!(
			zip(ZipMethod::Stored, None).check(true).unwrap_err().kind(),
			ErrorKind::InvalidState,
			"a password is never silently dropped"
		);
		assert_eq!(
			CompressFormat::Tar { compression: None }
				.check(true)
				.unwrap_err()
				.kind(),
			ErrorKind::InvalidState
		);
		assert_eq!(zip(ZipMethod::Stored, None).encoder_memory().unwrap(), 0);
		assert_eq!(
			zip(ZipMethod::Bzip2 { level: 9 }, None)
				.encoder_memory()
				.unwrap(),
			7_600_000
		);
	}

	#[test]
	fn every_formats_default_fits_the_smallest_codec_budget() {
		// iOS and wasm budget 128 MiB for a job's codec
		const SMALLEST_BUDGET: u64 = 128 << 20;
		let codecs = [
			StreamCodec::Gzip,
			StreamCodec::Bzip2,
			StreamCodec::Xz,
			StreamCodec::Lzma,
			StreamCodec::Lzip,
			StreamCodec::Lz4,
			StreamCodec::Brotli,
		];
		let mut formats: Vec<CompressFormat> = codecs
			.iter()
			.flat_map(|&codec| {
				[
					CompressFormat::Tar {
						compression: Some(compression(codec)),
					},
					CompressFormat::Single {
						compression: compression(codec),
					},
				]
			})
			.collect();
		formats.push(CompressFormat::Zip {
			method: ZipMethod::Deflate { level: 6 },
			encryption: None,
		});
		for method in [
			SevenZMethod::Lzma2 { level: 6 },
			SevenZMethod::Lzma { level: 6 },
			SevenZMethod::Ppmd { level: 6 },
			SevenZMethod::Bzip2 { level: 9 },
		] {
			formats.push(CompressFormat::SevenZ {
				method,
				solid: true,
				encryption: None,
			});
		}
		for format in formats {
			let memory = format.encoder_memory().unwrap();
			assert!(
				memory <= SMALLEST_BUDGET,
				"{format:?} states {memory} bytes"
			);
		}
	}
}
