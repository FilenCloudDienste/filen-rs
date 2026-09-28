use std::{borrow::Cow, sync::Mutex};

use filen_types::api::v3::dir::color::DirColor;
use tokio::task::JoinHandle;

use super::*;
use crate::{
	fs::{
		archive::{
			DisposalOutcome, KeptReason,
			compress::{
				CompressUpdate, Compression, RunState, StreamCodec, report::CompressCallback,
			},
			config::{CODEC_MEM_BUDGET, JOB_CONCURRENCY},
			test_support::{pattern, remote_file},
		},
		dir::{RemoteDirectory, meta::DecryptedDirectoryMeta},
		drive_job::{
			plan::{Listed, PlanTotals, SourceDir},
			test_support::wait_until,
		},
		file::{RemoteFile, enums::RemoteFileType},
	},
	job::test_support::{controls, settled_run_states},
};

#[expect(dead_code, reason = "only named by the compile-time check below")]
struct Ignore;

impl CompressCallback for Ignore {
	fn on_archive_created(&self, _: RemoteFile) {}
	fn on_update(&self, _: CompressUpdate) {}
}

/// The bindings run compression on the SDK's multi-threaded runtime, which needs a `Send`
/// future.
fn _compress_future_is_send(client: Arc<Client>, destination: DirType<'static, Normal>) {
	fn assert_send<T: Send>(_: T) {}
	assert_send(client.compress_items(
		CompressSources::Keep(Vec::new()),
		destination,
		ValidatedName::try_from("a.tar").unwrap(),
		CompressConfig {
			format: CompressFormat::Tar { compression: None },
			max_bytes: None,
			password: None,
		},
		Ignore,
		JobControl::default(),
	));
}

fn source_dir(name: &str) -> SourceDir<()> {
	SourceDir {
		uuid: Uuid::new_v4(),
		name: Some(name.to_owned()),
		created: None,
		color: DirColor::Default,
		handle: (),
	}
}

fn file(name: &str, size: u64) -> RemoteFileType<'static> {
	let data = pattern(usize::try_from(size).unwrap(), 1);
	remote_file(Uuid::new_v4(), Uuid::new_v4(), name, &data, None)
}

/// A plan of `Photos/` (with `a.jpg` and `2024/b.jpg`) and a top-level file also called
/// `Photos`, into an archive root.
fn plan() -> ItemPlan<()> {
	let root = Uuid::new_v4();
	let photos = source_dir("Photos");
	let year = source_dir("2024");
	let mut planner = ItemPlanner::default();
	planner.add_destination(root, std::iter::empty());
	let listed = |parent: &SourceDir<()>, item| Listed {
		parent: parent.uuid,
		item,
	};
	planner
		.plan(vec![
			PlanRequest {
				source: PlanSource::Dir {
					root: photos.clone(),
					dirs: vec![listed(&photos, year.clone())],
					files: vec![
						Listed {
							parent: photos.uuid,
							item: file("a.jpg", 3),
						},
						Listed {
							parent: year.uuid,
							item: file("b.jpg", 4),
						},
					],
				},
				destination: root,
				name: None,
			},
			PlanRequest {
				source: PlanSource::File(file("Photos", 5)),
				destination: root,
				name: None,
			},
		])
		.unwrap()
}

#[test]
fn a_plan_becomes_entries_with_joined_paths() {
	let renamed = top_level_renames(&plan());
	assert_eq!(renamed.len(), 1);
	assert_eq!(
		(renamed[0].source_path.as_str(), renamed[0].name.as_ref()),
		("Photos", "Photos (1)")
	);
	let (entries, sources) =
		archive_entries(plan(), CompressFormat::Tar { compression: None }).unwrap();
	let paths: Vec<(&str, Option<u64>)> = entries
		.iter()
		.map(|entry| match entry {
			ArchiveEntry::Dir { path, .. } => (path.as_str(), None),
			ArchiveEntry::File { path, size, .. } => (path.as_str(), Some(*size)),
		})
		.collect();
	let mut files: Vec<_> = paths
		.iter()
		.filter(|(_, size)| size.is_some())
		.copied()
		.collect();
	files.sort();
	assert_eq!(&paths[..2], [("Photos", None), ("Photos/2024", None)]);
	assert_eq!(
		files,
		[
			("Photos (1)", Some(5)),
			("Photos/2024/b.jpg", Some(4)),
			("Photos/a.jpg", Some(3)),
		]
	);
	assert_eq!(sources.len(), 3);
	// the source numbers point at the sources in order
	for entry in &entries {
		if let ArchiveEntry::File { source, path, .. } = entry {
			assert_eq!(&sources[*source as usize].path, path);
		}
	}

	assert_eq!(
		archive_entries(
			plan(),
			CompressFormat::Single {
				compression: Compression {
					codec: StreamCodec::Gzip,
					level: None,
				},
			},
		)
		.unwrap_err()
		.kind(),
		ErrorKind::InvalidState,
		"a single compressed file is exactly one file"
	);
}

