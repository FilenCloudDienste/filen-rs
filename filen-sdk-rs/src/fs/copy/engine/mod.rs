//! Runs a [`CopyPlan`]: creates the directories parent first, then copies the files, each
//! through a chunk pipeline.
//!
//! # Memory and deadlock-freedom
//!
//! Every chunk takes one reservation from the client's memory semaphore before it is
//! downloaded and keeps it until its upload has finished: download, decrypt, hash, encrypt and
//! upload all run on that one reservation. A chunk holding a reservation never waits for
//! another reservation, only for the network, the CPU pool, or (for hashing, which must follow
//! chunk order) for an earlier chunk of the same file. A file reserves its chunks in order, so
//! that earlier chunk already holds a reservation of its own. Hence some holder can always make
//! progress, and the copy cannot deadlock however small the memory budget is.
//!
//! # Pause and cancel
//!
//! Both are cooperative ([`JobControl`]). Pausing stops new work: no new directory, file, or
//! chunk reservation starts, while in-flight chunks run to completion and release their
//! reservations. Once nothing is in flight the job reports itself paused, holding no memory
//! reservation and no drive lock. Resuming continues each file from its next chunk.
//!
//! Cancelling drops in-flight chunk transfers at once (a file only becomes visible when it is
//! finalized, so a dropped one leaves nothing behind) but lets in-flight directory creates and
//! file finalizations finish, so every item that was created is known and reported.

use std::{
	borrow::Cow,
	collections::{HashMap, VecDeque},
	future::Future,
	sync::Arc,
};

use chrono::{DateTime, Utc};
use filen_types::{api::v3::dir::color::DirColor, crypto::Blake3Hash, fs::Uuid};
use futures::{
	StreamExt,
	stream::{FuturesOrdered, FuturesUnordered},
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::{
	Error, ErrorKind,
	connect::ConnectedTargets,
	consts::{CALLBACK_INTERVAL, CHUNK_SIZE_U64, FILE_CHUNK_SIZE_EXTRA},
	fs::{
		HasName, HasUUID,
		categories::{DirType, NonRootItemType, Normal},
		dir::RemoteDirectory,
		file::{
			RemoteFile,
			enums::RemoteFileType,
			read::chunks_consistent_with_size,
			traits::{HasFileInfo, HasRemoteFileInfo},
			write::{RemoteFileInfo, UploadCompletion},
		},
		name::ValidatedName,
	},
	util::{MaybeArc, MaybeSend, MaybeSendBoxFuture, MaybeSendSync},
};

use super::{
	control::{JobControl, JobTasks, MAX_CONCURRENT_OPERATIONS, Stopped},
	naming::TakenNames,
	plan::{CopyPlan, DestParent, PlannedFile, PlannedItem, RenameReason, RenamedEntry},
	report::{
		ActiveFile, CopiedTopLevel, CopyEvent, CopyFailure, CopyPhase, CopyReport, CopyStage,
		FailedSource, FailureInfo, OpGuard, PlannedTopLevelItem, Reporter,
	},
};

/// Chunks of one file in flight at once. More only helps a single large file; with several
/// files running, the memory budget is the bound.
pub(crate) const CHUNKS_PER_FILE: usize = 4;

/// How many names a top-level item tries when the ones it picks turn out to be taken at the
/// destination (by an entry the listing could not name, or one created since the listing).
const TOP_LEVEL_NAME_ATTEMPTS: usize = 8;

/// The keep-both names a top-level item moves through when the destination turns out to hold
/// the one it picked.
struct NameRetry {
	taken: TakenNames,
	attempts: usize,
	is_dir: bool,
}

impl NameRetry {
	fn new(is_dir: bool) -> Self {
		Self {
			taken: TakenNames::default(),
			attempts: 0,
			is_dir,
		}
	}

	/// The next name after `taken_name`, or an error once [`TOP_LEVEL_NAME_ATTEMPTS`] names
	/// were tried.
	fn next(&mut self, taken_name: &ValidatedName) -> Result<ValidatedName, Error> {
		self.attempts += 1;
		if self.attempts >= TOP_LEVEL_NAME_ATTEMPTS {
			return Err(Error::custom(
				ErrorKind::InvalidState,
				"could not find a free name for the copy at the destination",
			));
		}
		self.taken.insert(taken_name.as_ref());
		Ok(self.taken.allocate(taken_name.as_ref(), self.is_dir)?)
	}

	/// `name`, or the first following keep-both name the server reports free in `parent`.
	async fn free_name<B: CopyBackend>(
		&mut self,
		backend: &B,
		parent: Uuid,
		mut name: ValidatedName,
	) -> Result<ValidatedName, Error> {
		while backend.name_exists(parent, &name).await? {
			name = self.next(&name)?;
		}
		Ok(name)
	}
}

/// Outcome of creating a directory.
#[derive(Debug)]
pub(crate) enum CreatedDir {
	Created(RemoteDirectory),
	/// The server already had a directory with that name there and returned it instead.
	Merged,
}

/// What a new file is created as.
#[derive(Debug, Clone)]
pub(crate) struct UploadSpec {
	pub(crate) uuid: Uuid,
	pub(crate) parent: Uuid,
	pub(crate) name: ValidatedName,
	pub(crate) mime: Option<String>,
}

/// The drive operations a copy needs. The production implementation is the client; tests use
/// a fake with the real memory semaphore.
pub(crate) trait CopyBackend: MaybeSendSync + 'static {
	type DriveLock: MaybeSendSync + 'static;
	type Upload: MaybeSendSync + 'static;

	/// The client's file-IO memory semaphore, in bytes.
	fn memory(&self) -> Arc<Semaphore>;
	fn lock_drive(&self) -> impl Future<Output = Result<Self::DriveLock, Error>> + MaybeSend;
	fn connected_targets(
		&self,
		dir: Uuid,
	) -> impl Future<Output = Result<ConnectedTargets, Error>> + MaybeSend;
	/// Creates `name` in `parent` under the given `uuid`. The caller holds the drive lock.
	fn create_dir(
		&self,
		parent: Uuid,
		uuid: Uuid,
		name: &ValidatedName,
		created: DateTime<Utc>,
	) -> impl Future<Output = Result<CreatedDir, Error>> + MaybeSend;
	fn set_dir_color(
		&self,
		dir: &mut RemoteDirectory,
		color: DirColor<'static>,
	) -> impl Future<Output = Result<(), Error>> + MaybeSend;
	/// Adds a new item to `targets`; returns the operations that failed.
	fn propagate(
		&self,
		targets: &ConnectedTargets,
		item: NonRootItemType<'_, Normal>,
	) -> impl Future<Output = Vec<Error>> + MaybeSend;
	/// Adds a created top-level item, and for a directory everything below it, to `targets`;
	/// returns the operations that failed.
	fn propagate_tree(
		&self,
		targets: &ConnectedTargets,
		item: &NonRootItemType<'static, Normal>,
	) -> impl Future<Output = Vec<Error>> + MaybeSend;
	fn begin_upload(&self, spec: UploadSpec) -> Self::Upload;
	/// Downloads and decrypts chunk `index` of `file`.
	fn fetch_chunk(
		&self,
		file: &RemoteFileType<'static>,
		index: u64,
	) -> impl Future<Output = Result<Vec<u8>, Error>> + MaybeSend;
	/// Encrypts and uploads the plaintext `data` as chunk `index` of `upload`.
	fn upload_chunk(
		&self,
		upload: &Self::Upload,
		index: u64,
		data: Vec<u8>,
	) -> impl Future<Output = Result<RemoteFileInfo, Error>> + MaybeSend;
	/// Whether a file or directory called `name` exists in `parent`, compared as the server
	/// compares names.
	fn name_exists(
		&self,
		parent: Uuid,
		name: &ValidatedName,
	) -> impl Future<Output = Result<bool, Error>> + MaybeSend;
	/// Registers the uploaded file under `name`. The caller holds the drive lock.
	fn finish_upload(
		&self,
		upload: &Self::Upload,
		name: &ValidatedName,
		completion: UploadCompletion,
		info: RemoteFileInfo,
	) -> impl Future<Output = Result<RemoteFile, Error>> + MaybeSend;
}

