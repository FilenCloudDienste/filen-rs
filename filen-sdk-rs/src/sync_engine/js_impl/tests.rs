use std::{
	sync::{Arc, Mutex, mpsc},
	time::{Duration, Instant},
};

use tokio::sync::{mpsc::unbounded_channel, oneshot, watch};

use super::{
	JsSyncEngine, JsSyncPlan, LOOP_ENDED, LiveState, OPEN_ENGINES, PairSlot, PauseRecord,
	RECENT_EVENTS, StatusPump, SyncEngineCore, claim_key,
	delivery::Delivery,
	lock, pump_status,
	types::{
		DeleteGuardState, SyncConflictPage, SyncEventMessage, SyncGuardReason, SyncPairActivity,
		SyncPairInfo, SyncPauseOptions, SyncPlanSummary, delete_guard,
	},
};
use crate::{
	Error, ErrorKind,
	auth::{
		Client,
		test_support::{offline_client, offline_client_with_root},
	},
	sync_engine::{
		Backlog, BaselineInUse, ConflictResolution, DeleteGuard, GuardReason, LegacyImport,
		PauseMode, PlanOutcome, PlannedAction, PlannedActionKind, PlannedConflict, PlannedNodeKind,
		SyncEvent, SyncMode, SyncReport, TransferDirection, WatchState, WatchStatus,
		baseline::{BaselineEntry, BaselineState, BaselineStore, NodeKind},
	},
};

/// The JS environment the tests open from.
const HOST: u64 = 1;

/// A listener that forwards every delivery to the test.
fn recording() -> (super::Listener, mpsc::Receiver<Delivery>) {
	let (deliveries_tx, deliveries) = mpsc::channel();
	let listener: super::Listener = Arc::new(move |chunk: Vec<Delivery>| {
		for delivery in chunk {
			// the test may already have what it waits for
			let _ = deliveries_tx.send(delivery);
		}
	});
	(listener, deliveries)
}

/// A baseline in a directory of its own, and a local root beside it for one pair.
struct Scene {
	dir: tempfile::TempDir,
}

impl Scene {
	fn new() -> Self {
		let dir = tempfile::tempdir().unwrap();
		std::fs::create_dir(dir.path().join("local")).unwrap();
		Self { dir }
	}

	fn db(&self) -> std::path::PathBuf {
		self.dir.path().join("baseline.db")
	}

	fn local(&self) -> String {
		self.dir.path().join("local").to_string_lossy().into_owned()
	}

	async fn open(&self, client: Client) -> Result<Arc<SyncEngineCore>, Error> {
		SyncEngineCore::open(Arc::new(client), self.db(), HOST, recording().0).await
	}

	/// An open core with one registered pair.
	async fn with_pair(&self) -> (Arc<SyncEngineCore>, u64) {
		let core = self.open(offline_client()).await.unwrap();
		let pair = core
			.add_pair(self.local(), uuid::Uuid::from_u128(7), SyncMode::TwoWay)
			.await
			.unwrap();
		(core, pair)
	}
}

fn claimed(scene: &Scene) -> bool {
	lock(&OPEN_ENGINES).contains_key(&claim_key(&scene.db()).unwrap())
}

/// Wait for `done`, for at most 30 s of wall time: what it waits on runs on blocking threads and
/// the filesystem watcher's, which no paused clock drives.
async fn wait_until(mut done: impl FnMut() -> bool) {
	let deadline = Instant::now() + Duration::from_secs(30);
	while !done() {
		assert!(Instant::now() < deadline, "timed out");
		tokio::task::yield_now().await;
	}
}

fn refusal_message(result: Result<impl Sized, Error>) -> String {
	match result {
		Ok(_) => panic!("the call was supposed to be refused"),
		Err(error) => {
			assert_eq!(error.kind(), ErrorKind::InvalidState, "{error}");
			error.to_string()
		}
	}
}

fn is_baseline_in_use(result: Result<Arc<SyncEngineCore>, Error>) -> bool {
	result.is_err_and(|error| error.downcast_ref::<BaselineInUse>().is_some())
}

