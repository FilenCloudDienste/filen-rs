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

use std::{borrow::Cow, collections::VecDeque, future::Future, sync::Arc};

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
		categories::{NonRootItemType, Normal},
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
	plan::{CopyPlan, DestParent, PlannedFile, PlannedItem},
	report::{
		ActiveFile, CopiedTopLevel, CopyEvent, CopyFailure, CopyPhase, CopyReport, CopyStage,
		FailedSource, FailureInfo, OpGuard, PlannedTopLevelItem, Reporter,
	},
};

/// Chunks of one file in flight at once. More only helps a single large file; with several
/// files running, the memory budget is the bound.
pub(crate) const CHUNKS_PER_FILE: usize = 4;

/// How often a top-level directory is renamed and retried when a same-named one appears at the
/// destination between listing it and creating the copy.
const TOP_LEVEL_NAME_ATTEMPTS: usize = 8;

/// Outcome of creating a directory.
#[derive(Debug)]
pub(crate) enum CreatedDir {
	Created(RemoteDirectory),
	/// The server already had a directory with that name there and returned its uuid instead.
	Merged(Uuid),
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
	fn begin_upload(&self, spec: UploadSpec) -> Result<Self::Upload, Error>;
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
	/// Registers the uploaded file. The caller holds the drive lock.
	fn finish_upload(
		&self,
		upload: &Self::Upload,
		completion: UploadCompletion,
		info: RemoteFileInfo,
	) -> impl Future<Output = Result<RemoteFile, Error>> + MaybeSend;
}