/// A copy's report plus how it ended.
#[derive(Debug)]
pub struct CopyOutcome<D> {
	pub report: CopyReport<D>,
	/// `Err` with [`ErrorKind::Cancelled`] when cancelled, or the error that ended the job.
	pub result: Result<(), Error>,
}

/// Errors after which nothing else can succeed either.
fn ends_job(error: &Error) -> bool {
	matches!(
		error.kind(),
		ErrorKind::MaxStorageReached | ErrorKind::Unauthenticated
	)
}

/// A copy of `error` for returning while the original stays in the failure records.
fn job_error(error: &Arc<Error>) -> Error {
	Error::custom_with_source(error.kind(), Arc::clone(error), None::<&str>)
}

/// A created directory with the name it got, or why it was not created.
type DirResult = Result<(RemoteDirectory, ValidatedName), DirError>;

enum DirError {
	/// Paused or stopped before anything was sent; the create is tried again after a pause.
	NotStarted,
	Failed(CopyStage, Error),
}

/// A downloaded chunk: its index, its plaintext, and the reservation it holds.
type FetchedChunk = (u64, Result<Vec<u8>, Error>, ChunkReservation);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DirState {
	Pending,
	Created(Uuid),
	Failed,
}

struct Job<B: CopyBackend, D> {
	backend: Arc<B>,
	control: JobControl,
	reporter: MaybeArc<Reporter>,
	plan: CopyPlan<D>,
	dir_states: Vec<DirState>,
	/// Each planned directory once created.
	created_dirs: Vec<Option<RemoteDirectory>>,
	/// The destinations the top-level items are created in, by uuid.
	destination_dirs: HashMap<Uuid, DirType<'static, Normal>>,
	/// Planned subdirectories of each planned directory.
	child_dirs: Vec<Vec<usize>>,
	/// Connected targets of each request's destination.
	targets: Vec<Arc<ConnectedTargets>>,
	/// The destination of each request, when it has a planned item.
	destinations: Vec<Option<Uuid>>,
	/// Top-level plan item of each request.
	top_level_of: Vec<Option<PlannedItem>>,
	report: CopyReport<D>,
	/// The error that ended the job early.
	fatal: Option<Arc<Error>>,
}

