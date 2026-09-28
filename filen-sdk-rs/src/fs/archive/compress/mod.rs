//! Compressing drive items into an archive in the drive. See
//! [`Client::compress_items`](crate::auth::Client::compress_items).
//!
//! A compression is an archive job: up to [`ArchiveConfig::job_concurrency`] run at once, a later
//! one waiting in [`CompressPhase::WaitingForWorker`]. A job paused while it runs keeps its slot,
//! since its codec's state stays resident; one paused before it got a slot waits the pause out
//! without taking one, so it never keeps a later job from running, and one paused before it
//! starts waits in [`CompressPhase::Scanning`] before listing anything.
//!
//! [`ArchiveConfig::job_concurrency`]: super::ArchiveConfig::job_concurrency

mod client_impl;
pub(crate) mod codec;
mod engine;
mod read_back;
mod report;

pub use client_impl::CompressConfig;
pub use report::{
	CompressActiveFile, CompressCallback, CompressCounts, CompressEvent, CompressFailed,
	CompressPhase, CompressReport, CompressUpdate, HashMismatch, RunState,
};

use std::ops::RangeInclusive;

use crate::{
	Error, ErrorKind,
	fs::{
		archive::dispose::SourceDisposal,
		categories::{NonRootItemType, Normal},
	},
};

pub use super::{
	encode::Compression,
	format::StreamCodec,
	password::ArchivePassword,
	sevenz::write::{SevenZEncryption, SevenZMethod},
	zip::{crypto::AesStrength, write::ZipMethod},
};
pub use crate::fs::drive_job::{
	listing::{ItemSource, ItemSourceDir, ScanProgress},
	plan::{PlanTotals, RenameReason, RenamedEntry, SkipReason, SkippedEntry},
};

use super::format::{ArchiveFormat, match_extension};

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
	/// Before deleting anything for good, the archive is read back from the server as
	/// extracting would read it ([`CompressPhase::Verifying`], its progress in
	/// [`CompressCounts::bytes_verified`]), which downloads it once more: every entry has to be
	/// its source's data. Trashed sources can be restored, so trashing
	/// reads nothing back. The read back uses the password the archive was written with, so it
	/// cannot tell a mistyped one: with an encrypted format, have the user confirm the password
	/// before removing anything for good.
	///
	/// An item given twice, or inside another given folder, shares that one's outcome, its bytes
	/// counted there; a file of it that the folder's permanent removal deleted before stopping
	/// is reported removed.
	Dispose {
		how: SourceDisposal,
		items: Vec<NonRootItemType<'static, Normal>>,
	},
}

impl CompressSources {
	/// Checks the sources can be written in `format` as far as their kinds tell, before anything
	/// is listed: a single compressed file is made of exactly one file.
	pub(crate) fn check_for(&self, format: CompressFormat) -> Result<(), Error> {
		if !matches!(format, CompressFormat::Single { .. }) {
			return Ok(());
		}
		let one_file = match self {
			Self::Keep(sources) => matches!(sources.as_slice(), [ItemSource::File(_)]),
			Self::Dispose { items, .. } => matches!(items.as_slice(), [NonRootItemType::File(_)]),
		};
		if one_file {
			Ok(())
		} else {
			Err(single_is_one_file())
		}
	}
}

/// The error for a single compressed file asked of anything but one file.
pub(crate) fn single_is_one_file() -> Error {
	Error::custom(
		ErrorKind::InvalidState,
		"a single compressed file is made of exactly one file",
	)
}

/// The level a 7z method is written at unless a UI chooses another: 7-Zip's own default
/// ("normal", its `-mx5`), whose levels the SDK's follow, but for BZip2's (see
/// [`CompressFormat::default_level`]).
const SEVEN_Z_DEFAULT_LEVEL: u32 = 5;

