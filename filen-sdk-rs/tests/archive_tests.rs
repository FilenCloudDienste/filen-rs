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
	consts::{CHUNK_SIZE, CHUNK_SIZE_U64},
	fs::{
		HasName, HasUUID,
		archive::{
			AesStrength, ArchiveEntry, ArchiveEntryKind, ArchiveFormat, ArchivePassword,
			ArchiveSource, CompressCallback, CompressConfig, CompressFormat, CompressPhase,
			CompressReport, CompressRequest, CompressSources, CompressUpdate, Compression,
			DisposalOutcome, EntrySelection, ExtractCallback, ExtractConfig, ExtractFailed,
			ExtractPhase, ExtractRenameReason, ExtractReport, ExtractRequest, ExtractRoot,
			ExtractSkipReason, ExtractUpdate, ExtractWhat, ExtractedTopLevel, KeptReason,
			ListCallback, ListConfig, ListFailed, ListPhase, ListReport, ListTotals, ListUpdate,
			ListedSkipReason, PasswordCheck, SevenZEncryption, SevenZMethod, SourceDisposal,
			StreamCodec, ZipMethod,
		},
		categories::{DirType, NonRootItemType},
		copy::{ItemSource, ItemSourceDir, JobControl},
		dir::RemoteDirectory,
		file::{
			RemoteFile,
			enums::RemoteFileType,
			traits::{HasFileInfo, HasRemoteFileInfo},
		},
		name::ValidatedName,
	},
	io::client_impl::IoSharedClientExt,
};
use filen_types::fs::Uuid;

mod drive_helpers;
use drive_helpers::{assert_same_files, contents, data, dir_link_info, linked_file, noise, upload};

/// What a compress job told its callback.
#[derive(Default)]
struct CompressRecorder {
	archives: Mutex<Vec<RemoteFile>>,
	updates: Mutex<Vec<CompressUpdate>>,
}

impl CompressCallback for CompressRecorder {
	fn on_archive_created(&self, archive: RemoteFile) {
		self.archives.lock().unwrap().push(archive);
	}

	fn on_update(&self, update: CompressUpdate) {
		self.updates.lock().unwrap().push(update);
	}
}

/// What an extraction told its callback.
#[derive(Default)]
struct ExtractRecorder {
	top_level: Mutex<Vec<ExtractedTopLevel>>,
	updates: Mutex<Vec<ExtractUpdate>>,
}

impl ExtractCallback for ExtractRecorder {
	fn on_top_level_batch(&self, items: Vec<ExtractedTopLevel>) {
		self.top_level.lock().unwrap().extend(items);
	}

	fn on_update(&self, update: ExtractUpdate) {
		self.updates.lock().unwrap().push(update);
	}
}

/// The uuids of `items`, in order.
fn uuids<'a>(items: impl IntoIterator<Item = &'a ExtractedTopLevel>) -> Vec<Uuid> {
	items.into_iter().map(|top| top.item.uuid()).collect()
}

