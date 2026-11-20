//! Local changes: carrying what the user does in the sync root to the drive.
//!
//! Every watcher event and notification is a path to look at, never a verdict. A path waits in the
//! [`queue`] until it has been quiet for the debounce window, then a job reads the item's state and
//! decides: an ordinary file is uploaded and converted into a placeholder, an ordinary directory is
//! created and converted, a placeholder that is no longer in sync is uploaded as a new version, and
//! a placeholder in sync is left alone, which is how the provider's own writes (hydrations,
//! populations, converts) are told from the user's. A delete or rename of a placeholder is a
//! request the core answers ([`veto`]): a rename is applied to the drive before it is allowed, so
//! one the drive cannot take is vetoed; a delete is checked against the drive and allowed, and the
//! remote item is trashed only once the item is gone locally, since neither `NOTIFY_DELETE` nor its
//! completion proves a delete. Both sides of a conflict are kept: the local side moves aside under
//! a `.old` name and is uploaded as a new item.
//!
//! The same engine applies what changes on the drive ([`remote`]): the cache's change feed and the
//! walks queue remote jobs on the same paths, so a path is never worked on by both sides at once,
//! and the remote side changes only clean placeholders, handing anything holding local data to the
//! local side.

#[cfg(feature = "test-seams")]
mod convert_hold;
mod dehydrate;
mod hydration;
mod jobs;
mod leftover;
mod lineage;
mod queue;
mod records;
mod relocate;
mod remote;
mod report;
mod unsent;
mod veto;

use std::{
	collections::{HashMap, HashSet},
	mem,
	num::NonZeroUsize,
	path::PathBuf,
	sync::{
		Arc, Mutex, MutexGuard, PoisonError,
		atomic::{AtomicBool, Ordering},
	},
	time::Duration,
};

use filen_mobile_native_cache::auth::FilenMobileCacheState;
use filen_sdk_rs::sync_engine::{
	WATCH_DEBOUNCE, WATCH_MAX_DEBOUNCE_BURST, WATCH_SAFETY_NET, safety_net_interval, watch_backoff,
};
use filen_types::fs::Uuid;
use tokio::{
	pin,
	runtime::Handle,
	select,
	sync::{Mutex as AsyncMutex, Notify, OwnedSemaphorePermit, Semaphore, watch},
	task::{self, JoinSet},
	time::{Instant, sleep, sleep_until, timeout, timeout_at},
};
use tracing::{debug, warn};

#[cfg(feature = "test-seams")]
pub use self::convert_hold::{held_convert, hold_next_convert, release_convert};
#[cfg(feature = "test-seams")]
pub use self::relocate::relocate_for_test;
use self::{
	leftover::Leftovers,
	lineage::{FreshUploads, Lineage},
	queue::{Admit, Delay, Kind, Kinds, Outcome, Queue},
	records::{Records, RenameRecord, RootKey, SavedSeen, Snapshot, WriteError},
	remote::{Index, populated_uuid},
	veto::{DeleteStage, PendingDelete, PendingRename},
};
pub(crate) use self::{
	records::RECORD_FILE_NAMES,
	remote::{FileVersion, Recent},
};
pub use self::{
	remote::RemoteHints,
	report::{ConflictKind, KeptReason, LocalReport, LocalSkip, RemoteApplied, UndoneOp, VetoedOp},
};
use crate::{
	dispatch::CloudFilesCallback,
	drive::Drive,
	identity::PlaceholderIdentity,
	log_safe::path_tag,
	os::{
		Completed, DehydrateRequest, DehydrateResponder, DeleteRequest, DeleteResponder, IfPinned,
		LocalEntry, LocalError, LocalEvent, LocalFileId, LocalFs, LocalKind, PinAttr,
		RenameRequest, RenameResponder,
	},
	root_path::{CaseKey, RootPath, RootPathError},
};

/// How the local side runs.
#[derive(Debug, Clone, Copy)]
pub struct LocalConfig {
	/// How long a file must keep its size and times before it is uploaded. Default 2 s.
	pub settle: Duration,
	/// How many uploads run at once. Default 4.
	pub max_concurrent_uploads: NonZeroUsize,
	/// How many jobs run at once, uploads included. Default 8.
	pub max_concurrent_jobs: NonZeroUsize,
	/// The most paths queued at once; past it the queue is dropped and the root walked instead,
	/// as the sync engine collapses a changelist past its subtree cap. Default 16,384.
	pub max_queued: NonZeroUsize,
	/// How long after the last read of a file by another process ended the hydration job
	/// completes and checks it, so it never competes with a reader. Default 5 s.
	pub hydration_quiet: Duration,
	/// How many hydration jobs run at once, within the job limit: the jobs that complete, hash,
	/// free or pin files, so they never take every job slot from a user's change. Default 2.
	pub max_concurrent_hydrations: NonZeroUsize,
}

impl Default for LocalConfig {
	fn default() -> Self {
		Self {
			settle: Duration::from_secs(2),
			max_concurrent_uploads: NonZeroUsize::new(4).expect("4 is not zero"),
			max_concurrent_jobs: NonZeroUsize::new(8).expect("8 is not zero"),
			max_queued: NonZeroUsize::new(16_384).expect("16,384 is not zero"),
			hydration_quiet: Duration::from_secs(5),
			max_concurrent_hydrations: NonZeroUsize::new(2).expect("2 is not zero"),
		}
	}
}

/// Carries local changes in the sync root to the drive. Each `on_*` call returns at once and never
/// blocks, so the OS layer may call it from a watcher or a Cloud Files callback thread; the work
/// runs on the runtime handed to [`LocalEngine::new`].
pub struct LocalEngine<D: Drive, L: LocalFs> {
	// Reason for the Arc: the callers and the tasks they spawn share it.
	inner: Arc<Inner<D, L>>,
}

/// How long after the lineage changed it is saved, so a burst of changes is one write. A stop
/// sooner leaves the saved lineage one change behind, which at worst keeps both sides of a save
/// made before the next start.
const LINEAGE_SAVE_DELAY: Duration = Duration::from_secs(2);
/// How long a stop gives the renames it cut short to be put back on the drive, all of them: the
/// rest are put back on the next start, from their records. Well under the desktop app's 25 s
/// limit on the helper's whole shutdown.
const STOP_PUT_BACK_BUDGET: Duration = Duration::from_secs(8);
/// The longest the start waits before it reads a sync root it could not read for its records
/// again.
const RESTORE_AGAIN_MAX: Duration = Duration::from_secs(60);

/// The local engine the provider runs, over the cache.
pub type CacheLocalEngine<L> = LocalEngine<FilenMobileCacheState, L>;