/// What an archive is written as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(
	feature = "wasm-full",
	derive(serde::Serialize, serde::Deserialize, tsify::Tsify),
	tsify(into_wasm_abi, from_wasm_abi),
	serde(
		tag = "type",
		rename_all = "camelCase",
		rename_all_fields = "camelCase"
	)
)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
pub enum CompressFormat {
	/// A tar of the items, bare or compressed as a whole.
	Tar {
		#[cfg_attr(feature = "wasm-full", serde(default), tsify(optional))]
		compression: Option<Compression>,
	},
	/// One file, compressed on its own.
	Single { compression: Compression },
	/// A zip of the items, each entry compressed on its own, and encrypted with WinZip AES when
	/// `encryption` is set (names stay readable: zip encrypts data only).
	Zip {
		method: ZipMethod,
		#[cfg_attr(feature = "wasm-full", serde(default), tsify(optional))]
		encryption: Option<AesStrength>,
	},
	/// A 7z of the items: each file compressed on its own, or `solid` (files together in blocks
	/// of up to 2 GiB, which compresses better but reads slower when one file is wanted), and
	/// encrypted with AES-256 when `encryption` is set.
	SevenZ {
		method: SevenZMethod,
		solid: bool,
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
			Self::Zstd => ".zst",
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

	/// Checks everything about the format a job can know before it starts: its levels, that a
	/// password comes exactly with encryption, and that its encoder fits `budget`.
	pub(crate) fn check_within(self, has_password: bool, budget: u64) -> Result<(), Error> {
		within_budget(self.check(has_password)?, budget).map(drop)
	}

	/// Checks the format's levels, and that a password comes exactly with encryption; the
	/// memory its encoder needs, in bytes.
	pub(crate) fn check(self, has_password: bool) -> Result<u64, Error> {
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
			_ => self.encoder_memory(),
		}
	}

	/// Memory the format's encoder needs, in bytes (0 for a bare tar).
	pub fn encoder_memory(self) -> Result<u64, Error> {
		match self {
			Self::Zip { method, .. } => method.check()?,
			Self::SevenZ { method, .. } => method.check()?,
			Self::Tar { .. } | Self::Single { .. } => {}
		}
		match self {
			Self::Tar { compression: None } => Ok(0),
			Self::Tar {
				compression: Some(compression),
			}
			| Self::Single { compression } => compression.encoder_memory(),
			Self::Zip { method, .. } => Ok(match method {
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
			}),
			// an encrypted header is compressed with LZMA2 on its own, once the data is done
			Self::SevenZ {
				method,
				encryption: Some(SevenZEncryption::EntriesAndHeaders),
				..
			} => Ok(method
				.encoder_memory()
				.max(super::sevenz::write::header_encoder_memory())),
			Self::SevenZ { method, .. } => Ok(method.encoder_memory()),
		}
	}

	/// The encoder's memory, in bytes, refused with [`ErrorKind::InsufficientMemory`] when over
	/// `budget` (a client's is its [`ArchiveConfig::codec_mem_budget`]).
	///
	/// [`ArchiveConfig::codec_mem_budget`]: super::ArchiveConfig::codec_mem_budget
	pub fn check_budget(self, budget: u64) -> Result<u64, Error> {
		within_budget(self.encoder_memory()?, budget)
	}

	/// The levels the format's codec or method takes, for a UI to offer; `None` for one without
	/// levels (a bare tar, a stored zip, a 7z copy). Which of them fit a device is
	/// [`CompressFormat::max_level_within`].
	pub fn levels(self) -> Option<RangeInclusive<u32>> {
		match self {
			Self::Tar { compression: None } => None,
			Self::Tar {
				compression: Some(compression),
			}
			| Self::Single { compression } => Some(compression.codec.levels().0),
			Self::Zip { method, .. } => method.levels(),
			Self::SevenZ { method, .. } => method.levels(),
		}
	}

	/// The level for a UI to preselect among the format's [levels](CompressFormat::levels): a
	/// stream codec's own default (what `level: None` writes), which a zip's Deflate and BZip2
	/// and a 7z's BZip2 share with gzip and bzip2, and 7-Zip's for any other 7z method; `None`
	/// for a format without levels. A 7z's BZip2 levels are bzip2's block sizes, and 7-Zip's
	/// default writes the largest, 900 KB, as bzip2's does.
	pub fn default_level(self) -> Option<u32> {
		let codec = match self {
			Self::Tar { compression: None }
			| Self::Zip {
				method: ZipMethod::Stored,
				..
			}
			| Self::SevenZ {
				method: SevenZMethod::Copy,
				..
			} => return None,
			Self::SevenZ {
				method: SevenZMethod::Bzip2 { .. },
				..
			} => StreamCodec::Bzip2,
			Self::SevenZ { .. } => return Some(SEVEN_Z_DEFAULT_LEVEL),
			Self::Tar {
				compression: Some(compression),
			}
			| Self::Single { compression } => compression.codec,
			Self::Zip {
				method: ZipMethod::Deflate { .. },
				..
			} => StreamCodec::Gzip,
			Self::Zip {
				method: ZipMethod::Bzip2 { .. },
				..
			} => StreamCodec::Bzip2,
		};
		Some(codec.levels().1)
	}