/// A folder with a file over a chunk, a small one, an empty one and a subfolder: the folder, and
/// `big.bin`, `notes.txt` and `sub/empty`.
async fn tree(client: &Client, parent: &RemoteDirectory) -> (RemoteDirectory, [RemoteFile; 3]) {
	let source = client.create_dir(&parent.into(), "source").await.unwrap();
	let sub = client.create_dir(&(&source).into(), "sub").await.unwrap();
	let files = [
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
) -> (RemoteFile, CompressReport) {
	let recorder = Arc::new(CompressRecorder::default());
	let report = client
		.clone()
		.compress_items(
			CompressRequest {
				sources,
				destination: destination.clone().into(),
				name: ValidatedName::try_from(name).unwrap(),
			},
			CompressConfig {
				format,
				max_bytes: None,
				password: password.map(|p| ArchivePassword::new(p.into()).unwrap()),
			},
			Arc::clone(&recorder),
			JobControl::default(),
		)
		.await
		.unwrap_or_else(|failed| panic!("compressing {name}: {}", failed.error));
	let archive = report
		.archive
		.clone()
		.expect("a compression that ran to its end holds its archive");
	let told: Vec<Uuid> = recorder
		.archives
		.lock()
		.unwrap()
		.iter()
		.map(HasUUID::uuid)
		.collect();
	assert_eq!(
		told,
		[archive.uuid()],
		"{name}: the callback got the archive"
	);
	let phase = recorder
		.updates
		.lock()
		.unwrap()
		.last()
		.map(|update| update.phase);
	assert_eq!(phase, Some(CompressPhase::Done), "{name}");
	(archive, report)
}

/// Extracts `archive` into a new folder in `destination`, named after the archive.
async fn extract(
	client: &Arc<Client>,
	archive: ArchiveSource,
	destination: &RemoteDirectory,
	password: Option<&str>,
) -> Result<ExtractReport, ExtractFailed> {
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
) -> Result<ExtractReport, ExtractFailed> {
	run_extract(
		client,
		ExtractRequest {
			what: ExtractWhat::All(archive),
			destination: destination.clone().into(),
			root,
		},
		with_password(password),
	)
	.await
}

async fn run_extract(
	client: &Arc<Client>,
	request: ExtractRequest,
	config: ExtractConfig,
) -> Result<ExtractReport, ExtractFailed> {
	let recorder = Arc::new(ExtractRecorder::default());
	let report = client
		.clone()
		.extract_archive(
			request,
			config,
			Arc::clone(&recorder),
			JobControl::default(),
		)
		.await?;
	assert_eq!(
		uuids(recorder.top_level.lock().unwrap().iter()),
		uuids(&report.top_level),
		"the callback got every top-level item"
	);
	let phase = recorder
		.updates
		.lock()
		.unwrap()
		.last()
		.map(|update| update.phase);
	assert_eq!(phase, Some(ExtractPhase::Done));
	Ok(report)
}

fn with_password(password: Option<&str>) -> ExtractConfig {
	ExtractConfig {
		password: password.map(|p| ArchivePassword::new(p.into()).unwrap()),
		..ExtractConfig::default()
	}
}

/// What a listing told its callback.
#[derive(Default)]
struct ListRecorder {
	entries: Mutex<Vec<ArchiveEntry>>,
	updates: Mutex<Vec<ListUpdate>>,
}

impl ListCallback for ListRecorder {
	fn on_entries_batch(&self, entries: Vec<ArchiveEntry>) {
		self.entries.lock().unwrap().extend(entries);
	}

	fn on_update(&self, update: ListUpdate) {
		self.updates.lock().unwrap().push(update);
	}
}

/// Lists `archive` as `config` would extract it, checking the callback was handed the entries
/// the listing keeps and told it was done.
async fn list(
	client: &Arc<Client>,
	archive: RemoteFileType<'static>,
	config: ExtractConfig,
) -> Result<ListReport, ListFailed> {
	let config = ListConfig {
		expansion_limit: config.expansion_limit,
		skip_mac_metadata: config.skip_mac_metadata,
		password: config.password,
	};
	let recorder = Arc::new(ListRecorder::default());
	let listing = client
		.clone()
		.list_archive(
			archive,
			config,
			Arc::clone(&recorder),
			JobControl::default(),
		)
		.await?;
	assert_eq!(*recorder.entries.lock().unwrap(), listing.entries);
	let phase = recorder
		.updates
		.lock()
		.unwrap()
		.last()
		.map(|update| update.phase);
	assert_eq!(phase, Some(ListPhase::Done));
	Ok(listing)
}

/// Each listed entry's extracted path and kind, sorted by path.
fn outline(listing: &ListReport) -> Vec<(&str, &ArchiveEntryKind)> {
	let mut outline: Vec<_> = listing
		.entries
		.iter()
		.map(|entry| {
			(
				entry
					.path
					.as_ref()
					.map(|path| path.path.as_str())
					.unwrap_or_default(),
				&entry.kind,
			)
		})
		.collect();
	outline.sort_unstable_by_key(|(path, _)| *path);
	outline
}

/// The listed entry at `path`.
fn entry<'l>(listing: &'l ListReport, path: &str) -> &'l ArchiveEntry {
	listing
		.entries
		.iter()
		.find(|entry| entry.path.as_ref().map(|path| path.path.as_str()) == Some(path))
		.unwrap_or_else(|| panic!("{path} is listed"))
}