struct Inner<D, L> {
	// Reason for the Arc: the provider shares the cache with the dispatcher.
	drive: Arc<D>,
	// Reason for the Arc: each blocking call takes its own handle to the root.
	local: Arc<L>,
	runtime: Handle,
	// Reason for the Arc: the provider keeps its own handle to report through.
	callback: Arc<dyn CloudFilesCallback>,
	config: LocalConfig,
	/// A std Mutex, never held across an await.
	state: Mutex<State>,
	/// Wakes the scheduler: a path was queued or a job ended.
	wake: Notify,
	/// Wakes a walk waiting for room in the queue.
	room: Notify,
	// Reason for the Arc: permits move into the job tasks (`try_acquire_owned`).
	jobs: Arc<Semaphore>,
	uploads: Semaphore,
	// Reason for the Arc: permits move into the job tasks (`try_acquire_owned`).
	hydrations: Arc<Semaphore>,
	/// Set once the engine stops: a hash running on the blocking pool ends at it.
	// Reason for the Arc: each blocking hash takes its own handle to it.
	stop: Arc<AtomicBool>,
	/// Every task spawned; a std Mutex, held only to spawn into or take the set.
	tasks: Mutex<JoinSet<()>>,
	// Reason for the Arc: the dispatcher holds it too.
	hints: Arc<RemoteHints>,
	// Reason for the Arc: the cache's listener holds it too.
	feed_wake: Arc<Notify>,
	/// Set once the edits a previous run left marked in the cache are taken over ([`unsent`]): an
	/// edit's upload waits for it, as it goes through the same cache slot and would overwrite a
	/// marked edit no placeholder holds before the take-over kept it.
	taken_over: watch::Sender<bool>,
	/// How many walks of the root ended: walks run one at a time, so the walk that started
	/// `n`-th ended once it reaches `n`.
	walks_ended: watch::Sender<u64>,
	/// Moves on each time a job ends or finds a local change to carry: what may settle the paths
	/// the count of what the drive lacks waits for ([`LocalEngine::unsynced_count_after_walk`]).
	looks: watch::Sender<u64>,
	/// Where the engine keeps what it needs across a restart, if anywhere ([`records`]).
	records: Option<Records>,
	/// Held while the records are written, so an older snapshot never lands after a newer one.
	record_writes: AsyncMutex<()>,
}

/// Whether the lineage a previous run saved is taken back yet.
#[derive(Debug)]
enum Restore {
	/// Nothing to take back: none was saved, or it was taken back.
	Done,
	/// Not read yet: no ordinary file is taken in until it is.
	Loading,
	/// Read, by path, until the first walk to finish says which still claim their path.
	Walking(HashMap<CaseKey, SavedSeen>),
}

struct State {
	queue: Queue,
	lineage: Lineage,
	/// New files being uploaded, by local file id, with where each is now: a rename while the
	/// upload runs moves the entry, and the upload's end converts the file where it is.
	uploading: HashMap<LocalFileId, RootPath>,
	/// The new files this run uploaded lately: what tells a save's temporary file from a file the
	/// user keeps.
	fresh: FreshUploads,
	/// Allowed deletes waiting to be confirmed locally before the remote item is trashed.
	deletes: HashMap<CaseKey, PendingDelete>,
	/// Renames the drive was changed for, waiting to be confirmed locally or put back.
	renames: Vec<PendingRename>,
	walk: Walk,
	/// How many walks of the root started.
	walks_started: u64,
	/// The number (in [`State::walks_started`]) of the last walk that read the root and listed
	/// every directory it came to; `0` before one did.
	last_complete_walk: u64,
	/// Paths reported skipped, so each is reported once.
	skips_reported: HashSet<CaseKey>,
	/// Edits partly on disk whose hydration left them so, with how many times in a row: past a few,
	/// they wait for the next walk rather than hydrate again on their own hydration's events.
	stalled: HashMap<CaseKey, u32>,
	/// Where each remote item has its placeholder.
	index: Index,
	/// Whether a report of the populated directories to the cache is due.
	populated_report_due: bool,
	/// Remote children reported skipped, by their directory and id, so each is reported once.
	listed_skips_reported: HashSet<(Uuid, String)>,
	/// Reconciles in a row that left something for another job, by directory: past a limit the
	/// directory waits for the next walk.
	deferrals: HashMap<CaseKey, u32>,
	/// Completions in a row that left a file partly on disk, by path: past a few, the file waits
	/// for its next read or the next walk. A count of its own rather than the queue's attempts, as
	/// each completion raises a watcher event that queues the path again at once, which would cut
	/// the queue's backoff short.
	hydration_stalls: HashMap<CaseKey, u32>,
	/// Files a hint named that the index did not place: the next walk queues their hydration job.
	unplaced: HashSet<Uuid>,
	/// Whether the next walk to finish records the file placeholders it found holding changes, for
	/// the take-over of the edits the cache holds marked ([`unsent`]); only the first one does.
	collect_dirty: bool,
	/// The file placeholders holding changes a walk found, until the take-over takes them.
	walked_dirty: Option<unsent::Dirty>,
	/// Files whose marked edit the take-over could not keep yet: their edits are not uploaded
	/// until it is kept, as the upload's copy would overwrite it in the cache's slot.
	unkept: HashSet<Uuid>,
	/// The root the records belong to, once read at the start; nothing is saved before.
	records_key: Option<RootKey>,
	restore: Restore,
	/// Every rename the drive was changed for, or is about to be, not settled yet: saved, when the
	/// engine keeps records, before the drive is changed.
	rename_records: Vec<RenameRecord>,
	next_record: u64,
	/// The number the next snapshot of the records gets: taken with the state it holds, so a write
	/// of an older one never lands over a newer one.
	next_snapshot: u64,
	/// How many times the queue overflowed, and how many of those a walk that started after them
	/// has ended since: the queue does not hold every local change in between.
	overflows: u64,
	walked_overflows: u64,
	/// The directories this run created for a new local directory whose take-in a rename overtook,
	/// waiting to go to the trash once nothing can be sending into them ([`leftover`]).
	leftovers: Leftovers,
	stopped: bool,
}

/// What a walk found that taking back a saved lineage needs.
#[derive(Debug, Default)]
struct Found {
	/// Ordinary files at the saved paths.
	plain_files: HashSet<CaseKey>,
	/// The remote files the file placeholders stand for.
	placeholders: HashSet<Uuid>,
	/// Ordinary directories, which the walk does not descend into.
	plain_dirs: Vec<RootPath>,
}

/// Whether a walk of the root runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Walk {
	Idle,
	Running,
	/// Running, and another walk was asked for meanwhile.
	RunningAgain,
}

