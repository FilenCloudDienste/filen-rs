//! The public copy API on [`Client`]: scans the sources and destinations, plans the copy, and
//! runs it.

use std::{
	borrow::Cow,
	sync::{
		Arc,
		atomic::{AtomicU64, Ordering},
	},
};

use filen_types::{fs::Uuid, traits::CowHelpers};

use crate::{
	Error, ErrorKind,
	auth::Client,
	connect::{DirPublicLink, fs::SharingRole},
	consts::CALLBACK_INTERVAL,
	fs::{
		HasName, HasParent, HasUUID,
		categories::{DirType, Linked, Normal, Shared, fs::CategoryFS},
		dir::{
			RemoteDirectory,
			traits::{HasDirInfo, HasRemoteDirInfo},
		},
		file::enums::RemoteFileType,
	},
	job::{JobControl, Stopped},
	util::MaybeArc,
};

use super::{
	backend::ClientBackend,
	engine::{CopyOutcome, run_copy},
	plan::{CopyPlanner, Listed, PlanRequest, PlanSource, SourceDir},
	report::{CopyCallback, CopyPhase, CopyReport, Reporter, ScanProgress},
};

/// A directory to copy, with what is needed to list it.
#[derive(Debug, Clone)]
pub enum CopySourceDir {
	/// One of the user's own directories.
	Normal(RemoteDirectory),
	/// A directory shared with (or by) the user.
	Shared(DirType<'static, Shared>, SharingRole),
	/// A directory in a public link.
	Linked(DirType<'static, Linked>, DirPublicLink),
}

#[derive(Debug, Clone)]
pub enum CopySource {
	File(RemoteFileType<'static>),
	Dir(CopySourceDir),
}

/// One item to copy and where to put it.
#[derive(Debug, Clone)]
pub struct CopyRequest {
	pub source: CopySource,
	/// An existing directory of the user's own drive.
	pub destination: DirType<'static, Normal>,
	/// The name to give the copy instead of the source's. Either way, a name already taken at
	/// the destination is kept and the copy gets `name (1)`, `name (2)`, ...
	pub name: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct CopyOptions {
	/// Storage still free on the account, if the caller knows it: a copy larger than this fails
	/// with [`ErrorKind::MaxStorageReached`] before anything is written.
	pub max_bytes: Option<u64>,
}

/// The source directory for the root of a listing that can start at a category root.
fn root_source_dir<Cat>(
	dir: &DirType<'static, Cat>,
	handle: CopySourceDir,
) -> SourceDir<CopySourceDir>
where
	Cat: CategoryFS,
	Cat::Root: HasName + HasDirInfo + HasRemoteDirInfo,
	Cat::Dir: HasRemoteDirInfo,
{
	match dir {
		DirType::Root(root) => SourceDir::new(root.as_ref(), root.color().into_owned_cow(), handle),
		DirType::Dir(dir) => SourceDir::new(dir.as_ref(), dir.color().into_owned_cow(), handle),
	}
}

/// Bytes of listing responses received by earlier sources, and by the one being listed.
#[derive(Default)]
struct ListingBytes {
	done: AtomicU64,
	current: AtomicU64,
	/// `u64::MAX` while unknown.
	current_total: AtomicU64,
}

impl ListingBytes {
	fn scan(&self, sources_done: u64, sources_total: u64) -> ScanProgress {
		let done = self.done.load(Ordering::Relaxed);
		let current = self.current.load(Ordering::Relaxed);
		let total = self.current_total.load(Ordering::Relaxed);
		ScanProgress {
			sources_done,
			sources_total,
			listing_bytes: done + current,
			listing_total_bytes: (total != u64::MAX).then(|| done + total),
		}
	}

	fn next_source(&self) {
		let current = self.current.swap(0, Ordering::Relaxed);
		self.done.fetch_add(current, Ordering::Relaxed);
		self.current_total.store(u64::MAX, Ordering::Relaxed);
	}
}

/// Lists `root` recursively into a plan source. `handle_of` addresses a listed directory again
/// for a retry.
async fn list_source<Cat>(
	client: &Cat::Client,
	root: &DirType<'static, Cat>,
	context: Cat::ListDirContext<'_>,
	root_dir: SourceDir<CopySourceDir>,
	handle_of: impl Fn(&Cat::Dir) -> CopySourceDir,
	bytes: &ListingBytes,
) -> Result<PlanSource<CopySourceDir>, Error>
where
	Cat: CategoryFS,
	Cat::Dir: HasRemoteDirInfo,
	RemoteFileType<'static>: From<Cat::File>,
{
	let progress = |received: u64, total: Option<u64>| {
		bytes.current.store(received, Ordering::Relaxed);
		bytes
			.current_total
			.store(total.unwrap_or(u64::MAX), Ordering::Relaxed);
	};
	let (dirs, files) = Cat::list_dir_recursive(client, root, Some(&progress), context).await?;
	let dirs = dirs
		.into_iter()
		.filter_map(|dir| {
			let parent = Uuid::try_from(*dir.parent()).ok()?;
			let item = SourceDir::new(&dir, dir.color().into_owned_cow(), handle_of(&dir));
			Some(Listed { parent, item })
		})
		.collect();
	let files = files
		.into_iter()
		.filter_map(|file| {
			let parent = Uuid::try_from(*file.parent()).ok()?;
			Some(Listed {
				parent,
				item: RemoteFileType::from(file),
			})
		})
		.collect();
	Ok(PlanSource::Dir {
		root: root_dir,
		dirs,
		files,
	})
}

impl Client {
	/// Copies `sources` into `destination`, keeping both when a name is taken there.
	/// See [`copy_items_to`](Self::copy_items_to).
	pub async fn copy_items(
		self: Arc<Self>,
		sources: Vec<CopySource>,
		destination: DirType<'static, Normal>,
		options: CopyOptions,
		callback: impl CopyCallback,
		control: JobControl,
	) -> CopyOutcome<CopySourceDir> {
		let requests = sources
			.into_iter()
			.map(|source| CopyRequest {
				source,
				destination: destination.clone(),
				name: None,
			})
			.collect();
		self.copy_items_to(requests, options, callback, control)
			.await
	}

	/// Copies each request's source into its destination. There is no server-side copy: every
	/// file is downloaded, decrypted and uploaded again under a new key, and every directory is
	/// created anew.
	///
	/// The copy first lists every source directory and every destination (the scanning phase),
	/// then creates the directories and copies the files. It reports its progress to `callback`
	/// and can be paused, resumed and cancelled through `control` in every phase. A failed item
	/// does not stop the others; running out of storage stops the copy.
	///
	/// The outcome always carries the report (what was created, what failed, what was skipped);
	/// its result is `Err` with [`ErrorKind::Cancelled`] after a cancel, or the error that ended
	/// the copy.
	pub async fn copy_items_to(
		self: Arc<Self>,
		requests: Vec<CopyRequest>,
		options: CopyOptions,
		callback: impl CopyCallback,
		control: JobControl,
	) -> CopyOutcome<CopySourceDir> {
		let reporter = Reporter::new(callback);
		let destination_dirs = requests
			.iter()
			.map(|request| (request.destination.uuid(), request.destination.clone()))
			.collect();
		let scanned = self.scan(requests, &reporter, &control).await;
		let plan = scanned.and_then(|(planner, requests)| {
			let plan = planner.plan(requests).map_err(ScanError::Failed)?;
			match options.max_bytes {
				Some(max_bytes) if plan.totals.bytes > max_bytes => {
					Err(ScanError::Failed(Error::custom(
						ErrorKind::MaxStorageReached,
						format!(
							"the copy needs {} bytes but only {max_bytes} are free",
							plan.totals.bytes
						),
					)))
				}
				_ => Ok(plan),
			}
		});
		let plan = match plan {
			Ok(plan) => plan,
			Err(error) => {
				let (phase, error) = match error {
					ScanError::Stopped => (
						CopyPhase::Cancelled,
						Error::custom(ErrorKind::Cancelled, "copy cancelled"),
					),
					ScanError::Failed(error) => (CopyPhase::Failed, error),
				};
				reporter.finish(phase);
				return CopyOutcome {
					report: CopyReport::default(),
					result: Err(error),
				};
			}
		};
		let backend = Arc::new(ClientBackend::new(self));
		run_copy(backend, plan, destination_dirs, control, reporter).await
	}

	/// Lists every source directory and every destination.
	async fn scan(
		&self,
		requests: Vec<CopyRequest>,
		reporter: &MaybeArc<Reporter>,
		control: &JobControl,
	) -> Result<(CopyPlanner, Vec<PlanRequest<CopySourceDir>>), ScanError> {
		let dir_sources = requests
			.iter()
			.filter(|r| matches!(r.source, CopySource::Dir(_)))
			.count() as u64;
		let mut destinations: Vec<DirType<'static, Normal>> = Vec::new();
		for request in &requests {
			if !destinations
				.iter()
				.any(|d| d.uuid() == request.destination.uuid())
			{
				destinations.push(request.destination.clone());
			}
		}
		let sources_total = dir_sources + destinations.len() as u64;
		let bytes = ListingBytes::default();
		let mut sources_done = 0;
		let report = |sources_done| reporter.set_scan(bytes.scan(sources_done, sources_total));
		report(sources_done);

		let mut planner = CopyPlanner::default();
		for destination in destinations {
			checkpoint(reporter, control).await?;
			let listed = watch_listing(
				Normal::list_dir(self, &destination, None::<&fn(u64, Option<u64>)>, ()),
				reporter,
				control,
				|| report(sources_done),
			);
			let (dirs, files) = listed.await?.map_err(ScanError::Failed)?;
			let names = dirs
				.iter()
				.filter_map(|d| d.name())
				.chain(files.iter().filter_map(|f| f.name()));
			planner.add_destination(destination.uuid(), names);
			if dirs.iter().any(|d| d.name().is_none()) || files.iter().any(|f| f.name().is_none()) {
				planner.mark_unverified(destination.uuid());
			}
			sources_done += 1;
			report(sources_done);
		}

		let mut planned = Vec::with_capacity(requests.len());
		for request in requests {
			let source = match request.source {
				CopySource::File(file) => PlanSource::File(file),
				CopySource::Dir(dir) => {
					checkpoint(reporter, control).await?;
					bytes.next_source();
					let listing = self.list_dir_source(dir, &bytes);
					let source = watch_listing(listing, reporter, control, || report(sources_done))
						.await?
						.map_err(ScanError::Failed)?;
					sources_done += 1;
					report(sources_done);
					source
				}
			};
			planned.push(PlanRequest {
				source,
				destination: request.destination.uuid(),
				name: request.name,
			});
		}
		Ok((planner, planned))
	}

	async fn list_dir_source(
		&self,
		dir: CopySourceDir,
		bytes: &ListingBytes,
	) -> Result<PlanSource<CopySourceDir>, Error> {
		match dir {
			CopySourceDir::Normal(dir) => {
				let root = DirType::Dir(Cow::Owned(dir.clone()));
				let root_dir = SourceDir::new(
					&dir,
					dir.color().into_owned_cow(),
					CopySourceDir::Normal(dir.clone()),
				);
				list_source::<Normal>(
					self,
					&root,
					(),
					root_dir,
					|d| CopySourceDir::Normal(d.clone()),
					bytes,
				)
				.await
			}
			CopySourceDir::Shared(root, role) => {
				list_source::<Shared>(
					self,
					&root,
					&role,
					root_source_dir(&root, CopySourceDir::Shared(root.clone(), role.clone())),
					|d| CopySourceDir::Shared(DirType::Dir(Cow::Owned(d.clone())), role.clone()),
					bytes,
				)
				.await
			}
			CopySourceDir::Linked(root, link) => {
				list_source::<Linked>(
					self.unauthed(),
					&root,
					Cow::Borrowed(&link),
					root_source_dir(&root, CopySourceDir::Linked(root.clone(), link.clone())),
					|d| CopySourceDir::Linked(DirType::Dir(Cow::Owned(d.clone())), link.clone()),
					bytes,
				)
				.await
			}
		}
	}
}

/// Runs a listing, reporting scan progress while it downloads. A cancel drops it; a pause
/// lets it finish (it holds no transfer memory), and the copy counts as pausing until it has.
async fn watch_listing<T>(
	listing: impl Future<Output = T>,
	reporter: &MaybeArc<Reporter>,
	control: &JobControl,
	report: impl Fn(),
) -> Result<T, ScanError> {
	let _op = reporter.op();
	let listing = std::pin::pin!(control.until_stopping(listing));
	let ticker = async {
		loop {
			crate::util::sleep(CALLBACK_INTERVAL).await;
			reporter.set_pause_requested(control.is_pause_requested());
			report();
		}
	};
	let result = tokio::select! {
		result = listing => result,
		_ = ticker => unreachable!("the ticker never ends"),
	};
	result.map_err(|Stopped| {
		reporter.set_cancelling();
		ScanError::Stopped
	})
}

enum ScanError {
	Stopped,
	Failed(Error),
}

impl From<Stopped> for ScanError {
	fn from(_: Stopped) -> Self {
		Self::Stopped
	}
}

/// Waits out a pause before starting the next listing; `Err` once the copy is stopping.
async fn checkpoint(reporter: &MaybeArc<Reporter>, control: &JobControl) -> Result<(), ScanError> {
	reporter.set_pause_requested(control.is_pause_requested());
	let result = control.checkpoint().await;
	reporter.set_pause_requested(control.is_pause_requested());
	result.map_err(|Stopped| {
		reporter.set_cancelling();
		ScanError::Stopped
	})
}

#[cfg(test)]
mod tests {
	use std::{
		sync::{
			Mutex,
			atomic::{AtomicBool, AtomicU64},
		},
		time::Duration,
	};

	use tokio::sync::watch;

	use super::*;
	use crate::fs::copy::report::{CopiedTopLevel, CopyUpdate, PlannedTopLevelItem};

	#[derive(Default)]
	struct Updates(Mutex<Vec<CopyUpdate>>);

	impl CopyCallback for Updates {
		fn top_level_planned(&self, _: Vec<PlannedTopLevelItem>) {}
		fn top_level_created(&self, _: CopiedTopLevel) {}
		fn update(&self, update: CopyUpdate) {
			self.0.lock().unwrap().push(update);
		}
	}

	fn controls() -> (watch::Sender<bool>, watch::Sender<bool>, JobControl) {
		let (pause, pause_rx) = watch::channel(false);
		let (cancel, cancel_rx) = watch::channel(false);
		(
			pause,
			cancel,
			JobControl::from_receivers(Some(pause_rx), Some(cancel_rx)),
		)
	}

	/// The bindings run the copy on the SDK's multi-threaded runtime, which needs a `Send`
	/// future.
	#[allow(dead_code)]
	fn copy_future_is_send(client: Arc<Client>) {
		fn assert_send<T: Send>(_: T) {}
		assert_send(client.copy_items_to(
			Vec::new(),
			CopyOptions::default(),
			Updates::default(),
			JobControl::default(),
		));
	}

	#[test]
	fn listing_bytes_add_up_across_sources() {
		let bytes = ListingBytes::default();
		bytes.current_total.store(u64::MAX, Ordering::Relaxed);
		assert_eq!(bytes.scan(0, 3).listing_total_bytes, None);
		bytes.current.store(40, Ordering::Relaxed);
		bytes.current_total.store(100, Ordering::Relaxed);
		let scan = bytes.scan(1, 3);
		assert_eq!(
			(scan.listing_bytes, scan.listing_total_bytes),
			(40, Some(100))
		);
		assert_eq!((scan.sources_done, scan.sources_total), (1, 3));

		bytes.current.store(100, Ordering::Relaxed);
		bytes.next_source();
		bytes.current.store(5, Ordering::Relaxed);
		let scan = bytes.scan(2, 3);
		assert_eq!((scan.listing_bytes, scan.listing_total_bytes), (105, None));
	}

	#[tokio::test(start_paused = true)]
	async fn a_listing_reports_scan_progress_while_it_runs() {
		let updates = Arc::new(Updates::default());
		let reporter = Reporter::new(Arc::clone(&updates));
		let control = JobControl::default();
		let received = AtomicU64::new(0);
		let listing = async {
			for step in 1..=5u64 {
				tokio::time::sleep(Duration::from_millis(300)).await;
				received.store(step * 10, Ordering::Relaxed);
			}
			"listed"
		};
		let result = watch_listing(listing, &reporter, &control, || {
			reporter.set_scan(ScanProgress {
				sources_done: 0,
				sources_total: 1,
				listing_bytes: received.load(Ordering::Relaxed),
				listing_total_bytes: None,
			});
		})
		.await;
		assert!(matches!(result, Ok("listed")));
		assert_eq!(reporter.ops_in_flight(), 0);
		// updates are throttled by wall-clock time, so ask for the last state explicitly
		reporter.finish(CopyPhase::Done);
		let updates = updates.0.lock().unwrap();
		let last = updates.last().unwrap();
		assert_eq!(last.phase, CopyPhase::Done);
		assert!(
			last.scan.listing_bytes > 0,
			"scan progress was reported while listing"
		);
	}

	#[tokio::test(start_paused = true)]
	async fn a_cancel_drops_the_listing_in_progress() {
		let reporter = Reporter::new(Updates::default());
		let (_pause, cancel, control) = controls();
		let dropped = Arc::new(AtomicBool::new(false));
		struct SetOnDrop(Arc<AtomicBool>);
		impl Drop for SetOnDrop {
			fn drop(&mut self) {
				self.0.store(true, Ordering::SeqCst);
			}
		}
		let marker = SetOnDrop(Arc::clone(&dropped));
		let listing = async move {
			let _marker = marker;
			std::future::pending::<()>().await;
		};
		let cancel_later = async {
			tokio::time::sleep(Duration::from_secs(1)).await;
			cancel.send_replace(true);
		};
		let (result, ()) = tokio::join!(
			watch_listing(listing, &reporter, &control, || {}),
			cancel_later
		);
		assert!(matches!(result, Err(ScanError::Stopped)));
		assert!(
			dropped.load(Ordering::SeqCst),
			"the listing request is dropped"
		);
		assert_eq!(reporter.ops_in_flight(), 0);
	}

	#[tokio::test(start_paused = true)]
	async fn a_pause_lets_the_listing_finish_and_then_waits() {
		let reporter = Reporter::new(Updates::default());
		let (pause, _cancel, control) = controls();
		pause.send_replace(true);
		let listing = async {
			tokio::time::sleep(Duration::from_secs(1)).await;
			7
		};
		let result = watch_listing(listing, &reporter, &control, || {}).await;
		assert!(
			matches!(result, Ok(7)),
			"an in-flight listing completes while pausing"
		);
		assert!(reporter.is_paused(), "paused once the listing is done");

		let next = tokio::spawn({
			let reporter = MaybeArc::clone(&reporter);
			let control = control.clone();
			async move { checkpoint(&reporter, &control).await.is_ok() }
		});
		tokio::time::sleep(Duration::from_secs(60)).await;
		assert!(!next.is_finished(), "the next listing waits while paused");
		pause.send_replace(false);
		assert!(next.await.unwrap());
	}
}