#[test]
fn only_a_keep_both_rename_at_the_top_is_reported_as_one() {
	let root = Uuid::new_v4();
	let mut planner = ItemPlanner::default();
	planner.add_destination(root, std::iter::empty());
	let request = |source| PlanRequest {
		source,
		destination: root,
		name: None,
	};
	// a name only valid once encoded, a name in NFD, and two files of the same name
	let nfd = source_dir("Cafe\u{301}");
	let plan = planner
		.plan(vec![
			request(PlanSource::File(file("a:b.txt", 1))),
			request(PlanSource::Dir {
				root: nfd.clone(),
				dirs: Vec::new(),
				files: Vec::new(),
			}),
			request(PlanSource::File(file("x.txt", 2))),
			request(PlanSource::File(file("x.txt", 3))),
		])
		.unwrap();
	let legacy = plan.files[0].source.uuid();
	assert_eq!(
		plan.renamed
			.iter()
			.map(|renamed| (renamed.source_uuid, renamed.reason))
			.collect::<Vec<_>>(),
		[(legacy, RenameReason::InvalidName)],
		"the planner reports the encoded name"
	);
	let renamed = top_level_renames(&plan);
	assert_eq!(
		renamed
			.iter()
			.map(|renamed| (
				renamed.source_path.as_str(),
				renamed.name.as_ref(),
				renamed.reason
			))
			.collect::<Vec<_>>(),
		[("x.txt", "x (1).txt", RenameReason::DuplicateName)]
	);
}

#[test]
fn a_legacy_name_that_also_collides_is_reported_once() {
	let root = Uuid::new_v4();
	let mut planner = ItemPlanner::default();
	planner.add_destination(root, std::iter::empty());
	let request = |source: PlanSource<()>| PlanRequest {
		source,
		destination: root,
		name: None,
	};
	let plan = planner
		.plan(vec![
			request(PlanSource::File(file("a:b.txt", 1))),
			request(PlanSource::File(file("a:b.txt", 2))),
		])
		.unwrap();
	let [first, second] = [0, 1].map(|index| &plan.files[index]);
	let mut renamed = plan.renamed.clone();
	renamed.extend(top_level_renames(&plan));
	let renamed: Vec<_> = renamed
		.iter()
		.map(|renamed| (renamed.source_uuid, renamed.name.clone(), renamed.reason))
		.collect();
	assert_eq!(
		renamed,
		[
			(
				first.source.uuid(),
				first.name.clone(),
				RenameReason::InvalidName
			),
			(
				second.source.uuid(),
				second.name.clone(),
				RenameReason::DuplicateName
			),
		],
		"each once, under the name it got"
	);
	assert_ne!(first.name, second.name);
}

#[test]
fn disposal_targets_hold_exactly_what_was_read() {
	let plan = plan();
	let photos = &plan.dirs[0];
	let year = &plan.dirs[1];
	let top = plan
		.files
		.iter()
		.find(|file| file.request == 1)
		.unwrap()
		.source
		.clone();
	let RemoteFileType::File(top_file) = &top else {
		unreachable!()
	};
	let photos_dir = RemoteDirectory::new_from_parts(
		photos.source_uuid,
		DecryptedDirectoryMeta {
			name: Cow::Borrowed("Photos"),
			created: None,
		},
		Uuid::new_v4().into(),
		chrono::Utc::now(),
	);
	let mut normal_top = RemoteFile::from_meta(
		top.uuid(),
		filen_types::fs::StableUuid::new_for_test(top.uuid()),
		Uuid::new_v4().into(),
		top_file.size(),
		top_file.chunks(),
		"de-1",
		"bucket",
		chrono::Utc::now(),
		false,
		top_file.meta.clone(),
	);
	let parent = Uuid::try_from(normal_top.parent).unwrap();
	let items = |top: &RemoteFile| {
		vec![
			NonRootItemType::Dir(Cow::Owned(photos_dir.clone())),
			NonRootItemType::File(Cow::Owned(top.clone())),
		]
	};
	let disposal = disposal(
		&plan,
		SourceDisposal::Trash,
		items(&normal_top),
		Uuid::new_v4(),
	)
	.unwrap();
	assert_eq!(
		disposal.hashed,
		[false, false],
		"the plan's files carry no hash"
	);
	let [
		DisposalTarget::Dir { uuid, read },
		DisposalTarget::File(file),
	] = &disposal.targets[..]
	else {
		panic!("{:?}", disposal.targets);
	};
	assert_eq!(*uuid, photos.source_uuid);
	assert_eq!(read.dirs, [year.source_uuid].into_iter().collect());
	let sizes: Vec<u64> = read.files.values().copied().collect();
	assert_eq!(read.files.len(), 2);
	assert_eq!(sizes.iter().sum::<u64>(), 7);
	assert_eq!(
		*file,
		ExpectedFile {
			uuid: top.uuid(),
			size: 5,
			chunks: 1,
			parent,
		}
	);

	// the archive would land in a source that is removed afterwards
	for inside in [photos.source_uuid, year.source_uuid] {
		assert_eq!(
			disposal_of(&plan, items(&normal_top), inside)
				.unwrap_err()
				.kind(),
			ErrorKind::InvalidState
		);
	}

	// a file in the trash is left alone
	normal_top.parent = filen_types::fs::ParentUuid::Trash(parent);
	let disposal = disposal_of(&plan, items(&normal_top), Uuid::new_v4()).unwrap();
	assert!(matches!(
		disposal.targets[1],
		DisposalTarget::Unavailable { .. }
	));
}