/// Locks `mutex`, recovering it from a panic in another holder: every change made under these
/// locks leaves the data whole.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
	mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl<D: Drive, L: LocalFs> LocalEngine<D, L> {
	/// An engine carrying the changes in `local` to `drive`, running its work on `runtime` and
	/// reporting to `callback`, keeping nothing across a restart. Nothing runs before
	/// [`LocalEngine::start`].
	pub fn new(
		drive: Arc<D>,
		local: Arc<L>,
		runtime: Handle,
		config: LocalConfig,
		callback: Arc<dyn CloudFilesCallback>,
	) -> Self {
		Self::build(drive, local, runtime, config, callback, None)
	}

	/// As [`LocalEngine::new`], keeping in the existing directory `records` what the engine needs
	/// across a restart: which remote file each path held, so a save made while the provider was
	/// not running is a new version of it, and the renames to put back on the drive.
	pub fn new_keeping_records(
		drive: Arc<D>,
		local: Arc<L>,
		runtime: Handle,
		config: LocalConfig,
		callback: Arc<dyn CloudFilesCallback>,
		records: PathBuf,
	) -> Self {
		Self::build(
			drive,
			local,
			runtime,
			config,
			callback,
			Some(Records::new(records)),
		)
	}

	fn build(
		drive: Arc<D>,
		local: Arc<L>,
		runtime: Handle,
		config: LocalConfig,
		callback: Arc<dyn CloudFilesCallback>,
		records: Option<Records>,
	) -> Self {
		let restore = if records.is_some() {
			Restore::Loading
		} else {
			Restore::Done
		};
		let clamp = |n: NonZeroUsize| n.get().clamp(1, Semaphore::MAX_PERMITS);
		Self {
			inner: Arc::new(Inner {
				drive,
				local,
				runtime,
				callback,
				config,
				state: Mutex::new(State {
					queue: Queue::new(
						config.max_queued.get(),
						WATCH_DEBOUNCE,
						WATCH_MAX_DEBOUNCE_BURST,
					),
					lineage: Lineage::default(),
					uploading: HashMap::new(),
					fresh: FreshUploads::default(),
					deletes: HashMap::new(),
					renames: Vec::new(),
					walk: Walk::Idle,
					walks_started: 0,
					last_complete_walk: 0,
					skips_reported: HashSet::new(),
					stalled: HashMap::new(),
					index: Index::default(),
					populated_report_due: false,
					listed_skips_reported: HashSet::new(),
					deferrals: HashMap::new(),
					hydration_stalls: HashMap::new(),
					unplaced: HashSet::new(),
					collect_dirty: true,
					walked_dirty: None,
					unkept: HashSet::new(),
					records_key: None,
					restore,
					rename_records: Vec::new(),
					next_record: 0,
					next_snapshot: 0,
					overflows: 0,
					walked_overflows: 0,
					leftovers: Leftovers::default(),
					stopped: false,
				}),
				wake: Notify::new(),
				room: Notify::new(),
				jobs: Arc::new(Semaphore::new(clamp(config.max_concurrent_jobs))),
				uploads: Semaphore::new(clamp(config.max_concurrent_uploads)),
				hydrations: Arc::new(Semaphore::new(clamp(config.max_concurrent_hydrations))),
				stop: Arc::new(AtomicBool::new(false)),
				tasks: Mutex::new(JoinSet::new()),
				hints: Arc::new(RemoteHints::default()),
				feed_wake: Arc::new(Notify::new()),
				taken_over: watch::Sender::new(false),
				walks_ended: watch::Sender::new(0),
				looks: watch::Sender::new(0),
				records,
				record_writes: AsyncMutex::new(()),
			}),
		}
	}

	/// Starts the engine: the scheduler, the change feed's anchor, then a walk of the whole root
	/// (which reconciles every populated directory with the cache), then a walk again on the safety
	/// net's interval; the feed of the cache's changes; and, once the first walk has listed the
	/// placeholders holding changes, the take-over of the edits a previous run left marked in the
	/// cache ([`unsent`]). The cache's own drain never runs.
	pub fn start(&self) {
		self.inner.spawn(Inner::schedule(Arc::clone(&self.inner)));
		self.inner
			.spawn(Inner::startup_and_safety_net(Arc::clone(&self.inner)));
		self.inner.spawn(remote::run_hints(Arc::clone(&self.inner)));
	}

	/// What the dispatcher tells the remote side: a directory populated, a read in flight, a read
	/// refused as out of date. Hand it to [`Dispatcher::with_hints`](crate::Dispatcher::with_hints).
	pub fn hints(&self) -> Arc<RemoteHints> {
		Arc::clone(&self.inner.hints)
	}

	/// How many items in the sync root hold what the drive does not have yet: every path that
	/// waits for, or runs, a look at a local change (an edit or a new file in the debounce or the
	/// settle wait, held by a writer, on its way up, backing off after a failure or parked, a new
	/// directory, the local side of a conflict), and every edit a previous run left in the cache
	/// that is not kept in the sync root yet and no such path stands for. Each item once. A path the
	/// watcher named counts until its look finds nothing to send, the provider's own writes'
	/// events included, so the count can be high for a moment while files download. A new
	/// directory counts with the items in it that events named; one a walk found (filled while the
	/// provider was stopped) counts once, whatever it holds, until it is on the drive.
	///
	/// `None` while the engine cannot know: before its first walk of the root and the take-over of
	/// the edits a previous run left in the cache, and after the queue overflowed until the walk
	/// that follows it ends.
	pub fn unsynced_count(&self) -> Option<u64> {
		self.count(false)
	}

	/// [`LocalEngine::unsynced_count`], or with `looked` `None` too while a path waits for, or
	/// runs, a look at a change no job has found to hold anything yet. Read under one lock with the
	/// queue, so it agrees with every job that ended before it.
	fn count(&self, looked: bool) -> Option<u64> {
		if !*self.inner.taken_over.borrow() {
			return None;
		}
		let state = lock(&self.inner.state);
		if state.walked_overflows != state.overflows {
			return None;
		}
		if looked && state.queue.local_unlooked() {
			return None;
		}
		let mut unkept = state.unkept.clone();
		let mut count = 0_u64;
		for path in state.queue.local_paths() {
			count = count.saturating_add(1);
			if let Some(seen) = state.lineage.get(path) {
				unkept.remove(&seen.stable);
			}
		}
		Some(count.saturating_add(u64::try_from(unkept.len()).unwrap_or(u64::MAX)))
	}

	/// [`LocalEngine::unsynced_count`] once a walk of the whole root that started after this call
	/// has ended and a job has looked at every path the walk or an event named, waiting for both at
	/// most `limit`; `None` past that, or when no walk that started since read the root and listed
	/// every directory in it. A write through a handle its writer keeps open raises nothing until
	/// the writer closes it or flushes it (measured on Windows 11: no watcher event, no callback,
	/// though the file is no longer in sync), so only a look at each placeholder finds an edit its
	/// editor still holds, and a walk looks at each. A path is only named until its job looks: an
	/// item the default ignore patterns match, or a file the provider's own convert raised an event
	/// on, holds nothing for the drive, so the count is taken once no path waits for its first look.
	/// Each path then counts for as long as its job has not carried its change; with every job slot
	/// held by jobs that last (more uploads than slots), a path named meanwhile is not looked at
	/// within `limit`, and the answer is `None`.
	pub async fn unsynced_count_after_walk(&self, limit: Duration) -> Option<u64> {
		let deadline = Instant::now() + limit;
		let target = lock(&self.inner.state).walks_started.wrapping_add(1);
		let mut walks = self.inner.walks_ended.subscribe();
		Inner::request_walk(&self.inner);
		timeout_at(deadline, walks.wait_for(|ended| *ended >= target))
			.await
			.ok()?
			.ok()?;
		if lock(&self.inner.state).last_complete_walk < target {
			// A directory it could not list may hold an edit nothing else says of.
			return None;
		}
		let mut looks = self.inner.looks.subscribe();
		let mut taken_over = self.inner.taken_over.subscribe();
		loop {
			// Marked seen before the count is read, so a change after it wakes the wait below.
			looks.borrow_and_update();
			taken_over.borrow_and_update();
			walks.borrow_and_update();
			if let Some(count) = self.count(true) {
				return Some(count);
			}
			let changed = async {
				select! {
					changed = looks.changed() => changed,
					changed = taken_over.changed() => changed,
					changed = walks.changed() => changed,
				}
			};
			timeout_at(deadline, changed).await.ok()?.ok()?;
		}
	}

	/// A watcher event or a close notification: queues its paths to be looked at.
	pub fn on_local_event(&self, event: LocalEvent) {
		match event {
			LocalEvent::Created(path) | LocalEvent::Modified(path) | LocalEvent::Removed(path) => {
				self.inner.push(&path, Delay::Debounce);
			}
			LocalEvent::Renamed { from, to } => {
				let temporary =
					lock(&self.inner.state)
						.lineage
						.follow_upload(&from, &to, Instant::now());
				if let Some(stable) = temporary {
					// A save's temporary file, uploaded and not converted yet, renamed into the place
					// of the file it saves: that file's job takes its bytes as a new version.
					veto::trash_transient(&self.inner, stable);
				}
				self.inner.push(&from, Delay::Debounce);
				self.inner.push(&to, Delay::Debounce);
			}
			LocalEvent::Closed(path) => self.inner.push(&path, Delay::Close),
			LocalEvent::Rescan => Inner::request_walk(&self.inner),
		}
	}

	/// A `NOTIFY_DELETE`: answered from a task within the platform's time limit.
	pub fn on_notify_delete<R: DeleteResponder>(&self, request: DeleteRequest, responder: R) {
		let inner = Arc::clone(&self.inner);
		self.inner
			.spawn(async move { veto::answer_delete(&inner, request, responder).await });
	}

	/// A `NOTIFY_RENAME`: answered from a task within the platform's time limit.
	pub fn on_notify_rename<R: RenameResponder>(&self, request: RenameRequest, responder: R) {
		let inner = Arc::clone(&self.inner);
		self.inner
			.spawn(async move { veto::answer_rename(&inner, request, responder).await });
	}

	/// A `NOTIFY_DEHYDRATE`: answered from a task, and the file dehydrated once allowed.
	pub fn on_notify_dehydrate<R: DehydrateResponder>(
		&self,
		request: DehydrateRequest,
		responder: R,
	) {
		let inner = Arc::clone(&self.inner);
		self.inner
			.spawn(async move { dehydrate::answer(&inner, request, responder).await });
	}

	/// A delete or rename the platform reports finished: confirms an allowed rename at once. A
	/// delete is still confirmed after its grace period, which an atomic save's new file needs.
	pub fn on_completion(&self, completed: Completed) {
		match completed {
			Completed::Renamed { to, .. } => {
				let now = Instant::now();
				let mut state = lock(&self.inner.state);
				for pending in &mut state.renames {
					if pending.settled() && pending.record.to.case_key() == to.case_key() {
						pending.due = now;
					}
				}
				drop(state);
				self.inner.wake.notify_one();
			}
			Completed::Deleted(path) => debug!(%path, "a delete completed"),
		}
	}

	/// Stops every task and waits for them to end. A request a task left unanswered is vetoed as
	/// [`Failure::Transient`](crate::Failure::Transient) when its responder drops, and a rename the
	/// drive was being changed for is put back on the drive, within a few seconds for all of them;
	/// one not put back by then, or by a stop that never got this far, is put back on the next
	/// start from its record when the engine keeps records. The lineage is saved, and an upload in progress is dropped, and
	/// the file is looked at again on the next start. A dehydration allowed but not yet done stays
	/// undone: its caller fails after the platform's time limit, and the file keeps its bytes.
	pub async fn shutdown(&self) {
		lock(&self.inner.state).stopped = true;
		self.inner.stop.store(true, Ordering::Relaxed);
		self.inner.drive.watch_changes(None).await;
		let mut tasks = mem::take(&mut *lock(&self.inner.tasks));
		tasks.abort_all();
		while tasks.join_next().await.is_some() {}
		if timeout(
			STOP_PUT_BACK_BUDGET,
			veto::put_back_interrupted(&self.inner),
		)
		.await
		.is_err()
		{
			warn!("putting back the renames a stop cut short ran out of time; the next start does");
		}
		self.inner.save_lineage().await;
		// A write a task left running ends first, and none comes after: the data directory may be
		// wiped next.
		if let Some(records) = self.inner.records.clone() {
			self.inner.blocking(move || records.close()).await;
		}
	}
}

