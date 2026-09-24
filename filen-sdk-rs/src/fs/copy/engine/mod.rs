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
	iter, mem,
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
	consts::{
		CALLBACK_INTERVAL, CHUNK_SIZE_U64, FILE_CHUNK_SIZE_EXTRA, MAX_SMALL_PARALLEL_REQUESTS,
	},
	fs::{
		HasName, HasUUID,
		categories::{DirType, NonRootItemType, Normal},
		dir::RemoteDirectory,
		file::{
			RemoteFile,
			enums::RemoteFileType,
			read::{check_chunks_consistent, chunk_plaintext_len},
			traits::{HasFileInfo, HasRemoteFileInfo},
			write::{RemoteFileInfo, UploadCompletion},
		},
		name::ValidatedName,
	},
	job::{JobControl, JobTasks, Stopped},
	util::{MaybeArc, MaybeSend, MaybeSendBoxFuture, MaybeSendSync, sleep},
};

use super::{
	naming::TakenNames,
	plan::{CopyPlan, DestParent, PlannedFile, PlannedItem, RenameReason, RenamedEntry},
	report::{
		ActiveFile, CopiedTopLevel, CopyEvent, CopyFailed, CopyFailure, CopyPhase, CopyReport,
		CopyStage, FailedSource, FailureInfo, OpGuard, PlannedTopLevelItem, Reporter,
	},
};