fn disposal_of(
	plan: &ItemPlan<()>,
	items: Vec<NonRootItemType<'static, Normal>>,
	destination: Uuid,
) -> Result<CompressDisposal, Error> {
	disposal(plan, SourceDisposal::Trash, items, destination)
}

#[test]
fn paths_extracting_would_skip_are_refused() {
	let at_cap = "a".repeat(MAX_ARCHIVE_PATH_BYTES);
	assert!(check_path(&at_cap, 0).is_ok());
	assert_eq!(
		check_path(&at_cap, 1).unwrap_err().kind(),
		ErrorKind::InvalidName,
		"a directory's trailing slash counts"
	);
	let deep = vec!["d"; MAX_ARCHIVE_PATH_DEPTH].join("/");
	assert!(check_path(&deep, 0).is_ok());
	assert!(check_path(&format!("{deep}/x"), 0).is_err());
}

/// Makes up listings, one file per directory, recording which directories it was asked to
/// list.
#[derive(Default)]
struct FakeLister {
	listed: Mutex<Vec<Uuid>>,
}

impl SourceLister for FakeLister {
	async fn list_source(
		&self,
		dir: ItemSourceDir,
		_: &ListingBytes,
	) -> Result<PlanSource<ItemSourceDir>, Error> {
		let ItemSourceDir::Normal(dir) = dir else {
			unreachable!("the tests list only the user's own directories")
		};
		let uuid = dir.uuid();
		self.listed.lock().unwrap().push(uuid);
		Ok(PlanSource::Dir {
			root: SourceDir::new(dir, DirColor::Default, ItemSourceDir::Normal),
			dirs: Vec::new(),
			files: vec![Listed {
				parent: uuid,
				item: file("inside.txt", 7),
			}],
		})
	}
}

#[derive(Default)]
struct Updates(Mutex<Vec<CompressUpdate>>);

impl CompressCallback for Updates {
	fn on_archive_created(&self, _: RemoteFile) {}

	fn on_update(&self, update: CompressUpdate) {
		self.0.lock().unwrap().push(update);
	}
}

impl Updates {
	/// The run states the updates went through, each change once, pausing left out.
	fn run_states(&self) -> Vec<RunState> {
		let updates = self.0.lock().unwrap();
		settled_run_states(updates.iter().map(|update| update.run_state))
	}

	fn last_phase(&self) -> CompressPhase {
		self.0.lock().unwrap().last().unwrap().phase
	}
}

fn remote_dir(name: &str) -> RemoteDirectory {
	RemoteDirectory::new_from_parts(
		Uuid::new_v4(),
		DecryptedDirectoryMeta {
			name: Cow::Owned(name.to_owned()),
			created: None,
		},
		Uuid::new_v4().into(),
		chrono::Utc::now(),
	)
}

/// A compression of `sources` planned with a pause asked for before it starts.
struct PausedPlan {
	lister: Arc<FakeLister>,
	updates: Arc<Updates>,
	reporter: MaybeArc<Reporter>,
	pause: tokio::sync::watch::Sender<bool>,
	cancel: tokio::sync::watch::Sender<bool>,
	/// The plan's totals once it is done.
	planned: JoinHandle<Result<PlanTotals, CompressFailed>>,
}

