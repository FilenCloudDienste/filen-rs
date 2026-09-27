//! Compressing items into archives in the drive and extracting them again, against the live
//! backend: every format family, encryption, and removing the sources afterwards.

use std::sync::{Arc, Mutex};

use filen_macros::shared_test_runtime;
use filen_sdk_rs::{
	ErrorKind,
	auth::Client,
	consts::CHUNK_SIZE,
	fs::{
		HasName, HasUUID,
		archive::{
			AesStrength, ArchivePassword, DisposalOutcome, JobControl, SevenZEncryption,
			SevenZMethod, SourceDisposal,
			compress::{
				CompressCallback, CompressConfig, CompressFormat, CompressSources, CompressUpdate,
				Compression, ItemSource, ItemSourceDir, StreamCodec, ZipMethod,
			},
			extract::{
				ArchiveSource, ExtractCallback, ExtractConfig, ExtractRequest, ExtractRoot,
				ExtractUpdate, ExtractedTopLevel,
			},
		},
		categories::NonRootItemType,
		dir::RemoteDirectory,
		file::{RemoteFile, traits::HasFileInfo},
		name::ValidatedName,
	},
};

mod copy_helpers;
use copy_helpers::{assert_same_files, contents, data, upload};

#[derive(Default)]
struct CompressRecorder {
	archives: Mutex<Vec<RemoteFile>>,
}

impl CompressCallback for CompressRecorder {
	fn on_archive_created(&self, archive: RemoteFile) {
		self.archives.lock().unwrap().push(archive);
	}

	fn on_update(&self, _: CompressUpdate) {}
}

#[derive(Default)]
struct ExtractRecorder {
	top_level: Mutex<Vec<ExtractedTopLevel>>,
}

impl ExtractCallback for ExtractRecorder {
	fn on_top_level_created(&self, items: Vec<ExtractedTopLevel>) {
		self.top_level.lock().unwrap().extend(items);
	}

	fn on_update(&self, _: ExtractUpdate) {}
}

/// A folder with a file over a chunk, a small one, an empty one and a subfolder.
async fn tree(client: &Client, parent: &RemoteDirectory) -> (RemoteDirectory, Vec<RemoteFile>) {
	let source = client.create_dir(&parent.into(), "source").await.unwrap();
	let sub = client.create_dir(&(&source).into(), "sub").await.unwrap();
	let files = vec![
		upload(client, &source, "big.bin", &data(CHUNK_SIZE + 4321, 7)).await,
		upload(client, &source, "notes.txt", b"hello archive").await,
		upload(client, &sub, "empty", b"").await,
	];
	(source, files)
}

async fn compress(
	client: &Arc<Client>,
	sources: CompressSources,
	destination: &RemoteDirectory,
	name: &str,
	format: CompressFormat,
	password: Option<&str>,
) -> filen_sdk_rs::fs::archive::compress::CompressReport {
	client
		.clone()
		.compress_items(
			sources,
			destination.clone().into(),
			ValidatedName::try_from(name).unwrap(),
			CompressConfig {
				format,
				max_bytes: None,
				password: password.map(|p| ArchivePassword::new(p.into()).unwrap()),
			},
			CompressRecorder::default(),
			JobControl::default(),
		)
		.await
		.unwrap_or_else(|failed| panic!("compressing {name}: {}", failed.error))
}

async fn extract(
	client: &Arc<Client>,
	archive: ArchiveSource,
	destination: &RemoteDirectory,
	password: Option<&str>,
) -> Result<filen_sdk_rs::fs::archive::extract::ExtractReport, ErrorKind> {
	client
		.clone()
		.extract_archive(
			ExtractRequest::All {
				archive,
				destination: destination.clone().into(),
				root: ExtractRoot::NewFolder { name: None },
			},
			ExtractConfig {
				password: password.map(|p| ArchivePassword::new(p.into()).unwrap()),
				..ExtractConfig::default()
			},
			ExtractRecorder::default(),
			JobControl::default(),
		)
		.await
		.map_err(|failed| failed.error.kind())
}

/// The folder an extract created in `destination`.
fn created_folder(report: &filen_sdk_rs::fs::archive::extract::ExtractReport) -> RemoteDirectory {
	let [top] = &report.top_level[..] else {
		panic!("one new folder: {:?}", report.top_level.len());
	};
	match &top.item {
		NonRootItemType::Dir(dir) => dir.clone().into_owned(),
		NonRootItemType::File(_) => panic!("a folder"),
	}
}