	/// The format at `level`, unchecked; the same format when it has no levels.
	pub fn with_level(self, level: u32) -> Self {
		match self {
			Self::Tar {
				compression: Some(compression),
			} => Self::Tar {
				compression: Some(Compression {
					level: Some(level),
					..compression
				}),
			},
			Self::Single { compression } => Self::Single {
				compression: Compression {
					level: Some(level),
					..compression
				},
			},
			Self::Zip { method, encryption } => Self::Zip {
				method: match method {
					ZipMethod::Stored => ZipMethod::Stored,
					ZipMethod::Deflate { .. } => ZipMethod::Deflate { level },
					ZipMethod::Bzip2 { .. } => ZipMethod::Bzip2 { level },
				},
				encryption,
			},
			Self::SevenZ {
				method,
				solid,
				encryption,
			} => Self::SevenZ {
				method: match method {
					SevenZMethod::Copy => SevenZMethod::Copy,
					SevenZMethod::Lzma2 { .. } => SevenZMethod::Lzma2 { level },
					SevenZMethod::Lzma { .. } => SevenZMethod::Lzma { level },
					SevenZMethod::Ppmd { .. } => SevenZMethod::Ppmd { level },
					SevenZMethod::Bzip2 { .. } => SevenZMethod::Bzip2 { level },
					SevenZMethod::Deflate { .. } => SevenZMethod::Deflate { level },
				},
				solid,
				encryption,
			},
			Self::Tar { compression: None } => self,
		}
	}

	/// The highest of the format's [levels](CompressFormat::levels) whose encoder fits `budget`
	/// bytes (a client's is its [`ArchiveConfig::codec_mem_budget`]); `None` when the format has
	/// no levels or not even its lowest fits.
	///
	/// [`ArchiveConfig::codec_mem_budget`]: super::ArchiveConfig::codec_mem_budget
	pub fn max_level_within(self, budget: u64) -> Option<u32> {
		self.levels()?
			.rev()
			.find(|&level| self.with_level(level).check_budget(budget).is_ok())
	}

