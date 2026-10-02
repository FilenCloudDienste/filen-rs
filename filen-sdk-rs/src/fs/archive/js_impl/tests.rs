use std::{path::Path, time::Duration};

use futures::AsyncWriteExt;
use tokio_util::compat::Compat;

use super::{
	uniffi_impl::{
		CompressItemsCallback, DownloadArchiveEntryCallback, ExtractArchiveCallback,
		ListArchiveCallback, deliver_compress, deliver_entry_download, deliver_extract,
		deliver_list, download_into,
	},
	*,
};
use crate::{
	alloc_meter,
	fs::{
		HasUUID,
		archive::{
			Compression, SevenZMethod, StreamCodec, ZipMethod,
			config::CODEC_MEM_BUDGET,
			dispose::{DisposalOutcome, KeptReason, SourceDisposition},
			extract::codec::Selection,
			zip::crypto::AesStrength,
		},
		dir::{RemoteDirectory, RootDirectory},
		drive_job::plan::RenameReason,
		file::traits::HasFileInfo,
	},
	job::report::JobFailed,
	js::{
		Root,
		test_support::{Recorder, delivered_in_order, drive_dir, drive_file},
	},
};

/// The archive every test extracts from, a file of the user's drive.
const ARCHIVE: Uuid = Uuid::from_u128(0xa);

fn remote_file() -> RemoteFile {
	drive_file(ARCHIVE, "a.zip", 10)
}

fn dir() -> RemoteDirectory {
	drive_dir(Uuid::from_u128(0xd), "Photos")
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
		ExtractRoot::NewFolder { name: None },
		Some(SourceDisposal::Trash),
	)
	.unwrap();
	let ExtractRequest {
		what: ExtractWhat::All(ArchiveSource::Dispose {
			file: disposed,
			how,
		}),
		root: extract::ExtractRoot::NewFolder { name: None },
		..
	} = request
	else {
		panic!("an archive to remove, into a new folder");
	};
	assert_eq!((disposed.uuid(), how), (file.uuid(), SourceDisposal::Trash));

	let error = extract_request(
		AnyFile::File(remote_file().into()),
		destination(),
		ExtractRoot::NewFolder {
			name: Some(String::new()),
		},
		Some(SourceDisposal::DeletePermanently),
	)
	.unwrap_err();
	assert_eq!(error.kind(), ErrorKind::InvalidName, "an empty folder name");
}

#[test]
fn chosen_entries_go_to_the_extract_below_a_base_of_names() {
	let entries = vec![entry_id(4), entry_id(1)];
	let request = entries_request(
		AnyFile::File(remote_file().into()),
		entries.clone(),
		"/photos//2024/",
		destination(),
		ExtractRoot::Destination,
	)
	.unwrap();
	let ExtractRequest {
		what: ExtractWhat::Entries(chosen),
		root: extract::ExtractRoot::Destination,
		..
	} = request
	else {
		panic!("some entries, into the destination itself");
	};
	let (archive, selection) = chosen.into_parts();
	assert_eq!(archive.uuid(), ARCHIVE);
	assert_eq!(names(selection.base()), ["photos", "2024"]);
	assert_eq!(
		selection,
		Selection::new(
			entries.iter().map(|id| u64::from(id.index)),
			selection.base().to_vec()
		)
	);
}

#[test]
fn a_call_choosing_no_entry_of_its_archive_or_an_invalid_base_is_refused() {
	let refused = |entries, base| {
		entries_request(
			AnyFile::File(remote_file().into()),
			entries,
			base,
			destination(),
			ExtractRoot::Destination,
		)
		.unwrap_err()
		.kind()
	};
	let of_another = ArchiveEntryId {
		archive: Uuid::from_u128(0xb),
		index: 0,
	};
	assert_eq!(refused(Vec::new(), ""), ErrorKind::InvalidState);
	assert_eq!(
		refused(vec![entry_id(0), of_another], ""),
		ErrorKind::InvalidState
	);
	assert_eq!(
		refused(vec![entry_id(0)], "docs/\u{0}"),
		ErrorKind::InvalidName
	);
}

/// `base`'s names, for comparing.
fn names(base: &[ValidatedName]) -> Vec<&str> {
	base.iter().map(AsRef::as_ref).collect()
}