#[tokio::test]
async fn a_second_open_of_the_same_account_attaches_and_takes_the_listener() {
	let scene = Scene::new();
	let (first_listener, first) = recording();
	let core = SyncEngineCore::open(Arc::new(offline_client()), scene.db(), HOST, first_listener)
		.await
		.unwrap();
	let (second_listener, second) = recording();
	let attached = SyncEngineCore::open(
		Arc::new(offline_client()),
		scene.db(),
		HOST,
		second_listener,
	)
	.await
	.unwrap();
	assert!(
		Arc::ptr_eq(&core, &attached),
		"the same engine, not a second one"
	);

	let delivery = Delivery::Status {
		pair_id: 1,
		status: WatchStatus::default(),
	};
	core.live.deliveries.send(delivery.clone()).unwrap();
	assert_eq!(
		second.recv_timeout(Duration::from_secs(30)).unwrap(),
		delivery
	);
	assert!(
		first.try_recv().is_err(),
		"the replaced listener must receive nothing more"
	);
}

#[tokio::test]
async fn an_open_for_another_account_is_refused_as_baseline_in_use() {
	let scene = Scene::new();
	let _core = scene.open(offline_client()).await.unwrap();

	assert!(is_baseline_in_use(
		scene
			.open(offline_client_with_root(
				"00000000-0000-0000-0000-000000000001"
			))
			.await
	));
}

#[tokio::test]
async fn an_open_from_another_host_is_refused_as_baseline_in_use() {
	let scene = Scene::new();
	let _core = scene.open(offline_client()).await.unwrap();

	assert!(is_baseline_in_use(
		SyncEngineCore::open(
			Arc::new(offline_client()),
			scene.db(),
			HOST + 1,
			recording().0
		)
		.await
	));
}

#[tokio::test]
async fn a_closed_engine_is_refused_until_it_is_gone_then_opens_again() {
	let scene = Scene::new();
	let core = scene.open(offline_client()).await.unwrap();
	// What a call still in flight holds.
	let in_flight = core.engine().unwrap();
	Arc::clone(&core).close().await.unwrap();

	assert!(
		refusal_message(core.list_pairs().await).contains("closed"),
		"every call after close is refused"
	);
	assert!(
		is_baseline_in_use(scene.open(offline_client()).await),
		"the engine still runs while a call holds it"
	);
	drop(in_flight);
	assert!(!claimed(&scene));
	let reopened = scene.open(offline_client()).await.unwrap();
	assert!(!Arc::ptr_eq(&core, &reopened));
}

#[tokio::test]
async fn dropping_the_last_handle_without_close_releases_the_baseline() {
	let scene = Scene::new();
	let (core, pair) = scene.with_pair().await;
	let watch = Arc::clone(&core).watch_pair(pair, None).await.unwrap();
	let engine = JsSyncEngine { core };

	drop(engine);
	// The watch handle holds no core.
	drop(watch);
	wait_until(|| !claimed(&scene)).await;
	scene.open(offline_client()).await.unwrap();
}

#[tokio::test]
async fn two_racing_watches_of_one_pair_start_one() {
	let scene = Scene::new();
	let (core, pair) = scene.with_pair().await;

	let (first, second) = tokio::join!(
		Arc::clone(&core).watch_pair(pair, None),
		Arc::clone(&core).watch_pair(pair, None)
	);

	assert_eq!(
		u8::from(first.is_ok()) + u8::from(second.is_ok()),
		1,
		"exactly one watch may start"
	);
	let refused = refusal_message(if first.is_ok() { second } else { first });
	assert!(refused.contains("already being watched"), "{refused}");
	Arc::clone(&core).close().await.unwrap();
}

#[tokio::test]
async fn a_watched_pair_refuses_a_second_watch_and_a_one_shot_pass() {
	let scene = Scene::new();
	let (core, pair) = scene.with_pair().await;
	let _watch = Arc::clone(&core).watch_pair(pair, None).await.unwrap();

	let second = refusal_message(Arc::clone(&core).watch_pair(pair, None).await);
	assert!(second.contains("already being watched"), "{second}");
	let once = refusal_message(core.sync_once(pair).await);
	assert!(once.contains("request_pass"), "{once}");
	core.request_pass(pair).await.unwrap();

	Arc::clone(&core).close().await.unwrap();
	assert!(lock(&core.slots).is_empty(), "close stops every watch");
}