/// Runs `plan`, reporting to `reporter`. The plan's totals, skips and renames are reported
/// first, then the top-level items as planned. `destination_dirs` holds every destination of
/// the plan, by uuid.
pub(crate) async fn run_copy<B, D>(
	backend: Arc<B>,
	plan: CopyPlan<D>,
	destination_dirs: HashMap<Uuid, DirType<'static, Normal>>,
	control: JobControl,
	reporter: MaybeArc<Reporter>,
) -> CopyOutcome<D>
where
	B: CopyBackend,
	D: Clone + MaybeSendSync + 'static,
{
	let requests = plan
		.top_level
		.iter()
		.map(|t| t.request + 1)
		.max()
		.unwrap_or(0);
	let mut destinations = vec![None; requests];
	let mut top_level_of = vec![None; requests];
	for top in &plan.top_level {
		let parent = match top.item {
			PlannedItem::Dir(index) => plan.dirs[index].parent,
			PlannedItem::File(index) => plan.files[index].parent,
		};
		if let DestParent::Existing(destination) = parent {
			destinations[top.request] = Some(destination);
		}
		top_level_of[top.request] = Some(top.item);
	}
	let mut child_dirs = vec![Vec::new(); plan.dirs.len()];
	for (index, dir) in plan.dirs.iter().enumerate() {
		if let DestParent::Planned(parent) = dir.parent {
			child_dirs[parent].push(index);
		}
	}

	let report = CopyReport {
		skipped: plan.skipped.clone(),
		renamed: plan.renamed.clone(),
		totals: plan.totals,
		..CopyReport::default()
	};
	let mut job = Job {
		backend,
		control,
		reporter,
		dir_states: vec![DirState::Pending; plan.dirs.len()],
		created_dirs: vec![None; plan.dirs.len()],
		destination_dirs,
		child_dirs,
		targets: Vec::new(),
		destinations,
		top_level_of,
		plan,
		report,
		fatal: None,
	};
	let result = job.run().await;
	job.report.counts = job.reporter.counts();
	CopyOutcome {
		report: job.report,
		result,
	}
}

