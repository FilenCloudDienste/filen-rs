//! The public copy API on [`Client`]: scans the sources and destinations, plans the copy, and
//! runs it.

use std::sync::Arc;

use crate::{
	Error, ErrorKind,
	auth::Client,
	fs::{
		HasName, HasUUID,
		categories::{DirType, Normal, fs::CategoryFS},
		drive_job::{
			backend::ClientBackend,
			listing::{ItemSource, ItemSourceDir, ListingBytes, ScanError, watch_listing},
			plan::{ItemPlan, ItemPlanner, PlanRequest, PlanSource, PlanTotals},
		},
		name::ValidatedName,
	},
	job::JobControl,
	util::MaybeArc,
};

use super::{
	CopyFailed, CopyReport,
	engine::run_copy,
	report::{CopyCallback, CopyPhase, Reporter},
};

/// One item to copy and where to put it.
#[derive(Debug, Clone)]
pub struct CopyRequest {
	pub source: ItemSource,
	/// An existing directory of the user's own drive.
	pub destination: DirType<'static, Normal>,
	/// The name to give the copy instead of the source's. Either way, a name already taken at
	/// the destination is kept and the copy gets `name (1)`, `name (2)`, ...
	pub name: Option<ValidatedName>,
}

#[derive(Debug, Clone, Default)]
pub struct CopyConfig {
	/// Storage still free on the account, if the caller knows it: a copy larger than this fails
	/// with [`ErrorKind::MaxStorageReached`] before anything is written. Its report still
	/// carries the totals, counted as not attempted, so the caller can tell how much storage the
	/// copy needs.
	pub max_bytes: Option<u64>,
}

