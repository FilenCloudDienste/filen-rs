use std::{
	sync::{Arc, Mutex, mpsc},
	time::{Duration, Instant},
};

use tokio::sync::{mpsc::unbounded_channel, oneshot, watch};

use super::{
	JsSyncEngine, LOOP_ENDED, LiveState, OPEN_ENGINES, PairSlot, RECENT_EVENTS, StatusPump,
	SyncEngineCore, claim_key,
	delivery::Delivery,
	lock, pump_status,
	types::{SyncEventMessage, SyncPairActivity},
};
use crate::{
	Error, ErrorKind,
	auth::{
		Client,
		test_support::{offline_client, offline_client_with_root},
	},
	sync_engine::{
		BaselineInUse, SyncEvent, SyncMode, SyncReport, TransferDirection, WatchState, WatchStatus,
	},
};

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
		SyncEngineCore::open(Arc::new(client), self.db(), recording().0).await
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
	let core = SyncEngineCore::open(Arc::new(offline_client()), scene.db(), first_listener)
		.await
		.unwrap();
	let (second_listener, second) = recording();
	let attached = SyncEngineCore::open(Arc::new(offline_client()), scene.db(), second_listener)
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