/// Chunks of one file in flight at once. More only helps a single large file; with several
/// files running, the memory budget is the bound.
const CHUNKS_PER_FILE: usize = 4;

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
	fn next(&mut self, taken_name: ValidatedName) -> Result<ValidatedName, Error> {
		self.attempts += 1;
		if self.attempts >= TOP_LEVEL_NAME_ATTEMPTS {
			return Err(Error::custom(
				ErrorKind::InvalidState,
				"could not find a free name for the copy at the destination",
			));
		}
		self.taken.insert(taken_name.as_ref());
		Ok(self.taken.allocate(taken_name, self.is_dir)?)
	}

	/// `name`, or the first following keep-both name the server reports free in `parent`.
	async fn free_name<B: CopyBackend>(
		&mut self,
		backend: &B,
		parent: Uuid,
		mut name: ValidatedName,
	) -> Result<ValidatedName, Error> {
		while backend.name_exists(parent, &name).await? {
			name = self.next(name)?;
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
	/// Acquires the drive lock, waiting while another client holds it.
	fn acquire_drive_lock(
		&self,
	) -> impl Future<Output = Result<Self::DriveLock, Error>> + MaybeSend;
	fn connected_targets(
		&self,
		dir: Uuid,
	) -> impl Future<Output = Result<ConnectedTargets, Error>> + MaybeSend;
	/// Creates `name` in `parent` under the given `uuid`. The caller holds the drive lock.
	/// Unlike [`Client::create_dir`](crate::auth::Client::create_dir) it does not propagate
	/// the directory, and reports a merge into an existing one instead of returning it.
	fn create_copy_dir(
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

/// Errors after which nothing else can succeed either.
fn ends_job(error: &Error) -> bool {
	matches!(
		error.kind(),
		ErrorKind::MaxStorageReached | ErrorKind::Unauthenticated
	)
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

#[derive(Debug, Clone)]
enum DirState {
	Pending,
	Created(RemoteDirectory),
	Failed,
}

impl DirState {
	fn created(&self) -> Option<&RemoteDirectory> {
		match self {
			Self::Created(dir) => Some(dir),
			Self::Pending | Self::Failed => None,
		}
	}
}

/// Where a request's top-level item goes.
#[derive(Default)]
struct RequestState {
	/// The existing directory the top-level item is created in; `None` when nothing of the
	/// request was planned.
	destination: Option<Uuid>,
	/// Shares and links of the destination, which every item of the request is added to.
	targets: Arc<ConnectedTargets>,
}

struct Job<B: CopyBackend, D> {
	backend: Arc<B>,
	control: JobControl,
	reporter: MaybeArc<Reporter>,
	plan: CopyPlan<D>,
	/// Where each planned directory stands.
	dir_states: Vec<DirState>,
	/// The destinations the top-level items are created in, by uuid.
	destination_dirs: HashMap<Uuid, DirType<'static, Normal>>,
	/// Planned subdirectories of each planned directory.
	child_dirs: Vec<Vec<usize>>,
	/// Each request's destination, filled in by [`Job::fetch_targets`].
	requests: Vec<RequestState>,
	report: CopyReport<D>,
	/// The error that ended the job early, shared with the failure it came from.
	fatal: Option<Arc<Error>>,
}

/// Runs `plan`, reporting to `reporter`. The plan's totals, skips and renames are reported
/// first, then the top-level items as planned. `destination_dirs` holds every destination of
/// the plan, by uuid.
pub(crate) async fn run_copy<B, D>(
	backend: Arc<B>,
	mut plan: CopyPlan<D>,
	destination_dirs: HashMap<Uuid, DirType<'static, Normal>>,
	control: JobControl,
	reporter: MaybeArc<Reporter>,
) -> Result<CopyReport<D>, CopyFailed<D>>
where
	B: CopyBackend,
	D: Clone + MaybeSendSync + 'static,
{
	let mut child_dirs = vec![Vec::new(); plan.dirs.len()];
	for (index, dir) in plan.dirs.iter().enumerate() {
		if let DestParent::Planned(parent) = dir.parent {
			child_dirs[parent].push(index);
		}
	}

	let report = CopyReport {
		skipped: mem::take(&mut plan.skipped),
		renamed: mem::take(&mut plan.renamed),
		totals: plan.totals,
		..CopyReport::default()
	};
	let mut job = Job {
		backend,
		control,
		reporter,
		dir_states: vec![DirState::Pending; plan.dirs.len()],
		destination_dirs,
		child_dirs,
		requests: Vec::new(),
		plan,
		report,
		fatal: None,
	};
	let result = job.run().await;
	job.report.counts = job.reporter.counts();
	match result {
		Ok(()) => Ok(job.report),
		Err(error) => Err(CopyFailed {
			report: job.report,
			error,
		}),
	}
}

impl<B, D> Job<B, D>
where
	B: CopyBackend,
	D: Clone + MaybeSendSync + 'static,
{
	/// `Err` with [`ErrorKind::Cancelled`] when cancelled, or the error that ended the job.
	async fn run(&mut self) -> Result<(), Arc<Error>> {
		self.reporter
			.set_plan(self.plan.totals, &self.report.skipped, &self.report.renamed);
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
			(_, Some(error)) => (CopyPhase::Failed, Err(Arc::clone(error))),
			(Err(Stopped), None) if self.control.is_cancelled() => (
				CopyPhase::Cancelled,
				Err(Arc::new(Error::custom(
					ErrorKind::Cancelled,
					"copy cancelled",
				))),
			),
			(Err(Stopped), None) => (
				CopyPhase::Failed,
				Err(Arc::new(Error::custom(ErrorKind::Internal, "copy stopped"))),
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
			.map(|top| {
				let destination = self.top_level_destination(top.item);
				match top.item {
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
				}
			})
			.collect()
	}

	/// The existing directory the top-level `item` is created in.
	fn top_level_destination(&self, item: PlannedItem) -> Uuid {
		let parent = match item {
			PlannedItem::Dir(index) => self.plan.dirs[index].parent,
			PlannedItem::File(index) => self.plan.files[index].parent,
		};
		let DestParent::Existing(destination) = parent else {
			panic!("a top-level item is created in an existing directory");
		};
		destination
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

	/// Fills in [`Job::requests`], fetching each destination's targets once.
	async fn fetch_targets(&mut self) -> Result<(), Stopped> {
		let count = self
			.plan
			.top_level
			.iter()
			.map(|top| top.request + 1)
			.max()
			.unwrap_or(0);
		let mut requests: Vec<RequestState> = iter::repeat_with(RequestState::default)
			.take(count)
			.collect();
		// in request order, one top-level item per request
		for top in &self.plan.top_level {
			let destination = self.top_level_destination(top.item);
			let known = requests
				.iter()
				.find(|request| request.destination == Some(destination));
			let targets = if let Some(known) = known {
				Arc::clone(&known.targets)
			} else {
				self.reporter.checkpoint(&self.control).await?;
				let fetched = self
					.control
					.until_stopping(self.backend.connected_targets(destination))
					.await?;
				match fetched {
					Ok(fetched) => Arc::new(fetched),
					Err(error) => return Err(self.stop_error(error)),
				}
			};
			requests[top.request] = RequestState {
				destination: Some(destination),
				targets,
			};
		}
		self.requests = requests;
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
				self.dir_states[index]
					.created()
					.expect("an item is only attempted once its parent exists")
					.clone(),
			)),
		}
	}

	fn dest_parent(&self, parent: DestParent) -> Option<Uuid> {
		match parent {
			DestParent::Existing(uuid) => Some(uuid),
			DestParent::Planned(index) => self.dir_states[index].created().map(HasUUID::uuid),
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
					self.reporter.checkpoint(&self.control).await?;
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
				while in_flight.len() < MAX_SMALL_PARALLEL_REQUESTS
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
				() = sleep(CALLBACK_INTERVAL) => self.reporter.tick(),
			}
		}
	}

	fn create_dir_future(&self, index: usize) -> MaybeSendBoxFuture<'static, (usize, DirResult)> {
		let dir = &self.plan.dirs[index];
		let parent = self
			.dest_parent(dir.parent)
			.expect("a directory is only created once its parent exists");
		let top_level = matches!(dir.parent, DestParent::Existing(_));
		let task = DirTask {
			backend: Arc::clone(&self.backend),
			control: self.control.clone(),
			reporter: MaybeArc::clone(&self.reporter),
			targets: Arc::clone(&self.requests[dir.request].targets),
			parent,
			uuid: dir.dest_uuid,
			// the plan keeps the planned name to tell a later rename from it
			name: dir.name.clone(),
			created: dir.created.unwrap_or_else(Utc::now),
			color: dir.color.clone(),
			top_level,
			verify_name: top_level && self.plan.unverified_destinations.contains(&parent),
		};
		Box::pin(async move { (index, create_dir(task).await) })
	}

	fn dir_finished(&mut self, index: usize, result: DirResult, ready: &mut VecDeque<usize>) {
		let parent = self
			.dest_parent(self.plan.dirs[index].parent)
			.expect("a directory is only created once its parent exists");
		let top_level = matches!(self.plan.dirs[index].parent, DestParent::Existing(_));
		match result {
			Ok((dir, name)) => {
				if top_level {
					let planned = &self.plan.dirs[index];
					let renamed = renamed_top_level(
						planned.source_uuid,
						&planned.source_path,
						&planned.name,
						name,
					);
					self.note_renamed(renamed);
				}
				let planned = &self.plan.dirs[index];
				self.reporter.dir_created(
					planned.source_uuid,
					dir.uuid(),
					parent,
					dir.name().unwrap_or(planned.name.as_ref()),
				);
				ready.extend(self.child_dirs[index].iter().copied());
				if top_level {
					let item = CopiedTopLevel {
						request: planned.request,
						source_uuid: planned.source_uuid,
						// the job keeps its own copy to create the subtree in
						item: NonRootItemType::Dir(Cow::Owned(dir.clone())),
					};
					self.report.top_level.push(item.clone());
					self.reporter.top_level_created(item);
				}
				self.dir_states[index] = DirState::Created(dir);
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
					dest_parent_dir,
					dest_name: planned.name.as_ref().to_owned(),
					stage,
					error,
					affected_files: planned.descendant_files,
					affected_bytes: planned.descendant_bytes,
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
		// the report keeps the entry too
		self.reporter.event(CopyEvent::Renamed(entry.clone()));
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
				while tasks.len() < MAX_SMALL_PARALLEL_REQUESTS && next < self.plan.files.len() {
					let index = next;
					next += 1;
					let file = &self.plan.files[index];
					// files below a failed directory were counted with it
					let Some(parent) = self.dest_parent(file.parent) else {
						continue;
					};
					let top_level = matches!(file.parent, DestParent::Existing(_));
					tasks.spawn(copy_file(FileTask {
						backend: Arc::clone(&self.backend),
						control: self.control.clone(),
						reporter: MaybeArc::clone(&self.reporter),
						memory: Arc::clone(&memory),
						targets: Arc::clone(&self.requests[file.request].targets),
						file: file.clone(),
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
				self.reporter.checkpoint(&self.control).await?;
				continue;
			}

			tokio::select! {
				Some(outcome) = tasks.next() => self.file_finished(outcome),
				() = self.control.pause_changed(pause_requested) => {},
				() = self.control.stopping(), if !stopping => {},
				() = sleep(CALLBACK_INTERVAL) => self.reporter.tick(),
			}
		}
	}

	fn file_finished(&mut self, outcome: FileOutcome) {
		let FileOutcome {
			index,
			parent,
			top_level,
			result,
		} = outcome;
		let planned = &self.plan.files[index];
		let request = planned.request;
		let active = ActiveFile {
			source_uuid: planned.source.uuid(),
			dest_uuid: planned.dest_uuid,
			dest_parent: parent,
			name: planned.name.as_ref().to_owned(),
			size: planned.size,
			bytes_done: 0,
		};
		match result {
			Ok((file, name)) => {
				if top_level {
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
			Err(FileError::RegisteredAsVersion(file)) => self.record_file_failure(
				index,
				file.name().map_or(active.name, str::to_owned),
				CopyStage::RegisteredAsVersion {
					existing_file: file.stable_uuid.into(),
				},
				Arc::new(Error::custom(
					ErrorKind::InvalidState,
					"the copy was registered as a new version of an existing file",
				)),
			),
			Err(FileError::Failed(stage, error)) => {
				let error = Arc::new(error);
				self.note_error(&error);
				self.record_file_failure(index, active.name, stage, error);
			}
		}
	}

	/// Reports planned file `index` as failed and records it in the report.
	fn record_file_failure(
		&mut self,
		index: usize,
		dest_name: String,
		stage: CopyStage,
		error: Arc<Error>,
	) {
		let planned = &self.plan.files[index];
		let info = FailureInfo {
			source_uuid: planned.source.uuid(),
			source_path: planned.source_path.clone(),
			dest_parent_dir: self.failed_item_parent(planned.parent),
			dest_name,
			stage,
			error,
			affected_files: 1,
			affected_bytes: planned.size,
		};
		self.reporter.file_failed(planned.dest_uuid, info.clone());
		self.report.failures.push(CopyFailure {
			source: FailedSource::File(Box::new(planned.source.clone())),
			info,
		});
	}

	/// A destination may have been shared or linked while the copy ran; items created before
	/// that were propagated to the old targets only. Propagate everything created (each
	/// top-level item with its subtree) to the new ones.
	async fn recheck_targets(&self) -> Result<(), Stopped> {
		let mut checked: Vec<Uuid> = Vec::new();
		'requests: for request in &self.requests {
			let Some(destination) = request.destination else {
				continue;
			};
			if checked.contains(&destination) {
				continue;
			}
			checked.push(destination);
			self.reporter.checkpoint(&self.control).await?;
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
			let added = current.without(&request.targets);
			if added.is_empty() {
				continue;
			}
			let _lock = loop {
				match wait_for_lock(&*self.backend, &self.control, &self.reporter).await? {
					LockWait::Locked(held) => break held,
					LockWait::Paused => self.reporter.checkpoint(&self.control).await?,
					LockWait::Failed(error) => {
						tracing::warn!(
							"failed to lock the drive to propagate copied items: {error}"
						);
						continue 'requests;
					}
				}
			};
			// the top-level items this copy created in the destination
			let created = self
				.report
				.top_level
				.iter()
				.filter(|top| self.requests[top.request].destination == Some(destination));
			for top in created {
				for error in self.backend.propagate_tree(&added, &top.item).await {
					self.reporter.event(CopyEvent::PropagationFailed {
						dest_uuid: top.item.uuid(),
						error: Arc::new(error),
					});
				}
			}
		}
		Ok(())
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

struct DirTask<B> {
	backend: Arc<B>,
	control: JobControl,
	reporter: MaybeArc<Reporter>,
	targets: Arc<ConnectedTargets>,
	parent: Uuid,
	uuid: Uuid,
	name: ValidatedName,
	created: DateTime<Utc>,
	color: DirColor<'static>,
	top_level: bool,
	/// Check the top-level name with the server before creating the directory.
	verify_name: bool,
}

async fn create_dir<B: CopyBackend>(task: DirTask<B>) -> DirResult {
	let DirTask {
		backend,
		control,
		reporter,
		targets,
		parent,
		uuid,
		mut name,
		created,
		color,
		top_level,
		verify_name,
	} = task;
	let backend = &*backend;
	let stage = CopyStage::CreateDirectory;
	// The shared lock the job holds is normally handed out at once; a fresh acquisition (its
	// lease was lost) can wait long. Nothing is sent before the lock is held, so a pause or stop
	// until then leaves the create to be tried again; once held, the create runs to the end.
	let _lock = match wait_for_lock(backend, &control, &reporter).await {
		Ok(LockWait::Locked(held)) => held,
		Ok(LockWait::Paused) | Err(Stopped) => return Err(DirError::NotStarted),
		Ok(LockWait::Failed(error)) => return Err(DirError::Failed(stage, error)),
	};
	let mut retry = NameRetry::new(true);
	let mut dir = loop {
		if verify_name {
			name = retry
				.free_name(backend, parent, name)
				.await
				.map_err(|e| DirError::Failed(stage, e))?;
		}
		match backend
			.create_copy_dir(parent, uuid, &name, created)
			.await
			.map_err(|e| DirError::Failed(stage, e))?
		{
			CreatedDir::Created(created) => break created,
			// Someone created the same name at the destination after it was listed: keep both
			// by taking the next free name.
			CreatedDir::Merged if top_level => {
				name = retry.next(name).map_err(|e| DirError::Failed(stage, e))?
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
			.propagate(&targets, NonRootItemType::Dir(Cow::Borrowed(&dir)))
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
	top_level: bool,
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
		result = backend.acquire_drive_lock() => result,
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

/// Waits for the job to be running and for memory for chunk `index`. A pause that starts while
/// waiting hands the memory back until the job resumes, so a paused job holds none.
async fn reserve_chunk(
	control: JobControl,
	reporter: MaybeArc<Reporter>,
	memory: Arc<Semaphore>,
	size: u64,
	index: u64,
) -> Result<ChunkReservation, Stopped> {
	let bytes =
		u32::try_from(chunk_plaintext_len(size, index) + u64::from(FILE_CHUNK_SIZE_EXTRA.get()))
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
	let top_level = task.top_level;
	let result = copy_file_inner(task).await;
	FileOutcome {
		index,
		parent,
		top_level,
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
	check_chunks_consistent(source.chunks(), size)
		.map_err(|error| FileError::Failed(CopyStage::Download, error))?;
	// A stored count may include a chunk without data (one for an empty file, or a trailing
	// empty chunk); only the chunks holding data are copied.
	let chunks = size.div_ceil(CHUNK_SIZE_U64);
	let mut retry = NameRetry::new(false);
	let mut name = file.name;
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
		// the upload owns its name; the file may be renamed again before it is registered
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
					hasher.update_rayon(&data);
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
			reservation = async {
				reserving
					.as_mut()
					.expect("the select arm is guarded by `reserving.is_some()`")
					.await
			}, if reserving.is_some() => {
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