#[test]
fn what_a_call_leaves_out_is_the_sdks_default() {
	let defaults = ExtractConfig::default();
	let config = ExtractSettings::default().into_config(None);
	assert_eq!(
		(
			config.max_bytes,
			config.max_items,
			config.expansion_limit,
			config.skip_mac_metadata
		),
		(
			None,
			None,
			defaults.expansion_limit,
			defaults.skip_mac_metadata
		)
	);
	let limit = ExpansionLimit {
		ratio: 10,
		floor: 1 << 20,
	};
	let config = ExtractSettings {
		max_bytes: Some(5),
		max_items: Some(7),
		expansion_limit: Some(limit),
		skip_mac_metadata: Some(false),
	}
	.into_config(None);
	assert_eq!(
		(
			config.max_bytes,
			config.max_items,
			config.expansion_limit,
			config.skip_mac_metadata
		),
		(Some(5), Some(7), Some(limit), false)
	);
}

#[test]
fn an_extraction_folder_name_is_checked_at_the_edge() {
	let error = extract_request(
		AnyFile::File(remote_file().into()),
		destination(),
		ExtractRoot::NewFolder {
			name: Some("a/b".into()),
		},
		None,
	)
	.unwrap_err();
	assert_eq!(error.kind(), ErrorKind::InvalidName);
}

#[test]
fn an_empty_password_is_refused_at_the_edge() {
	assert_eq!(
		checked_password(Some(String::new())).unwrap_err().kind(),
		ErrorKind::InvalidState
	);
	assert!(checked_password(None).unwrap().is_none());
}

