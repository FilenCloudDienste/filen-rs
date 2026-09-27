//! The public compress API on [`Client`]: lists and plans the sources, checks the archive can be
//! written and extracted again, and runs the job.

use std::{collections::HashSet, sync::Arc};

use filen_types::fs::Uuid;

use crate::{
	Error, ErrorKind,
	auth::Client,
	fs::{
		HasUUID,
		archive::{
			dispose::{ExpectedFile, SourceDisposal, Tree},
			limits::{MAX_ARCHIVE_PATH_BYTES, MAX_ARCHIVE_PATH_DEPTH},
			worker,
		},
		categories::{DirType, NonRootItemType, Normal},
		drive_job::{
			backend::ClientBackend,
			listing::{ItemSource, ItemSourceDir, ListingBytes, ScanError, watch_listing},
			plan::{
				DestParent, ItemPlan, ItemPlanner, PlanRequest, PlanSource, PlannedItem,
				RenameReason, RenamedEntry,
			},
		},
		file::{
			enums::RemoteFileType,
			traits::{HasFileInfo, HasRemoteFileInfo},
		},
		name::{ValidatedName, keep_both::SourceName},
	},
	job::JobControl,
	util::MaybeArc,
};

use super::{
	ArchivePassword, CompressFormat, CompressSources,
	codec::{ArchiveEntry, CompressJob, compress, tar_size},
	engine::{
		CompressDisposal, CompressTask, DisposalTarget, Source, cancelled, end_early, run_compress,
	},
	report::{CompressCallback, CompressFailed, CompressPhase, CompressReport, Reporter},
};

/// What to compress, and where the archive goes.
#[derive(Debug, Clone)]
pub struct CompressRequest {
	/// The items, and whether to remove them afterwards.
	pub sources: CompressSources,
	/// The directory of the user's drive the archive is created in.
	pub destination: DirType<'static, Normal>,
	/// The archive's whole name, ending in the format's extension (see
	/// [`CompressFormat::extension`]); a name the destination holds gets the next keep-both name
	/// (`photos (1).tar.gz`).
	pub name: ValidatedName,
}

/// How a compression writes its archive.
#[derive(Debug, Clone)]
pub struct CompressConfig {
	/// What the archive is written as.
	pub format: CompressFormat,
	/// Storage still free on the account, if the caller knows it. A bare tar's size is known
	/// before anything is written, and one that would reach this is refused up front with
	/// [`ErrorKind::MaxStorageReached`], its size in the report's `needed_bytes`; a compressed
	/// archive is refused as soon as its written bytes would reach it, leaving nothing behind.
	/// Reaching it counts: an archive exactly as large as the free storage is refused, as the
	/// server refuses an upload that would fill the account.
	pub max_bytes: Option<u64>,
	/// For an encrypted format, and only then.
	pub password: Option<ArchivePassword>,
}

