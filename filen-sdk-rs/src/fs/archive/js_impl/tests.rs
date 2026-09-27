use chrono::Utc;
use filen_types::fs::ParentUuid;

use super::*;
use crate::{
	crypto::{file::FileKey, shared::CreateRandom, v3::EncryptionKey},
	fs::{
		HasUUID,
		archive::{
			compress::{SevenZMethod, StreamCodec, ZipMethod},
			config::CODEC_MEM_BUDGET,
			dispose::{DisposalOutcome, KeptReason, SourceDisposition},
			zip::crypto::AesStrength,
		},
		dir::{
			RemoteDirectory, RootDirectory,
			meta::{DecryptedDirectoryMeta, DirectoryMeta},
		},
		file::meta::{DecryptedFileMeta, FileMeta},
	},
	job::report::JobFailed,
	js::Root,
};

fn remote_file() -> RemoteFile {
	RemoteFile::from_meta(
		Uuid::new_v4(),
		filen_types::fs::StableUuid::new_for_test(Uuid::new_v4()),
		Uuid::new_v4().into(),
		10,
		1,
		"de-1",
		"bucket",
		Utc::now(),
		false,
		FileMeta::Decoded(DecryptedFileMeta {
			name: Cow::Borrowed("a.zip"),
			size: 10,
			mime: Cow::Borrowed("application/zip"),
			key: FileKey::V3(EncryptionKey::generate()),
			last_modified: Utc::now(),
			created: None,
			hash: None,
		}),
	)
}

fn dir() -> RemoteDirectory {
	RemoteDirectory::from_meta(
		Uuid::new_v4(),
		ParentUuid::Uuid(Uuid::new_v4()),
		filen_types::api::v3::dir::color::DirColor::Blue,
		false,
		Utc::now(),
		DirectoryMeta::Decoded(DecryptedDirectoryMeta {
			name: Cow::Borrowed("Photos"),
			created: None,
		}),
	)
}

fn destination() -> AnyNormalDir {
	AnyNormalDir::Dir(dir().into())
}

#[test]
fn an_archive_to_remove_has_to_be_the_users_own() {
	let file = remote_file();
	let request = extract_request(
		AnyFile::File(file.clone().into()),
		destination(),
		ExtractInto::NewFolder { name: None },
		Some(SourceDisposal::Trash),
	)
	.unwrap();
	let ExtractRequest::All {
		archive: ArchiveSource::Dispose {
			file: disposed,
			how,
		},
		root: ExtractRoot::NewFolder { name: None },
		..
	} = request
	else {
		panic!("an archive to remove, into a new folder");
	};
	assert_eq!((disposed.uuid(), how), (file.uuid(), SourceDisposal::Trash));

	let error = extract_request(
		AnyFile::File(remote_file().into()),
		destination(),
		ExtractInto::NewFolder {
			name: Some(String::new()),
		},
		Some(SourceDisposal::DeletePermanently),
	)
	.unwrap_err();
	assert_eq!(error.kind(), ErrorKind::InvalidName, "an empty folder name");
}

#[test]
fn names_and_passwords_are_checked_at_the_edge() {
	let error = extract_request(
		AnyFile::File(remote_file().into()),
		destination(),
		ExtractInto::NewFolder {
			name: Some("a/b".into()),
		},
		None,
	)
	.unwrap_err();
	assert_eq!(error.kind(), ErrorKind::InvalidName);
	assert_eq!(
		password(Some(String::new())).unwrap_err().kind(),
		ErrorKind::InvalidState
	);
	assert!(password(None).unwrap().is_none());
	let call = |name, format, budget| {
		CompressCall::new(
			Vec::new(),
			destination(),
			name,
			CompressConfig {
				format,
				max_bytes: None,
				password: None,
			},
			None,
			budget,
		)
	};
	let stored = CompressFormat::Zip {
		method: ZipMethod::Stored,
		encryption: None,
	};
	assert_eq!(
		call("a/b.zip", stored, CODEC_MEM_BUDGET)
			.err()
			.unwrap()
			.kind(),
		ErrorKind::InvalidName
	);
	// the encoder has to fit the client's budget: 7z PPMd 8 needs 129 MiB
	let ppmd = CompressFormat::SevenZ {
		method: SevenZMethod::Ppmd { level: 8 },
		solid: false,
		encryption: None,
	};
	assert_eq!(
		call("a.7z", ppmd, 128 << 20).err().unwrap().kind(),
		ErrorKind::InsufficientMemory
	);
	assert!(call("a.7z", ppmd, 256 << 20).is_ok());
}