/// The paths of the files an extraction with the listing's config creates, sorted.
fn listed_files(listing: &ListReport) -> Vec<&str> {
	let mut files: Vec<&str> = listing
		.entries
		.iter()
		.filter(|entry| entry.kind == ArchiveEntryKind::File && entry.skip.is_none())
		.filter_map(|entry| entry.path.as_ref().map(|path| path.path.as_str()))
		.collect();
	files.sort_unstable();
	files
}

/// The paths of `files`, sorted.
fn file_paths(files: &[(String, RemoteFile)]) -> Vec<&str> {
	let mut paths: Vec<&str> = files.iter().map(|(path, _)| path.as_str()).collect();
	paths.sort_unstable();
	paths
}

/// An archive made by a real tool, from `tests/fixtures/archives` (see the READMEs there).
fn fixture(path: &str) -> Vec<u8> {
	std::fs::read(
		std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
			.join("tests/fixtures/archives")
			.join(path),
	)
	.unwrap()
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
	let (resources, _lock) = test_utils::RESOURCES.get_resources_with_lock().await;
	let client = resources.client.clone();
	let test_dir = &resources.dir;
	let (source, [big, notes, empty]) = tree(&client, test_dir).await;
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
		let (archive, report) = compress(
			&client,
			CompressSources::Keep(vec![ItemSource::Dir(ItemSourceDir::Normal(source.clone()))]),
			&destination,
			name,
			format,
			password,
		)
		.await;
		assert_eq!(archive.name(), Some(name));
		assert_eq!(report.counts.files_done, 3, "{name}");

		if password.is_some() {
			for (given, kind) in [
				(None, ErrorKind::ArchivePasswordRequired),
				(Some("wrong"), ErrorKind::ArchiveWrongPassword),
			] {
				let failed = extract(
					&client,
					ArchiveSource::Keep(archive.clone().into()),
					&destination,
					given,
				)
				.await
				.unwrap_err();
				assert_eq!(failed.error.kind(), kind, "{name} with {given:?}");
				assert!(
					failed.report.top_level.is_empty(),
					"{name} with {given:?}: nothing created"
				);
			}
		}
		let report = extract(
			&client,
			ArchiveSource::Keep(archive.into()),
			&destination,
			password,
		)
		.await
		.unwrap_or_else(|failed| panic!("extracting {name}: {}", failed.error));
		let folder = created_folder(&report);
		assert_eq!(folder.name(), Some("bundle"), "{name}");
		let (dirs, extracted) = contents(&client, &folder).await;
		let dir_paths: Vec<&str> = dirs.iter().map(|(path, _)| path.as_str()).collect();
		assert!(dir_paths.contains(&"source/sub"), "{name}: {dir_paths:?}");
		assert_same_files(
			&client,
			&extracted,
			[
				("source/big.bin", &big),
				("source/notes.txt", &notes),
				("source/sub/empty", &empty),
			],
		)
		.await;
	}
}