impl<B, D> Job<B, D>
where
	B: CopyBackend,
	D: Clone + MaybeSendSync + 'static,
{
	async fn run(&mut self) -> Result<(), Error> {
		self.reporter
			.set_plan(self.plan.totals, &self.plan.skipped, &self.plan.renamed);
		self.reporter.top_level_planned(self.planned_top_level());
		self.reporter.set_phase(CopyPhase::CreatingDirectories);

		let outcome = async {
			self.fetch_targets().await?;
			self.create_dirs().await?;
			self.reporter.set_phase(CopyPhase::CopyingFiles);
			self.copy_files().await?;
			self.reporter.set_phase(CopyPhase::Finishing);
			self.recheck_targets().await
		}
		.await;

		let (phase, result) = match (outcome, &self.fatal) {
			(_, Some(error)) => (CopyPhase::Failed, Err(job_error(error))),
			(Err(Stopped), None) if self.control.is_cancelled() => (
				CopyPhase::Cancelled,
				Err(Error::custom(ErrorKind::Cancelled, "copy cancelled")),
			),
			(Err(Stopped), None) => (
				CopyPhase::Failed,
				Err(Error::custom(ErrorKind::Internal, "copy stopped")),
			),
			(Ok(()), None) => (CopyPhase::Done, Ok(())),
		};
		self.reporter.finish(phase);
		result
	}

	fn planned_top_level(&self) -> Vec<PlannedTopLevelItem> {
		self.plan
			.top_level
			.iter()
			.filter_map(|top| {
				let destination = self.destinations[top.request]?;
				Some(match top.item {
					PlannedItem::Dir(index) => {
						let dir = &self.plan.dirs[index];
						PlannedTopLevelItem {
							request: top.request,
							source_uuid: dir.source_uuid,
							dest_uuid: dir.dest_uuid,
							dest_parent: destination,
							name: dir.name.as_ref().to_owned(),
							is_dir: true,
						}
					}
					PlannedItem::File(index) => {
						let file = &self.plan.files[index];
						PlannedTopLevelItem {
							request: top.request,
							source_uuid: file.source.uuid(),
							dest_uuid: file.dest_uuid,
							dest_parent: destination,
							name: file.name.as_ref().to_owned(),
							is_dir: false,
						}
					}
				})
			})
			.collect()
	}

	/// Records `error` as ending the job when it is that kind of error.
	fn note_error(&mut self, error: &Arc<Error>) {
		if self.fatal.is_none() && ends_job(error) {
			self.fatal = Some(Arc::clone(error));
			self.control.stop();
			self.reporter.set_cancelling();
		}
	}

	fn stop_error(&mut self, error: Error) -> Stopped {
		let error = Arc::new(error);
		if self.fatal.is_none() {
			self.fatal = Some(error);
		}
		self.control.stop();
		Stopped
	}

	/// Waits out a pause, reporting it; `Err` once the job is stopping.
	async fn checkpoint(&self) -> Result<(), Stopped> {
		self.reporter
			.set_pause_requested(self.control.is_pause_requested());
		let result = self.control.checkpoint().await;
		self.reporter
			.set_pause_requested(self.control.is_pause_requested());
		if result.is_err() {
			self.reporter.set_cancelling();
		}
		result
	}

	async fn fetch_targets(&mut self) -> Result<(), Stopped> {
		let mut by_destination: Vec<(Uuid, Arc<ConnectedTargets>)> = Vec::new();
		let mut targets = Vec::with_capacity(self.destinations.len());
		for destination in self.destinations.clone() {
			let Some(destination) = destination else {
				targets.push(Arc::new(ConnectedTargets::default()));
				continue;
			};
			if let Some((_, known)) = by_destination.iter().find(|(d, _)| *d == destination) {
				targets.push(Arc::clone(known));
				continue;
			}
			self.checkpoint().await?;
			let fetched = self
				.control
				.until_stopping(self.backend.connected_targets(destination))
				.await?;
			let fetched = match fetched {
				Ok(fetched) => Arc::new(fetched),
				Err(error) => return Err(self.stop_error(error)),
			};
			by_destination.push((destination, Arc::clone(&fetched)));
			targets.push(fetched);
		}
		self.targets = targets;
		Ok(())
	}

	/// The directory a failed item was to be created in, so it can be retried there. It always
	/// exists: the destinations are given, and nothing is attempted before its parent exists.
	fn failed_item_parent(&self, parent: DestParent) -> DirType<'static, Normal> {
		match parent {
			DestParent::Existing(uuid) => self
				.destination_dirs
				.get(&uuid)
				.expect("every destination is given")
				.clone(),
			DestParent::Planned(index) => DirType::Dir(Cow::Owned(
				self.created_dirs[index]
					.clone()
					.expect("an item is only attempted once its parent exists"),
			)),
		}
	}

	fn dest_parent(&self, parent: DestParent) -> Option<Uuid> {
		match parent {
			DestParent::Existing(uuid) => Some(uuid),
			DestParent::Planned(index) => match self.dir_states[index] {
				DirState::Created(uuid) => Some(uuid),
				DirState::Pending | DirState::Failed => None,
			},
		}
	}

	async fn create_dirs(&mut self) -> Result<(), Stopped> {
		let mut ready: VecDeque<usize> = self
			.plan
			.dirs
			.iter()
			.enumerate()
			.filter(|(_, dir)| matches!(dir.parent, DestParent::Existing(_)))
			.map(|(index, _)| index)
			.collect();
		if ready.is_empty() {
			return Ok(());
		}
		// Held while directories are being created, so the creates share one lock instead of
		// each acquiring and releasing it; dropped while paused.
		let mut keep_warm: Option<HeldLock<B::DriveLock>> = None;
		let mut in_flight = FuturesUnordered::new();

		loop {
			let pause_requested = self.control.is_pause_requested();
			self.reporter.set_pause_requested(pause_requested);
			let stopping = self.control.is_stopping();
			if stopping || pause_requested {
				if stopping {
					self.reporter.set_cancelling();
				}
				if in_flight.is_empty() {
					keep_warm = None;
					if stopping {
						return Err(Stopped);
					}
					self.checkpoint().await?;
					continue;
				}
			} else {
				if keep_warm.is_none() && !ready.is_empty() {
					match wait_for_lock(&*self.backend, &self.control, &self.reporter).await {
						Ok(LockWait::Locked(held)) => keep_warm = Some(held),
						// the loop reports the pause or the stop and waits it out
						Ok(LockWait::Paused) | Err(Stopped) => continue,
						Ok(LockWait::Failed(error)) => return Err(self.stop_error(error)),
					}
				}
				while in_flight.len() < MAX_CONCURRENT_OPERATIONS
					&& let Some(index) = ready.pop_front()
				{
					in_flight.push(self.create_dir_future(index));
				}
			}
			if in_flight.is_empty() {
				if ready.is_empty() {
					return Ok(());
				}
				continue;
			}

			tokio::select! {
				Some((index, result)) = in_flight.next() => {
					self.dir_finished(index, result, &mut ready);
				}
				() = self.control.pause_changed(pause_requested) => {},
				() = self.control.stopping(), if !stopping => {},
				() = crate::util::sleep(CALLBACK_INTERVAL) => self.reporter.tick(),
			}
		}
	}

	fn create_dir_future(&self, index: usize) -> MaybeSendBoxFuture<'static, (usize, DirResult)> {
		let dir = &self.plan.dirs[index];
		let parent = self
			.dest_parent(dir.parent)
			.expect("a directory is only created once its parent exists");
		let top_level = matches!(dir.parent, DestParent::Existing(_));
		let verify_name = top_level && self.plan.unverified_destinations.contains(&parent);
		let name = dir.name.clone();
		let uuid = dir.dest_uuid;
		let created = dir.created.unwrap_or_else(Utc::now);
		let color = dir.color.clone();
		let targets = Arc::clone(&self.targets[dir.request]);
		let backend = Arc::clone(&self.backend);
		let control = self.control.clone();
		let reporter = MaybeArc::clone(&self.reporter);
		Box::pin(async move {
			let result = create_dir(
				&*backend,
				&control,
				&reporter,
				&targets,
				parent,
				uuid,
				name,
				created,
				color,
				top_level,
				verify_name,
			)
			.await;
			(index, result)
		})
	}

	fn dir_finished(&mut self, index: usize, result: DirResult, ready: &mut VecDeque<usize>) {
		let parent = self.dest_parent(self.plan.dirs[index].parent);
		match result {
			Ok((dir, name)) => {
				let planned = &self.plan.dirs[index];
				if let DestParent::Existing(_) = planned.parent {
					let renamed = renamed_top_level(
						planned.source_uuid,
						&planned.source_path,
						&planned.name,
						name,
					);
					self.note_renamed(renamed);
				}
				let planned = &self.plan.dirs[index];
				self.dir_states[index] = DirState::Created(dir.uuid());
				self.created_dirs[index] = Some(dir.clone());
				self.reporter.dir_created(
					planned.source_uuid,
					dir.uuid(),
					parent.unwrap_or_default(),
					dir.name().unwrap_or(planned.name.as_ref()),
				);
				ready.extend(self.child_dirs[index].iter().copied());
				if let DestParent::Existing(_) = planned.parent {
					let item = CopiedTopLevel {
						request: planned.request,
						source_uuid: planned.source_uuid,
						item: NonRootItemType::Dir(Cow::Owned(dir)),
					};
					self.report.top_level.push(item.clone());
					self.reporter.top_level_created(item);
				}
			}
			// the loop waits out the pause or the stop before creating it again
			Err(DirError::NotStarted) => ready.push_front(index),
			Err(DirError::Failed(stage, error)) => {
				let error = Arc::new(error);
				self.note_error(&error);
				self.fail_subtree(index);
				let dest_parent_dir = self.failed_item_parent(self.plan.dirs[index].parent);
				let planned = &self.plan.dirs[index];
				let info = FailureInfo {
					source_uuid: planned.source_uuid,
					source_path: planned.source_path.clone(),
					dest_parent: parent.unwrap_or_default(),
					dest_parent_dir,
					dest_name: planned.name.as_ref().to_owned(),
					stage,
					error,
					affected_files: planned.descendant_files,
					affected_bytes: planned.descendant_bytes,
					existing_file: None,
				};
				self.reporter
					.dir_failed(info.clone(), planned.descendant_dirs);
				self.report.failures.push(CopyFailure {
					source: FailedSource::Dir(planned.handle.clone()),
					info,
				});
			}
		}
	}

	fn note_renamed(&mut self, entry: Option<RenamedEntry>) {
		let Some(entry) = entry else {
			return;
		};
		self.reporter.event(CopyEvent::Renamed {
			source_uuid: entry.source_uuid,
			source_path: entry.source_path.clone(),
			name: entry.name.as_ref().to_owned(),
			reason: entry.reason,
		});
		self.report.renamed.push(entry);
	}

	fn fail_subtree(&mut self, root: usize) {
		let mut stack = vec![root];
		while let Some(index) = stack.pop() {
			self.dir_states[index] = DirState::Failed;
			stack.extend(self.child_dirs[index].iter().copied());
		}
	}

	async fn copy_files(&mut self) -> Result<(), Stopped> {
		let memory = self.backend.memory();
		let mut tasks = JobTasks::<FileOutcome>::new();
		let mut next = 0;

		loop {
			let pause_requested = self.control.is_pause_requested();
			self.reporter.set_pause_requested(pause_requested);
			let stopping = self.control.is_stopping();
			if stopping {
				self.reporter.set_cancelling();
			}
			if !stopping && !pause_requested {
				while tasks.len() < MAX_CONCURRENT_OPERATIONS && next < self.plan.files.len() {
					let index = next;
					next += 1;
					// files below a failed directory were counted with it
					let Some(parent) = self.dest_parent(self.plan.files[index].parent) else {
						continue;
					};
					let request = self.plan.files[index].request;
					let top_level = matches!(
						self.top_level_of.get(request),
						Some(Some(PlannedItem::File(file))) if *file == index
					);
					tasks.spawn(copy_file(FileTask {
						backend: Arc::clone(&self.backend),
						control: self.control.clone(),
						reporter: MaybeArc::clone(&self.reporter),
						memory: Arc::clone(&memory),
						targets: Arc::clone(&self.targets[self.plan.files[index].request]),
						file: self.plan.files[index].clone(),
						parent,
						index,
						top_level,
						verify_name: top_level
							&& self.plan.unverified_destinations.contains(&parent),
					}));
				}
			}
			if tasks.is_empty() {
				if stopping {
					return Err(Stopped);
				}
				if next >= self.plan.files.len() {
					return Ok(());
				}
				// paused with nothing in flight
				self.checkpoint().await?;
				continue;
			}

			tokio::select! {
				Some(outcome) = tasks.next() => self.file_finished(outcome),
				() = self.control.pause_changed(pause_requested) => {},
				() = self.control.stopping(), if !stopping => {},
				() = crate::util::sleep(CALLBACK_INTERVAL) => self.reporter.tick(),
			}
		}
	}

	fn file_finished(&mut self, outcome: FileOutcome) {
		let planned = &self.plan.files[outcome.index];
		let request = planned.request;
		let active = ActiveFile {
			source_uuid: planned.source.uuid(),
			dest_uuid: planned.dest_uuid,
			dest_parent: outcome.parent,
			name: planned.name.as_ref().to_owned(),
			size: planned.size,
			bytes_done: 0,
		};
		let top_level = matches!(
			self.top_level_of.get(request),
			Some(Some(PlannedItem::File(index))) if *index == outcome.index
		);
		match outcome.result {
			Ok((file, name)) => {
				if top_level {
					let planned = &self.plan.files[outcome.index];
					let renamed = renamed_top_level(
						active.source_uuid,
						&planned.source_path,
						&planned.name,
						name,
					);
					self.note_renamed(renamed);
				}
				let active = ActiveFile {
					name: file.name().map_or(active.name, str::to_owned),
					..active
				};
				self.reporter.file_done(&active);
				if top_level {
					let item = CopiedTopLevel {
						request,
						source_uuid: active.source_uuid,
						item: NonRootItemType::File(Cow::Owned(file)),
					};
					self.report.top_level.push(item.clone());
					self.reporter.top_level_created(item);
				}
			}
			Err(FileError::Stopped) => self.reporter.file_abandoned(active.dest_uuid),
			// Not a copy the user can keep or trash: trashing it would trash the existing file.
			// It counts as failed, so the counts still add up to the totals.
			Err(FileError::RegisteredAsVersion(file)) => {
				let dest_parent_dir =
					self.failed_item_parent(self.plan.files[outcome.index].parent);
				let planned = &self.plan.files[outcome.index];
				let info = FailureInfo {
					source_uuid: active.source_uuid,
					source_path: planned.source_path.clone(),
					dest_parent: outcome.parent,
					dest_parent_dir,
					dest_name: file.name().map_or(active.name, str::to_owned),
					stage: CopyStage::RegisteredAsVersion,
					error: Arc::new(Error::custom(
						ErrorKind::InvalidState,
						"the copy was registered as a new version of an existing file",
					)),
					affected_files: 1,
					affected_bytes: planned.size,
					existing_file: Some(file.stable_uuid.into()),
				};
				self.reporter.file_failed(active.dest_uuid, info.clone());
				self.report.failures.push(CopyFailure {
					source: FailedSource::File(Box::new(planned.source.clone())),
					info,
				});
			}
			Err(FileError::Failed(stage, error)) => {
				let error = Arc::new(error);
				self.note_error(&error);
				let dest_parent_dir =
					self.failed_item_parent(self.plan.files[outcome.index].parent);
				let planned = &self.plan.files[outcome.index];
				let info = FailureInfo {
					source_uuid: active.source_uuid,
					source_path: planned.source_path.clone(),
					dest_parent: outcome.parent,
					dest_parent_dir,
					dest_name: active.name.clone(),
					stage,
					error,
					affected_files: 1,
					affected_bytes: planned.size,
					existing_file: None,
				};
				self.reporter.file_failed(active.dest_uuid, info.clone());
				self.report.failures.push(CopyFailure {
					source: FailedSource::File(Box::new(planned.source.clone())),
					info,
				});
			}
		}
	}

	/// A destination may have been shared or linked while the copy ran; items created before
	/// that were propagated to the old targets only. Propagate everything created (each
	/// top-level item with its subtree) to the new ones.
	async fn recheck_targets(&mut self) -> Result<(), Stopped> {
		let mut checked: Vec<Uuid> = Vec::new();
		for (request, destination) in self.destinations.clone().into_iter().enumerate() {
			let Some(destination) = destination else {
				continue;
			};
			if checked.contains(&destination) {
				continue;
			}
			checked.push(destination);
			self.checkpoint().await?;
			let current = match self
				.control
				.until_stopping(self.backend.connected_targets(destination))
				.await?
			{
				Ok(current) => current,
				Err(error) => {
					// the copy itself succeeded; only report
					tracing::warn!("failed to re-check the copy destination's shares: {error}");
					continue;
				}
			};
			let added = current.without(&self.targets[request]);
			if added.is_empty() {
				continue;
			}
			let items = self.created_items_in(destination);
			let lock = loop {
				match wait_for_lock(&*self.backend, &self.control, &self.reporter).await? {
					LockWait::Locked(held) => break Some(held),
					LockWait::Paused => self.checkpoint().await?,
					LockWait::Failed(error) => {
						tracing::warn!(
							"failed to lock the drive to propagate copied items: {error}"
						);
						break None;
					}
				}
			};
			let Some(_lock) = lock else {
				continue;
			};
			for item in &items {
				for error in self.backend.propagate_tree(&added, item).await {
					self.reporter.event(CopyEvent::PropagationFailed {
						dest_uuid: item.uuid(),
						error: Arc::new(error),
					});
				}
			}
		}
		Ok(())
	}

	/// The top-level items this copy created in `destination`.
	fn created_items_in(&self, destination: Uuid) -> Vec<NonRootItemType<'static, Normal>> {
		let mut items = Vec::new();
		for top in &self.report.top_level {
			if self.destinations[top.request] == Some(destination) {
				items.push(top.item.clone());
			}
		}
		items
	}
}

