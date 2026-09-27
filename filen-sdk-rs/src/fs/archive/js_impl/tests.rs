use chrono::Utc;
use filen_types::fs::ParentUuid;

use super::*;
use crate::{
	crypto::{file::FileKey, shared::CreateRandom, v3::EncryptionKey},
	fs::{
		HasUUID,
		archive::{
			compress::{SevenZMethod, ZipMethod},
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
	let call = CompressCall::new(
		Vec::new(),
		destination(),
		"a/b.zip",
		CompressFormat::Zip {
			method: ZipMethod::Stored,
			encryption: None,
		},
		None,
		None,
		None,
	);
	assert_eq!(call.err().unwrap().kind(), ErrorKind::InvalidName);
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