#[tokio::test]
async fn a_one_shot_pass_is_refused_while_a_watch_stops() {
	let scene = Scene::new();
	let (core, pair) = scene.with_pair().await;
	let (_status_tx, status) = watch::channel(WatchStatus::default());
	lock(&core.slots).insert(
		super::pair_id_in(pair).unwrap(),
		PairSlot::Stopping {
			watch_id: 1,
			status,
		},
	);

	let refused = refusal_message(core.sync_once(pair).await);
	assert!(refused.contains("is being watched"), "{refused}");
	let not_watched = refusal_message(core.request_pass(pair).await);
	assert!(
		not_watched.contains("is not being watched"),
		"{not_watched}"
	);
	let info = core.list_pairs().await.unwrap();
	assert_eq!(info[0].activity, SyncPairActivity::Stopping);
	assert!(info[0].watch_status.is_some());
}

#[tokio::test]
async fn stopping_an_old_watch_leaves_a_newer_one_running() {
	let scene = Scene::new();
	let (core, pair) = scene.with_pair().await;
	let old = Arc::clone(&core).watch_pair(pair, None).await.unwrap();
	core.stop_watch(super::pair_id_in(pair).unwrap(), Some(old.watch_id))
		.await;
	let new = Arc::clone(&core).watch_pair(pair, None).await.unwrap();

	core.stop_watch(old.pair, Some(old.watch_id)).await;

	assert!(matches!(
		lock(&core.slots).get(&new.pair),
		Some(PairSlot::Watching(running)) if running.watch_id == new.watch_id
	));
	Arc::clone(&core).close().await.unwrap();
}

#[tokio::test]
async fn stopping_a_watch_that_is_still_starting_is_refused() {
	let scene = Scene::new();
	let (core, pair) = scene.with_pair().await;
	let pair_key = super::pair_id_in(pair).unwrap();
	lock(&core.slots).insert(pair_key, PairSlot::Starting { watch_id: 1 });

	let refused = refusal_message(core.stop_watch_for_js(pair).await);

	assert!(refused.contains("is still starting its watch"), "{refused}");
	assert!(matches!(
		lock(&core.slots).get(&pair_key),
		Some(PairSlot::Starting { watch_id: 1 })
	));
}

/// A core with one pair whose slot is reserved for a one-shot pass, as `sync_once` reserves it,
/// on a paused clock so a close's polling runs without real waits.
async fn with_reserved_pass(scene: &Scene) -> (Arc<SyncEngineCore>, u64, i64) {
	let (core, pair) = scene.with_pair().await;
	let pair_key = super::pair_id_in(pair).unwrap();
	core.reserve(pair_key, PairSlot::SyncingOnce, |_| {
		panic!("the slot was free")
	})
	.unwrap();
	tokio::time::pause();
	(core, pair, pair_key)
}

#[tokio::test]
async fn a_close_waits_for_a_reserved_pass_and_reservations_after_it_are_refused() {
	let scene = Scene::new();
	let (core, pair, pair_key) = with_reserved_pass(&scene).await;

	let closing = tokio::spawn(Arc::clone(&core).close());
	tokio::time::sleep(super::CLOSE_POLL * 3).await;
	assert!(!closing.is_finished(), "the reserved pass still runs");
	core.free_slot(pair_key, |slot| matches!(slot, PairSlot::SyncingOnce));
	closing.await.unwrap().unwrap();

	let refused = refusal_message(core.sync_once(pair).await);
	assert!(refused.contains("the sync engine is closed"), "{refused}");
	assert!(
		lock(&core.slots).is_empty(),
		"a refused call reserves nothing"
	);
}

#[tokio::test]
async fn a_second_close_returns_only_once_the_first_is_done() {
	let scene = Scene::new();
	let (core, _pair, pair_key) = with_reserved_pass(&scene).await;

	let first = tokio::spawn(Arc::clone(&core).close());
	tokio::task::yield_now().await;
	let second = tokio::spawn(Arc::clone(&core).close());
	tokio::time::sleep(super::CLOSE_POLL * 3).await;
	assert!(!first.is_finished());
	assert!(!second.is_finished(), "the second close must wait too");

	core.free_slot(pair_key, |slot| matches!(slot, PairSlot::SyncingOnce));
	first.await.unwrap().unwrap();
	second.await.unwrap().unwrap();
}

/// The live state of one engine, delivering into a channel the test reads.
fn live() -> (
	Arc<LiveState>,
	tokio::sync::mpsc::UnboundedReceiver<Delivery>,
) {
	let (deliveries, received) = unbounded_channel();
	let live = Arc::new(LiveState {
		pairs: Mutex::default(),
		recent: Mutex::default(),
		deliveries,
		passes: std::sync::atomic::AtomicU64::new(1),
	});
	(live, received)
}

