//! The core's entry point for the OS layer: each Cloud Files callback becomes one call here,
//! which returns at once and answers the request from a task on the provider's runtime.

use std::{
	collections::HashMap,
	mem,
	num::NonZeroUsize,
	sync::{
		Arc, Mutex, MutexGuard, OnceLock, PoisonError,
		atomic::{AtomicBool, AtomicU64, Ordering},
	},
};

use filen_mobile_native_cache::{abort::FfiAbortController, auth::FilenMobileCacheState};
use tokio::{
	runtime::Handle,
	select,
	sync::{OwnedSemaphorePermit, Semaphore},
	task::JoinSet,
};
use tracing::{debug, warn};

use crate::{
	drive::Drive,
	failure::{Answer, Failure},
	hydrate::{Verification, hydrate},
	identity::PlaceholderIdentity,
	local::{FileVersion, LocalReport, RemoteHints},
	log_safe,
	os::{
		AlignedChunk, CancelRequest, DataResponder, FetchDataRequest, OsError, PopulateRequest,
		PopulateResponder, TransferKey,
	},
	populate::{SkippedItem, populate},
	sealed,
};

/// Receives what the provider reports outside any one request.
pub trait CloudFilesCallback: Send + Sync + 'static {
	/// A population of `directory` placed no placeholder for `items`, each with its reason: the
	/// default for the open decision on names Windows cannot hold is to skip and report them.
	fn on_skipped_items(&self, directory: PlaceholderIdentity, items: &[SkippedItem]);

	/// The local side carried a change to the drive, or left an item alone and says why.
	fn on_local_report(&self, report: &LocalReport);
}

/// How the dispatcher runs requests.
#[derive(Debug, Clone, Copy)]
pub struct DispatchConfig {
	/// How many range reads run at once; clamped to [`Semaphore::MAX_PERMITS`]. Default 8.
	pub max_concurrent_reads: NonZeroUsize,
}

impl Default for DispatchConfig {
	fn default() -> Self {
		Self {
			max_concurrent_reads: NonZeroUsize::new(8).expect("8 is not zero"),
		}
	}
}

/// Answers the OS's requests from the drive. Each `on_*` call returns at once and never blocks,
/// so the OS layer may call it from a Cloud Files callback thread; the work runs on the runtime
/// handed to [`Dispatcher::new`].
pub struct Dispatcher<D: Drive> {
	// Reason for the Arc: the OS callback threads and the tasks they spawn share it.
	inner: Arc<Inner<D>>,
}

/// The dispatcher the provider runs, over the cache.
pub type CacheDispatcher = Dispatcher<FilenMobileCacheState>;

struct Inner<D> {
	// Reason for the Arc: the provider shares the cache with what else it runs on it.
	drive: Arc<D>,
	runtime: Handle,
	// Reason for the Arc: the provider keeps its own handle to report through.
	callback: Arc<dyn CloudFilesCallback>,
	// Reason for the Arc: permits move into the tasks (`acquire_owned`).
	reads: Arc<Semaphore>,
	/// The reads in progress, by the OS's transfer key, so a cancel finds them. A std Mutex: held
	/// only to add, remove or abort entries, never across an await.
	in_flight: Mutex<HashMap<TransferKey, Vec<InFlight>>>,
	next_id: AtomicU64,
	/// Every task spawned; a std Mutex, held only to spawn into or take the set.
	tasks: Mutex<JoinSet<()>>,
	/// Set by [`Dispatcher::close`], read and written only under `tasks`' lock: no task is spawned
	/// once it is set.
	closed: AtomicBool,
	/// Who to tell of populations, reads and out-of-date placeholders, once set.
	hints: OnceLock<Arc<RemoteHints>>,
}

/// One read in progress: the range the OS required, as it sent it, and the controller that
/// aborts the read.
struct InFlight {
	id: u64,
	offset: i64,
	len: i64,
	controller: FfiAbortController,
	/// Set before an abort the OS did not ask for by name, so the read fails rather than ends
	/// as cancelled: shared with its [`ReadResponder`].
	given_up: Arc<AtomicBool>,
}