	/// The length of the extension `name` ends in, which has to be one this format goes by
	/// (`.tgz` is a `.tar.gz`): readers tell brotli and LZMA streams by their extension alone.
	pub(crate) fn check_name(self, name: &str) -> Result<usize, Error> {
		let expected = match self {
			Self::Tar { compression } => ArchiveFormat::Tar {
				codec: compression.map(|compression| compression.codec),
			},
			Self::Single { compression } => ArchiveFormat::Single {
				codec: compression.codec,
			},
			Self::Zip { .. } => ArchiveFormat::Zip,
			Self::SevenZ { .. } => ArchiveFormat::SevenZ,
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

/// `memory`, an encoder's, refused with [`ErrorKind::InsufficientMemory`] when over `budget`.
fn within_budget(memory: u64, budget: u64) -> Result<u64, Error> {
	if memory > budget {
		return Err(Error::custom(
			ErrorKind::InsufficientMemory,
			format!("this format needs {memory} bytes of codec memory, over the {budget} allowed"),
		));
	}
	Ok(memory)
}

#[cfg(test)]
mod tests {
	use filen_types::fs::Uuid;

	use super::*;
	use crate::fs::archive::test_support::remote_file;

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
	fn a_formats_default_level_is_one_it_takes_and_fits_every_budget() {
		const SMALLEST_BUDGET: u64 = 128 << 20;
		let zip = |method| CompressFormat::Zip {
			method,
			encryption: None,
		};
		let sevenz = |method| CompressFormat::SevenZ {
			method,
			solid: true,
			encryption: None,
		};
		let leveled = [
			(
				CompressFormat::Tar {
					compression: Some(compression(StreamCodec::Xz)),
				},
				6,
			),
			(
				CompressFormat::Single {
					compression: compression(StreamCodec::Brotli),
				},
				9,
			),
			(zip(ZipMethod::Deflate { level: 1 }), 6),
			(zip(ZipMethod::Bzip2 { level: 1 }), 9),
			(sevenz(SevenZMethod::Lzma2 { level: 0 }), 5),
			(sevenz(SevenZMethod::Ppmd { level: 1 }), 5),
			(sevenz(SevenZMethod::Bzip2 { level: 1 }), 9),
		];
		for (format, default) in leveled {
			assert_eq!(format.default_level(), Some(default), "{format:?}");
			assert!(format.levels().unwrap().contains(&default), "{format:?}");
			assert!(
				format
					.with_level(default)
					.check_budget(SMALLEST_BUDGET)
					.is_ok(),
				"{format:?}"
			);
		}
		for format in [
			CompressFormat::Tar { compression: None },
			zip(ZipMethod::Stored),
			sevenz(SevenZMethod::Copy),
		] {
			assert_eq!(format.default_level(), None, "{format:?}");
		}
	}

	#[test]
	fn a_single_compressed_file_is_made_of_one_file() {
		let single = CompressFormat::Single {
			compression: compression(StreamCodec::Gzip),
		};
		let file = |name: &str| {
			ItemSource::File(remote_file(
				Uuid::from_u128(1),
				Uuid::from_u128(2),
				name,
				b"data",
				None,
			))
		};
		let sources = CompressSources::Keep;
		assert!(sources(vec![file("a.txt")]).check_for(single).is_ok());
		for sources in [
			sources(Vec::new()),
			sources(vec![file("a.txt"), file("b.txt")]),
		] {
			assert_eq!(
				sources.check_for(single).unwrap_err().kind(),
				ErrorKind::InvalidState
			);
			assert!(
				sources
					.check_for(CompressFormat::Tar { compression: None })
					.is_ok()
			);
		}
	}

	#[test]
	fn a_formats_levels_and_what_fits_a_budget_are_known_up_front() {
		const SMALLEST_BUDGET: u64 = 128 << 20;
		let tar = |codec| CompressFormat::Tar {
			compression: Some(compression(codec)),
		};
		let sevenz = |method| CompressFormat::SevenZ {
			method,
			solid: true,
			encryption: Some(SevenZEncryption::Entries),
		};
		assert_eq!(tar(StreamCodec::Brotli).levels(), Some(0..=11));
		assert_eq!(tar(StreamCodec::Lz4).levels(), Some(1..=1));
		assert_eq!(CompressFormat::Tar { compression: None }.levels(), None);
		let deflate = CompressFormat::Zip {
			method: ZipMethod::Deflate { level: 6 },
			encryption: None,
		};
		assert_eq!(deflate.levels(), Some(1..=9));
		assert_eq!(
			CompressFormat::Zip {
				method: ZipMethod::Stored,
				encryption: None,
			}
			.levels(),
			None
		);
		assert_eq!(
			sevenz(SevenZMethod::Lzma2 { level: 5 }).levels(),
			Some(0..=9)
		);
		assert_eq!(
			sevenz(SevenZMethod::Ppmd { level: 5 }).levels(),
			Some(1..=9)
		);
		assert_eq!(sevenz(SevenZMethod::Copy).levels(), None);
		assert_eq!(
			sevenz(SevenZMethod::Ppmd { level: 5 }).with_level(9),
			sevenz(SevenZMethod::Ppmd { level: 9 }),
			"only the level changes"
		);

		// the dictionaries and models that fit a phone's budget
		assert_eq!(
			sevenz(SevenZMethod::Lzma2 { level: 9 }).max_level_within(SMALLEST_BUDGET),
			Some(6)
		);
		assert_eq!(
			sevenz(SevenZMethod::Ppmd { level: 9 }).max_level_within(SMALLEST_BUDGET),
			Some(7)
		);
		assert_eq!(tar(StreamCodec::Xz).max_level_within(256 << 20), Some(7));
		assert_eq!(
			tar(StreamCodec::Gzip).max_level_within(SMALLEST_BUDGET),
			Some(9)
		);
		assert_eq!(tar(StreamCodec::Xz).max_level_within(1), None);
		assert_eq!(
			sevenz(SevenZMethod::Ppmd { level: 8 })
				.check_budget(SMALLEST_BUDGET)
				.unwrap_err()
				.kind(),
			ErrorKind::InsufficientMemory
		);
		assert_eq!(
			sevenz(SevenZMethod::Ppmd { level: 7 })
				.check_budget(SMALLEST_BUDGET)
				.unwrap(),
			sevenz(SevenZMethod::Ppmd { level: 7 })
				.encoder_memory()
				.unwrap()
		);
		assert_eq!(
			deflate
				.check_within(true, SMALLEST_BUDGET)
				.unwrap_err()
				.kind(),
			ErrorKind::InvalidState,
			"the password is checked with the budget"
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
			StreamCodec::Zstd,
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