impl Client {
	/// Copies `sources` into `destination`, keeping both when a name is taken there.
	/// See [`copy_items_to`](Self::copy_items_to).
	pub async fn copy_items(
		self: Arc<Self>,
		sources: Vec<ItemSource>,
		destination: DirType<'static, Normal>,
		config: CopyConfig,
		callback: impl CopyCallback,
		control: JobControl,
	) -> Result<CopyReport, CopyFailed> {
		let requests = sources
			.into_iter()
			.map(|source| CopyRequest {
				source,
				destination: destination.clone(),
				name: None,
			})
			.collect();
		self.copy_items_to(requests, config, callback, control)
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
	/// Returns the report (what was created, what failed, what was skipped). A copy that ended
	/// early fails with the report so far, and [`ErrorKind::Cancelled`] after a cancel or the
	/// error that ended it.
	pub async fn copy_items_to(
		self: Arc<Self>,
		requests: Vec<CopyRequest>,
		config: CopyConfig,
		callback: impl CopyCallback,
		control: JobControl,
	) -> Result<CopyReport, CopyFailed> {
		let reporter = Reporter::new(callback);
		let destination_dirs = requests
			.iter()
			.map(|request| (request.destination.uuid(), request.destination.clone()))
			.collect();
		let scanned = self.scan(requests, &reporter, &control).await;
		let plan = scanned
			.and_then(|(planner, requests)| planner.plan(requests).map_err(ScanError::Failed));
		let plan = plan_to_run(plan, config.max_bytes, &reporter).map_err(|failed| *failed)?;
		let backend = Arc::new(ClientBackend::new(self));
		run_copy(backend, plan, destination_dirs, control, reporter).await
	}

	/// Lists every source directory and every destination.
	async fn scan(
		&self,
		requests: Vec<CopyRequest>,
		reporter: &MaybeArc<Reporter>,
		control: &JobControl,
	) -> Result<(ItemPlanner, Vec<PlanRequest<ItemSourceDir>>), ScanError> {
		let dir_sources = requests
			.iter()
			.filter(|r| matches!(r.source, ItemSource::Dir(_)))
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

		let ops = reporter.ops();
		let mut planner = ItemPlanner::default();
		for destination in destinations {
			reporter.checkpoint(control).await?;
			let listed = watch_listing(
				Normal::list_dir(self, &destination, None::<&fn(u64, Option<u64>)>, ()),
				&ops,
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
				ItemSource::File(file) => PlanSource::File(file),
				ItemSource::Dir(dir) => {
					reporter.checkpoint(control).await?;
					bytes.next_source();
					let listing = self.list_item_source(dir, &bytes);
					let source =
						watch_listing(listing, &reporter.ops(), control, || report(sources_done))
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
}

/// The plan to run, or the end of a copy that never starts: its scan was cancelled or failed,
/// or it needs more than `max_bytes`. A copy refused for `max_bytes` still reports the plan's
/// totals, none of them attempted, so the caller can tell how much storage it needs; its skips
/// and renames stay out, since it never starts.
fn plan_to_run(
	plan: Result<ItemPlan<ItemSourceDir>, ScanError>,
	max_bytes: Option<u64>,
	reporter: &Reporter,
) -> Result<ItemPlan<ItemSourceDir>, Box<CopyFailed>> {
	let (phase, error, totals) = match plan {
		Ok(plan) => match max_bytes {
			Some(max_bytes) if plan.totals.bytes > max_bytes => (
				CopyPhase::Failed,
				Error::custom(
					ErrorKind::MaxStorageReached,
					format!(
						"the copy needs {} bytes but only {max_bytes} are free",
						plan.totals.bytes
					),
				),
				plan.totals,
			),
			_ => return Ok(plan),
		},
		Err(ScanError::Stopped) => (
			CopyPhase::Cancelled,
			Error::custom(ErrorKind::Cancelled, "copy cancelled"),
			PlanTotals::default(),
		),
		Err(ScanError::Failed(error)) => (CopyPhase::Failed, error, PlanTotals::default()),
	};
	reporter.finish_unstarted(phase, totals);
	Err(Box::new(CopyFailed {
		report: CopyReport {
			totals,
			counts: reporter.counts(),
			..CopyReport::default()
		},
		error: Arc::new(error),
	}))
}

#[cfg(test)]
mod tests {
	use std::{
		sync::{
			Mutex,
			atomic::{AtomicBool, AtomicU64, Ordering},
		},
		time::Duration,
	};

	use filen_types::fs::Uuid;

	use super::*;
	use crate::{
		fs::{
			copy::report::{CopiedTopLevel, CopyUpdate, PlannedTopLevelItem},
			drive_job::{
				counts::ItemCounts,
				listing::ScanProgress,
				plan::{SkipReason, SkippedEntry},
			},
		},
		job::test_support::{SetOnDrop, controls},
	};

	#[derive(Default)]
	struct Updates(Mutex<Vec<CopyUpdate>>);

	impl CopyCallback for Updates {
		fn on_top_level_planned(&self, _: Vec<PlannedTopLevelItem>) {}
		fn on_top_level_created(&self, _: CopiedTopLevel) {}
		fn on_update(&self, update: CopyUpdate) {
			self.0.lock().unwrap().push(update);
		}
	}

	/// The bindings run the copy on the SDK's multi-threaded runtime, which needs a `Send`
	/// future.
	fn _copy_future_is_send(client: Arc<Client>) {
		fn assert_send<T: Send>(_: T) {}
		assert_send(client.copy_items_to(
			Vec::new(),
			CopyConfig::default(),
			Updates::default(),
			JobControl::default(),
		));
	}

	impl Updates {
		fn last(&self) -> CopyUpdate {
			self.0.lock().unwrap().last().unwrap().clone()
		}
	}

	#[test]
	fn a_copy_larger_than_max_bytes_is_refused_with_its_totals_not_attempted() {
		let needs = PlanTotals {
			dirs: 1,
			files: 2,
			bytes: 1024,
		};
		let plan = || ItemPlan {
			skipped: vec![SkippedEntry {
				source_path: "/Top/secret".to_owned(),
				bytes: 7,
				reason: SkipReason::UndecryptableFile {
					uuid: Uuid::new_v4(),
				},
			}],
			totals: needs,
			..ItemPlan::default()
		};
		let updates = Arc::new(Updates::default());
		let reporter = Reporter::new(Arc::clone(&updates));

		let CopyFailed { report, error } =
			*plan_to_run(Ok(plan()), Some(needs.bytes - 1), &reporter).unwrap_err();

		assert_eq!(error.kind(), ErrorKind::MaxStorageReached);
		assert_eq!(report.totals, needs, "the report says what the copy needs");
		let not_attempted = ItemCounts {
			dirs_not_attempted: needs.dirs,
			files_not_attempted: needs.files,
			bytes_not_attempted: needs.bytes,
			..ItemCounts::default()
		};
		assert_eq!(report.counts, not_attempted);
		assert!(
			report.skipped.is_empty() && report.renamed.is_empty(),
			"a copy that never starts skips and renames nothing"
		);
		assert!(report.top_level.is_empty() && report.failures.is_empty());
		let last = updates.last();
		assert_eq!(
			(last.phase, last.totals, last.counts),
			(CopyPhase::Failed, needs, not_attempted),
			"the last update says what the copy needs"
		);
		assert!(last.events.is_empty());

		for max_bytes in [Some(needs.bytes), None] {
			let updates = Arc::new(Updates::default());
			let reporter = Reporter::new(Arc::clone(&updates));
			let plan = plan_to_run(Ok(plan()), max_bytes, &reporter).unwrap();
			assert_eq!(plan.totals, needs, "a copy that fits runs");
			assert!(updates.0.lock().unwrap().is_empty(), "and has not ended");
		}
	}

	#[test]
	fn a_copy_whose_scan_ended_reports_nothing() {
		let scans = [
			(
				ScanError::Stopped,
				CopyPhase::Cancelled,
				ErrorKind::Cancelled,
			),
			(
				ScanError::Failed(Error::custom(ErrorKind::Server, "listing failed")),
				CopyPhase::Failed,
				ErrorKind::Server,
			),
		];
		for (scan, phase, kind) in scans {
			let updates = Arc::new(Updates::default());
			let reporter = Reporter::new(Arc::clone(&updates));
			let CopyFailed { report, error } =
				*plan_to_run(Err(scan), Some(0), &reporter).unwrap_err();
			assert_eq!(error.kind(), kind);
			assert_eq!(report.totals, PlanTotals::default());
			assert_eq!(report.counts, ItemCounts::default());
			let last = updates.last();
			assert_eq!(
				(last.phase, last.totals, last.counts),
				(phase, PlanTotals::default(), ItemCounts::default())
			);
		}
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
		let result = watch_listing(listing, &reporter.ops(), &control, || {
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
		let marker = SetOnDrop(Arc::clone(&dropped));
		let listing = async move {
			let _marker = marker;
			std::future::pending::<()>().await;
		};
		let cancel_later = async {
			tokio::time::sleep(Duration::from_secs(1)).await;
			cancel.send_replace(true);
		};
		let ops = reporter.ops();
		let (result, ()) =
			tokio::join!(watch_listing(listing, &ops, &control, || {}), cancel_later);
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
		let result = watch_listing(listing, &reporter.ops(), &control, || {}).await;
		assert!(
			matches!(result, Ok(7)),
			"an in-flight listing completes while pausing"
		);
		assert!(reporter.is_paused(), "paused once the listing is done");

		let next = tokio::spawn({
			let reporter = MaybeArc::clone(&reporter);
			let control = control.clone();
			async move { reporter.checkpoint(&control).await.is_ok() }
		});
		tokio::time::sleep(Duration::from_secs(60)).await;
		assert!(!next.is_finished(), "the next listing waits while paused");
		pause.send_replace(false);
		assert!(next.await.unwrap());
	}
}