#[shared_test_runtime]
async fn sources_are_trashed_and_the_archive_deleted_once_verified() {
	let (resources, _lock) = test_utils::RESOURCES.get_resources_with_lock().await;
	let client = resources.client.clone();
	let test_dir = &resources.dir;
	let (source, _) = tree(&client, test_dir).await;
	let destination = client
		.create_dir(&test_dir.into(), "disposal")
		.await
		.unwrap();

	let (archive, report) = compress(
		&client,
		CompressSources::Dispose {
			how: SourceDisposal::Trash,
			items: vec![NonRootItemType::Dir(Cow::Owned(source.clone()))],
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
	let (resources, _lock) = test_utils::RESOURCES.get_resources_with_lock().await;
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
		let archive = compress(
			&client,
			CompressSources::Keep(vec![ItemSource::Dir(ItemSourceDir::Normal(source.clone()))]),
			&destination,
			name,
			format,
			None,
		)
		.await
		.0;
		assert!(
			archive.size() > 2 * CHUNK_SIZE_U64,
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
		.unwrap_or_else(|failed| panic!("extracting {name}: {}", failed.error));
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
	// the version locks first, so this test never holds the drive lock while it waits for a
	// test that holds them and writes to the drive
	let client = test_utils::RESOURCES.client().await;
	let _versions_lock = client
		.acquire_lock_with_default(test_utils::locks::VERSIONS)
		.await
		.unwrap();
	let _versioning_lock = client
		.acquire_lock_with_default(test_utils::locks::USER_VERSIONING)
		.await
		.unwrap();
	let (resources, _lock) = test_utils::RESOURCES.get_resources_with_lock().await;
	let test_dir = &resources.dir;
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

	let (_, report) = compress(
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
	let (resources, _lock) = test_utils::RESOURCES.get_resources_with_lock().await;
	let client = resources.client.clone();
	let test_dir = &resources.dir;
	let (source, [big, notes, empty]) = tree(&client, test_dir).await;
	let loose = upload(&client, test_dir, "loose.txt", b"a linked file").await;
	let linked_source = dir_link_info(&client, &source).await;
	let destination = client.create_dir(&test_dir.into(), "linked").await.unwrap();

	let (archive, report) = compress(
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
			("source/big.bin", &big),
			("source/notes.txt", &notes),
			("source/sub/empty", &empty),
			("loose.txt", &loose),
		],
	)
	.await;
}

#[shared_test_runtime]
async fn a_single_file_round_trips_as_a_gz() {
	let (resources, _lock) = test_utils::RESOURCES.get_resources_with_lock().await;
	let client = resources.client.clone();
	let test_dir = &resources.dir;
	let file = upload(&client, test_dir, "notes.txt", &data(CHUNK_SIZE + 99, 3)).await;
	let destination = client.create_dir(&test_dir.into(), "gz").await.unwrap();

	let archive = compress(
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
	.await
	.0;

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
	let (resources, _lock) = test_utils::RESOURCES.get_resources_with_lock().await;
	let client = resources.client.clone();
	let test_dir = &resources.dir;
	let (source, [big, notes, empty]) = tree(&client, test_dir).await;
	let readme = upload(&client, test_dir, "Readme.txt", b"read me").await;
	let archives = client
		.create_dir(&test_dir.into(), "archives")
		.await
		.unwrap();
	let archive = compress(
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
	.await
	.0;

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
			("source (1)/big.bin", &big),
			("source (1)/notes.txt", &notes),
			("source (1)/sub/empty", &empty),
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
			("source/big.bin", &big),
			("source/notes.txt", &notes),
			("source/sub/empty", &empty),
			("Readme.txt", &readme),
		],
	)
	.await;
}

/// A listing names each entry as an extraction would place it and says whether the password
/// given opens the encrypted ones, creating nothing; a 7z whose index is encrypted lists nothing
/// without the right one.
#[shared_test_runtime]
async fn a_listing_tells_the_entries_and_whether_the_password_opens_them() {
	let (resources, _lock) = test_utils::RESOURCES.get_resources_with_lock().await;
	let client = resources.client.clone();
	let test_dir = &resources.dir;
	let (source, [big, notes, _]) = tree(&client, test_dir).await;
	let destination = client.create_dir(&test_dir.into(), "listed").await.unwrap();
	let big = big.size();
	let expected = [
		("source", &ArchiveEntryKind::Dir),
		("source/big.bin", &ArchiveEntryKind::File),
		("source/notes.txt", &ArchiveEntryKind::File),
		("source/sub", &ArchiveEntryKind::Dir),
		("source/sub/empty", &ArchiveEntryKind::File),
	];
	let totals = ListTotals {
		entries: 5,
		dirs: 2,
		files: 3,
		bytes: big + notes.size(),
		skipped: 0,
		bytes_skipped: 0,
	};
	let compress_source = async |name, format, password| -> RemoteFileType<'static> {
		compress(
			&client,
			CompressSources::Keep(vec![ItemSource::Dir(ItemSourceDir::Normal(source.clone()))]),
			&destination,
			name,
			format,
			password,
		)
		.await
		.0
		.into()
	};
	let formats: [(&str, CompressFormat, Option<&str>, ArchiveFormat); 3] = [
		(
			"listed.tar.gz",
			CompressFormat::Tar {
				compression: Some(Compression {
					codec: StreamCodec::Gzip,
					level: None,
				}),
			},
			None,
			ArchiveFormat::Tar {
				codec: Some(StreamCodec::Gzip),
			},
		),
		(
			"listed.zip",
			CompressFormat::Zip {
				method: ZipMethod::Deflate { level: 6 },
				encryption: Some(AesStrength::Aes256),
			},
			Some("zip password"),
			ArchiveFormat::Zip,
		),
		(
			"listed.7z",
			CompressFormat::SevenZ {
				method: SevenZMethod::Lzma2 { level: 5 },
				solid: true,
				encryption: Some(SevenZEncryption::Entries),
			},
			Some("7z password"),
			ArchiveFormat::SevenZ,
		),
	];
	for (name, format, password, listed_as) in formats {
		let archive = compress_source(name, format, password).await;
		let listing = list(&client, archive.clone(), ExtractConfig::default())
			.await
			.unwrap_or_else(|failed| panic!("listing {name}: {}", failed.error));
		assert_eq!(listing.format, Some(listed_as), "{name}");
		assert_eq!(outline(&listing), expected, "{name}");
		assert_eq!(entry(&listing, "source/big.bin").size, Some(big), "{name}");
		assert_eq!(listing.totals, totals, "{name}");
		assert_eq!(listing.omitted_entries, 0, "{name}");
		let Some(password) = password else {
			assert_eq!(listing.password, PasswordCheck::NotNeeded, "{name}");
			continue;
		};
		assert_eq!(listing.password, PasswordCheck::Required, "{name}");
		assert!(entry(&listing, "source/big.bin").encrypted, "{name}");
		for (given, check) in [
			("wrong", PasswordCheck::Wrong),
			(password, PasswordCheck::Right),
		] {
			let listing = list(&client, archive.clone(), with_password(Some(given)))
				.await
				.unwrap_or_else(|failed| panic!("listing {name} with {given}: {}", failed.error));
			assert_eq!(listing.password, check, "{name} with {given}");
			assert_eq!(outline(&listing), expected, "{name} with {given}");
		}
	}

	let hidden = compress_source(
		"hidden.7z",
		CompressFormat::SevenZ {
			method: SevenZMethod::Lzma2 { level: 5 },
			solid: true,
			encryption: Some(SevenZEncryption::EntriesAndHeaders),
		},
		Some("7z password"),
	)
	.await;
	for (given, kind) in [
		(None, ErrorKind::ArchivePasswordRequired),
		(Some("wrong"), ErrorKind::ArchiveWrongPassword),
	] {
		assert_eq!(
			list(&client, hidden.clone(), with_password(given))
				.await
				.unwrap_err()
				.error
				.kind(),
			kind,
			"{given:?}"
		);
	}
	let listing = list(&client, hidden, with_password(Some("7z password")))
		.await
		.unwrap();
	assert_eq!(listing.password, PasswordCheck::Right);
	assert_eq!(outline(&listing), expected);

	// the archives, and nothing a listing made
	let (dirs, archives) = contents(&client, &destination).await;
	assert!(dirs.is_empty(), "{:?}", dirs.len());
	assert_eq!(
		file_paths(&archives),
		["hidden.7z", "listed.7z", "listed.tar.gz", "listed.zip"]
	);
}

/// Part of an archive extracts below the directory named as its base. An entry gone from where
/// an extraction put it lands there again when extracted the way a failure's retry says: its id,
/// into the directory it was created in, with that directory's path in the archive as the base.
/// (No entry of an extraction can be made to fail against the real server, so the retry target
/// is taken from the drive instead.)
#[shared_test_runtime]
async fn part_of_an_archive_extracts_below_its_base() {
	let (resources, _lock) = test_utils::RESOURCES.get_resources_with_lock().await;
	let client = resources.client.clone();
	let test_dir = &resources.dir;
	let (source, [_, notes, empty]) = tree(&client, test_dir).await;
	let archives = client
		.create_dir(&test_dir.into(), "archives")
		.await
		.unwrap();
	let archive: RemoteFileType<'static> = compress(
		&client,
		CompressSources::Keep(vec![ItemSource::Dir(ItemSourceDir::Normal(source))]),
		&archives,
		"bundle.zip",
		CompressFormat::Zip {
			method: ZipMethod::Deflate { level: 6 },
			encryption: None,
		},
		None,
	)
	.await
	.0
	.into();
	let listing = list(&client, archive.clone(), ExtractConfig::default())
		.await
		.unwrap();
	let id = |path| entry(&listing, path).id;
	let entries = |ids, base: &str, destination: &RemoteDirectory| ExtractRequest {
		what: ExtractWhat::Entries(
			EntrySelection::new(
				archive.clone(),
				ids,
				vec![ValidatedName::try_from(base).unwrap()],
			)
			.unwrap(),
		),
		destination: destination.clone().into(),
		root: ExtractRoot::Destination,
	};

	// the subfolder and a file beside it, below `source`
	let part = client.create_dir(&test_dir.into(), "part").await.unwrap();
	let report = run_extract(
		&client,
		entries(
			vec![id("source/notes.txt"), id("source/sub")],
			"source",
			&part,
		),
		ExtractConfig::default(),
	)
	.await
	.unwrap();
	assert_eq!(report.failures.len(), 0);
	let (dirs, extracted) = contents(&client, &part).await;
	let dirs: Vec<&str> = dirs.iter().map(|(path, _)| path.as_str()).collect();
	assert_eq!(dirs, ["sub"]);
	assert_eq!(file_paths(&extracted), ["notes.txt", "sub/empty"]);
	assert_same_files(
		&client,
		&extracted,
		[("notes.txt", &notes), ("sub/empty", &empty)],
	)
	.await;

	// the whole archive, then one file again where it was created
	let whole = client.create_dir(&test_dir.into(), "whole").await.unwrap();
	let report = extract(&client, ArchiveSource::Keep(archive.clone()), &whole, None)
		.await
		.unwrap();
	let folder = created_folder(&report);
	let (dirs, extracted) = contents(&client, &folder).await;
	let (_, lost) = extracted
		.iter()
		.find(|(path, _)| path == "source/notes.txt")
		.expect("notes.txt is extracted");
	client.delete_file_permanently(lost.clone()).await.unwrap();
	let (_, created_in) = dirs
		.iter()
		.find(|(path, _)| path == "source")
		.expect("source is extracted");
	let report = run_extract(
		&client,
		entries(vec![id("source/notes.txt")], "source", created_in),
		ExtractConfig::default(),
	)
	.await
	.unwrap();
	assert_eq!(report.renamed.len(), 0, "the name is free again");
	let (_, extracted) = contents(&client, &folder).await;
	assert_eq!(
		file_paths(&extracted),
		["source/big.bin", "source/notes.txt", "source/sub/empty"]
	);
	assert_same_files(&client, &extracted, [("source/notes.txt", &notes)]).await;
}

/// A tar's hard link is extracted as a copy of the file it names, a file of its own with the same
/// bytes and hash. bsdtar's pax fixture also holds a symlink, which the drive cannot hold.
#[shared_test_runtime]
async fn a_tar_hard_link_is_extracted_as_a_copy_of_its_target() {
	let (resources, _lock) = test_utils::RESOURCES.get_resources_with_lock().await;
	let client = resources.client.clone();
	let test_dir = &resources.dir;
	let links = client.create_dir(&test_dir.into(), "links").await.unwrap();
	let archive = upload(&client, &links, "pax.tar", &fixture("tar/pax.tar")).await;
	let symlink = ExtractSkipReason::Symlink {
		target: "text.txt".to_owned(),
	};

	let listing = list(&client, archive.clone().into(), ExtractConfig::default())
		.await
		.unwrap();
	assert_eq!(listing.format, Some(ArchiveFormat::Tar { codec: None }));
	assert_eq!(listing.password, PasswordCheck::NotNeeded);
	let hard = entry(&listing, "tree/dir/hard");
	assert_eq!(
		hard.kind,
		ArchiveEntryKind::Hardlink {
			target: "tree/dir/text.txt".to_owned(),
			target_id: Some(entry(&listing, "tree/dir/text.txt").id),
		}
	);
	assert_eq!(hard.size, Some(2110), "as large as the file it names");
	assert_eq!(hard.skip, None);
	let link = entry(&listing, "tree/dir/link");
	assert_eq!(
		link.kind,
		ArchiveEntryKind::Symlink {
			target: "text.txt".to_owned(),
		}
	);
	// a listed link's target is in its kind
	assert_eq!(link.skip, Some(ListedSkipReason::Symlink));

	let report = extract(&client, ArchiveSource::Keep(archive.into()), &links, None)
		.await
		.unwrap();
	let skipped: Vec<_> = report
		.skipped
		.iter()
		.map(|skipped| (skipped.path.as_str(), &skipped.reason))
		.collect();
	assert_eq!(skipped, [("tree/dir/link", &symlink)]);
	let (_, extracted) = contents(&client, &created_folder(&report)).await;
	assert_eq!(
		file_paths(&extracted),
		[
			"tree/a-directory-name-of-sixty-characters-for-the-long-path-cases/a-file-name-of-sixty-characters-for-the-long-path-cases.txt",
			"tree/café.txt",
			"tree/dir/bin.dat",
			"tree/dir/hard",
			"tree/dir/text.txt",
		]
	);
	let file = |path: &str| {
		&extracted
			.iter()
			.find(|(p, _)| p == path)
			.unwrap_or_else(|| panic!("{path} is extracted"))
			.1
	};
	let (hard, text) = (file("tree/dir/hard"), file("tree/dir/text.txt"));
	assert_ne!(hard.uuid(), text.uuid());
	assert_eq!(hard.size(), 2110);
	assert!(hard.hash().is_some());
	assert_eq!(hard.hash(), text.hash());
	assert_eq!(
		client.download_file(hard).await.unwrap(),
		client.download_file(text).await.unwrap()
	);
}

/// Finder's zips hold a `__MACOSX` folder of AppleDouble files beside the real ones: left out by
/// default, as a listing says, and extracted as ordinary files with `skip_mac_metadata` off.
#[shared_test_runtime]
async fn a_finder_zips_mac_metadata_is_left_out_unless_asked_for() {
	let (resources, _lock) = test_utils::RESOURCES.get_resources_with_lock().await;
	let client = resources.client.clone();
	let test_dir = &resources.dir;
	let mac = client.create_dir(&test_dir.into(), "mac").await.unwrap();
	let archive: RemoteFileType<'static> = upload(
		&client,
		&mac,
		"finder-ditto.zip",
		&fixture("zip/finder-ditto.zip"),
	)
	.await
	.into();
	let keep_metadata = ExtractConfig {
		skip_mac_metadata: false,
		..ExtractConfig::default()
	};
	for config in [ExtractConfig::default(), keep_metadata] {
		let skip = config.skip_mac_metadata;
		let listing = list(&client, archive.clone(), config.clone())
			.await
			.unwrap();
		let metadata: Vec<&ArchiveEntry> = listing
			.entries
			.iter()
			.filter(|entry| entry.mac_metadata)
			.collect();
		assert!(!metadata.is_empty());
		assert!(
			metadata
				.iter()
				.all(|entry| entry.stored_path.starts_with("__MACOSX/")
					&& (entry.skip == Some(ListedSkipReason::MacMetadata)) == skip),
			"skip {skip}: {metadata:?}"
		);
		let expected = listed_files(&listing);
		assert!(
			expected.contains(&"finder/naïve.txt") && expected.contains(&"finder/Café/Résumé.txt"),
			"{expected:?}"
		);
		assert_eq!(
			expected.iter().any(|path| path.starts_with("__MACOSX/")),
			!skip,
			"{expected:?}"
		);

		let destination = client
			.create_dir(&(&mac).into(), if skip { "skipped" } else { "kept" })
			.await
			.unwrap();
		let report = run_extract(
			&client,
			ExtractRequest {
				what: ExtractWhat::All(ArchiveSource::Keep(archive.clone())),
				destination: destination.clone().into(),
				root: ExtractRoot::NewFolder { name: None },
			},
			config,
		)
		.await
		.unwrap();
		let mut left_out: Vec<&str> = report
			.skipped
			.iter()
			.filter(|skipped| skipped.reason == ExtractSkipReason::MacMetadata)
			.map(|skipped| skipped.path.as_str())
			.collect();
		left_out.sort_unstable();
		let mut listed_out: Vec<&str> = metadata
			.iter()
			.filter(|_| skip)
			.map(|entry| entry.stored_path.as_str())
			.collect();
		listed_out.sort_unstable();
		assert_eq!(left_out, listed_out, "skip {skip}");
		let (_, extracted) = contents(&client, &created_folder(&report)).await;
		assert_eq!(file_paths(&extracted), expected, "skip {skip}");
	}
}