impl Client {
	/// Compresses the request's `sources` into a new archive called `name` in `destination`.
	/// Nothing can encode on the server (items are end-to-end encrypted), so every source is
	/// downloaded and encoded here, and the archive is encrypted and uploaded as one new file.
	///
	/// `name` is the archive's whole name and has to carry the format's extension (see
	/// [`CompressFormat::extension`]); a name the destination holds gets the next keep-both name
	/// (`photos (1).tar.gz`). Sources may be the user's own items, shared ones or linked ones.
	/// Undecryptable files are skipped and names that collide inside the archive renamed, both
	/// reported; a path longer or deeper than an archive may hold fails the job before anything
	/// is written, so every archive the SDK writes can be extracted by it again.
	///
	/// The archive only becomes visible once all of it is uploaded, so a job that ends early
	/// leaves nothing behind. Up to [`ArchiveConfig::job_concurrency`] archive jobs run at once;
	/// a later one waits, reporting [`CompressPhase::WaitingForWorker`]. It reports its progress
	/// to `callback` and can be paused, resumed and cancelled through `control`.
	///
	/// [`ArchiveConfig::job_concurrency`]: crate::fs::archive::ArchiveConfig::job_concurrency
	pub async fn compress_items(
		self: Arc<Self>,
		request: CompressRequest,
		config: CompressConfig,
		callback: impl CompressCallback,
		control: JobControl,
	) -> Result<CompressReport, CompressFailed> {
		let CompressRequest {
			sources,
			destination,
			name,
		} = request;
		let reporter = Reporter::new(callback);
		let archives = self.client().state().archives().clone();
		// every source a disposal was asked for is reported, however early the job ends
		let requested: Vec<Uuid> = match &sources {
			CompressSources::Keep(_) => Vec::new(),
			CompressSources::Dispose { items, .. } => {
				items.iter().map(|item| item.uuid()).collect()
			}
		};
		let refuse =
			|report, phase, error| end_early(&reporter, report, &requested, phase, Arc::new(error));
		let checked = config
			.format
			.check_name(name.as_ref())
			.and_then(|extension_len| {
				config.format.check(config.password.is_some())?;
				let memory = config.format.encoder_memory()?;
				if memory > archives.codec_mem_budget {
					return Err(Error::custom(
						ErrorKind::InsufficientMemory,
						format!(
							"this format needs {memory} bytes of codec memory, over the {} allowed",
							archives.codec_mem_budget
						),
					));
				}
				Ok(extension_len)
			});
		let extension_len = match checked {
			Ok(extension_len) => extension_len,
			Err(error) => {
				return Err(refuse(
					CompressReport::default(),
					CompressPhase::Failed,
					error,
				));
			}
		};

		let (sources, dispose) = match sources {
			CompressSources::Keep(sources) => (sources, None),
			CompressSources::Dispose { how, items } => {
				let sources = items
					.iter()
					.map(|item| match item {
						NonRootItemType::File(file) => {
							ItemSource::File(RemoteFileType::from(file.clone().into_owned()))
						}
						NonRootItemType::Dir(dir) => {
							ItemSource::Dir(ItemSourceDir::Normal(dir.clone().into_owned()))
						}
					})
					.collect();
				(sources, Some((how, items)))
			}
		};
		let mut plan = match self.plan_sources(sources, &reporter, &control).await {
			Ok(plan) => plan,
			Err(ScanError::Stopped) => {
				return Err(end_early(
					&reporter,
					CompressReport::default(),
					&requested,
					CompressPhase::Cancelled,
					cancelled(),
				));
			}
			Err(ScanError::Failed(error)) => {
				return Err(refuse(
					CompressReport::default(),
					CompressPhase::Failed,
					error,
				));
			}
		};
		let top_level_renamed = top_level_renames(&plan);
		// the plan's records move into the report; nothing reads them in the plan again
		let mut report = CompressReport {
			skipped: std::mem::take(&mut plan.skipped),
			renamed: std::mem::take(&mut plan.renamed),
			totals: plan.totals,
			..CompressReport::default()
		};
		report.renamed.extend(top_level_renamed);
		let disposal =
			match dispose.map(|(how, items)| disposal(&plan, how, items, destination.uuid())) {
				None => None,
				Some(Ok(disposal)) => Some(disposal),
				Some(Err(error)) => return Err(refuse(report, CompressPhase::Failed, error)),
			};
		let (entries, sources) = match archive_entries(plan, config.format) {
			Ok(entries) => entries,
			Err(error) => return Err(refuse(report, CompressPhase::Failed, error)),
		};
		if let (CompressFormat::Tar { compression: None }, Some(max)) =
			(config.format, config.max_bytes)
		{
			let needed = tar_size(&entries);
			if needed >= max {
				report.needed_bytes = Some(needed);
				let error = Error::custom(
					ErrorKind::MaxStorageReached,
					format!("the archive needs {needed} bytes but only {max} are free"),
				);
				return Err(refuse(report, CompressPhase::Failed, error));
			}
		}
		reporter.set_plan(report.totals, &report.skipped, &report.renamed);

		let job = CompressJob {
			format: config.format,
			entries,
			password: config.password,
		};
		run_compress(CompressTask {
			backend: Arc::new(ClientBackend::new(self)),
			control,
			reporter,
			destination,
			name,
			extension_len,
			sources,
			max_bytes: config.max_bytes,
			config: archives,
			start: Box::new(move || worker::start(move |port| compress(&port, job))),
			report,
			disposal,
		})
		.await
	}

	/// Lists every source directory and plans the archive's entries under a root of its own.
	async fn plan_sources(
		&self,
		sources: Vec<ItemSource>,
		reporter: &MaybeArc<Reporter>,
		control: &JobControl,
	) -> Result<ItemPlan<ItemSourceDir>, ScanError> {
		let sources_total = sources
			.iter()
			.filter(|source| matches!(source, ItemSource::Dir(_)))
			.count() as u64;
		let bytes = ListingBytes::default();
		let ops = reporter.ops();
		let mut sources_done = 0;
		let report = |sources_done| reporter.set_scan(bytes.scan(sources_done, sources_total));
		report(sources_done);

		// the archive's root: nothing is in it yet, and nothing is listed for it
		let root = Uuid::new_v4();
		let mut planner = ItemPlanner::default();
		planner.add_destination(root, std::iter::empty());
		let mut requests = Vec::with_capacity(sources.len());
		for source in sources {
			let source = match source {
				ItemSource::File(file) => PlanSource::File(file),
				ItemSource::Dir(dir) => {
					reporter.checkpoint(control).await?;
					bytes.next_source();
					let listing = self.list_item_source(dir, &bytes);
					let source = watch_listing(listing, &ops, control, || report(sources_done))
						.await?
						.map_err(ScanError::Failed)?;
					sources_done += 1;
					report(sources_done);
					source
				}
			};
			requests.push(PlanRequest {
				source,
				destination: root,
				name: None,
			});
		}
		planner.plan(requests).map_err(ScanError::Failed)
	}
}

