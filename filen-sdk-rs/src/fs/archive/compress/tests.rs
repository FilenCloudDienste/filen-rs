use filen_types::fs::Uuid;

use super::*;
use crate::fs::{archive::test_support::archive_password, drive_job::test_support::remote_file};

/// The smallest codec budget a platform runs jobs with: iOS's and wasm's.
const SMALLEST_BUDGET: u64 = 128 << 20;

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
	let password = || Some(archive_password("pw"));
	zip(ZipMethod::Stored, None).check(None).unwrap();
	let checked = zip(ZipMethod::Deflate { level: 9 }, aes)
		.check(password())
		.unwrap();
	assert!(
		matches!(
			checked,
			CheckedFormat::Archive(CheckedArchive::Zip {
				encryption: Some((AesStrength::Aes256, _)),
				..
			})
		),
		"the checked format holds its password"
	);
	zip(ZipMethod::Bzip2 { level: 1 }, None)
		.check(None)
		.unwrap();
	for level in [0, 10] {
		for method in [ZipMethod::Deflate { level }, ZipMethod::Bzip2 { level }] {
			assert_eq!(
				zip(method, None).check(None).unwrap_err().kind(),
				ErrorKind::InvalidState,
				"{method:?}"
			);
		}
	}
	assert_eq!(
		zip(ZipMethod::Stored, aes).check(None).unwrap_err().kind(),
		ErrorKind::ArchivePasswordRequired
	);
	assert_eq!(
		zip(ZipMethod::Stored, None)
			.check(password())
			.unwrap_err()
			.kind(),
		ErrorKind::InvalidState,
		"a password is never silently dropped"
	);
	assert_eq!(
		CompressFormat::Tar { compression: None }
			.check(password())
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
			.check_within(Some(archive_password("pw")), SMALLEST_BUDGET,)
			.unwrap_err()
			.kind(),
		ErrorKind::InvalidState,
		"the password is checked with the budget"
	);
}

#[test]
fn every_formats_default_fits_the_smallest_codec_budget() {
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