/// A top-level name can be taken after the destination was listed; the item then gets the next
/// keep-both name, which differs from the announced one and is reported as a rename.
fn renamed_top_level(
	source_uuid: Uuid,
	source_path: &str,
	planned: &ValidatedName,
	name: ValidatedName,
) -> Option<RenamedEntry> {
	(name.as_ref() != planned.as_ref()).then(|| RenamedEntry {
		source_uuid,
		source_path: source_path.to_owned(),
		name,
		reason: RenameReason::DuplicateName,
	})
}

#[allow(clippy::too_many_arguments)]
async fn create_dir<B: CopyBackend>(
	backend: &B,
	control: &JobControl,
	reporter: &MaybeArc<Reporter>,
	targets: &ConnectedTargets,
	parent: Uuid,
	uuid: Uuid,
	name: ValidatedName,
	created: DateTime<Utc>,
	color: DirColor<'static>,
	top_level: bool,
	verify_name: bool,
) -> DirResult {
	let stage = CopyStage::CreateDirectory;
	// The shared lock the job holds is normally handed out at once; a fresh acquisition (its
	// lease was lost) can wait long. Nothing is sent before the lock is held, so a pause or stop
	// until then leaves the create to be tried again; once held, the create runs to the end.
	let _lock = match wait_for_lock(backend, control, reporter).await {
		Ok(LockWait::Locked(held)) => held,
		Ok(LockWait::Paused) | Err(Stopped) => return Err(DirError::NotStarted),
		Ok(LockWait::Failed(error)) => return Err(DirError::Failed(stage, error)),
	};
	let mut retry = NameRetry::new(true);
	let mut name = name;
	let mut dir = loop {
		if verify_name {
			name = retry
				.free_name(backend, parent, name)
				.await
				.map_err(|e| DirError::Failed(stage, e))?;
		}
		match backend
			.create_dir(parent, uuid, &name, created)
			.await
			.map_err(|e| DirError::Failed(stage, e))?
		{
			CreatedDir::Created(created) => break created,
			// Someone created the same name at the destination after it was listed: keep both
			// by taking the next free name.
			CreatedDir::Merged if top_level => {
				name = retry.next(&name).map_err(|e| DirError::Failed(stage, e))?
			}
			CreatedDir::Merged => {
				return Err(DirError::Failed(
					stage,
					Error::custom(
						ErrorKind::InvalidState,
						"a directory with this name already exists in the new directory",
					),
				));
			}
		}
	};
	if color != DirColor::Default
		&& let Err(error) = backend.set_dir_color(&mut dir, color).await
	{
		reporter.event(CopyEvent::ColorFailed {
			dest_uuid: dir.uuid(),
			error: Arc::new(error),
		});
	}
	if !targets.is_empty() {
		for error in backend
			.propagate(targets, NonRootItemType::Dir(Cow::Borrowed(&dir)))
			.await
		{
			reporter.event(CopyEvent::PropagationFailed {
				dest_uuid: dir.uuid(),
				error: Arc::new(error),
			});
		}
	}
	Ok((dir, name))
}