async fn plan_paused(sources: CompressSources) -> PausedPlan {
	let lister = Arc::new(FakeLister::default());
	let updates = Arc::new(Updates::default());
	let reporter = Reporter::new(Arc::clone(&updates));
	let (pause, cancel, control) = controls();
	pause.send_replace(true);
	let planned = tokio::spawn({
		let lister = Arc::clone(&lister);
		let reporter = MaybeArc::clone(&reporter);
		async move {
			plan_compression(
				&*lister,
				&ArchiveConfig::new(CODEC_MEM_BUDGET, JOB_CONCURRENCY),
				sources,
				Uuid::new_v4(),
				&ValidatedName::try_from("a.tar").unwrap(),
				CompressConfig {
					format: CompressFormat::Tar { compression: None },
					max_bytes: None,
					password: None,
				},
				&reporter,
				&control,
			)
			.await
			.map(|planned| planned.report.totals)
		}
	});
	wait_until("the job reports itself paused", || reporter.is_paused()).await;
	PausedPlan {
		lister,
		updates,
		reporter,
		pause,
		cancel,
		planned,
	}
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_single_file_of_a_folder_or_of_two_files_is_refused_before_listing() {
	let single = CompressFormat::Single {
		compression: Compression {
			codec: StreamCodec::Gzip,
			level: None,
		},
	};
	for (case, sources) in [
		(
			"a folder",
			vec![ItemSource::Dir(ItemSourceDir::Normal(remote_dir("docs")))],
		),
		(
			"two files",
			vec![
				ItemSource::File(file("a.txt", 3)),
				ItemSource::File(file("b.txt", 4)),
			],
		),
	] {
		let lister = FakeLister::default();
		let reporter = Reporter::new(Arc::new(Updates::default()));
		let (_pause, _cancel, control) = controls();
		let failed = plan_compression(
			&lister,
			&ArchiveConfig::new(CODEC_MEM_BUDGET, JOB_CONCURRENCY),
			CompressSources::Keep(sources),
			Uuid::from_u128(1),
			&ValidatedName::try_from("a.gz").unwrap(),
			CompressConfig {
				format: single,
				max_bytes: None,
				password: None,
			},
			&reporter,
			&control,
		)
		.await
		.err()
		.expect(case);
		assert_eq!(failed.error.kind(), ErrorKind::InvalidState, "{case}");
		assert!(lister.listed.lock().unwrap().is_empty(), "{case}");
	}
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_compression_paused_before_it_starts_lists_nothing_until_resumed() {
	let dir = remote_dir("docs");
	let folder = CompressSources::Keep(vec![ItemSource::Dir(ItemSourceDir::Normal(dir.clone()))]);
	let file_only = CompressSources::Keep(vec![ItemSource::File(file("a.txt", 3))]);
	for (case, sources, listed) in [
		("folder", folder, vec![dir.uuid()]),
		("file-only", file_only, Vec::new()),
	] {
		let paused = plan_paused(sources).await;
		assert!(paused.lister.listed.lock().unwrap().is_empty(), "{case}");
		assert_eq!(paused.updates.run_states(), [RunState::Paused], "{case}");
		assert_eq!(
			paused.updates.last_phase(),
			CompressPhase::Scanning,
			"{case}"
		);
		assert!(!paused.planned.is_finished(), "{case}");

		paused.pause.send_replace(false);
		let totals = paused.planned.await.unwrap().unwrap();
		assert_eq!(*paused.lister.listed.lock().unwrap(), listed, "{case}");
		assert_eq!(totals.files, 1, "{case}");
		// the plan's update is sent once resumed, never reading running before
		assert_eq!(
			paused.updates.run_states(),
			[RunState::Paused, RunState::Running],
			"{case}"
		);
		assert_eq!(paused.reporter.ops_in_flight(), 0, "{case}");
	}
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_compression_cancelled_while_paused_before_it_starts_keeps_its_sources() {
	let dir = remote_dir("docs");
	let paused = plan_paused(CompressSources::Dispose {
		how: SourceDisposal::Trash,
		items: vec![NonRootItemType::Dir(Cow::Owned(dir.clone()))],
	})
	.await;
	paused.cancel.send_replace(true);
	let failed = paused.planned.await.unwrap().unwrap_err();

	assert_eq!(failed.error.kind(), ErrorKind::Cancelled);
	assert!(paused.lister.listed.lock().unwrap().is_empty());
	let [disposition] = &failed.report.dispositions[..] else {
		panic!("one source: {:?}", failed.report.dispositions);
	};
	assert_eq!(disposition.uuid, dir.uuid());
	assert!(
		matches!(
			disposition.outcome,
			DisposalOutcome::Kept {
				reason: KeptReason::Interrupted,
				bytes_freed: 0,
			}
		),
		"{:?}",
		disposition.outcome
	);
	assert_eq!(
		paused.updates.run_states(),
		[RunState::Paused, RunState::Cancelling]
	);
	assert_eq!(paused.updates.last_phase(), CompressPhase::Cancelled);
}
