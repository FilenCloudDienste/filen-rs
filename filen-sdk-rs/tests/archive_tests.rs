//! Compressing items into archives in the drive and extracting them again, against the live
//! backend: every format family, archives of several chunks, encryption, sources read through
//! public links, names already taken, and removing the sources afterwards.

use std::{
	borrow::Cow,
	sync::{Arc, Mutex},
};

use filen_macros::shared_test_runtime;
use filen_sdk_rs::{
	ErrorKind,
	auth::Client,
	consts::CHUNK_SIZE,
	fs::{
		HasName, HasUUID,
		archive::{
			AesStrength, ArchivePassword, DisposalOutcome, JobControl, KeptReason,
			SevenZEncryption, SevenZMethod, SourceDisposal,
			compress::{
				CompressCallback, CompressConfig, CompressFormat, CompressReport, CompressSources,
				CompressUpdate, Compression, ItemSource, ItemSourceDir, StreamCodec, ZipMethod,
			},
			extract::{
				ArchiveSource, ExtractCallback, ExtractConfig, ExtractRenameReason, ExtractReport,
				ExtractRequest, ExtractRoot, ExtractUpdate, ExtractedTopLevel,
			},
		},
		categories::{DirType, NonRootItemType},
		dir::RemoteDirectory,
		file::{
			RemoteFile,
			traits::{HasFileInfo, HasRemoteFileInfo},
		},
		name::ValidatedName,
	},
	io::client_impl::IoSharedClientExt,
};