fn pass_ids(received: &mut tokio::sync::mpsc::UnboundedReceiver<Delivery>) -> Vec<u64> {
	let mut ids = Vec::new();
	while let Ok(delivery) = received.try_recv() {
		let Delivery::Event(event) = delivery else {
			panic!("only events were observed")
		};
		ids.push(event.pass_id);
	}
	ids
}

fn started() -> SyncEvent {
	SyncEvent::PassStarted {
		mode: SyncMode::TwoWay,
	}
}

fn planned() -> SyncEvent {
	SyncEvent::Planned { actions: 1 }
}

fn failed() -> SyncEvent {
	SyncEvent::PassFailed {
		error: "offline".to_owned(),
		kind: ErrorKind::Reqwest,
	}
}

fn completed() -> SyncEvent {
	SyncEvent::PassCompleted {
		report: SyncReport {
			uploaded: 2,
			..SyncReport::default()
		},
	}
}

#[test]
fn a_pass_shares_one_id_and_every_start_mints_a_fresh_one() {
	let (live, mut received) = live();
	let mut observer = live.observer(1, 1, None);

	for event in [started(), planned(), completed()] {
		observer(event);
	}
	// A pass that never ended, then one that did.
	for event in [started(), planned(), started(), failed()] {
		observer(event);
	}
	// A failure with no pass started, then an event outside any pass.
	observer(failed());
	observer(planned());

	assert_eq!(pass_ids(&mut received), vec![1, 1, 1, 2, 2, 3, 3, 4, 5]);
}

#[test]
fn a_pairs_live_state_follows_its_passes() {
	let (live, _received) = live();
	let mut observer = live.observer(1, 1, None);

	observer(started());
	assert_eq!(lock(&live.pairs)[&1].pass_in_flight, Some(1));

	observer(completed());
	{
		let pairs = lock(&live.pairs);
		let pair = &pairs[&1];
		assert_eq!(pair.pass_in_flight, None);
		assert!(pair.last_pass_completed_at_ms.is_some());
		assert_eq!(pair.last_pass_error, None);
		assert_eq!(pair.last_pass_error_kind, None);
		assert_eq!(
			pair.last_report.as_ref().map(|report| report.uploaded),
			Some(2)
		);
	}

	observer(started());
	observer(failed());
	let pairs = lock(&live.pairs);
	assert_eq!(pairs[&1].pass_in_flight, None);
	assert_eq!(pairs[&1].last_pass_error.as_deref(), Some("offline"));
	assert_eq!(pairs[&1].last_pass_error_kind, Some(ErrorKind::Reqwest));
	assert_eq!(
		pairs[&1].last_report.as_ref().map(|report| report.uploaded),
		Some(2),
		"a failed pass leaves the last report alone"
	);
}

#[test]
fn the_recent_ring_keeps_the_latest_events_without_progress_or_reports() {
	let (live, _received) = live();
	let mut observer = live.observer(1, 1, None);

	observer(started());
	for index in 0..RECENT_EVENTS {
		observer(SyncEvent::Conflict {
			rel_path: format!("conflict-{index}"),
		});
		observer(SyncEvent::Progress {
			rel_path: "a".to_owned(),
			direction: TransferDirection::Upload,
			bytes: 1,
			total: 2,
		});
	}
	observer(completed());

	let recent = lock(&live.recent);
	assert_eq!(recent.len(), RECENT_EVENTS);
	assert_eq!(
		recent.front().map(|event| &event.event),
		Some(&SyncEventMessage::Conflict {
			rel_path: "conflict-0".to_owned()
		}),
		"the oldest of the ring's events is the first conflict: PassStarted fell out"
	);
	assert!(
		recent
			.iter()
			.all(|event| matches!(event.event, SyncEventMessage::Conflict { .. }))
	);
}