struct FileTask<B> {
	backend: Arc<B>,
	control: JobControl,
	reporter: MaybeArc<Reporter>,
	memory: Arc<Semaphore>,
	targets: Arc<ConnectedTargets>,
	file: PlannedFile,
	parent: Uuid,
	index: usize,
	top_level: bool,
	/// Check the top-level name with the server before uploading.
	verify_name: bool,
}

enum FileError {
	Stopped,
	Failed(CopyStage, Error),
	/// Registered as a new version of the existing file it holds instead of as a new file.
	RegisteredAsVersion(Box<RemoteFile>),
}

impl From<Stopped> for FileError {
	fn from(_: Stopped) -> Self {
		Self::Stopped
	}
}

struct FileOutcome {
	index: usize,
	parent: Uuid,
	/// The registered file and the name it got.
	result: Result<(RemoteFile, ValidatedName), FileError>,
}

/// Memory reservation of one chunk, and its place among the job's in-flight operations.
struct ChunkReservation {
	_permit: OwnedSemaphorePermit,
	_op: OpGuard,
}

/// The drive lock, held as one of the job's in-flight operations. The lock is dropped before
/// the operation ends, so the job is only reported paused once the lock is gone.
struct HeldLock<L> {
	_lock: L,
	_op: OpGuard,
}