/// What removing `items`, the job's sources in request order, has to find unchanged: a file as
/// it was listed, a directory holding exactly the files and directories the plan read below it.
fn disposal<D>(
	plan: &ItemPlan<D>,
	how: SourceDisposal,
	items: Vec<NonRootItemType<'static, Normal>>,
	destination: Uuid,
) -> Result<CompressDisposal, Error> {
	let mut targets = Vec::with_capacity(items.len());
	for (request, item) in items.into_iter().enumerate() {
		targets.push(match item {
			NonRootItemType::File(file) => match Uuid::try_from(file.parent) {
				Ok(parent) => DisposalTarget::File(ExpectedFile::of(&*file, file.uuid(), parent)),
				Err(_) => DisposalTarget::Unavailable { uuid: file.uuid() },
			},
			NonRootItemType::Dir(dir) => {
				let uuid = dir.uuid();
				let below: Vec<Uuid> = plan
					.dirs
					.iter()
					.filter(|planned| planned.request == request && planned.source_uuid != uuid)
					.map(|planned| planned.source_uuid)
					.collect();
				// trashing the directory would take the archive with it
				if destination == uuid || below.contains(&destination) {
					return Err(Error::custom(
						ErrorKind::InvalidState,
						"the archive cannot be written into a source that is removed afterwards",
					));
				}
				let files = plan
					.files
					.iter()
					.filter(|planned| planned.request == request)
					.map(|planned| (planned.source.uuid(), planned.size))
					.collect();
				DisposalTarget::Dir {
					uuid,
					read: Tree {
						files,
						dirs: below.into_iter().collect(),
					},
				}
			}
		});
	}
	let mut hashed = vec![true; targets.len()];
	for file in plan
		.files
		.iter()
		.filter(|file| file.source.hash().is_none())
	{
		hashed[file.request] = false;
	}
	Ok(CompressDisposal {
		how,
		targets,
		hashed,
	})
}

/// Top-level items the planner gave keep-both names, since two sources had the same name. The
/// planner reports the other renames of top-level items itself. An item is reported once, for
/// the reason the planner gave it: a legacy name encoded to be valid that then also collided is
/// a keep-both rename, under the name it ended up with.
fn top_level_renames<D>(plan: &ItemPlan<D>) -> Vec<RenamedEntry> {
	let reported: HashSet<Uuid> = plan
		.renamed
		.iter()
		.map(|renamed| renamed.source_uuid)
		.collect();
	plan.top_level
		.iter()
		.filter_map(|top| {
			let (source_uuid, source_path, name) = match top.item {
				PlannedItem::Dir(index) => {
					let dir = &plan.dirs[index];
					(dir.source_uuid, &dir.source_path, &dir.name)
				}
				PlannedItem::File(index) => {
					let file = &plan.files[index];
					(file.source.uuid(), &file.source_path, &file.name)
				}
			};
			// an item's own name, as the planner made it valid (NFC, legacy names encoded)
			let own = SourceName::parse(source_path).map(SourceName::into_name);
			let kept_both = !reported.contains(&source_uuid)
				&& own.is_ok_and(|own| own.as_ref() != name.as_ref());
			kept_both.then(|| RenamedEntry {
				source_uuid,
				source_path: source_path.clone(),
				name: name.clone(),
				reason: RenameReason::DuplicateName,
			})
		})
		.collect()
}