/// A pump over a status channel the test drives, whose loop the test ends by dropping
/// `loop_alive`.
fn pump() -> (
	watch::Sender<WatchStatus>,
	oneshot::Sender<()>,
	tokio::sync::mpsc::UnboundedReceiver<Delivery>,
	tokio::task::JoinHandle<()>,
) {
	let (status_tx, status) = watch::channel(WatchStatus::default());
	let (loop_alive, loop_gone) = oneshot::channel();
	let (deliveries, received) = unbounded_channel();
	let pumping = tokio::spawn(pump_status(
		StatusPump {
			core: std::sync::Weak::new(),
			deliveries,
			pair: 1,
			pair_id: 1,
			watch_id: 1,
			status,
		},
		loop_gone,
	));
	(status_tx, loop_alive, received, pumping)
}

async fn next_status(received: &mut tokio::sync::mpsc::UnboundedReceiver<Delivery>) -> WatchStatus {
	match received.recv().await {
		Some(Delivery::Status { status, .. }) => status,
		other => panic!("expected a status, got {other:?}"),
	}
}

#[tokio::test]
async fn a_loop_that_ends_without_a_terminal_status_is_reported_stopped() {
	let (status_tx, loop_alive, mut received, pumping) = pump();
	assert_eq!(next_status(&mut received).await, WatchStatus::default());
	status_tx.send_modify(|status| status.consecutive_failures = 1);
	assert_eq!(next_status(&mut received).await.consecutive_failures, 1);

	// The loop panicked: its observer went, its status sender (held by the watcher) did not.
	drop(loop_alive);
	pumping.await.unwrap();

	assert_eq!(
		next_status(&mut received).await,
		WatchStatus {
			consecutive_failures: 1,
			last_error: Some(LOOP_ENDED.to_owned()),
			last_error_kind: Some(ErrorKind::Internal),
			state: WatchState::Stopped,
			..WatchStatus::default()
		}
	);
	assert!(received.recv().await.is_none(), "the pump ends after it");
	drop(status_tx);
}

#[tokio::test]
async fn a_loop_that_ends_with_a_terminal_status_is_reported_as_it_said() {
	let (status_tx, loop_alive, mut received, pumping) = pump();
	assert_eq!(next_status(&mut received).await, WatchStatus::default());

	status_tx.send_modify(|status| status.state = WatchState::PairRemoved);
	drop(loop_alive);
	pumping.await.unwrap();

	assert_eq!(
		next_status(&mut received).await.state,
		WatchState::PairRemoved
	);
	assert!(received.recv().await.is_none());
}

fn action(kind: PlannedActionKind, index: usize, size: Option<u64>) -> PlannedAction {
	PlannedAction {
		kind,
		rel_path: format!("action-{index}"),
		node: PlannedNodeKind::File,
		size,
		transfer: None,
	}
}

/// A plan of `actions` uploads, numbered from 0.
fn plan_of(actions: usize) -> JsSyncPlan {
	JsSyncPlan::new(PlanOutcome {
		actions: (0..actions)
			.map(|index| action(PlannedActionKind::UploadFile, index, Some(1)))
			.collect(),
		..PlanOutcome::default()
	})
}

fn paths(actions: &[PlannedAction]) -> Vec<&str> {
	actions
		.iter()
		.map(|action| action.rel_path.as_str())
		.collect()
}

#[test]
fn a_plan_page_runs_from_its_offset_for_its_limit() {
	let plan = plan_of(1500);

	assert_eq!(
		paths(&plan.actions(1490, 3)),
		["action-1490", "action-1491", "action-1492"]
	);
	assert_eq!(
		paths(&plan.actions(1497, 3)),
		["action-1497", "action-1498", "action-1499"],
		"a page ending exactly at the end"
	);
	assert_eq!(paths(&plan.actions(1499, 10)), ["action-1499"]);
}

#[test]
fn a_plan_page_past_the_end_or_of_no_entries_is_empty() {
	let plan = plan_of(1500);

	assert!(plan.actions(1500, 10).is_empty());
	assert!(plan.actions(u64::MAX, 10).is_empty());
	assert!(plan.actions(0, 0).is_empty());
}

#[test]
fn a_plan_page_holds_at_most_a_thousand_entries() {
	let plan = plan_of(1500);

	let page = plan.actions(0, 5000);
	assert_eq!(page.len(), 1000);
	assert_eq!(
		page.last().map(|action| action.rel_path.as_str()),
		Some("action-999")
	);
	assert_eq!(plan.actions(0, u64::MAX).len(), 1000);
}