/// The responder a read runs with, around the OS layer's. A read the dispatcher aborted without
/// the OS naming it is failed as [`Failure::Transient`] instead of ended as cancelled, as its
/// request may still be live and would otherwise wait out the OS's 60 s timeout.
struct ReadResponder<R> {
	inner: R,
	given_up: Arc<AtomicBool>,
	/// The read permit, once taken.
	permit: Option<OwnedSemaphorePermit>,
}

impl<R> sealed::Sealed for ReadResponder<R> {}

impl<R: DataResponder> DataResponder for ReadResponder<R> {
	fn transfer(&mut self, chunk: AlignedChunk<'_>) -> Result<(), OsError> {
		self.inner.transfer(chunk)
	}

	fn complete(self) {
		self.inner.complete();
	}

	fn fail(self, failure: Failure) {
		self.inner.fail(failure);
	}

	fn cancelled(self) {
		if self.given_up.load(Ordering::Acquire) {
			self.inner.fail(Failure::Transient);
		} else {
			self.inner.cancelled();
		}
	}
}

/// Removes its read's [`InFlight`] entry when dropped, on every path the task takes.
struct InFlightGuard<D: Drive> {
	inner: Arc<Inner<D>>,
	key: TransferKey,
	id: u64,
}

impl<D: Drive> Drop for InFlightGuard<D> {
	fn drop(&mut self) {
		let mut in_flight = lock(&self.inner.in_flight);
		if let Some(reads) = in_flight.get_mut(&self.key) {
			reads.retain(|read| read.id != self.id);
			if reads.is_empty() {
				in_flight.remove(&self.key);
			}
		}
	}
}

/// Locks `mutex`, recovering it from a panic in another holder: every change made under these
/// locks is a single insert, remove or take, so the data is whole either way.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
	mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// `[offset, offset + len)`, or `None` when it holds no byte or its end overflows.
fn span(offset: i64, len: i64) -> Option<(i64, i64)> {
	if len <= 0 {
		return None;
	}
	Some((offset, offset.checked_add(len)?))
}

impl<D: Drive> Dispatcher<D> {
	/// A dispatcher answering from `drive`, running its work on `runtime` and reporting skipped
	/// items to `callback`.
	pub fn new(
		drive: Arc<D>,
		runtime: Handle,
		config: DispatchConfig,
		callback: Arc<dyn CloudFilesCallback>,
	) -> Self {
		let permits = config
			.max_concurrent_reads
			.get()
			.clamp(1, Semaphore::MAX_PERMITS);
		Self {
			inner: Arc::new(Inner {
				drive,
				runtime,
				callback,
				reads: Arc::new(Semaphore::new(permits)),
				in_flight: Mutex::new(HashMap::new()),
				next_id: AtomicU64::new(0),
				tasks: Mutex::new(JoinSet::new()),
				closed: AtomicBool::new(false),
				hints: OnceLock::new(),
			}),
		}
	}

	/// Tells `hints`, the local engine's ([`LocalEngine::hints`](crate::LocalEngine::hints)), of
	/// every directory populated, every read in flight and every read refused because its
	/// placeholder is out of date, so the remote side reconciles the directory, never dehydrates a
	/// file during a read, and applies the update the reader waits for at once. Set once; a second
	/// call keeps the first.
	pub fn with_hints(self, hints: Arc<RemoteHints>) -> Self {
		if self.inner.hints.set(hints).is_err() {
			warn!("the dispatcher's remote hints were set already");
		}
		self
	}

	/// The flavour of the runtime the dispatcher runs its work on. The Windows layer refuses a
	/// current-thread one: it blocks a worker on each cfapi transfer, which only a multi-thread
	/// runtime can hand off.
	#[cfg(any(windows, test))]
	pub(crate) fn runtime_flavor(&self) -> tokio::runtime::RuntimeFlavor {
		self.inner.runtime.runtime_flavor()
	}

	/// Spawns `task` on the runtime, reaping the tasks that finished. Once the dispatcher is closed
	/// the task is dropped instead, which fails its request as its responder drops.
	fn spawn(&self, task: impl Future<Output = ()> + Send + 'static) {
		let mut tasks = lock(&self.inner.tasks);
		if self.inner.closed.load(Ordering::Relaxed) {
			drop(tasks);
			drop(task);
			return;
		}
		while tasks.try_join_next().is_some() {}
		tasks.spawn_on(task, &self.inner.runtime);
	}