mod drive_helpers;
use drive_helpers::{assert_same_files, contents, data, dir_link_info, linked_file, noise, upload};

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
) -> CompressReport {
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

/// Extracts `archive` into a new folder in `destination`, named after the archive.
async fn extract(
	client: &Arc<Client>,
	archive: ArchiveSource,
	destination: &RemoteDirectory,
	password: Option<&str>,
) -> Result<ExtractReport, ErrorKind> {
	extract_into(
		client,
		archive,
		destination,
		ExtractRoot::NewFolder { name: None },
		password,
	)
	.await
}

async fn extract_into(
	client: &Arc<Client>,
	archive: ArchiveSource,
	destination: &RemoteDirectory,
	root: ExtractRoot,
	password: Option<&str>,
) -> Result<ExtractReport, ErrorKind> {
	client
		.clone()
		.extract_archive(
			ExtractRequest::All {
				archive,
				destination: destination.clone().into(),
				root,
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
fn created_folder(report: &ExtractReport) -> RemoteDirectory {
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

/// Archives of data no codec shrinks, so each spans three chunks: a 7z (whose first chunk is
/// uploaded last, its hash put together around it), a stored zip and a zstd tar.
#[shared_test_runtime]
async fn archives_of_several_chunks_keep_their_hash_and_contents() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = resources.client.clone();
	let test_dir = &resources.dir;
	let source = client.create_dir(&test_dir.into(), "noise").await.unwrap();
	let files = [
		upload(&client, &source, "a.bin", &noise(2 * CHUNK_SIZE + 17, 1)).await,
		upload(&client, &source, "b.bin", &noise(CHUNK_SIZE / 2 + 3, 2)).await,
	];
	let formats: [(&str, CompressFormat); 3] = [
		(
			"noise.7z",
			CompressFormat::SevenZ {
				method: SevenZMethod::Copy,
				solid: false,
				encryption: None,
			},
		),
		(
			"noise.zip",
			CompressFormat::Zip {
				method: ZipMethod::Stored,
				encryption: None,
			},
		),
		(
			"noise.tar.zst",
			CompressFormat::Tar {
				compression: Some(Compression {
					codec: StreamCodec::Zstd,
					level: None,
				}),
			},
		),
	];
	for (name, format) in formats {
		let destination = client.create_dir(&test_dir.into(), name).await.unwrap();
		let report = compress(
			&client,
			CompressSources::Keep(vec![ItemSource::Dir(ItemSourceDir::Normal(source.clone()))]),
			&destination,
			name,
			format,
			None,
		)
		.await;
		let archive = report.archive.expect("the archive is registered");
		assert!(
			archive.size() > 2 * CHUNK_SIZE as u64,
			"{name} spans three chunks: {} bytes",
			archive.size()
		);
		let bytes = client.download_file(&archive).await.unwrap();
		assert_eq!(bytes.len() as u64, archive.size(), "{name}");
		assert_eq!(
			archive.hash(),
			Some(blake3::hash(&bytes).into()),
			"{name} is registered with the hash of what was uploaded"
		);

		let report = extract(
			&client,
			ArchiveSource::Keep(archive.into()),
			&destination,
			None,
		)
		.await
		.unwrap_or_else(|kind| panic!("extracting {name}: {kind:?}"));
		let (_, extracted) = contents(&client, &created_folder(&report)).await;
		assert_same_files(
			&client,
			&extracted,
			[("noise/a.bin", &files[0]), ("noise/b.bin", &files[1])],
		)
		.await;
	}
}

/// Deleting sources for good keeps a file with older versions, as the server lists them, and
/// deletes one without.
#[shared_test_runtime]
async fn deleting_sources_for_good_keeps_a_file_with_versions() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = resources.client.clone();
	let test_dir = &resources.dir;
	let _versions_lock = client
		.acquire_lock_with_default(test_utils::locks::VERSIONS)
		.await
		.unwrap();
	let _versioning_lock = client
		.acquire_lock_with_default(test_utils::locks::USER_VERSIONING)
		.await
		.unwrap();
	let versioning = client.get_user_info().await.unwrap().versioning_enabled;
	if !versioning {
		client.set_versioning_enabled(true).await.unwrap();
	}

	let sources = client
		.create_dir(&test_dir.into(), "versioned")
		.await
		.unwrap();
	upload(&client, &sources, "report.txt", b"first draft").await;
	// the same name in the same folder: the server keeps the first as an older version
	let versioned = upload(&client, &sources, "report.txt", b"final draft").await;
	let single = upload(&client, &sources, "plain.txt", b"only draft").await;
	assert!(
		client.list_file_versions(&versioned).await.unwrap().len() >= 2,
		"the second upload keeps the first as a version"
	);
	let destination = client
		.create_dir(&test_dir.into(), "archives")
		.await
		.unwrap();

	let report = compress(
		&client,
		CompressSources::Dispose {
			how: SourceDisposal::DeletePermanently,
			items: vec![
				NonRootItemType::File(Cow::Owned(versioned.clone())),
				NonRootItemType::File(Cow::Owned(single.clone())),
			],
		},
		&destination,
		"drafts.tar",
		CompressFormat::Tar { compression: None },
		None,
	)
	.await;
	assert!(report.archive.is_some());
	let outcome = |uuid| {
		&report
			.dispositions
			.iter()
			.find(|disposition| disposition.uuid == uuid)
			.expect("every source is reported")
			.outcome
	};
	assert!(
		matches!(
			outcome(versioned.uuid()),
			DisposalOutcome::Kept {
				reason: KeptReason::HasVersions,
				..
			}
		),
		"{:?}",
		outcome(versioned.uuid())
	);
	assert!(
		matches!(
			outcome(single.uuid()),
			DisposalOutcome::Disposed {
				how: SourceDisposal::DeletePermanently,
				bytes_freed,
			} if *bytes_freed == single.size()
		),
		"{:?}",
		outcome(single.uuid())
	);
	let (_, left) = contents(&client, &sources).await;
	let left: Vec<_> = left
		.iter()
		.map(|(path, file)| (path.as_str(), file.uuid()))
		.collect();
	assert_eq!(left, [("report.txt", versioned.uuid())]);

	if !versioning {
		client.set_versioning_enabled(false).await.unwrap();
	}
}

/// Items read through public links compress like the user's own, and an archive read through
/// its file link extracts like one in the drive.
#[shared_test_runtime]
async fn linked_sources_compress_and_a_linked_archive_extracts() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = resources.client.clone();
	let test_dir = &resources.dir;
	let (source, files) = tree(&client, test_dir).await;
	let loose = upload(&client, test_dir, "loose.txt", b"a linked file").await;
	let linked_source = dir_link_info(&client, &source).await;
	let destination = client.create_dir(&test_dir.into(), "linked").await.unwrap();

	let report = compress(
		&client,
		CompressSources::Keep(vec![
			ItemSource::Dir(ItemSourceDir::Linked(
				DirType::Root(Cow::Owned(linked_source.root)),
				linked_source.link,
			)),
			ItemSource::File(linked_file(&client, &loose).await),
		]),
		&destination,
		"linked.zip",
		CompressFormat::Zip {
			method: ZipMethod::Deflate { level: 6 },
			encryption: None,
		},
		None,
	)
	.await;
	assert_eq!(report.counts.files_done, 4);
	let archive = report.archive.expect("the archive is registered");

	let report = extract(
		&client,
		ArchiveSource::Keep(linked_file(&client, &archive).await),
		&destination,
		None,
	)
	.await
	.unwrap();
	let folder = created_folder(&report);
	assert_eq!(folder.name(), Some("linked"));
	let (_, extracted) = contents(&client, &folder).await;
	assert_same_files(
		&client,
		&extracted,
		[
			("source/big.bin", &files[0]),
			("source/notes.txt", &files[1]),
			("source/sub/empty", &files[2]),
			("loose.txt", &loose),
		],
	)
	.await;
}

#[shared_test_runtime]
async fn a_single_file_round_trips_as_a_gz() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = resources.client.clone();
	let test_dir = &resources.dir;
	let file = upload(&client, test_dir, "notes.txt", &data(CHUNK_SIZE + 99, 3)).await;
	let destination = client.create_dir(&test_dir.into(), "gz").await.unwrap();

	let report = compress(
		&client,
		CompressSources::Keep(vec![ItemSource::File(file.clone().into())]),
		&destination,
		"notes.txt.gz",
		CompressFormat::Single {
			compression: Compression {
				codec: StreamCodec::Gzip,
				level: None,
			},
		},
		None,
	)
	.await;
	let archive = report.archive.expect("the archive is registered");

	let report = extract(
		&client,
		ArchiveSource::Keep(archive.into()),
		&destination,
		None,
	)
	.await
	.unwrap();
	// one file, straight into the destination, named after the archive
	let [top] = &report.top_level[..] else {
		panic!("one file: {:?}", report.top_level.len());
	};
	let NonRootItemType::File(extracted) = &top.item else {
		panic!("a file");
	};
	assert_eq!(extracted.name(), Some("notes.txt"));
	assert_same_files(
		&client,
		&[("notes.txt".to_owned(), extracted.clone().into_owned())],
		[("notes.txt", &file)],
	)
	.await;
}

/// Extracting keeps both where a name is taken, compared as the server compares names
/// (case-insensitively): at the top of the destination, and for a named new folder.
#[shared_test_runtime]
async fn extracting_keeps_both_where_a_name_is_taken() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = resources.client.clone();
	let test_dir = &resources.dir;
	let (source, files) = tree(&client, test_dir).await;
	let readme = upload(&client, test_dir, "Readme.txt", b"read me").await;
	let archives = client
		.create_dir(&test_dir.into(), "archives")
		.await
		.unwrap();
	let report = compress(
		&client,
		CompressSources::Keep(vec![
			ItemSource::Dir(ItemSourceDir::Normal(source)),
			ItemSource::File(readme.clone().into()),
		]),
		&archives,
		"bundle.tar.gz",
		CompressFormat::Tar {
			compression: Some(Compression {
				codec: StreamCodec::Gzip,
				level: None,
			}),
		},
		None,
	)
	.await;
	let archive = report.archive.expect("the archive is registered");

	// straight into a destination holding both top-level names, cased differently
	let destination = client
		.create_dir(&test_dir.into(), "destination")
		.await
		.unwrap();
	let taken_dir = client
		.create_dir(&(&destination).into(), "SOURCE")
		.await
		.unwrap();
	let taken_file = upload(&client, &destination, "README.TXT", b"already here").await;
	let report = extract_into(
		&client,
		ArchiveSource::Keep(archive.clone().into()),
		&destination,
		ExtractRoot::Destination,
		None,
	)
	.await
	.unwrap();
	let mut renamed: Vec<_> = report
		.renamed
		.iter()
		.map(|entry| (entry.path.as_str(), entry.name.as_str(), entry.reason))
		.collect();
	renamed.sort_unstable_by_key(|(path, ..)| *path);
	assert_eq!(
		renamed,
		[
			(
				"Readme.txt",
				"Readme (1).txt",
				ExtractRenameReason::DuplicateName
			),
			("source", "source (1)", ExtractRenameReason::DuplicateName),
		]
	);
	let mut top: Vec<_> = report
		.top_level
		.iter()
		.map(|top| top.item.name().unwrap().to_owned())
		.collect();
	top.sort_unstable();
	assert_eq!(top, ["Readme (1).txt", "source (1)"]);
	let (dirs, extracted) = contents(&client, &destination).await;
	// what was there is left as it was
	assert!(
		dirs.iter()
			.any(|(path, dir)| path == "SOURCE" && dir.uuid() == taken_dir.uuid())
	);
	assert!(
		extracted
			.iter()
			.any(|(path, file)| path == "README.TXT" && file.uuid() == taken_file.uuid())
	);
	assert_same_files(
		&client,
		&extracted,
		[
			("source (1)/big.bin", &files[0]),
			("source (1)/notes.txt", &files[1]),
			("source (1)/sub/empty", &files[2]),
			("Readme (1).txt", &readme),
		],
	)
	.await;

	// into a new folder named after one there, cased differently
	let unpacked = client
		.create_dir(&(&destination).into(), "UNPACKED")
		.await
		.unwrap();
	let report = extract_into(
		&client,
		ArchiveSource::Keep(archive.into()),
		&destination,
		ExtractRoot::NewFolder {
			name: Some(ValidatedName::try_from("unpacked").unwrap()),
		},
		None,
	)
	.await
	.unwrap();
	let folder = created_folder(&report);
	assert_eq!(folder.name(), Some("unpacked (1)"));
	assert_ne!(folder.uuid(), unpacked.uuid());
	let (_, extracted) = contents(&client, &folder).await;
	assert_same_files(
		&client,
		&extracted,
		[
			("source/big.bin", &files[0]),
			("source/notes.txt", &files[1]),
			("source/sub/empty", &files[2]),
			("Readme.txt", &readme),
		],
	)
	.await;
}