enum LockWait<L> {
	Locked(HeldLock<L>),
	/// A pause was requested while waiting; nothing is held.
	Paused,
	Failed(Error),
}

/// Waits for the drive lock, which another client may hold for a long time. A stop ends the
/// wait (`Err`), and a pause requested meanwhile ends it or drops the lock just acquired, so a
/// paused job holds no lock. After [`LockWait::Paused`] the caller waits out the pause before
/// trying again.
async fn wait_for_lock<B: CopyBackend>(
	backend: &B,
	control: &JobControl,
	reporter: &MaybeArc<Reporter>,
) -> Result<LockWait<B::DriveLock>, Stopped> {
	let op = reporter.op();
	let result = tokio::select! {
		biased;
		() = control.stopping() => return Err(Stopped),
		() = control.pause_changed(false) => return Ok(LockWait::Paused),
		result = backend.lock_drive() => result,
	};
	Ok(match result {
		Ok(_) if control.is_pause_requested() => LockWait::Paused,
		Ok(lock) => LockWait::Locked(HeldLock {
			_lock: lock,
			_op: op,
		}),
		Err(error) => LockWait::Failed(error),
	})
}

/// Plaintext length of chunk `index` of a `size`-byte file.
fn chunk_len(size: u64, index: u64) -> u64 {
	size.saturating_sub(index * CHUNK_SIZE_U64)
		.min(CHUNK_SIZE_U64)
}

/// Waits for the job to be running and for memory for chunk `index`. A pause that starts while
/// waiting hands the memory back until the job resumes, so a paused job holds none.
async fn reserve_chunk(
	control: JobControl,
	reporter: MaybeArc<Reporter>,
	memory: Arc<Semaphore>,
	size: u64,
	index: u64,
) -> Result<ChunkReservation, Stopped> {
	let bytes = u32::try_from(chunk_len(size, index) + u64::from(FILE_CHUNK_SIZE_EXTRA.get()))
		.expect("a chunk fits in u32");
	loop {
		control.checkpoint().await?;
		// In flight before anything is held, so the job is never reported paused holding memory.
		let op = reporter.op();
		let permit = tokio::select! {
			biased;
			() = control.stopping() => return Err(Stopped),
			// a waiting acquisition is handed free memory as it comes; dropping it gives that back
			() = control.pause_changed(false) => continue,
			permit = Arc::clone(&memory).acquire_many_owned(bytes) => {
				permit.expect("the memory semaphore is never closed")
			}
		};
		if !control.is_pause_requested() {
			return Ok(ChunkReservation {
				_permit: permit,
				_op: op,
			});
		}
	}
}

async fn copy_file<B: CopyBackend>(task: FileTask<B>) -> FileOutcome {
	let index = task.index;
	let parent = task.parent;
	let result = copy_file_inner(task).await;
	FileOutcome {
		index,
		parent,
		result,
	}
}