	/// A `FETCH_PLACEHOLDERS` request: populates the directory from the drive. A failure is logged
	/// with its cause; skipped children go to the callback.
	pub fn on_fetch_placeholders<R: PopulateResponder>(
		&self,
		request: PopulateRequest,
		responder: R,
	) {
		let inner = Arc::clone(&self.inner);
		self.spawn(async move {
			// Only a directory that decodes can have children to skip.
			let directory = PlaceholderIdentity::decode(request.identity()).ok();
			let (report, error) = match populate(&*inner.drive, request, responder).await {
				Ok(report) => (report, None),
				Err(failed) => (failed.report, Some(failed.error)),
			};
			match &error {
				None => debug!(
					?directory,
					created = report.created,
					skipped = report.skipped.len(),
					"populated a directory"
				),
				Some(error) => warn!(
					?directory,
					error = %log_safe::error(error),
					created = report.created,
					skipped = report.skipped.len(),
					"populating a directory failed"
				),
			}
			if let Some(directory) = directory
				&& !report.skipped.is_empty()
			{
				inner.callback.on_skipped_items(directory, &report.skipped);
			}
			if error.is_none()
				&& let (Some(directory), Some(hints)) = (directory, inner.hints.get())
			{
				hints.populated(directory);
			}
		});
	}

	/// A `FETCH_DATA` request: registers the read so a cancel finds it, then hydrates it from a
	/// task once a read permit is free. A cancel reaching the request while it waits for a permit
	/// ends it there, so it neither reads nor holds up the requests behind it. Never blocks.
	pub fn on_fetch_data<R: DataResponder>(&self, request: FetchDataRequest, responder: R) {
		let key = request.transfer();
		let range = request.range();
		let controller = FfiAbortController::new();
		let abort = controller.signal();
		let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
		let given_up = Arc::new(AtomicBool::new(false));
		let mut responder = ReadResponder {
			inner: responder,
			given_up: Arc::clone(&given_up),
			permit: None,
		};
		// Registered before the task exists, so a cancel arriving at once still finds it.
		lock(&self.inner.in_flight)
			.entry(key)
			.or_default()
			.push(InFlight {
				id,
				offset: range.required_offset(),
				len: range.required_len(),
				controller,
				given_up,
			});
		let guard = InFlightGuard {
			inner: Arc::clone(&self.inner),
			key,
			id,
		};
		let file = PlaceholderIdentity::decode(request.identity())
			.ok()
			.and_then(FileVersion::of);
		let stable = file.map(|file| file.stable);
		let own = request.own_process();
		let reading = file
			.zip(self.inner.hints.get())
			.map(|(file, hints)| hints.reading(file, own));
		let inner = Arc::clone(&self.inner);
		self.spawn(async move {
			let _guard = guard;
			let _reading = reading;
			if let (Some(file), Some(hints)) = (file, inner.hints.get())
				&& hints.is_damaged(file)
			{
				// Damaged content found before is never served again, so a reader can never refill
				// what the hydration job dehydrates; a fix on the drive is a new version.
				debug!(?key, "refused a read of a version found damaged");
				responder.fail(Failure::ContentDamaged);
				return;
			}
			let permit = select! {
				// Biased toward the cancel, so a request cancelled while queued never reads.
				biased;
				() = abort.aborted() => {
					debug!(?key, "a queued hydration was cancelled");
					responder.cancelled();
					return;
				}
				permit = Arc::clone(&inner.reads).acquire_owned() => permit,
			};
			let Ok(permit) = permit else {
				// The semaphore is never closed.
				warn!(?key, "the read permits are gone");
				responder.fail(Failure::Internal);
				return;
			};
			responder.permit = Some(permit);
			match hydrate(&*inner.drive, request, responder, &abort).await {
				Ok(report) => {
					debug!(
						?key,
						offset = report.read.range.offset(),
						end = report.read.range.end(),
						verification = ?report.verification,
						"hydrated a range"
					);
					if report.verification == Verification::HashChecked
						&& let (Some(file), Some(hints)) = (file, inner.hints.get())
					{
						hints.verified(file);
					}
				}
				Err(failed) if failed.error.answer() == Answer::Damaged => {
					warn!(?key, error = %log_safe::error(&failed.error), "a read found the file's content damaged");
					if let (Some(file), Some(hints)) = (file, inner.hints.get()) {
						hints.damaged(file);
					}
				}
				Err(failed) if failed.error.answer() == Answer::Cancelled => debug!(
					?key,
					transferred = failed.transferred,
					"a hydration was cancelled"
				),
				Err(failed) if failed.error.answer() == Answer::Fail(Failure::StalePlaceholder) => {
					debug!(?key, "a read found its placeholder out of date");
					if let (Some(stable), Some(hints)) = (stable, inner.hints.get()) {
						hints.stale(stable);
					}
				}
				Err(failed) => warn!(
					?key,
					error = %log_safe::error(&failed.error),
					transferred = failed.transferred,
					"hydrating a range failed"
				),
			}
		});
	}