/// The archive's entries (directories parent first, then files) and the files the codec reads,
/// with every path checked against what extracting accepts.
fn archive_entries<D>(
	plan: ItemPlan<D>,
	format: CompressFormat,
) -> Result<(Vec<ArchiveEntry>, Vec<Source>), Error> {
	if matches!(format, CompressFormat::Single { .. })
		&& (plan.files.len() != 1 || !plan.dirs.is_empty())
	{
		return Err(Error::custom(
			ErrorKind::InvalidState,
			"a single compressed file is made of exactly one file",
		));
	}
	let mut dir_paths: Vec<String> = Vec::with_capacity(plan.dirs.len());
	let path_in = |parent: DestParent, name: &ValidatedName, dir_paths: &[String]| match parent {
		DestParent::Existing(_) => name.as_ref().to_owned(),
		DestParent::Planned(index) => format!("{}/{}", dir_paths[index], name.as_ref()),
	};
	let mut entries = Vec::with_capacity(plan.dirs.len() + plan.files.len());
	for dir in &plan.dirs {
		let path = path_in(dir.parent, &dir.name, &dir_paths);
		// a directory's stored path ends in a `/`
		check_path(&path, 1)?;
		dir_paths.push(path.clone());
		entries.push(ArchiveEntry::Dir {
			path,
			modified: dir.created,
		});
	}
	let mut sources = Vec::with_capacity(plan.files.len());
	for (index, file) in plan.files.into_iter().enumerate() {
		let path = path_in(file.parent, &file.name, &dir_paths);
		check_path(&path, 0)?;
		entries.push(ArchiveEntry::File {
			source: u32::try_from(index).map_err(|_| {
				Error::custom(ErrorKind::InvalidState, "too many files for one archive")
			})?,
			path: path.clone(),
			size: file.size,
			modified: file.source.last_modified(),
		});
		sources.push(Source {
			file: file.source,
			path,
			request: file.request,
		});
	}
	Ok((entries, sources))
}

/// Refuses a path extracting would skip, with `extra` bytes stored after it.
fn check_path(path: &str, extra: usize) -> Result<(), Error> {
	if path.len() + extra > MAX_ARCHIVE_PATH_BYTES {
		return Err(Error::custom(
			ErrorKind::InvalidName,
			format!("a path in the archive is longer than {MAX_ARCHIVE_PATH_BYTES} bytes"),
		));
	}
	if path.split('/').count() > MAX_ARCHIVE_PATH_DEPTH {
		return Err(Error::custom(
			ErrorKind::InvalidName,
			format!("a path in the archive is more than {MAX_ARCHIVE_PATH_DEPTH} levels deep"),
		));
	}
	Ok(())
}

#[cfg(test)]
mod tests {
	use std::borrow::Cow;

	use super::*;
	use crate::{
		crypto::{file::FileKey, shared::CreateRandom, v3::EncryptionKey},
		fs::{
			archive::compress::{CompressUpdate, report::CompressCallback},
			dir::RemoteDirectory,
			drive_job::plan::{Listed, SourceDir},
			file::{
				AnonymousRemoteFile, RemoteFile,
				enums::RemoteFileType,
				meta::{DecryptedFileMeta, FileMeta},
			},
		},
	};

	#[expect(dead_code, reason = "only named by the compile-time check below")]
	struct Ignore;

	impl CompressCallback for Ignore {
		fn on_archive_created(&self, _: crate::fs::file::RemoteFile) {}
		fn on_update(&self, _: CompressUpdate) {}
	}

	/// The bindings run compression on the SDK's multi-threaded runtime, which needs a `Send`
	/// future.
	fn _compress_future_is_send(client: Arc<Client>, destination: DirType<'static, Normal>) {
		fn assert_send<T: Send>(_: T) {}
		assert_send(client.compress_items(
			CompressRequest {
				sources: CompressSources::Keep(Vec::new()),
				destination,
				name: ValidatedName::try_from("a.tar").unwrap(),
			},
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
			color: filen_types::api::v3::dir::color::DirColor::Default,
			handle: (),
		}
	}

	fn file(name: &str, size: u64) -> RemoteFileType<'static> {
		let anonymous: AnonymousRemoteFile = RemoteFile::from_meta(
			Uuid::new_v4(),
			(),
			Uuid::new_v4().into(),
			size,
			size.div_ceil(crate::consts::CHUNK_SIZE_U64),
			"de-1",
			"bucket",
			chrono::Utc::now(),
			false,
			FileMeta::Decoded(DecryptedFileMeta {
				name: Cow::Owned(name.to_owned()),
				size,
				mime: Cow::Borrowed("text/plain"),
				key: FileKey::V3(EncryptionKey::generate()),
				last_modified: chrono::Utc::now(),
				created: None,
				hash: None,
			}),
		);
		RemoteFileType::File(Cow::Owned(anonymous))
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
					compression: crate::fs::archive::compress::Compression {
						codec: crate::fs::archive::format::StreamCodec::Gzip,
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
			crate::fs::dir::meta::DecryptedDirectoryMeta {
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
}