impl<D: Drive, L: LocalFs> Drop for LocalEngine<D, L> {
	/// Aborts every task: they hold the engine's state, so a task waiting forever would keep it
	/// alive. Closes the records, waiting for a write under way (a file of at most a few
	/// megabytes), so none an aborted task left lands after the engine is gone.
	fn drop(&mut self) {
		self.inner.stop.store(true, Ordering::Relaxed);
		lock(&self.inner.tasks).abort_all();
		if let Some(records) = &self.inner.records {
			records.close();
		}
	}
}

impl<D: Drive, L: LocalFs> Inner<D, L> {
	/// Spawns `task` on the runtime, reaping the tasks that finished; nothing once stopped.
	fn spawn(&self, task: impl Future<Output = ()> + Send + 'static) {
		if lock(&self.state).stopped {
			return;
		}
		let mut tasks = lock(&self.tasks);
		while tasks.try_join_next().is_some() {}
		tasks.spawn_on(task, &self.runtime);
	}

	/// Runs `call` on the blocking pool with the local root: every [`LocalFs`] call goes through
	/// here, off the async workers.
	async fn fs<T: Send + 'static>(&self, call: impl FnOnce(&L) -> T + Send + 'static) -> T {
		let local = Arc::clone(&self.local);
		match self.runtime.spawn_blocking(move || call(&local)).await {
			Ok(out) => out,
			Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
			// The runtime is shutting down, and this task with it.
			Err(_) => std::future::pending().await,
		}
	}

	/// Whether `path` is the target of a rename that is being decided or was allowed and has not
	/// happened as the drive has it. Nothing may read it meanwhile: the path may hold a file made
	/// only to be replaced, as `ReplaceFileW` reserves its backup name, and the rename over it fails
	/// while any other handle on it is open, even one for reading attributes only (measured,
	/// Windows 11 in the VM). A rename this run vetoed is one the platform never makes: its target
	/// is read as any other path while the drive is put back, however long that takes.
	fn renaming_onto(&self, path: &RootPath) -> bool {
		self.renaming_onto_as(path).is_some()
	}

	/// The item renamed onto `path` when [`Inner::renaming_onto`] holds.
	fn renaming_onto_as(&self, path: &RootPath) -> Option<PlaceholderIdentity> {
		let key = path.case_key();
		let state = lock(&self.state);
		state
			.rename_records
			.iter()
			.find(|record| {
				!record.confirmed
					&& record.to.case_key() == key
					&& !state
						.renames
						.iter()
						.any(|pending| pending.record.id == record.id && pending.vetoed())
			})
			.map(|record| record.identity)
	}

	/// Reports `report` to the app, keeping the index in step with what the local side did.
	fn report(&self, report: LocalReport) {
		{
			let mut state = lock(&self.state);
			let index = &mut state.index;
			match &report {
				LocalReport::Uploaded { path, identity }
				| LocalReport::Adopted { path, identity }
				| LocalReport::DirCreated { path, identity } => {
					if let Some(uuid) = populated_uuid(*identity) {
						index.insert_if_populated(path, uuid);
					}
				}
				LocalReport::Moved { from, to } => index.rebase(from, to),
				LocalReport::Trashed { path } | LocalReport::RemoteOutlivedDelete { path } => {
					index.remove_under(path);
				}
				LocalReport::Conflict { kept_as, .. } => index.remove_under(kept_as),
				LocalReport::Skipped { .. }
				| LocalReport::Busy { .. }
				| LocalReport::Parked { .. }
				| LocalReport::Vetoed { .. }
				| LocalReport::Undone { .. }
				| LocalReport::Damaged { .. }
				| LocalReport::Freed { .. }
				| LocalReport::KeptDownloaded { .. }
				| LocalReport::Remote(_) => {}
			}
		}
		if let LocalReport::Adopted {
			identity: PlaceholderIdentity::File { stable, version },
			..
		} = &report
		{
			// Its bytes were just hashed equal to the version's.
			self.hints.verified(FileVersion {
				stable: *stable,
				version: *version,
			});
		}
		if let LocalReport::Skipped { path, reason } = &report {
			if *reason == LocalSkip::DefaultIgnored {
				debug!(%path, "skipped an item the default ignore patterns match");
				return;
			}
			if !lock(&self.state).skips_reported.insert(path.case_key()) {
				return;
			}
		}
		debug!(?report, "local change");
		self.callback.on_local_report(&report);
	}

	/// Queues a look at `path` after `delay`; an overflowing queue is dropped for a walk.
	fn push(self: &Arc<Self>, path: &RootPath, delay: Delay) {
		self.push_kind(path, delay, Kind::Local);
	}

	/// Queues a `kind` job on `path` after `delay`; an overflowing queue is dropped for a walk.
	fn push_kind(self: &Arc<Self>, path: &RootPath, delay: Delay, kind: Kind) {
		let now = Instant::now();
		let admit = lock(&self.state).queue.push_kind(path, now, delay, kind);
		if admit == Admit::Overflow {
			self.overflowed();
		}
		self.wake.notify_one();
	}

	/// Drops the queue, which had no room for a path, and walks the root instead.
	fn overflowed(self: &Arc<Self>) {
		warn!("the local change queue is full; walking the sync root instead");
		{
			let mut state = lock(&self.state);
			state.queue.clear();
			state.overflows = state.overflows.wrapping_add(1);
		}
		Self::request_walk(self);
	}

	/// Starts a walk of the root, or another one after the walk running.
	fn request_walk(self: &Arc<Self>) {
		let mut state = lock(&self.state);
		match state.walk {
			Walk::Idle => state.walk = Walk::Running,
			Walk::Running | Walk::RunningAgain => {
				state.walk = Walk::RunningAgain;
				return;
			}
		}
		drop(state);
		self.spawn(Self::run_walks(Arc::clone(self)));
	}

	/// Walks the root, again for as long as another walk was asked for meanwhile; the walk state
	/// is [`Walk::Running`] when it starts.
	async fn run_walks(self: Arc<Self>) {
		loop {
			self.walk().await;
			if !self.walk_again() {
				return;
			}
		}
	}

	/// Ends a walk: whether another one was asked for while it ran, which then runs.
	fn walk_again(&self) -> bool {
		let mut state = lock(&self.state);
		let again = state.walk == Walk::RunningAgain;
		state.walk = if again { Walk::Running } else { Walk::Idle };
		again
	}

	/// The scheduler: starts each due path's job as a job permit frees up, and each pending delete
	/// or rename confirmation as it falls due.
	async fn schedule(self: Arc<Self>) {
		loop {
			let woken = self.wake.notified();
			pin!(woken);
			woken.as_mut().enable();
			let now = Instant::now();
			let (paths, deletes, renames, next) = {
				let mut state = lock(&self.state);
				let free = self.jobs.available_permits();
				let hydrations = self.hydrations.available_permits();
				let paths = state.queue.take_due(now, free, hydrations);
				// With every permit taken, the paths still due wait for a job to end, which wakes
				// this loop, rather than for a time already past.
				let queue_next = if paths.len() < free {
					let hydrations_left = paths
						.iter()
						.filter(|(_, kinds)| *kinds == Kinds::of(Kind::Hydration))
						.count() < hydrations;
					state.queue.next_due(hydrations_left)
				} else {
					None
				};
				let deletes: Vec<PendingDelete> = state
					.deletes
					.values_mut()
					.filter(|pending| pending.stage == DeleteStage::Waiting && pending.due <= now)
					.map(PendingDelete::start)
					.collect();
				let (due, waiting): (Vec<_>, Vec<_>) = mem::take(&mut state.renames)
					.into_iter()
					.partition(|pending| pending.settled() && pending.due <= now);
				state.renames = waiting;
				let next = [
					queue_next,
					state
						.deletes
						.values()
						.filter(|pending| pending.stage == DeleteStage::Waiting)
						.map(|pending| pending.due)
						.min(),
					state
						.renames
						.iter()
						.filter(|pending| pending.settled())
						.map(|pending| pending.due)
						.min(),
				]
				.into_iter()
				.flatten()
				.min();
				(paths, deletes, due, next)
			};
			for (path, kinds) in paths {
				let hydration = if kinds == Kinds::of(Kind::Hydration) {
					match Arc::clone(&self.hydrations).try_acquire_owned() {
						Ok(permit) => Some(permit),
						// A job running more kinds than the hydration took one meanwhile.
						Err(_) => {
							lock(&self.state).queue.put_back(&path, now);
							continue;
						}
					}
				} else {
					None
				};
				let Ok(permit) = Arc::clone(&self.jobs).try_acquire_owned() else {
					// Only the scheduler takes job permits, and it took no more paths than were
					// free; put it back to be safe.
					lock(&self.state).queue.put_back(&path, now);
					self.wake.notify_one();
					continue;
				};
				let inner = Arc::clone(&self);
				self.spawn(async move { inner.run_job(path, kinds, permit, hydration).await });
			}
			for pending in deletes {
				let inner = Arc::clone(&self);
				self.spawn(async move { veto::confirm_delete(&inner, pending).await });
			}
			for pending in renames {
				let inner = Arc::clone(&self);
				self.spawn(async move { veto::confirm_rename(&inner, pending).await });
			}
			match next {
				Some(at) => {
					select! {
						() = woken => {}
						() = sleep_until(at) => {}
					}
				}
				None => woken.await,
			}
		}
	}

	/// Runs the `kinds` of job on `path` and records how they ended; `hydration` is the hydration
	/// permit the scheduler took for a path waiting for the hydration job alone.
	async fn run_job(
		self: Arc<Self>,
		path: RootPath,
		kinds: Kinds,
		permit: OwnedSemaphorePermit,
		hydration: Option<OwnedSemaphorePermit>,
	) {
		let (outcome, owed) = self.run_kinds(&path, kinds, hydration).await;
		let now = Instant::now();
		let reports = {
			let mut state = lock(&self.state);
			let reports = state
				.queue
				.finish(&path, &outcome, owed, now, self.config.settle);
			if outcome == Outcome::Done {
				state.queue.unpark_unreachable(now);
			}
			reports
		};
		self.looks
			.send_modify(|looks| *looks = looks.wrapping_add(1));
		if let Outcome::Blocked(parent) = &outcome {
			self.push(parent, Delay::Now);
		}
		for report in reports {
			self.report(report);
		}
		drop(permit);
		self.wake.notify_one();
		self.room.notify_waiters();
	}

	/// The kinds of job on one path, in order: the directory's reconcile, then the remote change,
	/// then the local one, which the remote job may hand the path to, and last the hydration job,
	/// only once every other kind finished, so the local side always goes first. Ends at the first
	/// that does not finish, with what is still owed; a reconcile that left something for another
	/// job to look at again does not hold back the remote and local jobs, which may be that job.
	async fn run_kinds(
		self: &Arc<Self>,
		path: &RootPath,
		mut kinds: Kinds,
		hydration: Option<OwnedSemaphorePermit>,
	) -> (Outcome, Kinds) {
		let mut deferred = None;
		if kinds.has(Kind::Children) {
			match remote::children(self, path).await {
				Outcome::Done => kinds = kinds.without(Kind::Children),
				again @ (Outcome::Again(_) | Outcome::Wait(_)) => deferred = Some(again),
				outcome => return (outcome, kinds),
			}
		}
		if kinds.has(Kind::Remote) {
			kinds = match remote::apply(self, path).await {
				remote::Step::Ended(Outcome::Done) => kinds.without(Kind::Remote),
				remote::Step::Ended(outcome) => return (outcome, kinds),
				remote::Step::Local => {
					lock(&self.state).queue.runs_local(path);
					kinds.without(Kind::Remote).with(Kind::Local)
				}
			};
		}
		if kinds.has(Kind::Local) {
			let outcome = jobs::look(self, path).await;
			if outcome != Outcome::Done {
				return (outcome, kinds);
			}
			kinds = kinds.without(Kind::Local);
		}
		if let Some(again) = deferred {
			return (
				again,
				kinds.intersect(Kinds::of(Kind::Children).with(Kind::Hydration)),
			);
		}
		if kinds.has(Kind::Hydration) {
			let outcome = hydration::run(self, path, hydration).await;
			if outcome != Outcome::Done {
				return (outcome, Kinds::of(Kind::Hydration));
			}
		}
		(Outcome::Done, Kinds::NONE)
	}

	/// The change feed's anchor, then the first walk, then the take-over of the edits the cache
	/// holds marked, in a task of its own, and a walk on every safety-net interval, scaled by what
	/// the last walk cost. The walks never wait for the queue: they are what finds an event the
	/// watcher lost.
	async fn startup_and_safety_net(self: Arc<Self>) {
		self.restore_records().await;
		if self.records.is_some() {
			self.spawn(Self::run_lineage_saver(Arc::clone(&self)));
		}
		let anchor = remote::start_anchor(&self).await;
		self.spawn(remote::run_feed(Arc::clone(&self), anchor));
		self.spawn(unsent::take_over(Arc::clone(&self)));
		let mut cost = match self.walk_now().await {
			Some(cost) => cost,
			None => {
				// A walk asked for while the records were read may have started before them, and
				// would not take back the saved lineage: one more runs after it.
				Self::request_walk(&self);
				Duration::ZERO
			}
		};
		loop {
			sleep(safety_net_interval(WATCH_SAFETY_NET, cost)).await;
			if let Some(walked) = self.walk_now().await {
				cost = walked;
			}
		}
	}

	/// Walks the root now and says how long it took; `None` when a walk was running already.
	async fn walk_now(self: &Arc<Self>) -> Option<Duration> {
		{
			let mut state = lock(&self.state);
			if state.walk != Walk::Idle {
				return None;
			}
			state.walk = Walk::Running;
		}
		let started = Instant::now();
		self.walk().await;
		let cost = started.elapsed();
		if self.walk_again() {
			self.spawn(Self::run_walks(Arc::clone(self)));
		}
		Some(cost)
	}

	/// Walks the whole root, queuing every item that needs a job: an ordinary item, a placeholder
	/// not in sync or holding modified bytes, a detached or unreadable placeholder. It descends into
	/// directory placeholders only (an ordinary directory's job queues its children), and waits for
	/// room when the queue is full, so its memory is bounded by its own frontier.
	///
	/// It also rebuilds the index of the placeholders in populated directories, and once it is
	/// done, reconciles each populated directory with the cache. The first walk to finish records
	/// the file placeholders holding changes for the take-over of the cache's marked edits.
	async fn walk(self: &Arc<Self>) {
		let number = {
			let mut state = lock(&self.state);
			state.walks_started += 1;
			state.walks_started
		};
		if self.walk_root().await {
			let mut state = lock(&self.state);
			state.last_complete_walk = state.last_complete_walk.max(number);
		}
		self.walks_ended.send_modify(|ended| *ended += 1);
	}

	/// The walk itself: see [`Inner::walk`]. Whether it read the root and listed every directory
	/// it came to.
	async fn walk_root(self: &Arc<Self>) -> bool {
		let (unplaced, restoring, overflows) = {
			let mut state = lock(&self.state);
			state.stalled.clear();
			state.hydration_stalls.clear();
			let restoring: Option<HashSet<CaseKey>> = match &state.restore {
				Restore::Walking(saved) => Some(saved.keys().cloned().collect()),
				Restore::Done | Restore::Loading => None,
			};
			(mem::take(&mut state.unplaced), restoring, state.overflows)
		};
		// While a saved lineage waits to be taken back: the ordinary files at its paths, the remote
		// files any placeholder stands for, and the ordinary directories, which hold placeholders
		// moved into them.
		let mut found = Found::default();
		let mut built = Index::default();
		let mut populated = Vec::new();
		let mut dirty = unsent::Dirty::new();
		let (root_populated, root_pin) = match self.inspect(&RootPath::root()).await {
			Ok(LocalEntry::Placeholder {
				identity: Ok(PlaceholderIdentity::Root { root }),
				state,
				..
			}) => {
				built.insert(&RootPath::root(), root);
				let filled = !state.partial();
				if filled {
					built.mark_populated(root);
					populated.push(RootPath::root());
				}
				(filled, state.pin())
			}
			Ok(_) => (false, PinAttr::Unspecified),
			// Taken as not populated, the root would leave its placeholders out of the index the
			// walk builds, and the remote side would then take them for unplaced: the walk ends,
			// the index it had stays, and the next walk tries again.
			Err(error) => {
				warn!(%error, "the walk could not read the sync root; walking it later");
				lock(&self.state).unplaced.extend(unplaced);
				return false;
			}
		};
		let mut complete = true;
		// Each directory with whether it is populated and the pin it passes down.
		let mut dirs = vec![(RootPath::root(), root_populated, root_pin)];
		let mut seen = 0usize;
		while let Some((dir, dir_populated, dir_pin)) = dirs.pop() {
			let listed = {
				let dir = dir.clone();
				self.fs(move |local| local.list_dir(&dir)).await
			};
			let children = match listed {
				Ok(children) => children,
				Err(error) => {
					warn!(dir = %path_tag(&dir), %error, "the walk could not list a directory");
					complete = false;
					continue;
				}
			};
			for child in children {
				let Ok(path) = child else {
					self.report(LocalReport::Skipped {
						path: dir.clone(),
						reason: LocalSkip::Unrepresentable,
					});
					continue;
				};
				if let Some(identity @ PlaceholderIdentity::File { stable, .. }) =
					self.renaming_onto_as(&path)
				{
					// Never read before the rename happened: placed in the index as the rename
					// leaves it, and looked at once the rename settles. A directory's rename
					// replaces nothing, so its target is walked as any other.
					if dir_populated && let Some(uuid) = populated_uuid(identity) {
						built.insert(&path, uuid);
					}
					if restoring.is_some() {
						found.placeholders.insert(stable);
					}
					continue;
				}
				let entry = {
					let path = path.clone();
					self.fs(move |local| local.inspect(&path)).await
				};
				if dir_populated
					&& let Ok(LocalEntry::Placeholder {
						identity: Ok(identity),
						..
					}) = &entry && let Some(uuid) = populated_uuid(*identity)
				{
					if let Some(other) = built.path_of(uuid).cloned() {
						// Two placeholders of one item: the remote job of each finds which is
						// where the drive has it, and the other goes.
						warn!(item = %path_tag(&path), other = %path_tag(&other), "two placeholders of one item");
						self.push_waiting_for_room(&other, Kind::Remote).await;
						self.push_waiting_for_room(&path, Kind::Remote).await;
					}
					built.insert(&path, uuid);
				}
				let queue = match entry {
					Ok(LocalEntry::Missing) => None,
					Err(error) => {
						warn!(item = %path_tag(&path), %error, "the walk could not read an item");
						complete = false;
						None
					}
					Ok(LocalEntry::Placeholder {
						kind: LocalKind::Dir { .. },
						identity,
						state,
						..
					}) => {
						let filled = dir_populated && !state.partial();
						if filled && let Ok(PlaceholderIdentity::Dir { uuid }) = identity {
							built.mark_populated(uuid);
							populated.push(path.clone());
						}
						let pin = hydration::effective(state.pin(), dir_pin);
						dirs.push((path.clone(), filled, pin));
						if !state.in_sync() {
							Some(Kind::Local)
						} else if state.pin() == PinAttr::Unpinned {
							// Its unpin not yet passed down before a restart.
							Some(Kind::Hydration)
						} else if pin == PinAttr::Pinned && state.partial() {
							// Pinned and never populated: its job populates it.
							Some(Kind::Hydration)
						} else {
							None
						}
					}
					Ok(LocalEntry::Placeholder {
						kind,
						identity: Ok(PlaceholderIdentity::File { stable, version }),
						state,
						..
					}) => {
						if restoring.is_some() {
							found.placeholders.insert(stable);
						}
						// Known after a restart too: a save over it is a new version of it.
						lock(&self.state)
							.lineage
							.remember(&path, stable, version, Instant::now());
						if !state.in_sync() || state.modified_bytes() > 0 {
							dirty.insert(stable);
							Some(Kind::Local)
						} else if hydration::wanted(
							kind,
							state,
							hydration::effective(state.pin(), dir_pin),
						) || unplaced.contains(&stable)
						{
							// Left partly on disk before a restart, pinned and not all on disk, or
							// hinted while unplaced.
							Some(Kind::Hydration)
						} else {
							None
						}
					}
					Ok(LocalEntry::Plain { kind, .. }) => {
						if let Some(keys) = &restoring {
							match kind {
								LocalKind::File(_) if keys.contains(&path.case_key()) => {
									found.plain_files.insert(path.case_key());
								}
								LocalKind::File(_) => {}
								LocalKind::Dir { .. } => found.plain_dirs.push(path.clone()),
							}
						}
						Some(Kind::Local)
					}
					Ok(_) => Some(Kind::Local),
				};
				if let Some(kind) = queue {
					self.push_waiting_for_room(&path, kind).await;
				}
				seen += 1;
				if seen.is_multiple_of(256) {
					task::yield_now().await;
				}
			}
		}
		{
			let mut state = lock(&self.state);
			state.index.replace(built);
			state.walked_overflows = overflows;
			if mem::take(&mut state.collect_dirty) {
				state.walked_dirty = Some(dirty);
			}
		}
		if restoring.is_some() {
			self.take_back_lineage(found, complete).await;
		}
		self.populated_changed();
		for dir in populated {
			self.push_waiting_for_room(&dir, Kind::Children).await;
		}
		complete
	}

	/// Takes back the saved lineage once a walk found what the root holds: a saved path claims the
	/// ordinary file now there only when no placeholder of its remote file is left anywhere in the
	/// root, ordinary directories included. Then ordinary files at its paths are taken in. After a
	/// walk that could not list every directory (`complete` false), or a directory here that cannot
	/// be listed, a placeholder of it may be left unseen: no path claims anything, and a save made
	/// while the provider was stopped keeps both sides.
	async fn take_back_lineage(&self, mut found: Found, complete: bool) {
		let candidates: Vec<SavedSeen> = match &lock(&self.state).restore {
			Restore::Walking(saved) => saved
				.values()
				.filter(|seen| {
					found.plain_files.contains(&seen.path.case_key())
						&& !found.placeholders.contains(&seen.stable)
				})
				.cloned()
				.collect(),
			Restore::Done | Restore::Loading => return,
		};
		let seen_all = complete
			&& (candidates.is_empty()
				|| self
					.find_placeholders_under(
						mem::take(&mut found.plain_dirs),
						&mut found.placeholders,
					)
					.await);
		let now = Instant::now();
		let mut state = lock(&self.state);
		state.restore = Restore::Done;
		if !seen_all {
			warn!("not every directory could be listed; no saved path claims a file");
			return;
		}
		for seen in candidates {
			if !found.placeholders.contains(&seen.stable) {
				debug!(path = %seen.path, "a saved path claims the ordinary file now there");
				state.lineage.restore(seen, now);
			}
		}
	}

	/// Adds the remote files every file placeholder under the ordinary directories `dirs` stands for
	/// to `placeholders`: the walk does not descend into them. Whether it listed every one.
	async fn find_placeholders_under(
		&self,
		mut dirs: Vec<RootPath>,
		placeholders: &mut HashSet<Uuid>,
	) -> bool {
		let mut complete = true;
		while let Some(dir) = dirs.pop() {
			let listed = {
				let dir = dir.clone();
				self.fs(move |local| local.list_dir(&dir)).await
			};
			let children = match listed {
				Ok(children) => children,
				Err(error) => {
					warn!(dir = %path_tag(&dir), %error, "listing an ordinary directory for placeholders failed");
					complete = false;
					continue;
				}
			};
			for path in children.into_iter().flatten() {
				if self.renaming_onto(&path) {
					continue;
				}
				match self.inspect(&path).await {
					Ok(LocalEntry::Placeholder {
						identity: Ok(PlaceholderIdentity::File { stable, .. }),
						..
					}) => {
						placeholders.insert(stable);
					}
					Ok(
						LocalEntry::Placeholder {
							kind: LocalKind::Dir { .. },
							..
						}
						| LocalEntry::Plain {
							kind: LocalKind::Dir { .. },
							..
						},
					) => dirs.push(path),
					Ok(_) => {}
					Err(error) => {
						warn!(item = %path_tag(&path), %error, "reading an item for placeholders failed");
						complete = false;
					}
				}
			}
		}
		complete
	}

	/// Queues a `kind` job on each of `paths` from a task of its own, which waits for room in the
	/// queue as the walk does: a job that waited for room itself would hold the job slot the jobs
	/// it queued need to make room, and with every slot so held, no job would ever end.
	fn queue_waiting_for_room(self: &Arc<Self>, paths: Vec<RootPath>, kind: Kind) {
		if paths.is_empty() {
			return;
		}
		let inner = Arc::clone(self);
		self.spawn(async move {
			for path in &paths {
				inner.push_waiting_for_room(path, kind).await;
			}
		});
	}

	/// The items `listed` in the directory `dir` that are paths, each other reported skipped as a
	/// name the provider cannot represent, as the walk reports it.
	fn representable(
		&self,
		dir: &RootPath,
		listed: Vec<Result<RootPath, RootPathError>>,
	) -> Vec<RootPath> {
		listed
			.into_iter()
			.filter_map(|child| match child {
				Ok(path) => Some(path),
				Err(error) => {
					debug!(%dir, %error, "an item that cannot be a path was left");
					self.report(LocalReport::Skipped {
						path: dir.clone(),
						reason: LocalSkip::Unrepresentable,
					});
					None
				}
			})
			.collect()
	}

	/// Dehydrates the clean placeholder `identity` at `path`, a pin refused or overridden as
	/// `if_pinned` says; a version so dehydrated is checked again when it is next all on disk.
	async fn dehydrate_clean(
		&self,
		path: &RootPath,
		identity: PlaceholderIdentity,
		if_pinned: IfPinned,
	) -> Result<(), LocalError> {
		let target = path.clone();
		let dehydrated = self
			.fs(move |local| local.dehydrate_clean(&target, identity, if_pinned))
			.await;
		if dehydrated.is_ok()
			&& let Some(version) = FileVersion::of(identity)
		{
			self.hints.forget_verified(version);
		}
		dehydrated
	}

	/// Queues a `kind` job on `path` at once, waiting while the queue is full. Never from a job:
	/// see [`Inner::queue_waiting_for_room`].
	async fn push_waiting_for_room(self: &Arc<Self>, path: &RootPath, kind: Kind) {
		loop {
			let room = self.room.notified();
			pin!(room);
			room.as_mut().enable();
			let admit = lock(&self.state)
				.queue
				.push_kind(path, Instant::now(), Delay::Now, kind);
			if admit == Admit::Queued {
				self.wake.notify_one();
				return;
			}
			room.await;
		}
	}
}