async fn copy_file_inner<B: CopyBackend>(
	task: FileTask<B>,
) -> Result<(RemoteFile, ValidatedName), FileError> {
	let FileTask {
		backend,
		control,
		reporter,
		memory,
		targets,
		file,
		parent,
		top_level,
		verify_name,
		..
	} = task;
	control.checkpoint().await?;
	let source = Arc::new(file.source);
	let size = file.size;
	let stored_chunks = source.chunks();
	if !chunks_consistent_with_size(stored_chunks, size) {
		return Err(FileError::Failed(
			CopyStage::Download,
			Error::custom(
				ErrorKind::Response,
				format!(
					"file chunk count ({stored_chunks}) is inconsistent with its size ({size})"
				),
			),
		));
	}
	// A stored count may include a chunk without data (one for an empty file, or a trailing
	// empty chunk); only the chunks holding data are copied.
	let chunks = size.div_ceil(CHUNK_SIZE_U64);
	let mut retry = NameRetry::new(false);
	let mut name = file.name.clone();
	if verify_name {
		name = retry
			.free_name(&*backend, parent, name)
			.await
			.map_err(|e| FileError::Failed(CopyStage::Upload, e))?;
	}
	reporter.file_started(ActiveFile {
		source_uuid: source.uuid(),
		dest_uuid: file.dest_uuid,
		dest_parent: parent,
		name: name.as_ref().to_owned(),
		size,
		bytes_done: 0,
	});
	let upload = Arc::new(backend.begin_upload(UploadSpec {
		uuid: file.dest_uuid,
		parent,
		name: name.clone(),
		mime: source.mime().map(str::to_owned),
	}));

	let mut hasher = blake3::Hasher::new();
	let mut written = 0u64;
	let mut info: Option<RemoteFileInfo> = None;
	let mut next = 0u64;
	let mut reserving: Option<MaybeSendBoxFuture<'static, Result<ChunkReservation, Stopped>>> =
		None;
	let mut fetches: FuturesOrdered<MaybeSendBoxFuture<'static, FetchedChunk>> =
		FuturesOrdered::new();
	let mut uploads: FuturesUnordered<
		MaybeSendBoxFuture<'static, Result<(RemoteFileInfo, u64), Error>>,
	> = FuturesUnordered::new();
	loop {
		if reserving.is_none() && next < chunks && fetches.len() + uploads.len() < CHUNKS_PER_FILE {
			reserving = Some(Box::pin(reserve_chunk(
				control.clone(),
				MaybeArc::clone(&reporter),
				Arc::clone(&memory),
				size,
				next,
			)));
		}
		if reserving.is_none() && fetches.is_empty() && uploads.is_empty() {
			break;
		}
		tokio::select! {
			biased;
			// dropping the in-flight transfers releases their reservations
			() = control.stopping() => return Err(FileError::Stopped),
			Some(result) = uploads.next() => match result {
				Ok((chunk_info, len)) => {
					reporter.chunk_uploaded(file.dest_uuid, len);
					info = Some(chunk_info);
				}
				Err(error) => return Err(FileError::Failed(CopyStage::Upload, error)),
			},
			Some((chunk, result, reservation)) = fetches.next() => match result {
				Ok(data) => {
					let data: Vec<u8> = data;
					hasher.update(&data);
					written += data.len() as u64;
					let backend = Arc::clone(&backend);
					let upload = Arc::clone(&upload);
					uploads.push(Box::pin(async move {
						let len = data.len() as u64;
						let result = backend.upload_chunk(&upload, chunk, data).await;
						drop(reservation);
						result.map(|info| (info, len))
					}) as MaybeSendBoxFuture<'static, _>);
				}
				Err(error) => return Err(FileError::Failed(CopyStage::Download, error)),
			},
			reservation = async { reserving.as_mut().expect("guarded").await }, if reserving.is_some() => {
				reserving = None;
				let reservation = reservation?;
				let chunk = next;
				next += 1;
				let backend = Arc::clone(&backend);
				let source = Arc::clone(&source);
				fetches.push_back(Box::pin(async move {
					let result = backend.fetch_chunk(&source, chunk).await;
					(chunk, result, reservation)
				}) as MaybeSendBoxFuture<'static, _>);
			}
		}
	}
	drop(reserving);

	if written != size {
		return Err(FileError::Failed(
			CopyStage::Download,
			Error::custom(
				ErrorKind::Response,
				format!("read {written} bytes of a {size}-byte file"),
			),
		));
	}
	let hash = Blake3Hash::from(hasher.finalize());
	if let Some(expected) = source.hash()
		&& expected != hash
	{
		tracing::warn!(
			"copied file {} does not match the hash in its metadata",
			source.uuid()
		);
	}
	let modified = source.last_modified().unwrap_or_else(Utc::now);
	let completion = UploadCompletion {
		written,
		num_chunks: chunks,
		hash,
		final_times: (source.created().unwrap_or(modified), modified),
	};

	// Registering the file is not started while paused, but once started it runs to the end
	// even on cancel, so a file that exists is always reported.
	let _lock = loop {
		control.checkpoint().await?;
		match wait_for_lock(&*backend, &control, &reporter).await? {
			LockWait::Locked(held) => break held,
			LockWait::Paused => {}
			LockWait::Failed(error) => return Err(FileError::Failed(CopyStage::Finalize, error)),
		}
	};
	if top_level {
		// Registering a file under a name the parent already holds would make the copy a new
		// version of that file instead of a new file, so the name is checked again here, while
		// holding the drive lock: clients that write under the lock cannot take it in between.
		name = retry
			.free_name(&*backend, parent, name)
			.await
			.map_err(|e| FileError::Failed(CopyStage::Finalize, e))?;
	}
	let remote = backend
		.finish_upload(&upload, &name, completion, info.unwrap_or_default())
		.await
		.map_err(|e| FileError::Failed(CopyStage::Finalize, e))?;
	let registered_as_version = remote.stable_uuid != remote.uuid;
	if registered_as_version {
		tracing::error!(
			"copied file {} was registered as a new version of the existing file {}",
			remote.uuid,
			Uuid::from(remote.stable_uuid)
		);
	}
	if !targets.is_empty() {
		for error in backend
			.propagate(&targets, NonRootItemType::File(Cow::Borrowed(&remote)))
			.await
		{
			reporter.event(CopyEvent::PropagationFailed {
				dest_uuid: remote.uuid(),
				error: Arc::new(error),
			});
		}
	}
	if registered_as_version {
		return Err(FileError::RegisteredAsVersion(Box::new(remote)));
	}
	Ok((remote, name))
}

#[cfg(test)]
mod tests;