#[test]
fn a_plans_summary_counts_its_lists_and_bytes() {
	let plan = JsSyncPlan::new(PlanOutcome {
		actions: vec![
			action(PlannedActionKind::UploadFile, 0, Some(10)),
			action(PlannedActionKind::UploadFile, 1, None),
			action(PlannedActionKind::UploadFile, 2, Some(5)),
			action(PlannedActionKind::DownloadFile, 3, Some(u64::MAX)),
			action(PlannedActionKind::DownloadFile, 4, Some(1)),
			action(
				PlannedActionKind::MoveRemote {
					to: "elsewhere".to_owned(),
				},
				5,
				Some(100),
			),
		],
		held: vec![
			action(PlannedActionKind::TrashRemote, 6, Some(1)),
			action(PlannedActionKind::DeleteLocal, 7, Some(1)),
			action(PlannedActionKind::CreateLocalDir, 8, None),
		],
		held_reason: Some(GuardReason::ExceededThreshold {
			deletions: 2,
			limit: 1,
		}),
		pass_token: Some("token".to_owned()),
		..PlanOutcome::default()
	});

	assert_eq!(
		plan.summary(),
		SyncPlanSummary {
			actions_total: 6,
			upload_bytes: 15,
			download_bytes: u64::MAX,
			held_total: 3,
			held_deletions: 2,
			held_reason: Some(SyncGuardReason::ExceededThreshold {
				deletions: 2,
				limit: 1,
			}),
			pass_token: Some("token".to_owned()),
			conflicts_total: 0,
			unsyncable_total: 0,
			ignored_total: 0,
			refused: None,
		}
	);
}

#[test]
fn releasing_a_plan_empties_its_lists_and_keeps_its_summary() {
	let plan = JsSyncPlan::new(PlanOutcome {
		actions: vec![action(PlannedActionKind::UploadFile, 0, Some(1))],
		held: vec![action(PlannedActionKind::TrashRemote, 1, None)],
		conflicts: vec![PlannedConflict {
			rel_path: "conflict".to_owned(),
			local: Some(PlannedNodeKind::File),
			remote: Some(PlannedNodeKind::File),
		}],
		..PlanOutcome::default()
	});
	let summary = plan.summary();
	assert_eq!(plan.held(0, 10).len(), 1);

	plan.release();
	plan.release();

	assert_eq!(plan.summary(), summary);
	assert_eq!(summary.actions_total, 1);
	assert!(plan.actions(0, 10).is_empty());
	assert!(plan.held(0, 10).is_empty());
	assert!(plan.conflicts(0, 10).is_empty());
	assert!(plan.unsyncable(0, 10).is_empty());
	assert!(plan.ignored(0, 10).is_empty());
}

#[test]
fn a_delete_guard_ratio_above_one_is_refused() {
	let refused = refusal_message(delete_guard(3, 1.5));
	assert!(refused.contains("between 0.0 and 1.0"), "{refused}");
	assert_eq!(
		delete_guard(3, 0.25).unwrap(),
		DeleteGuard::new(3, 0.25).unwrap()
	);
}

async fn pair_info(core: &SyncEngineCore) -> SyncPairInfo {
	let mut pairs = core.list_pairs().await.unwrap();
	assert_eq!(pairs.len(), 1);
	pairs.remove(0)
}

/// A pair no pass has run for reads as not past its first pass: every pass of it is a first sync.
#[tokio::test]
async fn a_new_pair_reads_as_not_past_its_first_pass() {
	let scene = Scene::new();
	let (core, _pair) = scene.with_pair().await;
	assert!(!pair_info(&core).await.first_pass_completed);
}

#[tokio::test]
async fn a_pairs_delete_guard_reads_back_as_set() {
	let scene = Scene::new();
	let (core, pair) = scene.with_pair().await;

	core.set_delete_guard(pair, delete_guard(3, 0.25).unwrap())
		.await
		.unwrap();
	assert_eq!(
		pair_info(&core).await.delete_guard,
		DeleteGuardState::Limited {
			floor: 3,
			ratio: 0.25
		}
	);

	core.set_delete_guard(pair, DeleteGuard::unlimited())
		.await
		.unwrap();
	assert_eq!(
		pair_info(&core).await.delete_guard,
		DeleteGuardState::Unlimited
	);
}

#[tokio::test]
async fn a_pause_without_options_records_the_default() {
	let scene = Scene::new();
	let (core, pair) = scene.with_pair().await;

	core.pause_pair(pair, None).await.unwrap();

	let info = pair_info(&core).await;
	assert!(info.paused);
	assert!(info.paused_at_ms.is_some());
	assert_eq!(
		info.pause_options,
		Some(SyncPauseOptions {
			mode: PauseMode::Suspend,
			cancel_after_ms: Some(300_000),
		})
	);
}