/// A copy's report plus how it ended.
#[derive(Debug)]
pub(crate) struct CopyOutcome<D> {
	pub(crate) report: CopyReport<D>,
	/// `Err` with [`ErrorKind::Cancelled`] when cancelled, or the error that ended the job.
	pub(crate) result: Result<(), Error>,
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

/// A created directory, or the stage and error it failed with.
type DirResult = Result<RemoteDirectory, (CopyStage, Error)>;

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
/// first, then the top-level items as planned.
pub(crate) async fn run_copy<B, D>(
	backend: Arc<B>,
	plan: CopyPlan<D>,
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
		// each acquiring and releasing it; dropped while paused. It counts as in flight, so the
		// job only reports itself paused once the lock is gone (the tuple drops the lock first).
		let mut keep_warm: Option<(B::DriveLock, OpGuard)> = None;
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
					let op = self.reporter.op();
					match self.control.until_stopping(self.backend.lock_drive()).await {
						Ok(Ok(lock)) => keep_warm = Some((lock, op)),
						Ok(Err(error)) => return Err(self.stop_error(error)),
						Err(Stopped) => continue,
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
		let name = dir.name.clone();
		let uuid = dir.dest_uuid;
		let created = dir.created.unwrap_or_else(Utc::now);
		let color = dir.color.clone();
		let targets = Arc::clone(&self.targets[dir.request]);
		let backend = Arc::clone(&self.backend);
		let reporter = MaybeArc::clone(&self.reporter);
		Box::pin(async move {
			let result = create_dir(
				&*backend, &reporter, &targets, parent, uuid, name, created, color, top_level,
			)
			.await;
			(index, result)
		})
	}

	fn dir_finished(&mut self, index: usize, result: DirResult, ready: &mut VecDeque<usize>) {
		let parent = self.dest_parent(self.plan.dirs[index].parent);
		match result {
			Ok(dir) => {
				let planned = &self.plan.dirs[index];
				self.dir_states[index] = DirState::Created(dir.uuid());
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
			Err((stage, error)) => {
				let error = Arc::new(error);
				self.note_error(&error);
				self.fail_subtree(index);
				let planned = &self.plan.dirs[index];
				let info = FailureInfo {
					source_uuid: planned.source_uuid,
					source_path: planned.source_path.clone(),
					dest_parent: parent.unwrap_or_default(),
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
					tasks.spawn(copy_file(FileTask {
						backend: Arc::clone(&self.backend),
						control: self.control.clone(),
						reporter: MaybeArc::clone(&self.reporter),
						memory: Arc::clone(&memory),
						targets: Arc::clone(&self.targets[self.plan.files[index].request]),
						file: self.plan.files[index].clone(),
						parent,
						index,
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
			Ok(file) => {
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
			Err(FileError::Failed(stage, error)) => {
				let error = Arc::new(error);
				self.note_error(&error);
				let planned = &self.plan.files[outcome.index];
				let info = FailureInfo {
					source_uuid: active.source_uuid,
					source_path: planned.source_path.clone(),
					dest_parent: outcome.parent,
					dest_name: active.name.clone(),
					stage,
					error,
					affected_files: 1,
					affected_bytes: planned.size,
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
			let _op = self.reporter.op();
			let _lock = match self.backend.lock_drive().await {
				Ok(lock) => lock,
				Err(error) => {
					tracing::warn!("failed to lock the drive to propagate copied items: {error}");
					continue;
				}
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

#[allow(clippy::too_many_arguments)]
async fn create_dir<B: CopyBackend>(
	backend: &B,
	reporter: &MaybeArc<Reporter>,
	targets: &ConnectedTargets,
	parent: Uuid,
	uuid: Uuid,
	name: ValidatedName,
	created: DateTime<Utc>,
	color: DirColor<'static>,
	top_level: bool,
) -> DirResult {
	let _op = reporter.op();
	let stage = CopyStage::CreateDirectory;
	let _lock = backend.lock_drive().await.map_err(|e| (stage, e))?;
	let mut taken: Option<TakenNames> = None;
	let mut name = name;
	let mut dir = None;
	for _ in 0..TOP_LEVEL_NAME_ATTEMPTS {
		match backend
			.create_dir(parent, uuid, &name, created)
			.await
			.map_err(|e| (stage, e))?
		{
			CreatedDir::Created(created) => {
				dir = Some(created);
				break;
			}
			CreatedDir::Merged(_) if top_level => {
				// Someone created the same name at the destination after it was listed: keep
				// both by taking the next free name.
				let taken = taken.get_or_insert_with(TakenNames::default);
				taken.insert(name.as_ref());
				name = taken
					.allocate(name.as_ref(), true)
					.map_err(|e| (stage, e.into()))?;
			}
			CreatedDir::Merged(_) => {
				return Err((
					stage,
					Error::custom(
						ErrorKind::InvalidState,
						"a directory with this name already exists in the new directory",
					),
				));
			}
		}
	}
	let Some(mut dir) = dir else {
		return Err((
			stage,
			Error::custom(
				ErrorKind::InvalidState,
				"could not find a free name for the directory at the destination",
			),
		));
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
	Ok(dir)
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
}

enum FileError {
	Stopped,
	Failed(CopyStage, Error),
}

impl From<Stopped> for FileError {
	fn from(_: Stopped) -> Self {
		Self::Stopped
	}
}

struct FileOutcome {
	index: usize,
	parent: Uuid,
	result: Result<RemoteFile, FileError>,
}

/// Memory reservation of one chunk, and its place among the job's in-flight operations.
struct ChunkReservation {
	_permit: OwnedSemaphorePermit,
	_op: OpGuard,
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
		let permit = control
			.until_stopping(Arc::clone(&memory).acquire_many_owned(bytes))
			.await?
			.expect("the memory semaphore is never closed");
		if !control.is_pause_requested() {
			return Ok(ChunkReservation {
				_permit: permit,
				_op: reporter.op(),
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

async fn copy_file_inner<B: CopyBackend>(task: FileTask<B>) -> Result<RemoteFile, FileError> {
	let FileTask {
		backend,
		control,
		reporter,
		memory,
		targets,
		file,
		parent,
		..
	} = task;
	control.checkpoint().await?;
	let source = Arc::new(file.source);
	let size = file.size;
	let chunks = source.chunks();
	if !chunks_consistent_with_size(chunks, size) {
		return Err(FileError::Failed(
			CopyStage::Download,
			Error::custom(
				ErrorKind::Response,
				format!("file chunk count ({chunks}) is inconsistent with its size ({size})"),
			),
		));
	}
	reporter.file_started(ActiveFile {
		source_uuid: source.uuid(),
		dest_uuid: file.dest_uuid,
		dest_parent: parent,
		name: file.name.as_ref().to_owned(),
		size,
		bytes_done: 0,
	});
	let upload = Arc::new(
		backend
			.begin_upload(UploadSpec {
				uuid: file.dest_uuid,
				parent,
				name: file.name.clone(),
				mime: source.mime().map(str::to_owned),
			})
			.map_err(|e| FileError::Failed(CopyStage::Upload, e))?,
	);

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
	control.checkpoint().await?;
	let _op = reporter.op();
	let _lock = backend
		.lock_drive()
		.await
		.map_err(|e| FileError::Failed(CopyStage::Finalize, e))?;
	let remote = backend
		.finish_upload(&upload, completion, info.unwrap_or_default())
		.await
		.map_err(|e| FileError::Failed(CopyStage::Finalize, e))?;
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
	Ok(remote)
}

#[cfg(test)]
mod tests {
	use std::{
		collections::{HashMap, HashSet},
		sync::{
			Mutex,
			atomic::{AtomicUsize, Ordering},
		},
		time::Duration,
	};

	use tokio::sync::{Notify, watch};

	use super::*;
	use crate::{
		consts::{CHUNK_SIZE, FILE_CHUNK_SIZE_EXTRA_USIZE},
		crypto::{file::FileKey, shared::CreateRandom, v3::EncryptionKey},
		fs::{
			copy::{
				plan::{CopyPlanner, Listed, PlanRequest, PlanSource, SourceDir},
				report::{CopyCallback, CopyUpdate},
			},
			dir::meta::DecryptedDirectoryMeta,
			file::meta::{DecryptedFileMeta, FileMeta},
		},
	};
	use filen_types::fs::StableUuid;

	fn budget(chunks: usize) -> usize {
		chunks * (CHUNK_SIZE + FILE_CHUNK_SIZE_EXTRA_USIZE)
	}

	/// The plaintext of chunk `index` of the source file `uuid`.
	fn chunk_data(uuid: Uuid, index: u64, size: u64) -> Vec<u8> {
		let fill = (uuid.as_u128() as u8) ^ (index as u8);
		vec![fill; chunk_len(size, index) as usize]
	}

	fn file_hash(uuid: Uuid, size: u64) -> Blake3Hash {
		let mut hasher = blake3::Hasher::new();
		for index in 0..size.div_ceil(CHUNK_SIZE_U64) {
			hasher.update(&chunk_data(uuid, index, size));
		}
		Blake3Hash::from(hasher.finalize())
	}

	fn source_file(name: &str, size: u64) -> RemoteFileType<'static> {
		let uuid = Uuid::new_v4();
		let meta = FileMeta::Decoded(DecryptedFileMeta {
			name: Cow::Owned(name.to_owned()),
			size,
			mime: Cow::Borrowed("text/plain"),
			key: FileKey::V3(EncryptionKey::generate()),
			last_modified: Utc::now(),
			created: None,
			hash: Some(file_hash(uuid, size)),
		});
		let file: crate::fs::file::AnonymousRemoteFile = RemoteFile::from_meta(
			uuid,
			(),
			Uuid::new_v4().into(),
			size,
			size.div_ceil(CHUNK_SIZE_U64),
			"de-1",
			"bucket",
			Utc::now(),
			false,
			meta,
		);
		RemoteFileType::File(Cow::Owned(file))
	}

	fn source_dir(name: &str) -> SourceDir {
		SourceDir {
			uuid: Uuid::new_v4(),
			name: Some(name.to_owned()),
			created: Some(Utc::now()),
			color: DirColor::Blue,
			handle: (),
		}
	}

	struct FakeLock(Arc<AtomicUsize>);
	impl Drop for FakeLock {
		fn drop(&mut self) {
			self.0.fetch_sub(1, Ordering::SeqCst);
		}
	}

	#[derive(Default)]
	struct FakeLog {
		fetched: Vec<(Uuid, u64)>,
		uploaded: Vec<(Uuid, u64)>,
		finished: HashMap<Uuid, (String, UploadCompletion)>,
		created_dirs: Vec<(Uuid, String)>,
		out_of_order_dirs: Vec<String>,
		colored: Vec<Uuid>,
		propagated: Vec<Uuid>,
		propagated_trees: Vec<Uuid>,
		target_fetches: usize,
	}

	struct FakeUpload {
		spec: UploadSpec,
	}

	struct FakeBackend {
		memory: Arc<Semaphore>,
		budget: usize,
		live_locks: Arc<AtomicUsize>,
		log: Mutex<FakeLog>,
		known_dirs: Mutex<HashSet<Uuid>>,
		delay: Duration,
		slow: HashMap<String, Duration>,
		fail_fetch: HashMap<String, ErrorKind>,
		fail_upload: HashMap<String, ErrorKind>,
		blocked_uploads: HashSet<String>,
		fail_create: HashSet<String>,
		merge_once: Mutex<HashSet<String>>,
		targets: ConnectedTargets,
		later_targets: Option<ConnectedTargets>,
		/// Later chunks of a file download faster than earlier ones.
		reverse_chunks: bool,
		never: Notify,
	}

	impl FakeBackend {
		fn new(memory_chunks: usize, destinations: &[Uuid]) -> Self {
			Self {
				memory: Arc::new(Semaphore::new(budget(memory_chunks))),
				budget: budget(memory_chunks),
				live_locks: Arc::new(AtomicUsize::new(0)),
				log: Mutex::new(FakeLog::default()),
				known_dirs: Mutex::new(destinations.iter().copied().collect()),
				delay: Duration::from_millis(10),
				slow: HashMap::new(),
				fail_fetch: HashMap::new(),
				fail_upload: HashMap::new(),
				blocked_uploads: HashSet::new(),
				fail_create: HashSet::new(),
				merge_once: Mutex::new(HashSet::new()),
				targets: ConnectedTargets::default(),
				later_targets: None,
				reverse_chunks: false,
				never: Notify::new(),
			}
		}

		fn log(&self) -> std::sync::MutexGuard<'_, FakeLog> {
			self.log.lock().unwrap()
		}

		async fn wait(&self, name: &str) {
			tokio::time::sleep(*self.slow.get(name).unwrap_or(&self.delay)).await;
		}
	}

	impl CopyBackend for FakeBackend {
		type DriveLock = FakeLock;
		type Upload = FakeUpload;

		fn memory(&self) -> Arc<Semaphore> {
			Arc::clone(&self.memory)
		}

		async fn lock_drive(&self) -> Result<FakeLock, Error> {
			self.live_locks.fetch_add(1, Ordering::SeqCst);
			Ok(FakeLock(Arc::clone(&self.live_locks)))
		}

		async fn connected_targets(&self, _dir: Uuid) -> Result<ConnectedTargets, Error> {
			let fetches = {
				let mut log = self.log();
				log.target_fetches += 1;
				log.target_fetches
			};
			Ok(match (&self.later_targets, fetches) {
				(Some(later), 2..) => later.clone(),
				_ => self.targets.clone(),
			})
		}

		async fn create_dir(
			&self,
			parent: Uuid,
			uuid: Uuid,
			name: &ValidatedName,
			created: DateTime<Utc>,
		) -> Result<CreatedDir, Error> {
			assert!(
				self.live_locks.load(Ordering::SeqCst) > 0,
				"creates hold the drive lock"
			);
			self.wait(name.as_ref()).await;
			if self.fail_create.contains(name.as_ref()) {
				return Err(Error::custom(ErrorKind::Server, "create failed"));
			}
			if self.merge_once.lock().unwrap().remove(name.as_ref()) {
				return Ok(CreatedDir::Merged(Uuid::new_v4()));
			}
			if !self.known_dirs.lock().unwrap().contains(&parent) {
				self.log().out_of_order_dirs.push(name.as_ref().to_owned());
			}
			self.known_dirs.lock().unwrap().insert(uuid);
			self.log()
				.created_dirs
				.push((uuid, name.as_ref().to_owned()));
			Ok(CreatedDir::Created(RemoteDirectory::new_from_parts(
				uuid,
				DecryptedDirectoryMeta {
					name: Cow::Owned(name.as_ref().to_owned()),
					created: Some(created),
				},
				parent.into(),
				Utc::now(),
			)))
		}

		async fn set_dir_color(
			&self,
			dir: &mut RemoteDirectory,
			_color: DirColor<'static>,
		) -> Result<(), Error> {
			self.log().colored.push(dir.uuid());
			Ok(())
		}

		async fn propagate(
			&self,
			_targets: &ConnectedTargets,
			item: NonRootItemType<'_, Normal>,
		) -> Vec<Error> {
			self.log().propagated.push(item.uuid());
			Vec::new()
		}

		async fn propagate_tree(
			&self,
			_targets: &ConnectedTargets,
			item: &NonRootItemType<'static, Normal>,
		) -> Vec<Error> {
			self.log().propagated_trees.push(item.uuid());
			Vec::new()
		}

		fn begin_upload(&self, spec: UploadSpec) -> Result<FakeUpload, Error> {
			Ok(FakeUpload { spec })
		}

		async fn fetch_chunk(
			&self,
			file: &RemoteFileType<'static>,
			index: u64,
		) -> Result<Vec<u8>, Error> {
			let name = file.name().unwrap_or_default().to_owned();
			if self.reverse_chunks {
				tokio::time::sleep(Duration::from_millis(10 * (file.chunks() - index))).await;
			} else {
				self.wait(&name).await;
			}
			if let Some(kind) = self.fail_fetch.get(&name) {
				return Err(Error::custom(*kind, "fetch failed"));
			}
			self.log().fetched.push((file.uuid(), index));
			Ok(chunk_data(file.uuid(), index, file.size()))
		}

		async fn upload_chunk(
			&self,
			upload: &FakeUpload,
			index: u64,
			data: Vec<u8>,
		) -> Result<RemoteFileInfo, Error> {
			let name = upload.spec.name.as_ref();
			if self.blocked_uploads.contains(name) {
				self.never.notified().await;
			}
			self.wait(name).await;
			if let Some(kind) = self.fail_upload.get(name) {
				return Err(Error::custom(*kind, "upload failed"));
			}
			assert!(data.len() <= CHUNK_SIZE);
			self.log().uploaded.push((upload.spec.uuid, index));
			Ok(RemoteFileInfo::default())
		}

		async fn finish_upload(
			&self,
			upload: &FakeUpload,
			completion: UploadCompletion,
			_info: RemoteFileInfo,
		) -> Result<RemoteFile, Error> {
			assert!(
				self.live_locks.load(Ordering::SeqCst) > 0,
				"finalizing holds the drive lock"
			);
			self.log().finished.insert(
				upload.spec.uuid,
				(upload.spec.name.as_ref().to_owned(), completion),
			);
			Ok(RemoteFile::from_meta(
				upload.spec.uuid,
				StableUuid::new_for_test(upload.spec.uuid),
				upload.spec.parent.into(),
				completion.written,
				completion.num_chunks,
				"de-1",
				"bucket",
				Utc::now(),
				false,
				FileMeta::Decoded(DecryptedFileMeta {
					name: Cow::Owned(upload.spec.name.as_ref().to_owned()),
					size: completion.written,
					mime: Cow::Borrowed("text/plain"),
					key: FileKey::V3(EncryptionKey::generate()),
					last_modified: completion.final_times.1,
					created: Some(completion.final_times.0),
					hash: Some(completion.hash),
				}),
			))
		}
	}

	#[derive(Default)]
	struct Recorder {
		planned: Mutex<Vec<PlannedTopLevelItem>>,
		created: Mutex<Vec<CopiedTopLevel>>,
		updates: Mutex<Vec<CopyUpdate>>,
	}

	impl CopyCallback for Arc<Recorder> {
		fn top_level_planned(&self, items: Vec<PlannedTopLevelItem>) {
			self.planned.lock().unwrap().extend(items);
		}

		fn top_level_created(&self, item: CopiedTopLevel) {
			self.created.lock().unwrap().push(item);
		}

		fn update(&self, update: CopyUpdate) {
			self.updates.lock().unwrap().push(update);
		}
	}

	impl Recorder {
		fn events(&self) -> Vec<CopyEvent> {
			self.updates
				.lock()
				.unwrap()
				.iter()
				.flat_map(|u| u.events.clone())
				.collect()
		}

		fn last(&self) -> CopyUpdate {
			self.updates.lock().unwrap().last().unwrap().clone()
		}
	}

	fn plan(destination: Uuid, sources: Vec<PlanSource>) -> CopyPlan {
		let mut planner = CopyPlanner::default();
		planner.add_destination(destination, std::iter::empty());
		planner
			.plan(
				sources
					.into_iter()
					.map(|source| PlanRequest {
						source,
						destination,
						name: None,
					})
					.collect(),
			)
			.unwrap()
	}

	fn tree(
		root: &SourceDir,
		dirs: Vec<Listed<SourceDir>>,
		files: Vec<Listed<RemoteFileType<'static>>>,
	) -> PlanSource {
		PlanSource::Dir {
			root: root.clone(),
			dirs,
			files,
		}
	}

	fn listed<T>(parent: &SourceDir, item: T) -> Listed<T> {
		Listed {
			parent: parent.uuid,
			item,
		}
	}

	fn controls() -> (watch::Sender<bool>, watch::Sender<bool>, JobControl) {
		let (pause, pause_rx) = watch::channel(false);
		let (cancel, cancel_rx) = watch::channel(false);
		(
			pause,
			cancel,
			JobControl::new(Some(pause_rx), Some(cancel_rx)),
		)
	}

	type Running = tokio::task::JoinHandle<CopyOutcome<()>>;

	fn start(
		backend: &Arc<FakeBackend>,
		plan: CopyPlan,
		control: JobControl,
	) -> (Running, Arc<Recorder>, MaybeArc<Reporter>) {
		let recorder = Arc::new(Recorder::default());
		let reporter = Reporter::new(Arc::clone(&recorder));
		let running = tokio::spawn(run_copy(
			Arc::clone(backend),
			plan,
			control,
			MaybeArc::clone(&reporter),
		));
		(running, recorder, reporter)
	}

	async fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
		for _ in 0..100_000 {
			if condition() {
				return;
			}
			tokio::time::sleep(Duration::from_millis(1)).await;
		}
		panic!("timed out waiting until {what}");
	}

	/// Everything a finished job must have given back.
	fn assert_released(backend: &FakeBackend, reporter: &Reporter) {
		assert_eq!(
			backend.memory.available_permits(),
			backend.budget,
			"every memory reservation is released"
		);
		assert_eq!(
			backend.live_locks.load(Ordering::SeqCst),
			0,
			"no drive lock is held"
		);
		assert_eq!(reporter.ops_in_flight(), 0, "nothing is in flight");
	}

	fn assert_each_chunk_once(backend: &FakeBackend) {
		let log = backend.log();
		let fetched: HashSet<_> = log.fetched.iter().copied().collect();
		assert_eq!(
			fetched.len(),
			log.fetched.len(),
			"no chunk is fetched twice"
		);
		let uploaded: HashSet<_> = log.uploaded.iter().copied().collect();
		assert_eq!(
			uploaded.len(),
			log.uploaded.len(),
			"no chunk is uploaded twice"
		);
	}

	#[tokio::test(start_paused = true)]
	async fn copies_a_tree_parent_first_with_every_chunk_once() {
		let destination = Uuid::new_v4();
		let top = source_dir("Top");
		let sub = source_dir("Sub");
		let files = [
			("a.bin", 2 * CHUNK_SIZE_U64 + 12_345),
			("empty", 0),
			("exact", CHUNK_SIZE_U64),
			("t.txt", 10),
		];
		let sources: Vec<_> = files
			.iter()
			.map(|(name, size)| source_file(name, *size))
			.collect();
		let plan = plan(
			destination,
			vec![
				tree(
					&top,
					vec![listed(&top, sub.clone())],
					vec![
						listed(&top, sources[0].clone()),
						listed(&sub, sources[1].clone()),
						listed(&sub, sources[2].clone()),
					],
				),
				PlanSource::File(sources[3].clone()),
			],
		);
		let backend = Arc::new(FakeBackend::new(4, &[destination]));
		let (running, recorder, reporter) = start(&backend, plan, JobControl::default());
		let outcome = running.await.unwrap();

		outcome.result.unwrap();
		assert_released(&backend, &reporter);
		assert_each_chunk_once(&backend);
		{
			let log = backend.log();
			assert!(
				log.out_of_order_dirs.is_empty(),
				"directories are created parent first"
			);
			assert_eq!(log.created_dirs.len(), 2);
			assert_eq!(log.colored.len(), 2, "colors are kept");
			assert!(
				log.propagated.is_empty(),
				"an unconnected destination gets no propagation"
			);
			assert_eq!(log.finished.len(), 4);
			for (source, (_, size)) in sources.iter().zip(files) {
				let (_, completion) = log
					.finished
					.values()
					.find(|(name, _)| name == source.name().unwrap())
					.unwrap();
				assert_eq!(completion.written, size);
				assert_eq!(completion.num_chunks, size.div_ceil(CHUNK_SIZE_U64));
				assert_eq!(completion.hash, file_hash(source.uuid(), size));
			}
			assert!(
				!log.fetched
					.iter()
					.any(|(uuid, _)| *uuid == sources[1].uuid()),
				"a zero-byte file transfers no chunk"
			);
		}

		let counts = outcome.report.counts;
		assert_eq!(counts.dirs_created, 2);
		assert_eq!(counts.files_done, 4);
		assert_eq!(counts.bytes_done, outcome.report.totals.bytes);
		assert_eq!(counts.files_failed + counts.dirs_failed, 0);
		assert_eq!(outcome.report.top_level.len(), 2);

		let planned = recorder.planned.lock().unwrap().clone();
		let created = recorder.created.lock().unwrap().clone();
		assert_eq!(planned.len(), 2);
		assert_eq!(created.len(), 2);
		for item in &created {
			assert!(
				planned.iter().any(|p| p.dest_uuid == item.item.uuid()),
				"created top-level items were announced up front"
			);
		}

		let events = recorder.events();
		for source in &sources {
			let started = events.iter().position(
				|e| matches!(e, CopyEvent::FileStarted(f) if f.source_uuid == source.uuid()),
			);
			let done = events.iter().position(
				|e| matches!(e, CopyEvent::FileDone { source_uuid, .. } if *source_uuid == source.uuid()),
			);
			assert!(
				started.unwrap() < done.unwrap(),
				"a file starts before it is done"
			);
		}
		let last = recorder.last();
		assert_eq!(last.phase, CopyPhase::Done);
		assert!(last.active.is_empty());
		assert_eq!(last.counts, counts);
	}

	async fn copy_many(memory_chunks: usize, files: usize, chunks_per_file: u64) {
		let destination = Uuid::new_v4();
		let sources: Vec<_> = (0..files)
			.map(|i| {
				source_file(
					&format!("f{i}"),
					chunks_per_file * CHUNK_SIZE_U64 - i as u64,
				)
			})
			.collect();
		let plan = plan(
			destination,
			sources.iter().cloned().map(PlanSource::File).collect(),
		);
		let mut backend = FakeBackend::new(memory_chunks, &[destination]);
		for (i, source) in sources.iter().enumerate() {
			backend.slow.insert(
				source.name().unwrap().to_owned(),
				Duration::from_millis(1 + (i as u64 * 7) % 13),
			);
		}
		let backend = Arc::new(backend);
		let (running, _recorder, reporter) = start(&backend, plan, JobControl::default());
		let outcome = tokio::time::timeout(Duration::from_secs(600), running)
			.await
			.expect("the copy must not deadlock")
			.unwrap();
		outcome.result.unwrap();
		assert_released(&backend, &reporter);
		assert_each_chunk_once(&backend);
		let log = backend.log();
		assert_eq!(log.finished.len(), files);
		for source in &sources {
			let (_, completion) = log
				.finished
				.values()
				.find(|(name, _)| name == source.name().unwrap())
				.unwrap();
			assert_eq!(completion.hash, file_hash(source.uuid(), source.size()));
		}
	}

	#[tokio::test(start_paused = true)]
	async fn never_deadlocks_on_the_smallest_memory_budget() {
		copy_many(1, 8, 5).await;
	}

	#[tokio::test(start_paused = true)]
	async fn never_deadlocks_with_many_files_on_a_small_budget() {
		copy_many(3, 20, 3).await;
	}

	/// Tasks run in parallel here, so reservations are taken and released concurrently.
	#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
	async fn never_deadlocks_on_a_multi_threaded_runtime() {
		copy_many(2, 12, 4).await;
	}

	#[tokio::test(start_paused = true)]
	async fn hashes_chunks_in_order_when_they_complete_out_of_order() {
		let destination = Uuid::new_v4();
		let source = source_file("f", 6 * CHUNK_SIZE_U64);
		let mut backend = FakeBackend::new(8, &[destination]);
		backend.reverse_chunks = true;
		let backend = Arc::new(backend);
		let plan = plan(destination, vec![PlanSource::File(source.clone())]);
		let (running, _recorder, _reporter) = start(&backend, plan, JobControl::default());
		running.await.unwrap().result.unwrap();
		let log = backend.log();
		let order: Vec<u64> = log.fetched.iter().map(|(_, index)| *index).collect();
		assert_ne!(
			order,
			(0..6).collect::<Vec<_>>(),
			"chunks completed out of order"
		);
		let (_, completion) = log.finished.values().next().unwrap();
		assert_eq!(completion.hash, file_hash(source.uuid(), source.size()));
	}

	#[tokio::test(start_paused = true)]
	async fn pause_during_file_copies_releases_everything_and_resumes() {
		let destination = Uuid::new_v4();
		let sources: Vec<_> = (0..3)
			.map(|i| source_file(&format!("f{i}"), 6 * CHUNK_SIZE_U64))
			.collect();
		let plan = plan(
			destination,
			sources.iter().cloned().map(PlanSource::File).collect(),
		);
		let backend = Arc::new(FakeBackend::new(4, &[destination]));
		let (pause, _cancel, control) = controls();
		let (running, recorder, reporter) = start(&backend, plan, control);

		wait_until("some chunks are uploaded", || {
			backend.log().uploaded.len() >= 3
		})
		.await;
		pause.send_replace(true);
		wait_until("the job is paused", || reporter.is_paused()).await;
		assert_released(&backend, &reporter);
		let uploaded = backend.log().uploaded.len();
		tokio::time::sleep(Duration::from_secs(60)).await;
		assert_eq!(
			backend.log().uploaded.len(),
			uploaded,
			"nothing moves while paused"
		);
		assert!(recorder.updates.lock().unwrap().iter().any(|u| u.paused));
		assert!(recorder.updates.lock().unwrap().iter().any(|u| u.pausing));

		pause.send_replace(false);
		let outcome = running.await.unwrap();
		outcome.result.unwrap();
		assert_released(&backend, &reporter);
		assert_each_chunk_once(&backend);
		assert_eq!(backend.log().uploaded.len(), 18);
		assert_eq!(outcome.report.counts.bytes_done, 18 * CHUNK_SIZE_U64);
		let last = recorder.last();
		assert!(!last.paused && !last.pausing);
	}

	fn wide_tree(dirs: usize) -> (SourceDir, PlanSource) {
		let root = source_dir("Root");
		let children: Vec<_> = (0..dirs).map(|i| source_dir(&format!("d{i}"))).collect();
		let files = children
			.iter()
			.enumerate()
			.map(|(i, dir)| listed(dir, source_file(&format!("file{i}"), 100)))
			.collect();
		let source = tree(
			&root,
			children
				.iter()
				.map(|child| listed(&root, child.clone()))
				.collect(),
			files,
		);
		(root, source)
	}

	#[tokio::test(start_paused = true)]
	async fn pause_during_directory_creation_holds_no_lock_and_resumes() {
		let destination = Uuid::new_v4();
		let (_, source) = wide_tree(200);
		let backend = Arc::new(FakeBackend::new(4, &[destination]));
		let (pause, _cancel, control) = controls();
		let (running, _recorder, reporter) =
			start(&backend, plan(destination, vec![source]), control);

		wait_until("the root is created", || {
			!backend.log().created_dirs.is_empty()
		})
		.await;
		pause.send_replace(true);
		wait_until("the job is paused", || reporter.is_paused()).await;
		assert_released(&backend, &reporter);
		let created = backend.log().created_dirs.len();
		assert!(created < 201, "paused before every directory existed");
		tokio::time::sleep(Duration::from_secs(60)).await;
		assert_eq!(
			backend.log().created_dirs.len(),
			created,
			"no create starts while paused"
		);

		pause.send_replace(false);
		running.await.unwrap().result.unwrap();
		assert_eq!(backend.log().created_dirs.len(), 201);
		assert_eq!(backend.log().finished.len(), 200);
		assert!(backend.log().out_of_order_dirs.is_empty());
		assert_released(&backend, &reporter);
	}

	#[tokio::test(start_paused = true)]
	async fn a_job_paused_before_it_starts_waits() {
		let destination = Uuid::new_v4();
		let (_, source) = wide_tree(3);
		let backend = Arc::new(FakeBackend::new(4, &[destination]));
		let (pause, _cancel, control) = controls();
		pause.send_replace(true);
		let (running, _recorder, reporter) =
			start(&backend, plan(destination, vec![source]), control);
		tokio::time::sleep(Duration::from_secs(60)).await;
		assert_eq!(backend.log().target_fetches, 0);
		assert!(backend.log().created_dirs.is_empty());
		assert!(reporter.is_paused());
		pause.send_replace(false);
		running.await.unwrap().result.unwrap();
		assert_eq!(backend.log().finished.len(), 3);
	}

	#[tokio::test(start_paused = true)]
	async fn cancel_during_directory_creation_reports_what_was_created() {
		let destination = Uuid::new_v4();
		let (first_root, first) = wide_tree(200);
		let (_, second) = wide_tree(1);
		let backend = Arc::new(FakeBackend::new(4, &[destination]));
		let (_pause, cancel, control) = controls();
		let (running, recorder, reporter) =
			start(&backend, plan(destination, vec![first, second]), control);

		wait_until("a top-level directory exists", || {
			!recorder.created.lock().unwrap().is_empty()
		})
		.await;
		cancel.send_replace(true);
		let outcome = running.await.unwrap();

		assert_eq!(outcome.result.unwrap_err().kind(), ErrorKind::Cancelled);
		assert_released(&backend, &reporter);
		assert!(
			backend.log().finished.is_empty(),
			"no file is copied after a cancel"
		);
		let created_uuids: HashSet<_> = backend
			.log()
			.created_dirs
			.iter()
			.map(|(uuid, _)| *uuid)
			.collect();
		for item in &outcome.report.top_level {
			assert!(created_uuids.contains(&item.item.uuid()));
		}
		assert!(
			outcome
				.report
				.top_level
				.iter()
				.any(|item| item.source_uuid == first_root.uuid)
		);
		assert_eq!(
			outcome.report.top_level.len(),
			recorder.created.lock().unwrap().len(),
			"every created top-level item was reported as it was created"
		);
		assert_eq!(recorder.last().phase, CopyPhase::Cancelled);
	}

	#[tokio::test(start_paused = true)]
	async fn cancel_during_file_copies_drops_transfers_and_keeps_finished_files() {
		let destination = Uuid::new_v4();
		let small = source_file("small", 10);
		let big = source_file("big", 5 * CHUNK_SIZE_U64);
		let mut backend = FakeBackend::new(4, &[destination]);
		backend.blocked_uploads.insert("big".to_owned());
		let backend = Arc::new(backend);
		let (_pause, cancel, control) = controls();
		let (running, recorder, reporter) = start(
			&backend,
			plan(
				destination,
				vec![PlanSource::File(small.clone()), PlanSource::File(big)],
			),
			control,
		);

		wait_until("the small file is copied", || {
			backend.log().finished.len() == 1
		})
		.await;
		cancel.send_replace(true);
		let outcome = running.await.unwrap();

		assert_eq!(outcome.result.unwrap_err().kind(), ErrorKind::Cancelled);
		assert_released(&backend, &reporter);
		assert_eq!(
			backend.log().finished.len(),
			1,
			"the interrupted file never becomes visible"
		);
		assert_eq!(outcome.report.top_level.len(), 1);
		assert_eq!(outcome.report.top_level[0].source_uuid, small.uuid());
		assert_eq!(outcome.report.counts.files_done, 1);
		assert_eq!(
			outcome.report.counts.files_failed, 0,
			"an interrupted file is not a failure"
		);
		assert_eq!(outcome.report.counts.bytes_done, 10);
		assert_eq!(recorder.last().phase, CopyPhase::Cancelled);
	}

	#[tokio::test(start_paused = true)]
	async fn running_out_of_storage_ends_the_job() {
		let destination = Uuid::new_v4();
		let sources: Vec<_> = (0..10)
			.map(|i| source_file(&format!("f{i}"), 100))
			.collect();
		let mut backend = FakeBackend::new(16, &[destination]);
		backend.delay = Duration::from_secs(10);
		backend
			.slow
			.insert("f2".to_owned(), Duration::from_millis(1));
		backend
			.fail_upload
			.insert("f2".to_owned(), ErrorKind::MaxStorageReached);
		let backend = Arc::new(backend);
		let (running, recorder, reporter) = start(
			&backend,
			plan(
				destination,
				sources.into_iter().map(PlanSource::File).collect(),
			),
			JobControl::default(),
		);
		let outcome = running.await.unwrap();

		assert_eq!(
			outcome.result.unwrap_err().kind(),
			ErrorKind::MaxStorageReached
		);
		assert_released(&backend, &reporter);
		assert!(
			backend.log().finished.is_empty(),
			"the other files are stopped, not finished"
		);
		assert_eq!(outcome.report.failures.len(), 1);
		assert_eq!(
			outcome.report.failures[0].info.error.kind(),
			ErrorKind::MaxStorageReached
		);
		assert_eq!(recorder.last().phase, CopyPhase::Failed);
	}

	#[tokio::test(start_paused = true)]
	async fn a_failed_file_does_not_stop_the_others() {
		let destination = Uuid::new_v4();
		let good = source_file("good", 2 * CHUNK_SIZE_U64);
		let bad = source_file("bad", 3 * CHUNK_SIZE_U64);
		let mut backend = FakeBackend::new(4, &[destination]);
		backend
			.fail_fetch
			.insert("bad".to_owned(), ErrorKind::FileChunkNotFound);
		let backend = Arc::new(backend);
		let (running, recorder, reporter) = start(
			&backend,
			plan(
				destination,
				vec![PlanSource::File(good), PlanSource::File(bad.clone())],
			),
			JobControl::default(),
		);
		let outcome = running.await.unwrap();

		outcome.result.unwrap();
		assert_released(&backend, &reporter);
		assert_eq!(backend.log().finished.len(), 1);
		let [failure] = outcome.report.failures.as_slice() else {
			panic!("exactly one failure");
		};
		assert!(matches!(&failure.source, FailedSource::File(file) if file.uuid() == bad.uuid()));
		assert_eq!(failure.info.stage, CopyStage::Download);
		assert_eq!(failure.info.dest_parent, destination);
		assert_eq!(failure.info.dest_name, "bad");
		assert_eq!(failure.info.error.kind(), ErrorKind::FileChunkNotFound);
		let counts = outcome.report.counts;
		assert_eq!((counts.files_done, counts.files_failed), (1, 1));
		assert_eq!(counts.bytes_failed, 3 * CHUNK_SIZE_U64);
		assert_eq!(
			counts.bytes_done + counts.bytes_failed,
			outcome.report.totals.bytes
		);
		assert!(
			recorder
				.events()
				.iter()
				.any(|e| matches!(e, CopyEvent::FileFailed(info) if info.dest_name == "bad"))
		);
	}

	#[tokio::test(start_paused = true)]
	async fn a_failed_directory_fails_its_subtree_without_attempting_it() {
		let destination = Uuid::new_v4();
		let top = source_dir("Top");
		let sub = source_dir("Sub");
		let deep = source_dir("Deep");
		let source = tree(
			&top,
			vec![listed(&top, sub.clone()), listed(&sub, deep.clone())],
			vec![
				listed(&top, source_file("kept", 5)),
				listed(&sub, source_file("lost1", 7)),
				listed(&deep, source_file("lost2", 11)),
			],
		);
		let mut backend = FakeBackend::new(4, &[destination]);
		backend.fail_create.insert("Sub".to_owned());
		let backend = Arc::new(backend);
		let (running, _recorder, reporter) = start(
			&backend,
			plan(destination, vec![source]),
			JobControl::default(),
		);
		let outcome = running.await.unwrap();

		outcome.result.unwrap();
		assert_released(&backend, &reporter);
		let log = backend.log();
		assert_eq!(
			log.created_dirs.len(),
			1,
			"nothing below the failed directory is attempted"
		);
		assert_eq!(log.finished.len(), 1);
		let [failure] = outcome.report.failures.as_slice() else {
			panic!("one failure for the whole subtree");
		};
		assert!(matches!(failure.source, FailedSource::Dir(())));
		assert_eq!(failure.info.stage, CopyStage::CreateDirectory);
		assert_eq!(failure.info.source_uuid, sub.uuid);
		assert_eq!(
			(failure.info.affected_files, failure.info.affected_bytes),
			(2, 18)
		);
		let counts = outcome.report.counts;
		assert_eq!((counts.dirs_created, counts.dirs_failed), (1, 2));
		assert_eq!((counts.files_done, counts.files_failed), (1, 2));
		assert_eq!(
			counts.bytes_done + counts.bytes_failed,
			outcome.report.totals.bytes
		);
	}

	#[tokio::test(start_paused = true)]
	async fn a_top_level_name_taken_after_listing_gets_the_next_name() {
		let destination = Uuid::new_v4();
		let top = source_dir("Top");
		let backend = FakeBackend::new(4, &[destination]);
		backend.merge_once.lock().unwrap().insert("Top".to_owned());
		let backend = Arc::new(backend);
		let (running, _recorder, _reporter) = start(
			&backend,
			plan(destination, vec![tree(&top, Vec::new(), Vec::new())]),
			JobControl::default(),
		);
		let outcome = running.await.unwrap();
		outcome.result.unwrap();
		assert_eq!(backend.log().created_dirs[0].1, "Top (1)");
		let NonRootItemType::Dir(dir) = &outcome.report.top_level[0].item else {
			panic!("a directory");
		};
		assert_eq!(dir.name(), Some("Top (1)"));
	}

	#[tokio::test(start_paused = true)]
	async fn propagates_every_created_item_into_a_connected_destination() {
		let destination = Uuid::new_v4();
		let (_, source) = wide_tree(3);
		let mut backend = FakeBackend::new(4, &[destination]);
		backend.targets = ConnectedTargets::with_test_users(2);
		let backend = Arc::new(backend);
		let (running, _recorder, _reporter) = start(
			&backend,
			plan(destination, vec![source]),
			JobControl::default(),
		);
		running.await.unwrap().result.unwrap();
		let log = backend.log();
		let created: HashSet<Uuid> = log
			.created_dirs
			.iter()
			.map(|(uuid, _)| *uuid)
			.chain(log.finished.keys().copied())
			.collect();
		let propagated: HashSet<Uuid> = log.propagated.iter().copied().collect();
		assert_eq!(created.len(), 7);
		assert_eq!(propagated, created);
		assert!(
			log.propagated_trees.is_empty(),
			"nothing changed during the copy"
		);
	}

	#[tokio::test(start_paused = true)]
	async fn a_destination_shared_during_the_copy_gets_the_copied_trees() {
		let destination = Uuid::new_v4();
		let (_, source) = wide_tree(2);
		let file = source_file("f", 1);
		let mut backend = FakeBackend::new(4, &[destination]);
		backend.later_targets = Some(ConnectedTargets::with_test_users(1));
		let backend = Arc::new(backend);
		let (running, recorder, _reporter) = start(
			&backend,
			plan(destination, vec![source, PlanSource::File(file)]),
			JobControl::default(),
		);
		let outcome = running.await.unwrap();
		outcome.result.unwrap();
		let log = backend.log();
		assert!(log.propagated.is_empty());
		let top_level: HashSet<Uuid> = outcome
			.report
			.top_level
			.iter()
			.map(|t| t.item.uuid())
			.collect();
		let trees: HashSet<Uuid> = log.propagated_trees.iter().copied().collect();
		assert_eq!(trees, top_level);
		assert!(
			recorder
				.updates
				.lock()
				.unwrap()
				.iter()
				.any(|u| u.phase == CopyPhase::Finishing)
		);
	}
}