	/// A `CANCEL_FETCH_DATA` request: aborts the reads under its transfer key whose required range
	/// is exactly the cancelled one, which end as cancelled. A cancel naming no range (none was
	/// read, its length holds no byte or its end overflows) aborts every read under the key instead, failing each as
	/// [`Failure::Transient`]: which of them the OS cancelled is unknown, and a live request must
	/// not be left unanswered. So does a cancel whose range more than one read has, failing each of
	/// those. A cancel finding nothing (the read finished, or the range matches
	/// none) does nothing. Never blocks.
	///
	/// Measured on Windows 11: every concurrent request for one file shares its transfer key, from
	/// one handle or several processes alike, and each cancelled request gets its own cancel
	/// carrying exactly that request's required range, so the range, not the key alone, says which
	/// read to stop; one that merely overlaps it belongs to another request, still live. A read a
	/// cancel misses is not lost: the OS refuses its next transfer as cancelled, which ends it.
	pub fn on_cancel_fetch_data(&self, cancel: CancelRequest) {
		let in_flight = lock(&self.inner.in_flight);
		let Some(reads) = in_flight.get(&cancel.transfer()) else {
			return;
		};
		let named = cancel
			.range()
			.filter(|range| span(range.offset(), range.length()).is_some());
		if let Some(range) = named {
			let named = || {
				reads
					.iter()
					.filter(|read| (read.offset, read.len) == (range.offset(), range.length()))
			};
			let several = named().nth(1).is_some();
			for read in named() {
				if several {
					read.given_up.store(true, Ordering::Release);
				}
				read.controller.abort();
			}
		} else {
			for read in reads {
				read.given_up.store(true, Ordering::Release);
				read.controller.abort();
			}
		}
	}

	/// Aborts every task and waits for them to end. Each request a task left unanswered fails as
	/// [`Failure::Transient`] when its responder drops. No read is aborted through its controller:
	/// a read aborted that way ends as a cancel the OS asked for, which tells the OS nothing, and
	/// dropping the task stops the read anyway. A request arriving during or after the shutdown is
	/// spawned as usual; dropping the dispatcher aborts it.
	pub async fn shutdown(&self) {
		let tasks = mem::take(&mut *lock(&self.inner.tasks));
		end(tasks).await;
	}

	/// As [`Dispatcher::shutdown`], and every request arriving after it fails at once as
	/// [`Failure::Transient`], the helper's own included: no read or population runs again. A
	/// request either was spawned before the close, and is aborted, or is refused, as both happen
	/// under the task set's lock. For a stop that must not wait on a fetch the helper itself started
	/// (its hydrate returns once its `FETCH_DATA` fails).
	pub async fn close(&self) {
		let tasks = {
			let mut tasks = lock(&self.inner.tasks);
			self.inner.closed.store(true, Ordering::Relaxed);
			mem::take(&mut *tasks)
		};
		end(tasks).await;
	}
}

/// Aborts every task in `tasks` and waits for them to end.
async fn end(mut tasks: JoinSet<()>) {
	tasks.abort_all();
	while tasks.join_next().await.is_some() {}
}

impl<D: Drive> Drop for Dispatcher<D> {
	/// Aborts every task, whose unanswered responders fail their requests as they drop. Tasks hold
	/// the dispatcher's state, so without this a task waiting forever would keep it alive.
	fn drop(&mut self) {
		lock(&self.inner.tasks).abort_all();
	}
}

#[cfg(test)]
mod tests;