#[tokio::test]
async fn a_pause_records_its_options_until_the_pair_resumes() {
	let scene = Scene::new();
	let (core, pair) = scene.with_pair().await;
	let options = SyncPauseOptions {
		mode: PauseMode::Cancel,
		cancel_after_ms: Some(1500),
	};

	core.pause_pair(pair, Some(options.clone())).await.unwrap();
	let paused = pair_info(&core).await;
	assert!(paused.paused);
	assert!(paused.paused_at_ms.is_some());
	assert_eq!(paused.pause_options, Some(options));

	core.resume_pair(pair).await.unwrap();
	let resumed = pair_info(&core).await;
	assert!(!resumed.paused);
	assert_eq!(resumed.paused_at_ms, None);
	assert_eq!(resumed.pause_options, None);
}

#[tokio::test]
async fn cancelling_a_suspended_pauses_actions_shows_the_pause_as_cancel() {
	let scene = Scene::new();
	let (core, pair) = scene.with_pair().await;
	core.pause_pair(pair, None).await.unwrap();
	let paused_at_ms = pair_info(&core).await.paused_at_ms;

	core.cancel_paused_actions(pair).await.unwrap();

	let info = pair_info(&core).await;
	assert_eq!(
		info.pause_options,
		Some(SyncPauseOptions {
			mode: PauseMode::Cancel,
			cancel_after_ms: Some(300_000),
		})
	);
	assert_eq!(info.paused_at_ms, paused_at_ms, "the pause's time stays");
}

#[tokio::test]
async fn a_pause_record_left_on_an_unpaused_pair_is_not_shown() {
	let scene = Scene::new();
	let (core, pair) = scene.with_pair().await;
	lock(&core.live.pairs)
		.entry(super::pair_id_in(pair).unwrap())
		.or_default()
		.pause = Some(PauseRecord {
		at_ms: 1,
		options: SyncPauseOptions {
			mode: PauseMode::Suspend,
			cancel_after_ms: None,
		},
	});

	let info = pair_info(&core).await;

	assert!(!info.paused);
	assert_eq!(info.paused_at_ms, None);
	assert_eq!(info.pause_options, None);
}

/// Hold a conflict at each of `paths` on `pair`, written to the baseline on a connection of the
/// test's own, as a pass that surfaced them would leave them.
fn hold_conflicts(scene: &Scene, pair: u64, paths: &[&str]) {
	let store = BaselineStore::open(&scene.db()).unwrap();
	let pair = super::pair_id_in(pair).unwrap();
	for path in paths {
		store
			.upsert_entry(
				pair,
				&BaselineEntry {
					rel_path: (*path).to_owned(),
					kind: NodeKind::File,
					remote_uuid: None,
					content_hash: None,
					size: None,
					local_mtime: None,
					remote_modified: None,
					state: BaselineState::Conflicted,
					local_kind: Some(NodeKind::File),
					remote_kind: None,
					remote_hash: None,
					remote_size: None,
					remote_stable_uuid: None,
					agreed_hash: None,
				},
			)
			.unwrap();
	}
}

fn conflict_paths(page: &SyncConflictPage) -> Vec<&str> {
	page.conflicts
		.iter()
		.map(|conflict| conflict.rel_path.as_str())
		.collect()
}

#[tokio::test]
async fn a_conflict_page_of_no_entries_still_counts_them_all() {
	let scene = Scene::new();
	let (core, pair) = scene.with_pair().await;
	hold_conflicts(&scene, pair, &["c", "a", "b"]);

	assert_eq!(
		core.list_conflicts(pair, 0, 0).await.unwrap(),
		SyncConflictPage {
			conflicts: Vec::new(),
			total: 3,
		}
	);
}

#[tokio::test]
async fn a_conflict_page_runs_in_path_order_from_its_offset_for_its_limit() {
	let scene = Scene::new();
	let (core, pair) = scene.with_pair().await;
	hold_conflicts(&scene, pair, &["c", "a", "b"]);

	let middle = core.list_conflicts(pair, 1, 1).await.unwrap();
	assert_eq!(conflict_paths(&middle), ["b"]);
	assert_eq!(middle.total, 3);
	let tail = core.list_conflicts(pair, 1, 10).await.unwrap();
	assert_eq!(conflict_paths(&tail), ["b", "c"], "clamped at the end");
	let past = core.list_conflicts(pair, 3, 10).await.unwrap();
	assert!(past.conflicts.is_empty());
	assert_eq!(past.total, 3);
}