#[test]
fn items_to_remove_have_to_be_the_users_own_and_not_the_root() {
	let file = remote_file();
	let dir = dir();
	let sources = compress_sources(
		vec![
			AnyItemWithContext::File(AnyFile::File(file.clone().into())),
			AnyItemWithContext::Dir(AnyDirWithContext::Normal(AnyNormalDir::Dir(
				dir.clone().into(),
			))),
		],
		Some(SourceDisposal::DeletePermanently),
	)
	.unwrap();
	let CompressSources::Dispose { how, items } = sources else {
		panic!("sources to remove");
	};
	assert_eq!(how, SourceDisposal::DeletePermanently);
	assert_eq!(
		items.iter().map(HasUUID::uuid).collect::<Vec<_>>(),
		[file.uuid(), dir.uuid()]
	);
	let root = AnyItemWithContext::Dir(AnyDirWithContext::Normal(AnyNormalDir::Root(Root::from(
		RootDirectory::new(Uuid::new_v4()),
	))));
	for dispose in [None, Some(SourceDisposal::Trash)] {
		let error = compress_sources(vec![root.clone()], dispose).unwrap_err();
		assert_eq!(error.kind(), ErrorKind::InvalidState, "{dispose:?}");
	}
}

#[test]
fn helpers_name_the_formats() {
	assert_eq!(
		archive_extension(CompressFormat::SevenZ {
			method: SevenZMethod::Lzma2 { level: 5 },
			solid: true,
			encryption: None,
		}),
		".7z"
	);
	assert_eq!(
		archive_encoder_memory(CompressFormat::Zip {
			method: ZipMethod::Deflate { level: 10 },
			encryption: Some(AesStrength::Aes256),
		})
		.unwrap_err()
		.kind(),
		ErrorKind::InvalidState
	);
	assert_eq!(archive_default_name("photos.tar.gz".into()), "photos");
	assert_eq!(
		archive_default_name("..zip".into()),
		String::from(extract::archive_default_name("..zip")),
		"the name extraction gives the folder"
	);
}

#[test]
fn helpers_tell_a_formats_levels_and_what_fits() {
	let deflate = CompressFormat::Zip {
		method: ZipMethod::Deflate { level: 6 },
		encryption: None,
	};
	assert_eq!(
		archive_format_levels(deflate),
		Some(ArchiveLevels { min: 1, max: 9 })
	);
	assert_eq!(
		archive_format_levels(CompressFormat::Tar { compression: None }),
		None
	);
	let lzma2 = CompressFormat::SevenZ {
		method: SevenZMethod::Lzma2 { level: 1 },
		solid: true,
		encryption: None,
	};
	// levels up to 6 need 97 MiB, 7 193 MiB, 8 385 MiB
	assert_eq!(archive_max_level(lzma2, 128 << 20), Some(6));
	assert_eq!(archive_max_level(lzma2, 256 << 20), Some(7));
	assert_eq!(archive_max_level(lzma2, 1 << 20), None);
}

#[test]
fn a_names_extension_tells_what_it_holds() {
	let tar = |codec| Some(ArchiveFormat::Tar { codec });
	for (name, format) in [
		("photos.TGZ", tar(Some(StreamCodec::Gzip))),
		("photos.tar.zst", tar(Some(StreamCodec::Zstd))),
		("photos.tar", tar(None)),
		(
			"notes.txt.gz",
			Some(ArchiveFormat::Single {
				codec: StreamCodec::Gzip,
			}),
		),
		("a.zip", Some(ArchiveFormat::Zip)),
		("a.7z", Some(ArchiveFormat::SevenZ)),
		("notes.txt", None),
		(".zip", None),
	] {
		assert_eq!(archive_format_of_name(name.into()), format, "{name}");
	}
}

#[test]
fn a_report_carries_why_the_job_ended_and_what_became_of_its_sources() {
	let removal = Arc::new(Error::custom(ErrorKind::Server, "no"));
	let report = extract::ExtractReport {
		top_level: Vec::new(),
		failures: Vec::new(),
		skipped: Vec::new(),
		renamed: Vec::new(),
		misleading_names: Vec::new(),
		omitted: OmittedRecords::default(),
		totals: ArchiveTotals::Streaming { archive_bytes: 0 },
		counts: ItemCounts::default(),
		unaccounted_bytes: 0,
		duplicates: None,
		dispositions: vec![
			SourceDisposition {
				uuid: Uuid::new_v4(),
				outcome: DisposalOutcome::Kept {
					reason: KeptReason::Failed {
						error: Arc::clone(&removal),
					},
					bytes_freed: 3,
				},
			},
			SourceDisposition {
				uuid: Uuid::new_v4(),
				outcome: DisposalOutcome::Disposed {
					how: SourceDisposal::DeletePermanently,
					bytes_freed: 7,
				},
			},
		],
	};
	let ended = Arc::new(Error::custom(ErrorKind::Cancelled, "cancelled"));
	let report = ExtractReport::from(JobFailed {
		report,
		error: Arc::clone(&ended),
	});
	assert!(Arc::ptr_eq(report.error.as_ref().unwrap(), &ended));
	let [first, second] = report.dispositions.as_slice() else {
		panic!("two dispositions");
	};
	let ArchiveDisposalOutcome::Kept {
		reason: ArchiveKeptReason::Failed { error },
		bytes_freed: 3,
	} = &first.outcome
	else {
		panic!("{:?}", first.outcome);
	};
	assert!(Arc::ptr_eq(error, &removal));
	assert!(matches!(
		second.outcome,
		ArchiveDisposalOutcome::Disposed { bytes_freed: 7, .. }
	));
	assert!(
		CompressReport::from(compress::CompressReport::default())
			.error
			.is_none()
	);
}