/// A compress call of `items` into an archive `name` of `format`, on a client whose codec
/// budget is `budget`.
fn compress_call(
	items: Vec<AnyItemWithContext>,
	name: &str,
	format: CompressFormat,
	budget: u64,
) -> Result<CompressCall, Error> {
	CompressCall::new(
		items,
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
}

#[test]
fn a_compress_name_is_checked_at_the_edge() {
	let stored = CompressFormat::Zip {
		method: ZipMethod::Stored,
		encryption: None,
	};
	assert_eq!(
		compress_call(Vec::new(), "a/b.zip", stored, CODEC_MEM_BUDGET)
			.err()
			.unwrap()
			.kind(),
		ErrorKind::InvalidName
	);
}

#[test]
fn a_compress_encoder_has_to_fit_the_clients_budget() {
	// 7z PPMd 8 needs 129 MiB
	let ppmd = CompressFormat::SevenZ {
		method: SevenZMethod::Ppmd { level: 8 },
		solid: false,
		encryption: None,
	};
	assert_eq!(
		compress_call(Vec::new(), "a.7z", ppmd, 128 << 20)
			.err()
			.unwrap()
			.kind(),
		ErrorKind::InsufficientMemory
	);
	assert!(compress_call(Vec::new(), "a.7z", ppmd, 256 << 20).is_ok());
}

#[test]
fn a_single_compressed_file_takes_one_file_which_each_items_kind_tells_up_front() {
	let one = |item| {
		compress_call(
			vec![item],
			"a.gz",
			CompressFormat::Single {
				compression: Compression {
					codec: StreamCodec::Gzip,
					level: None,
				},
			},
			CODEC_MEM_BUDGET,
		)
	};
	let folder = AnyItemWithContext::Dir(AnyDirWithContext::Normal(destination()));
	assert_eq!(one(folder).err().unwrap().kind(), ErrorKind::InvalidState);
	assert!(
		one(AnyItemWithContext::File(AnyFile::File(
			remote_file().into()
		)))
		.is_ok()
	);
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
		RootDirectory::new(Uuid::from_u128(0x7)),
	))));
	for (dispose, refusal) in [
		(None, "the root directory cannot be compressed"),
		(
			Some(SourceDisposal::Trash),
			"only items in the user's own drive, not its root, can be removed after compressing",
		),
	] {
		let error = compress_sources(vec![root.clone()], dispose).unwrap_err();
		assert_eq!(error.kind(), ErrorKind::InvalidState, "{dispose:?}");
		assert!(error.to_string().contains(refusal), "{error}");
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
		String::from(super::super::archive_default_name("..zip")),
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
		Some(ArchiveLevels {
			min: 1,
			max: 9,
			default_level: 6
		})
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
	let misleading = ExtractMisleadingName {
		entry: entry_id(2),
		path: "invoice\u{202E}fdp.exe".into(),
	};
	let report = extract::ExtractReport {
		top_level: Vec::new(),
		failures: vec![failure(ErrorKind::MaxStorageReached)],
		skipped: Vec::new(),
		renamed: Vec::new(),
		misleading_names: vec![misleading.clone()],
		omitted: OmittedRecords::default(),
		archive_bytes: 0,
		counts: ItemCounts::default(),
		unaccounted_bytes: 0,
		duplicates: None,
		dispositions: vec![
			SourceDisposition {
				uuid: Uuid::from_u128(0x5a),
				outcome: DisposalOutcome::Kept {
					reason: KeptReason::Failed {
						error: Arc::clone(&removal),
					},
					bytes_freed: 3,
				},
			},
			SourceDisposition {
				uuid: Uuid::from_u128(0x5b),
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
	assert_eq!(report.misleading_names, [misleading]);
	let [failed] = report.failures.as_slice() else {
		panic!("one failure");
	};
	let retry = failed
		.retry
		.as_ref()
		.expect("a failure keeps where to retry it");
	assert_eq!(
		(retry.destination, retry.base.as_str(), failed.error.kind()),
		(
			Uuid::from_u128(0xd),
			"docs/sub",
			ErrorKind::MaxStorageReached
		)
	);
	// the directory itself, for a retry to extract into without looking it up
	assert_eq!(
		DirType::<'static, Normal>::from(retry.destination_dir.clone()),
		DirType::Dir(Cow::Owned(dir()))
	);
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

fn entry_id(index: u32) -> ArchiveEntryId {
	ArchiveEntryId {
		archive: ARCHIVE,
		index,
	}
}

fn failure(kind: ErrorKind) -> extract::ExtractFailure {
	extract::ExtractFailure {
		entry: entry_id(3),
		path: "docs/a.txt".into(),
		dest_parent: Uuid::from_u128(0xd),
		dest_name: "a.txt".into(),
		stage: ExtractStage::Upload,
		retry: Some(extract::ExtractRetry {
			destination: DirType::Dir(Cow::Owned(dir())),
			base: vec![
				ValidatedName::try_from("docs").unwrap(),
				ValidatedName::try_from("sub").unwrap(),
			],
		}),
		error: Arc::new(Error::custom(kind, "failed")),
	}
}

fn extract_update(millis: u64) -> extract::ExtractUpdate {
	extract::ExtractUpdate {
		phase: ExtractPhase::Extracting,
		run_state: RunState::Running,
		archive_bytes: 100,
		counts: ItemCounts::default(),
		bytes_read: 40,
		active: Vec::new(),
		events: Vec::new(),
		bytes_per_second: None,
		eta: None,
		active_time: Duration::from_millis(millis),
	}
}

fn compress_update(millis: u64) -> compress::CompressUpdate {
	compress::CompressUpdate {
		phase: CompressPhase::Compressing,
		run_state: RunState::Running,
		scan: ScanProgress::default(),
		totals: PlanTotals::default(),
		counts: CompressCounts::default(),
		active: Vec::new(),
		events: Vec::new(),
		bytes_per_second: None,
		eta: None,
		active_time: Duration::from_millis(millis),
	}
}

fn list_update(millis: u64) -> extract::ListUpdate {
	extract::ListUpdate {
		phase: ListPhase::Reading,
		run_state: RunState::Running,
		bytes_read: 40,
		archive_bytes: 100,
		entries: 1,
		bytes_per_second: None,
		eta: None,
		active_time: Duration::from_millis(millis),
	}
}

fn archive_entry(index: u32) -> ArchiveEntry {
	ArchiveEntry {
		id: entry_id(index),
		stored_path: format!("docs/{index}.txt"),
		stored_path_truncated: false,
		path: Some(extract::ListedPath::plain(format!("docs/{index}.txt"))),
		kind: extract::ArchiveEntryKind::File,
		size: Some(u64::from(index) * 3),
		modified: None,
		encrypted: false,
		method: Some("Deflate".into()),
		skip: None,
		mac_metadata: false,
		access: Some(extract::EntryAccess::Direct {
			packed_bytes: u64::from(index) * 2,
		}),
	}
}

#[test]
fn an_extract_update_reports_milliseconds_and_the_parts_of_its_events() {
	let dest_uuid = Uuid::from_u128(0xe);
	let error = Arc::new(Error::custom(ErrorKind::Server, "link"));
	let update = ExtractUpdate::from(extract::ExtractUpdate {
		events: vec![
			extract::ExtractEvent::DirCreated {
				dest_uuid,
				dest_parent: Uuid::from_u128(0xd),
				name: "docs".into(),
			},
			extract::ExtractEvent::FileFailed(failure(ErrorKind::MaxStorageReached)),
			extract::ExtractEvent::PropagationFailed {
				dest_uuid,
				error: Arc::clone(&error),
			},
		],
		bytes_per_second: Some(100),
		eta: Some(Duration::from_millis(1500)),
		..extract_update(2500)
	});
	assert_eq!(
		(update.eta_ms, update.active_time_ms, update.bytes_read),
		(Some(1500), 2500, 40)
	);
	let [
		ExtractEvent::DirCreated(created),
		ExtractEvent::FileFailed(failed),
		ExtractEvent::PropagationFailed(propagation),
	] = update.events.as_slice()
	else {
		panic!("{:?}", update.events);
	};
	assert_eq!(
		(created.dest_uuid, created.name.as_str()),
		(dest_uuid, "docs")
	);
	assert_eq!(
		(
			failed.entry,
			failed.dest_parent,
			failed.retry.as_ref().map(|retry| retry.base.as_str()),
			failed.error.kind()
		),
		(
			entry_id(3),
			Uuid::from_u128(0xd),
			Some("docs/sub"),
			ErrorKind::MaxStorageReached
		)
	);
	assert_eq!(propagation.dest_uuid, dest_uuid);
	assert!(
		Arc::ptr_eq(&propagation.error, &error),
		"the SDK error itself"
	);
}

#[test]
fn a_compress_update_carries_its_active_file_and_events() {
	let source_uuid = Uuid::from_u128(0x5);
	let active = compress::CompressActiveFile {
		source_uuid,
		name: "a.txt".into(),
		path: "docs/a.txt".into(),
		size: 30,
		bytes_done: 12,
	};
	let update = CompressUpdate::from(compress::CompressUpdate {
		phase: CompressPhase::Verifying,
		active: vec![active.clone()],
		events: vec![
			compress::CompressEvent::SourceHashMismatch(HashMismatch {
				source_uuid,
				path: "docs/a.txt".into(),
			}),
			compress::CompressEvent::Renamed(RenamedEntry {
				source_uuid,
				source_path: "/docs/A.txt".into(),
				name: ValidatedName::try_from("A (1).txt").unwrap(),
				reason: RenameReason::DuplicateName,
			}),
		],
		eta: Some(Duration::from_millis(700)),
		..compress_update(900)
	});
	assert_eq!(
		(update.phase, update.eta_ms, update.active_time_ms),
		(CompressPhase::Verifying, Some(700), 900)
	);
	assert_eq!(update.active, [active]);
	let [
		CompressEvent::SourceHashMismatch(mismatch),
		CompressEvent::Renamed(renamed),
	] = update.events.as_slice()
	else {
		panic!("{:?}", update.events);
	};
	assert_eq!(
		*mismatch,
		HashMismatch {
			source_uuid,
			path: "docs/a.txt".into(),
		},
		"the event carries the report's record"
	);
	assert_eq!(renamed.name.as_ref(), "A (1).txt");
}

#[test]
fn a_listing_carries_why_it_ended_and_the_entries_read_by_then() {
	let update = ListUpdate::new(
		extract::ListUpdate {
			eta: Some(Duration::from_millis(300)),
			..list_update(1200)
		},
		7,
	);
	assert_eq!(
		(
			update.eta_ms,
			update.active_time_ms,
			update.entries,
			update.undelivered_entries
		),
		(Some(300), 1200, 1, 7)
	);
	let listing = extract::ListReport {
		format: Some(ArchiveFormat::Zip),
		password: PasswordCheck::Wrong,
		entries: vec![archive_entry(0), archive_entry(1)],
		omitted_entries: 4,
		totals: ListTotals::default(),
		unaccounted_bytes: 9,
		duplicates: None,
	};
	let ended = Arc::new(Error::custom(ErrorKind::ArchiveWrongPassword, "wrong"));
	let failed = ListReport::new(
		Err(JobFailed {
			report: listing.clone(),
			error: Arc::clone(&ended),
		}),
		3,
	);
	assert_eq!(failed.undelivered_entries, 3);
	assert!(Arc::ptr_eq(failed.error.as_ref().unwrap(), &ended));
	assert_eq!(
		(
			failed.format,
			failed.password,
			failed.entries,
			failed.omitted_entries,
			failed.unaccounted_bytes
		),
		(
			Some(ArchiveFormat::Zip),
			PasswordCheck::Wrong,
			vec![archive_entry(0), archive_entry(1)],
			4,
			9
		)
	);
	assert!(ListReport::new(Ok(listing), 0).error.is_none());
}

impl ExtractArchiveCallback for Recorder {
	fn on_top_level_batch(&self, items: Vec<ExtractedTopLevelItem>) {
		for item in items {
			let ExtractTopLevelKey::Entry { id } = item.key else {
				panic!("an entry at the top");
			};
			self.push(u64::from(id.index));
		}
	}

	fn on_update(&self, update: ExtractUpdate) {
		self.push(update.active_time_ms);
	}
}

impl CompressItemsCallback for Recorder {
	fn on_archive_created(&self, archive: File) {
		let archive: RemoteFile = archive
			.try_into()
			.expect("the archive is a file of the drive");
		self.push(archive.size());
	}

	fn on_update(&self, update: CompressUpdate) {
		self.push(update.active_time_ms);
	}
}

impl ListArchiveCallback for Recorder {
	fn on_entries_batch(&self, entries: Vec<ArchiveEntry>) {
		for entry in entries {
			self.push(u64::from(entry.id.index));
		}
	}

	fn on_update(&self, update: ListUpdate) {
		self.push(update.active_time_ms);
	}
}

#[test]
fn extract_callbacks_are_delivered_in_order_until_the_job_lets_go() {
	delivered_in_order(
		|recorder, delivery| deliver_extract(recorder, delivery),
		|sender| {
			let channel = ExtractChannel(sender);
			let mut sent = Vec::new();
			for i in 0..300 {
				match i % 2 {
					0 => channel.on_top_level_batch(vec![extract::ExtractedTopLevel {
						key: ExtractTopLevelKey::Entry {
							id: entry_id(u32::try_from(i).unwrap()),
						},
						item: NonRootItemType::Dir(Cow::Owned(dir())),
					}]),
					_ => channel.on_update(extract_update(i)),
				}
				sent.push(i);
			}
			sent
		},
	);
}

#[test]
fn compress_callbacks_are_delivered_in_order_until_the_job_lets_go() {
	delivered_in_order(
		|recorder, delivery| deliver_compress(recorder, delivery),
		|sender| {
			let channel = CompressChannel(sender);
			let mut sent = Vec::new();
			for i in 0..300 {
				match i % 3 {
					0 => channel.on_archive_created(drive_file(ARCHIVE, "a.tar", i)),
					_ => channel.on_update(compress_update(i)),
				}
				sent.push(i);
			}
			sent
		},
	);
}

#[test]
fn list_callbacks_are_delivered_in_order_until_the_job_lets_go() {
	delivered_in_order(
		|recorder, delivery| deliver_list(recorder, delivery),
		|sender| {
			let channel = ListChannel::new(sender);
			let mut sent = Vec::new();
			for i in 0..300 {
				match i % 2 {
					0 => channel.on_entries_batch(vec![archive_entry(u32::try_from(i).unwrap())]),
					_ => channel.on_update(list_update(i)),
				}
				sent.push(i);
			}
			sent
		},
	);
}

fn entry_download_update(millis: u64) -> extract::EntryDownloadUpdate {
	extract::EntryDownloadUpdate {
		phase: EntryDownloadPhase::Reading,
		run_state: RunState::Running,
		bytes_read: 40,
		archive_bytes: 100,
		bytes_written: 12,
		entry_bytes: Some(30),
		bytes_per_second: None,
		eta: None,
		active_time: Duration::from_millis(millis),
	}
}

#[test]
fn an_entry_download_update_reports_milliseconds() {
	let update = EntryDownloadUpdate::from(extract::EntryDownloadUpdate {
		bytes_per_second: Some(9),
		eta: Some(Duration::from_millis(1500)),
		..entry_download_update(2500)
	});
	assert_eq!(
		(
			update.phase,
			update.bytes_read,
			update.archive_bytes,
			update.bytes_written,
			update.entry_bytes,
			update.bytes_per_second,
			update.eta_ms,
			update.active_time_ms
		),
		(
			EntryDownloadPhase::Reading,
			40,
			100,
			12,
			Some(30),
			Some(9),
			Some(1500),
			2500
		)
	);
}

#[test]
fn an_entry_download_report_carries_why_it_ended() {
	let written = extract::EntryDownloadReport {
		bytes_written: 7,
		bytes_read: 9,
		checked: true,
	};
	let report = EntryDownloadReport::new(Ok(written.clone()));
	assert_eq!(
		(report.bytes_written, report.bytes_read, report.checked),
		(7, 9, true)
	);
	assert!(report.error.is_none());
	let ended = Arc::new(Error::custom(ErrorKind::ArchiveWrongPassword, "wrong"));
	let failed = EntryDownloadReport::new(Err(JobFailed {
		report: extract::EntryDownloadReport {
			checked: false,
			..written
		},
		error: Arc::clone(&ended),
	}));
	assert_eq!(
		(failed.bytes_written, failed.bytes_read, failed.checked),
		(7, 9, false)
	);
	assert!(Arc::ptr_eq(failed.error.as_ref().unwrap(), &ended));

	// a download the binding failed once it completed (its file could not take the path's
	// place, its stream failed to close) delivered nothing checked
	let undelivered = EntryDownloadReport::new(Err(JobFailed {
		report: written,
		error: Arc::new(Error::custom(ErrorKind::IO, "not closed")),
	}));
	assert_eq!((undelivered.bytes_written, undelivered.checked), (7, false));
}

impl DownloadArchiveEntryCallback for Recorder {
	fn on_update(&self, update: EntryDownloadUpdate) {
		self.push(update.active_time_ms);
	}
}

#[test]
fn entry_download_updates_are_delivered_in_order_until_the_job_lets_go() {
	delivered_in_order(
		|recorder, update| deliver_entry_download(recorder, update),
		|sender| {
			let channel = EntryDownloadChannel(sender);
			let mut sent = Vec::new();
			for i in 0..300 {
				channel.on_update(entry_download_update(i));
				sent.push(i);
			}
			sent
		},
	);
}

/// The names in `dir`, sorted.
fn names_in(dir: &Path) -> Vec<String> {
	let mut names: Vec<String> = std::fs::read_dir(dir)
		.unwrap()
		.map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
		.collect();
	names.sort_unstable();
	names
}

#[tokio::test]
async fn an_entry_download_lands_at_its_path_only_once_whole() {
	let dir = tempfile::tempdir().unwrap();
	let path = dir.path().join("entry.bin");
	std::fs::write(&path, b"old").unwrap();
	let written = extract::EntryDownloadReport {
		bytes_written: 3,
		bytes_read: 5,
		checked: true,
	};
	let whole = |report: extract::EntryDownloadReport| {
		move |mut file: Compat<tokio::fs::File>| async move {
			file.write_all(b"new").await.unwrap();
			file.close().await.unwrap();
			Ok(report)
		}
	};

	// a download that ends early leaves the file at its path as it was, and nothing beside it
	let failed = download_into(&path, |mut file| async move {
		file.write_all(b"partial").await.unwrap();
		Err(JobFailed {
			report: extract::EntryDownloadReport::default(),
			error: Arc::new(Error::custom(ErrorKind::ArchiveCorrupt, "damaged")),
		})
	})
	.await
	.unwrap();
	assert_eq!(failed.unwrap_err().error.kind(), ErrorKind::ArchiveCorrupt);
	assert_eq!(std::fs::read(&path).unwrap(), b"old");
	assert_eq!(names_in(dir.path()), ["entry.bin"]);

	// one that completes replaces it
	let landed = download_into(&path, whole(written.clone())).await.unwrap();
	assert_eq!(landed.unwrap(), written);
	assert_eq!(std::fs::read(&path).unwrap(), b"new");
	assert_eq!(names_in(dir.path()), ["entry.bin"]);

	// one whose file cannot take the path's place fails with its report, its file removed
	let taken = dir.path().join("taken");
	std::fs::create_dir(&taken).unwrap();
	std::fs::write(taken.join("inside"), b"kept").unwrap();
	let refused = download_into(&taken, whole(written.clone()))
		.await
		.unwrap()
		.unwrap_err();
	assert_eq!(
		(refused.report, refused.error.kind()),
		(written, ErrorKind::IO)
	);
	assert_eq!(names_in(dir.path()), ["entry.bin", "taken"]);
	assert_eq!(names_in(&taken), ["inside"]);
}

#[test]
fn a_foreign_entry_is_refused_before_the_download_starts() {
	let call = |entry| {
		EntryDownloadCall::new(
			AnyFile::File(remote_file().into()),
			entry,
			None,
			Some(0),
			None,
		)
	};
	let of_another = ArchiveEntryId {
		archive: Uuid::from_u128(0xb),
		index: 0,
	};
	let error = call(of_another).err().unwrap();
	assert_eq!(error.kind(), ErrorKind::InvalidState);
	assert!(
		error
			.to_string()
			.contains("an entry chosen to extract is of another archive"),
		"{error}"
	);
	let chosen = call(entry_id(3)).unwrap();
	assert_eq!(
		(chosen.archive.uuid(), chosen.entry),
		(ARCHIVE, entry_id(3))
	);
}

#[test]
fn a_download_call_takes_the_sdks_expansion_limit_unless_it_names_one_but_never_a_solid_skip() {
	let config = |expansion_limit, max_solid_skip| {
		EntryDownloadCall::new(
			AnyFile::File(remote_file().into()),
			entry_id(0),
			expansion_limit,
			max_solid_skip,
			None,
		)
		.unwrap()
		.config
	};
	let left_out = config(None, None);
	assert_eq!(
		(left_out.expansion_limit, left_out.max_solid_skip),
		(EntryDownloadConfig::default().expansion_limit, None),
		"`None` allows any skip, unlike the Rust default"
	);
	let limit = ExpansionLimit {
		ratio: 10,
		floor: 1 << 20,
	};
	let named = config(Some(limit), Some(5));
	assert_eq!(
		(named.expansion_limit, named.max_solid_skip),
		(Some(limit), Some(5))
	);
}

#[test]
fn a_listing_holds_no_more_entries_than_its_limit_for_an_app_that_lags() {
	let (sender, mut delivery) = tokio::sync::mpsc::unbounded_channel();
	let channel = ListChannel::new(sender);
	// entries of 1 MiB of text each: a lagging app gets as many as fit, and the rest are counted
	let big = |index| ArchiveEntry {
		stored_path: "a".repeat(1 << 20),
		..archive_entry(index)
	};
	let sent = 3 * MAX_UNDELIVERED_ENTRY_BYTES / (1 << 20);
	for index in 0..sent {
		channel.on_entries_batch(vec![big(u32::try_from(index).unwrap())]);
	}
	channel.on_update(list_update(1));
	let queued = channel.queued.load(Ordering::Relaxed);
	assert!(queued <= MAX_UNDELIVERED_ENTRY_BYTES, "{queued}");
	let mut delivered = Vec::new();
	let mut undelivered = None;
	while let Ok(item) = delivery.try_recv() {
		match item {
			ListDelivery::Entries(entries, _) => {
				delivered.extend(entries.iter().map(|e| e.id.index))
			}
			ListDelivery::Update(update) => undelivered = Some(update.undelivered_entries),
		}
	}
	// the first ones, in order, and the update counts the rest
	let kept = delivered.len();
	assert!(kept > 0 && kept < sent, "{kept} of {sent}");
	assert_eq!(
		delivered,
		(0..u32::try_from(kept).unwrap()).collect::<Vec<_>>()
	);
	assert_eq!(undelivered, Some((sent - kept) as u64));
	// delivered, the entries no longer count, and the next batch goes through
	assert_eq!(channel.queued.load(Ordering::Relaxed), 0);
	channel.on_entries_batch(vec![big(0)]);
	assert!(matches!(delivery.try_recv(), Ok(ListDelivery::Entries(..))));
}

#[test]
fn a_listing_holds_no_more_memory_than_its_limit_in_short_entries() {
	let (sender, delivery) = tokio::sync::mpsc::unbounded_channel();
	let channel = ListChannel::new(sender);
	// short paths: the entries holding them cost several times their text, which alone would
	// let all of them queue for an app that never takes one
	let sent = 300_000;
	let ((), peak) = alloc_meter::peak_bytes(|| {
		for index in 0..sent {
			channel.on_entries_batch(vec![archive_entry(index)]);
		}
	});
	// what one batch takes on its way in, and the channel's own blocks
	const SLACK: u64 = 1 << 20;
	assert!(
		peak <= MAX_UNDELIVERED_ENTRY_BYTES as u64 + SLACK,
		"{peak} bytes held"
	);
	assert!(channel.undelivered.load(Ordering::Relaxed) > 0);
	drop(delivery);
}