#[tokio::test]
async fn a_pair_without_conflicts_lists_an_empty_page() {
	let scene = Scene::new();
	let (core, pair) = scene.with_pair().await;

	assert_eq!(
		core.list_conflicts(pair, 0, 0).await.unwrap(),
		SyncConflictPage {
			conflicts: Vec::new(),
			total: 0,
		}
	);
}

/// A filen-sync state directory whose two trees list one file alike.
fn filen_sync_state() -> tempfile::TempDir {
	let dir = tempfile::tempdir().unwrap();
	std::fs::write(
		dir.path().join("previousLocalTree"),
		"{\"prop\":\"/a.txt\",\"data\":{\"type\":\"file\",\"size\":5,\"lastModified\":1700000000123}}\n",
	)
	.unwrap();
	std::fs::write(
		dir.path().join("previousRemoteTree"),
		format!(
			"{{\"prop\":\"/a.txt\",\"data\":{{\"type\":\"file\",\"uuid\":\"{}\",\"size\":5}}}}\n",
			uuid::Uuid::from_u128(10)
		),
	)
	.unwrap();
	dir
}

fn state_dir(state: &tempfile::TempDir) -> String {
	state.path().to_string_lossy().into_owned()
}

#[tokio::test]
async fn filen_sync_state_imports_into_a_pair_that_has_not_synced() {
	let scene = Scene::new();
	let (core, pair) = scene.with_pair().await;
	let state = filen_sync_state();

	assert_eq!(
		core.import_legacy_state(pair, state_dir(&state))
			.await
			.unwrap(),
		LegacyImport {
			imported: 1,
			skipped: 0,
		}
	);
}

#[tokio::test]
async fn filen_sync_state_is_refused_once_the_pair_has_a_row() {
	let scene = Scene::new();
	let (core, pair) = scene.with_pair().await;
	hold_conflicts(&scene, pair, &["a.txt"]);
	let state = filen_sync_state();

	let refused = core
		.import_legacy_state(pair, state_dir(&state))
		.await
		.unwrap_err();

	assert_eq!(refused.kind(), ErrorKind::InvalidState);
	assert!(
		refused.to_string().contains("has already synced"),
		"{refused}"
	);
}

#[tokio::test]
async fn filen_sync_state_for_a_pair_id_past_i64_is_refused_as_unknown() {
	let scene = Scene::new();
	let (core, _) = scene.with_pair().await;
	let state = filen_sync_state();

	let refused = refusal_message(core.import_legacy_state(u64::MAX, state_dir(&state)).await);

	assert!(refused.contains("unknown sync pair"), "{refused}");
}

#[tokio::test]
async fn every_control_verb_is_refused_once_the_engine_is_closed() {
	let scene = Scene::new();
	let (core, pair) = scene.with_pair().await;
	Arc::clone(&core).close().await.unwrap();

	let refusals = [
		refusal_message(
			core.reconfigure_pair(pair, SyncMode::LocalToRemote, Backlog::Propagate)
				.await,
		),
		refusal_message(core.set_delete_guard(pair, DeleteGuard::unlimited()).await),
		refusal_message(core.pause_pair(pair, None).await),
		refusal_message(core.cancel_paused_actions(pair).await),
		refusal_message(core.resume_pair(pair).await),
		refusal_message(core.approve_deletions(pair, "token").await),
		refusal_message(core.list_conflicts(pair, 0, 10).await),
		refusal_message(
			core.resolve_conflict(pair, "a", ConflictResolution::KeepLocal)
				.await,
		),
		refusal_message(core.retry_path(pair, "a").await),
		refusal_message(core.retry_all(pair).await),
		refusal_message(core.user_ignore().await),
		refusal_message(core.set_user_ignore("*.tmp").await),
		refusal_message(core.plan_pair(pair).await),
		refusal_message(core.import_legacy_state(pair, scene.local()).await),
	];
	for refused in refusals {
		assert!(refused.contains("the sync engine is closed"), "{refused}");
	}
}
