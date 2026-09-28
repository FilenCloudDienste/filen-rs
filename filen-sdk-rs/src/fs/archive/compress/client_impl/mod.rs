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
			ArchiveConfig,
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
	read_back::ReadBack,
	report::{CompressCallback, CompressFailed, CompressPhase, CompressReport, Reporter},
	single_is_one_file,
};

#[derive(Debug, Clone)]
pub struct CompressConfig {
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
	/// Compresses `sources` into a new archive called `name` in `destination`. Nothing can
	/// encode on the server (items are end-to-end encrypted), so every source is downloaded and
	/// encoded here, and the archive is encrypted and uploaded as one new file.
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
	/// a later one waits, reporting [`CompressPhase::WaitingForWorker`]. The destination is only
	/// listed once the job runs, so a missing or trashed one fails it then, after any such wait.
	/// It reports its progress to `callback` and can be paused, resumed and cancelled through
	/// `control`; a job paused before it starts waits in [`CompressPhase::Scanning`], having
	/// listed nothing, until it is resumed or cancelled.
	///
	/// [`ArchiveConfig::job_concurrency`]: crate::fs::archive::ArchiveConfig::job_concurrency
	pub async fn compress_items(
		self: Arc<Self>,
		sources: CompressSources,
		destination: DirType<'static, Normal>,
		name: ValidatedName,
		config: CompressConfig,
		callback: impl CompressCallback,
		control: JobControl,
	) -> Result<CompressReport, CompressFailed> {
		let reporter = Reporter::new(callback);
		let (format, max_bytes) = (config.format, config.max_bytes);
		let archives = self.archive_config().clone();
		let Planned {
			extension_len,
			report,
			disposal,
			job,
			sources,
			read_back,
		} = plan_compression(
			&*self,
			&archives,
			sources,
			destination.uuid(),
			&name,
			config,
			&reporter,
			&control,
		)
		.await?;
		run_compress(CompressTask {
			backend: Arc::new(ClientBackend::new(self)),
			control,
			reporter,
			destination,
			name,
			extension_len,
			sources,
			max_bytes,
			config: archives,
			head_last: matches!(format, CompressFormat::SevenZ { .. }),
			start: Box::new(move || worker::start(move |port| compress(&port, job))),
			report,
			disposal,
			read_back,
		})
		.await
	}
}

/// Lists a compression's source directories: the client, or a test's fake.
trait SourceLister {
	async fn list_source(
		&self,
		dir: ItemSourceDir,
		bytes: &ListingBytes,
	) -> Result<PlanSource<ItemSourceDir>, Error>;
}

impl SourceLister for Client {
	async fn list_source(
		&self,
		dir: ItemSourceDir,
		bytes: &ListingBytes,
	) -> Result<PlanSource<ItemSourceDir>, Error> {
		self.list_item_source(dir, bytes).await
	}
}

/// A compression planned, ready for its engine.
struct Planned {
	extension_len: usize,
	report: CompressReport,
	disposal: Option<CompressDisposal>,
	job: CompressJob,
	sources: Vec<Source>,
	read_back: Option<ReadBack>,
}

/// Checks the format and `name`, lists and plans the sources, and checks the archive can be
/// written and extracted again. A job paused before it starts waits here, in
/// [`CompressPhase::Scanning`], before listing anything.
#[expect(
	clippy::too_many_arguments,
	reason = "the request's parts, the archive settings, and the job's reporter and control"
)]
async fn plan_compression(
	lister: &impl SourceLister,
	archives: &ArchiveConfig,
	sources: CompressSources,
	destination: Uuid,
	name: &ValidatedName,
	config: CompressConfig,
	reporter: &MaybeArc<Reporter>,
	control: &JobControl,
) -> Result<Planned, CompressFailed> {
	// every source a disposal was asked for is reported, however early the job ends
	let requested: Vec<Uuid> = match &sources {
		CompressSources::Keep(_) => Vec::new(),
		CompressSources::Dispose { items, .. } => items.iter().map(|item| item.uuid()).collect(),
	};
	let refuse =
		|report, phase, error| end_early(reporter, report, &requested, phase, Arc::new(error));
	let checked = config
		.format
		.check_name(name.as_ref())
		.and_then(|extension_len| {
			config
				.format
				.check_within(config.password.is_some(), archives.codec_mem_budget)?;
			sources.check_for(config.format)?;
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
	let mut plan = match plan_sources(lister, sources, reporter, control).await {
		Ok(plan) => plan,
		Err(ScanError::Stopped) => {
			return Err(end_early(
				reporter,
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
	let how = dispose.as_ref().map(|(how, _)| *how);
	let disposal = match dispose.map(|(how, items)| disposal(&plan, how, items, destination)) {
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

	// a permanent disposal reads the archive back as extracting would
	let read_back = (how == Some(SourceDisposal::DeletePermanently))
		.then(|| ReadBack::as_extracting(&entries, archives, config.password.clone()));
	Ok(Planned {
		extension_len,
		report,
		disposal,
		job: CompressJob {
			format: config.format,
			entries,
			password: config.password,
		},
		sources,
		read_back,
	})
}

/// Lists every source directory and plans the archive's entries under a root of its own. A
/// pause is waited out before anything is listed, and between source directories.
async fn plan_sources(
	lister: &impl SourceLister,
	sources: Vec<ItemSource>,
	reporter: &MaybeArc<Reporter>,
	control: &JobControl,
) -> Result<ItemPlan<ItemSourceDir>, ScanError> {
	reporter.checkpoint(control).await?;
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
				let listing = lister.list_source(dir, &bytes);
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
	// the file checked up front may still have been skipped, undecryptable
	if matches!(format, CompressFormat::Single { .. })
		&& (plan.files.len() != 1 || !plan.dirs.is_empty())
	{
		return Err(single_is_one_file());
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
mod tests;