#[shared_test_runtime]
async fn every_format_family_round_trips_through_the_drive() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = resources.client.clone();
	let test_dir = &resources.dir;
	let (source, files) = tree(&client, test_dir).await;
	let formats: [(&str, CompressFormat, Option<&str>); 4] = [
		(
			"bundle.tar.gz",
			CompressFormat::Tar {
				compression: Some(Compression {
					codec: StreamCodec::Gzip,
					level: None,
				}),
			},
			None,
		),
		(
			"bundle.tar.xz",
			CompressFormat::Tar {
				compression: Some(Compression {
					codec: StreamCodec::Xz,
					level: Some(6),
				}),
			},
			None,
		),
		(
			"bundle.zip",
			CompressFormat::Zip {
				method: ZipMethod::Deflate { level: 6 },
				encryption: Some(AesStrength::Aes256),
			},
			Some("zip password"),
		),
		(
			"bundle.7z",
			CompressFormat::SevenZ {
				method: SevenZMethod::Lzma2 { level: 5 },
				solid: true,
				encryption: Some(SevenZEncryption::EntriesAndHeaders),
			},
			Some("7z password"),
		),
	];
	for (name, format, password) in formats {
		let destination = client.create_dir(&test_dir.into(), name).await.unwrap();
		let report = compress(
			&client,
			CompressSources::Keep(vec![ItemSource::Dir(ItemSourceDir::Normal(source.clone()))]),
			&destination,
			name,
			format,
			password,
		)
		.await;
		let archive = report.archive.expect("the archive is registered");
		assert_eq!(archive.name(), Some(name));
		assert_eq!(report.counts.files_done, 3, "{name}");

		if password.is_some() {
			assert_eq!(
				extract(
					&client,
					ArchiveSource::Keep(archive.clone().into()),
					&destination,
					None
				)
				.await
				.unwrap_err(),
				ErrorKind::ArchivePasswordRequired,
				"{name}"
			);
			assert_eq!(
				extract(
					&client,
					ArchiveSource::Keep(archive.clone().into()),
					&destination,
					Some("wrong")
				)
				.await
				.unwrap_err(),
				ErrorKind::ArchiveWrongPassword,
				"{name}"
			);
		}
		let report = extract(
			&client,
			ArchiveSource::Keep(archive.into()),
			&destination,
			password,
		)
		.await
		.unwrap_or_else(|kind| panic!("extracting {name}: {kind:?}"));
		let folder = created_folder(&report);
		assert_eq!(folder.name(), Some("bundle"), "{name}");
		let (dirs, extracted) = contents(&client, &folder).await;
		let dir_paths: Vec<&str> = dirs.iter().map(|(path, _)| path.as_str()).collect();
		assert!(dir_paths.contains(&"source/sub"), "{name}: {dir_paths:?}");
		assert_same_files(
			&client,
			&extracted,
			[
				("source/big.bin", &files[0]),
				("source/notes.txt", &files[1]),
				("source/sub/empty", &files[2]),
			],
		)
		.await;
	}
}

#[shared_test_runtime]
async fn sources_are_trashed_and_the_archive_deleted_once_verified() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = resources.client.clone();
	let test_dir = &resources.dir;
	let (source, _) = tree(&client, test_dir).await;
	let destination = client
		.create_dir(&test_dir.into(), "disposal")
		.await
		.unwrap();

	let report = compress(
		&client,
		CompressSources::Dispose {
			how: SourceDisposal::Trash,
			items: vec![NonRootItemType::Dir(std::borrow::Cow::Owned(
				source.clone(),
			))],
		},
		&destination,
		"bundle.tar",
		CompressFormat::Tar { compression: None },
		None,
	)
	.await;
	let [disposition] = &report.dispositions[..] else {
		panic!("one source");
	};
	assert_eq!(disposition.uuid, source.uuid());
	assert!(
		matches!(disposition.outcome, DisposalOutcome::Disposed { .. }),
		"{:?}",
		disposition.outcome
	);
	let archive = report.archive.expect("the archive is registered");
	// the source folder is in the trash now, no longer in the test directory
	let listed = client
		.list_dir(&test_dir.into(), None::<&fn(u64, Option<u64>)>)
		.await
		.unwrap()
		.0;
	assert!(listed.iter().all(|dir| dir.uuid() != source.uuid()));

	let report = extract(
		&client,
		ArchiveSource::Dispose {
			file: archive.clone(),
			how: SourceDisposal::DeletePermanently,
		},
		&destination,
		None,
	)
	.await
	.unwrap();
	let [disposition] = &report.dispositions[..] else {
		panic!("the archive");
	};
	assert!(
		matches!(
			disposition.outcome,
			DisposalOutcome::Disposed {
				how: SourceDisposal::DeletePermanently,
				bytes_freed,
			} if bytes_freed == archive.size()
		),
		"{:?}",
		disposition.outcome
	);
	let (_, extracted) = contents(&client, &created_folder(&report)).await;
	let mut paths: Vec<&str> = extracted.iter().map(|(path, _)| path.as_str()).collect();
	paths.sort_unstable();
	assert_eq!(
		paths,
		["source/big.bin", "source/notes.txt", "source/sub/empty"]
	);
}