/// A zip whose entries are compressed with zstd (method 93), as CPython's `zipfile` writes it.
#[shared_test_runtime]
async fn a_zip_of_zstd_entries_extracts() {
	let (resources, _lock) = test_utils::RESOURCES.get_resources_with_lock().await;
	let client = resources.client.clone();
	let test_dir = &resources.dir;
	let dir = client.create_dir(&test_dir.into(), "zstd").await.unwrap();
	let archive = upload(
		&client,
		&dir,
		"zstd-python.zip",
		&fixture("zip/zstd-python.zip"),
	)
	.await;

	let report = extract(&client, ArchiveSource::Keep(archive.into()), &dir, None)
		.await
		.unwrap();
	assert_eq!(report.skipped.len(), 0);
	assert_eq!(report.failures.len(), 0);
	let (_, extracted) = contents(&client, &created_folder(&report)).await;
	let far = [
		fixture_noise(1, 1000),
		vec![0; 34_000],
		fixture_noise(1, 1000),
	]
	.concat();
	let lines: Vec<u8> = (0..500)
		.flat_map(|i| format!("line {i}\n").into_bytes())
		.collect();
	let expected: [(&str, &[u8]); 3] = [
		("hello.txt", b"hello from a real zip tool\n"),
		("sub/far.bin", &far),
		("sub/lines.txt", &lines),
	];
	assert_eq!(file_paths(&extracted), expected.map(|(path, _)| path));
	for (path, data) in expected {
		let (_, file) = extracted.iter().find(|(p, _)| p == path).unwrap();
		assert_eq!(client.download_file(file).await.unwrap(), data, "{path}");
	}
}

/// What the zip fixtures' script (`tests/fixtures/archives/zip/generate.sh`) calls `noise(seed, n)`:
/// the high bits of a C `rand`-style linear congruential generator.
fn fixture_noise(mut seed: u32, len: usize) -> Vec<u8> {
	(0..len)
		.map(|_| {
			seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345) & 0x7FFF_FFFF;
			(seed >> 16).to_le_bytes()[0]
		})
		.collect()
}