/// Why a record could not be saved.
#[derive(Debug, thiserror::Error)]
enum RecordError {
	/// The root the records belong to is not read yet, or could not be.
	#[error("the sync root the records belong to is not known")]
	RootUnknown,
	#[error(transparent)]
	Write(#[from] WriteError),
}

/// Marks the lineage changed again when dropped armed: a save that took the changes and did not
/// write them, failed or stopped, leaves them for the next one.
struct Unsaved<'a>(&'a Mutex<State>, bool);

impl Drop for Unsaved<'_> {
	fn drop(&mut self) {
		if self.1 {
			lock(self.0).lineage.mark_changed();
		}
	}
}

impl<D: Drive, L: LocalFs> Inner<D, L> {
	/// Runs `call` on the blocking pool.
	async fn blocking<T: Send + 'static>(&self, call: impl FnOnce() -> T + Send + 'static) -> T {
		match self.runtime.spawn_blocking(call).await {
			Ok(out) => out,
			Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
			// The runtime is shutting down, and this task with it.
			Err(_) => std::future::pending().await,
		}
	}

	/// Reads back what a previous run saved for the root as it is now: the renames it did not
	/// settle, queued to be put back at once, and the lineage, which the first walk takes back.
	/// A root that cannot be read is read again, backing off, for as long as it takes: until then,
	/// no ordinary file is taken in and every rename is vetoed, as its record cannot be saved. A
	/// root that is not its placeholder keeps nothing across a restart in this run, and still
	/// vetoes every rename.
	async fn restore_records(&self) {
		let Some(records) = self.records.clone() else {
			return;
		};
		let mut attempts = 0_u32;
		let key = loop {
			match self.inspect(&RootPath::root()).await {
				Ok(LocalEntry::Placeholder {
					identity: Ok(PlaceholderIdentity::Root { root }),
					id,
					..
				}) => break Some(RootKey { root, dir: id }),
				Ok(entry) => {
					warn!(
						?entry,
						"the sync root is not its placeholder; nothing is kept across a restart"
					);
					break None;
				}
				Err(error) => {
					attempts = attempts.saturating_add(1);
					warn!(%error, "reading the sync root for its records failed; reading it again");
					sleep(watch_backoff(attempts).unwrap_or(RESTORE_AGAIN_MAX)).await;
				}
			}
		};
		let Some(key) = key else {
			lock(&self.state).restore = Restore::Done;
			return;
		};
		let first_id = lock(&self.state).next_record;
		let (lineage, renames) = self
			.blocking(move || {
				(
					records.load_lineage(key),
					records.load_renames(key, first_id),
				)
			})
			.await;
		if !renames.is_empty() {
			debug!(
				count = renames.len(),
				"putting back the renames a previous run left"
			);
		}
		let mut state = lock(&self.state);
		state.records_key = Some(key);
		state.next_record =
			first_id.saturating_add(u64::try_from(renames.len()).unwrap_or(u64::MAX));
		for record in renames {
			state.renames.push(veto::restored_rename(record.clone()));
			state.rename_records.push(record);
		}
		state.restore = if lineage.is_empty() {
			Restore::Done
		} else {
			Restore::Walking(
				lineage
					.into_iter()
					.map(|seen| (seen.path.case_key(), seen))
					.collect(),
			)
		};
		drop(state);
		self.wake.notify_one();
	}

	/// Records `record`, numbered, and saves the renames, before the drive is changed for it: an
	/// error drops it again, and the rename is vetoed.
	async fn add_rename_record(
		&self,
		mut record: RenameRecord,
	) -> Result<RenameRecord, RecordError> {
		{
			let mut state = lock(&self.state);
			if self.records.is_some() && state.records_key.is_none() {
				return Err(RecordError::RootUnknown);
			}
			record.id = state.next_record;
			state.next_record = state.next_record.wrapping_add(1);
			state.rename_records.push(record.clone());
		}
		if let Err(error) = self.save_renames().await {
			let id = record.id;
			lock(&self.state)
				.rename_records
				.retain(|held| held.id != id);
			return Err(error);
		}
		Ok(record)
	}

	/// The rename `id` happened as the drive has it: its record goes with the next lineage save.
	fn confirm_rename_record(&self, id: u64) {
		let mut state = lock(&self.state);
		for record in &mut state.rename_records {
			if record.id == id {
				record.confirmed = true;
			}
		}
	}

	/// The rename `id` is settled: its record goes.
	async fn drop_rename_record(&self, id: u64) {
		lock(&self.state)
			.rename_records
			.retain(|record| record.id != id);
		if let Err(error) = self.save_renames().await {
			warn!(%error, "saving the renames after one settled failed; the next start finds it settled");
		}
	}

	/// Saves every rename record, when the engine keeps records and knows its root.
	async fn save_renames(&self) -> Result<(), RecordError> {
		let Some(records) = self.records.clone() else {
			return Ok(());
		};
		let _write = self.record_writes.lock().await;
		let (key, snapshot, renames) = {
			let mut state = lock(&self.state);
			let snapshot = Snapshot(state.next_snapshot);
			state.next_snapshot = state.next_snapshot.wrapping_add(1);
			(state.records_key, snapshot, state.rename_records.clone())
		};
		let key = key.ok_or(RecordError::RootUnknown)?;
		self.blocking(move || records.save_renames(key, snapshot, &renames))
			.await
			.map_err(RecordError::from)
	}

	/// Saves the lineage when it changed, once the start took back what the last run saved, then
	/// drops the records of the renames confirmed before it was saved.
	async fn save_lineage(&self) {
		let Some(records) = self.records.clone() else {
			return;
		};
		let write = self.record_writes.lock().await;
		let (key, saved, confirmed) = {
			let mut state = lock(&self.state);
			let (Restore::Done, Some(key)) = (&state.restore, state.records_key) else {
				return;
			};
			let saved = state.lineage.take_changed().then(|| {
				let snapshot = Snapshot(state.next_snapshot);
				state.next_snapshot = state.next_snapshot.wrapping_add(1);
				(snapshot, state.lineage.saved())
			});
			let confirmed: Vec<u64> = state
				.rename_records
				.iter()
				.filter(|record| record.confirmed)
				.map(|record| record.id)
				.collect();
			(key, saved, confirmed)
		};
		if let Some((snapshot, saved)) = saved {
			let mut unsaved = Unsaved(&self.state, true);
			let held = records.clone();
			match self
				.blocking(move || held.save_lineage(key, snapshot, &saved))
				.await
			{
				Ok(()) => unsaved.1 = false,
				Err(error) => {
					warn!(%error, "saving the lineage failed; trying again");
					return;
				}
			}
		}
		drop(write);
		if confirmed.is_empty() {
			return;
		}
		lock(&self.state)
			.rename_records
			.retain(|record| !confirmed.contains(&record.id));
		if let Err(error) = self.save_renames().await {
			warn!(%error, "saving the renames after the lineage failed");
		}
	}

	/// Saves the lineage a moment after it changes, for as long as the engine runs.
	async fn run_lineage_saver(self: Arc<Self>) {
		loop {
			sleep(LINEAGE_SAVE_DELAY).await;
			self.save_lineage().await;
		}
	}
}

#[cfg(test)]
mod tests;
