//! Live tests of the sync engine's mobile binding: `JsClient::open_sync_engine` and the objects it
//! hands out, driven only through what the binding exports, the way the app's JS drives them.
//!
//! Run: `cargo test -p filen-sdk-rs -F uniffi,sync-engine --test sync_engine_js_tests --
//! --test-threads=1` (needs TEST_EMAIL / TEST_PASSWORD, and TEST_SHARE_EMAIL /
//! TEST_SHARE_PASSWORD for the second account a refused open uses).

use std::{
	borrow::Cow,
	collections::BTreeSet,
	path::{Path, PathBuf},
	sync::{Arc, Mutex},
};

use filen_macros::shared_test_runtime;
use filen_sdk_rs::{
	Error, ErrorKind,
	auth::{Client, JsClient, http::JsClientConfig, js_impls::UnauthJsClient},
	cache::js_impl::{CacheStatusListener, CacheStatusMessage, ResyncProgressMessage},
	fs::{
		HasName, HasUUID,
		categories::{DirType, Normal},
	},
	sync_engine::{
		Backlog, ConflictResolution, FullPassReason, IgnoreLevel, JsSyncEngine, PauseMode,
		PlannedConflict, PlannedNodeKind, SyncEngineListener, SyncEventMessage, SyncMode,
		SyncPassEvent, SyncRefusal, WatchState, WatchStatus,
	},
};
use uuid::Uuid;

mod helpers;
use helpers::{CACHE_CONVERGE_TIMEOUT, poll_for_item, poll_until};

/// A `JsClient` for `base`'s account, made the way the app restores a session.
fn js_client(base: &Client) -> JsClient {
	let config = JsClientConfig {
		concurrency: None,
		rate_limit_per_sec: None,
		upload_bandwidth_kilobytes_per_sec: None,
		download_bandwidth_kilobytes_per_sec: None,
		log_level: None,
		file_io_memory_budget: None,
		thumbnail_mem_budget: None,
		thumbnail_max_source_bytes: None,
		thumbnail_decode_concurrency: None,
		#[cfg(feature = "archive")]
		archive_codec_mem_budget: None,
		#[cfg(feature = "archive")]
		archive_job_concurrency: None,
		batch_window_ms: None,
	};
	UnauthJsClient::from_config(config)
		.unwrap()
		.from_stringified(base.to_stringified())
		.unwrap()
}

fn path_string(path: &Path) -> String {
	path.to_str().unwrap().to_owned()
}

/// Every cache status message, in the order the cache sent them.
#[derive(Default)]
struct CacheLog(Mutex<Vec<CacheStatusMessage>>);

impl CacheStatusListener for CacheLog {
	fn on_messages(&self, messages: Vec<CacheStatusMessage>) {
		self.0.lock().unwrap().extend(messages);
	}
}

impl CacheLog {
	/// Whether a resync covering `root` has committed a converged listing. Resyncs run one at a
	/// time, so the `Finished` after a `Started` is that resync's.
	fn converged_on(&self, root: Uuid) -> bool {
		let mut covers_root = false;
		for message in self.0.lock().unwrap().iter() {
			match message {
				CacheStatusMessage::ResyncProgress {
					progress: ResyncProgressMessage::Started { roots },
				} => covers_root = roots.contains(&root),
				CacheStatusMessage::ResyncProgress {
					progress: ResyncProgressMessage::Finished { converged: true },
				} if covers_root => return true,
				_ => {}
			}
		}
		false
	}
}

/// Every delivery the engine's listener got, in the order it got them.
#[derive(Default)]
struct Recorder {
	events: Mutex<Vec<SyncPassEvent>>,
	statuses: Mutex<Vec<(u64, WatchStatus)>>,
}

impl SyncEngineListener for Recorder {
	fn on_events(&self, events: Vec<SyncPassEvent>) {
		self.events.lock().unwrap().extend(events);
	}

	fn on_watch_status(&self, pair_id: u64, status: WatchStatus) {
		self.statuses.lock().unwrap().push((pair_id, status));
	}
}

impl Recorder {
	fn events(&self) -> Vec<SyncPassEvent> {
		self.events.lock().unwrap().clone()
	}

	fn delivered(&self) -> usize {
		self.events.lock().unwrap().len()
	}

	/// Pass `pass_id`'s events.
	fn pass(&self, pass_id: u64) -> Vec<SyncPassEvent> {
		self.events()
			.into_iter()
			.filter(|event| event.pass_id == pass_id)
			.collect()
	}

	/// The first `PassCompleted` delivered at index `from` or later that `accept` takes, waiting
	/// for it as long as a pass can wait for the drive lock.
	async fn completed(
		&self,
		from: usize,
		accept: impl Fn(&SyncEventMessage) -> bool,
	) -> SyncPassEvent {
		let mut found = None;
		let arrived = poll_until(CACHE_CONVERGE_TIMEOUT, || {
			found = self.events.lock().unwrap()[from..]
				.iter()
				.find(|event| {
					matches!(event.event, SyncEventMessage::PassCompleted { .. })
						&& accept(&event.event)
				})
				.cloned();
			found.is_some()
		})
		.await;
		assert!(
			arrived,
			"no such pass completed; delivered since: {:?}",
			&self.events()[from..]
		);
		found.unwrap()
	}
}

/// The rel paths of `events` of one kind.
fn paths(events: &[SyncPassEvent], pick: impl Fn(&SyncEventMessage) -> Option<&str>) -> Vec<&str> {
	events
		.iter()
		.filter_map(|event| pick(&event.event))
		.collect()
}

/// A fresh remote directory (deleted when this drops), a local root and the engine's files in a
/// temp directory, an engine open through the binding with one two-way pair between them, and the
/// pair's cache converged on the remote directory.
struct Scene {
	resources: test_utils::TestResources,
	dir: tempfile::TempDir,
	listener: Arc<Recorder>,
	engine: JsSyncEngine,
	pair: u64,
}

impl Scene {
	async fn new() -> Self {
		let resources = test_utils::RESOURCES.get_resources().await;
		let remote = resources.dir.uuid();
		// The cache refuses a root the server does not show yet, and the engine then only warns.
		assert!(
			resources.wait_visible().await,
			"the server never showed the new test directory"
		);
		let dir = tempfile::tempdir().unwrap();
		std::fs::create_dir(dir.path().join("local")).unwrap();
		let client = js_client(&resources.client);
		let cache = Arc::new(CacheLog::default());
		client
			.configure_cache(path_string(&dir.path().join("cache.db")), cache.clone())
			.await
			.unwrap();
		let listener = Arc::new(Recorder::default());
		let engine = client
			.open_sync_engine(
				path_string(&dir.path().join("baseline.db")),
				listener.clone(),
			)
			.await
			.unwrap();
		let pair = engine
			.add_pair(
				path_string(&dir.path().join("local")),
				remote,
				SyncMode::TwoWay,
			)
			.await
			.unwrap();
		assert!(
			poll_until(CACHE_CONVERGE_TIMEOUT, || cache.converged_on(remote)).await,
			"the cache never converged on the test directory"
		);
		Self {
			resources,
			dir,
			listener,
			engine,
			pair,
		}
	}

	fn local(&self) -> PathBuf {
		self.dir.path().join("local")
	}

	fn write(&self, rel_path: &str, bytes: &[u8]) {
		let path = self.local().join(rel_path);
		std::fs::create_dir_all(path.parent().unwrap()).unwrap();
		std::fs::write(path, bytes).unwrap();
	}

	/// The names directly in the remote directory.
	async fn remote_names(&self) -> BTreeSet<String> {
		let (dirs, files) = self
			.resources
			.client
			.list_dir(
				&DirType::<Normal>::Dir(Cow::Borrowed(&self.resources.dir)),
				None::<&fn(u64, Option<u64>)>,
			)
			.await
			.unwrap();
		dirs.iter()
			.filter_map(HasName::name)
			.chain(files.iter().filter_map(HasName::name))
			.map(str::to_owned)
			.collect()
	}
}

fn strings<'a>(items: impl IntoIterator<Item = &'a str>) -> BTreeSet<String> {
	items.into_iter().map(str::to_owned).collect()
}

#[shared_test_runtime]
async fn a_dry_run_is_read_a_page_at_a_time_names_what_the_user_ignores_and_releases() {
	let scene = Scene::new().await;
	scene.write("a.txt", b"alpha");
	scene.write("held.txt", b"base");
	scene.write("docs/b.txt", b"bravo!");
	scene.write("skip.tmp", b"scratch");
	scene
		.engine
		.set_user_ignore("*.tmp".to_owned())
		.await
		.unwrap();
	assert_eq!(scene.engine.user_ignore().await.unwrap(), "*.tmp");

	let plan = scene.engine.plan_pair(scene.pair).await.unwrap();
	let summary = plan.summary();
	assert_eq!(summary.actions_total, 4, "{summary:?}");
	assert_eq!(summary.upload_bytes, 5 + 4 + 6, "{summary:?}");
	assert_eq!(
		(
			summary.ignored_total,
			summary.conflicts_total,
			summary.held_total
		),
		(1, 0, 0),
		"{summary:?}"
	);
	let first = plan.actions(0, 3);
	let rest = plan.actions(3, 3);
	assert_eq!((first.len(), rest.len()), (3, 1));
	assert!(plan.actions(4, 3).is_empty(), "a page past the end");
	assert_eq!(
		first
			.iter()
			.chain(&rest)
			.map(ToString::to_string)
			.collect::<BTreeSet<_>>(),
		strings([
			"create remote dir \"docs\"",
			"upload file \"a.txt\"",
			"upload file \"held.txt\"",
			"upload file \"docs/b.txt\"",
		])
	);
	let ignored = plan.ignored(0, 10);
	assert_eq!(ignored.len(), 1, "{ignored:?}");
	assert_eq!(
		(
			ignored[0].rel_path.as_str(),
			&ignored[0].level,
			ignored[0].pattern.as_str()
		),
		("skip.tmp", &IgnoreLevel::User, "*.tmp")
	);
	plan.release();
	assert!(
		plan.actions(0, 10).is_empty(),
		"a released plan keeps a list"
	);
	assert_eq!(plan.summary(), summary, "release changed the summary");
	assert!(
		scene.remote_names().await.is_empty(),
		"a dry run changed the remote side"
	);
	scene.engine.close().await.unwrap();
}

#[shared_test_runtime]
async fn a_one_shot_pass_is_announced_in_order_and_reported() {
	let scene = Scene::new().await;
	scene.write("a.txt", b"alpha");
	scene.write("docs/b.txt", b"bravo!");

	let from = scene.listener.delivered();
	let report = scene.engine.sync_once(scene.pair).await.unwrap();
	assert_eq!(
		(report.uploaded, report.remote_dirs_created),
		(2, 1),
		"{report:?}"
	);
	assert!(
		report.errors.is_empty() && report.pass_error.is_none(),
		"{report:?}"
	);
	let done = scene.listener.completed(from, |_| true).await;
	let events = scene.listener.pass(done.pass_id);
	assert!(
		events.iter().all(|event| event.pair_id == scene.pair),
		"{events:?}"
	);
	assert_eq!(
		events.first().map(|event| &event.event),
		Some(&SyncEventMessage::PassStarted {
			mode: SyncMode::TwoWay
		})
	);
	assert_eq!(
		events.last().map(|event| &event.event),
		Some(&SyncEventMessage::PassCompleted {
			report: report.clone()
		})
	);
	let planned = events
		.iter()
		.position(|event| event.event == SyncEventMessage::Planned { actions: 3 })
		.expect("the pass never said what it planned");
	let first_upload = events
		.iter()
		.position(|event| matches!(event.event, SyncEventMessage::Uploading { .. }))
		.expect("the pass announced no upload");
	assert!(planned < first_upload, "{events:?}");
	assert_eq!(
		strings(paths(&events, |event| match event {
			SyncEventMessage::Uploading { rel_path } => Some(rel_path),
			_ => None,
		})),
		strings(["a.txt", "docs/b.txt"])
	);
	assert_eq!(
		paths(&events, |event| match event {
			SyncEventMessage::CreatingRemoteDir { rel_path } => Some(rel_path),
			_ => None,
		}),
		["docs"]
	);

	// What a screen mounted after the pass reads back.
	assert_eq!(
		scene.engine.last_report(scene.pair).await.unwrap(),
		Some(report)
	);
	let recent = scene.engine.recent_events().await.unwrap();
	assert!(
		recent.iter().any(|event| event.pass_id == done.pass_id
			&& matches!(event.event, SyncEventMessage::PassStarted { .. })),
		"{recent:?}"
	);
	assert!(
		!recent.iter().any(|event| matches!(
			event.event,
			SyncEventMessage::Progress { .. } | SyncEventMessage::PassCompleted { .. }
		)),
		"{recent:?}"
	);
	assert_eq!(scene.remote_names().await, strings(["a.txt", "docs"]));
	let pairs = scene.engine.list_pairs().await.unwrap();
	assert_eq!(pairs.len(), 1, "{pairs:?}");
	assert_eq!(
		(
			pairs[0].id,
			pairs[0].remote_root,
			pairs[0].paused,
			pairs[0].pass_in_flight
		),
		(scene.pair, scene.resources.dir.uuid(), false, None),
		"{pairs:?}"
	);
	assert!(pairs[0].last_pass_completed_at_ms.is_some(), "{pairs:?}");
	scene.engine.close().await.unwrap();
}

#[shared_test_runtime]
async fn a_paused_pair_s_pass_does_nothing_and_announces_nothing_until_resumed() {
	let scene = Scene::new().await;
	scene.engine.pause_pair(scene.pair, None).await.unwrap();
	let paused = &scene.engine.list_pairs().await.unwrap()[0];
	assert!(paused.paused && paused.paused_at_ms.is_some(), "{paused:?}");
	assert_eq!(
		paused
			.pause_options
			.as_ref()
			.map(|options| (options.mode, options.cancel_after_ms)),
		Some((PauseMode::Suspend, Some(300_000)))
	);
	scene.write("a.txt", b"alpha, written while paused");
	let from = scene.listener.delivered();
	let report = scene.engine.sync_once(scene.pair).await.unwrap();
	assert!(report.paused && report.uploaded == 0, "{report:?}");

	scene.engine.resume_pair(scene.pair).await.unwrap();
	let resumed = &scene.engine.list_pairs().await.unwrap()[0];
	assert!(
		!resumed.paused && resumed.pause_options.is_none(),
		"{resumed:?}"
	);
	let report = scene.engine.sync_once(scene.pair).await.unwrap();
	assert_eq!(report.uploaded, 1, "{report:?}");
	// Deliveries keep their order, so the resumed pass's events coming first proves the paused
	// one announced nothing.
	let done = scene.listener.completed(from, |_| true).await;
	assert_eq!(
		done.event,
		SyncEventMessage::PassCompleted { report },
		"the paused pass announced its completion"
	);
	let first = scene.listener.events()[from].clone();
	assert_eq!(
		(first.pass_id, first.event),
		(
			done.pass_id,
			SyncEventMessage::PassStarted {
				mode: SyncMode::TwoWay
			}
		),
		"the paused pass announced something"
	);
	scene.engine.close().await.unwrap();
}

#[shared_test_runtime]
async fn a_file_both_sides_edit_is_held_as_a_conflict() {
	let scene = Scene::new().await;
	scene.write("held.txt", b"base");
	let report = scene.engine.sync_once(scene.pair).await.unwrap();
	assert_eq!(report.uploaded, 1, "{report:?}");

	scene.write("held.txt", b"local edit");
	let builder = scene
		.resources
		.client
		.make_file_builder("held.txt", scene.resources.dir.uuid())
		.unwrap();
	let remote_edit = scene
		.resources
		.client
		.upload_file(builder, b"remote edit".as_slice())
		.await
		.unwrap();
	assert!(
		poll_for_item(
			&scene.dir.path().join("cache.db"),
			remote_edit.uuid(),
			CACHE_CONVERGE_TIMEOUT
		)
		.await,
		"the cache never saw the remote edit"
	);
	let from = scene.listener.delivered();
	let report = scene.engine.sync_once(scene.pair).await.unwrap();
	let held = PlannedConflict {
		rel_path: "held.txt".to_owned(),
		local: Some(PlannedNodeKind::File),
		remote: Some(PlannedNodeKind::File),
	};
	assert_eq!(
		(
			report.uploaded,
			report.conflicts_total,
			report.conflicts.as_slice()
		),
		(0, 1, [held.clone()].as_slice()),
		"{report:?}"
	);
	let done = scene.listener.completed(from, |_| true).await;
	assert!(
		scene
			.listener
			.pass(done.pass_id)
			.iter()
			.any(|event| event.event
				== SyncEventMessage::Conflict {
					rel_path: "held.txt".to_owned()
				}),
		"the conflict was never announced"
	);
	let conflicts = scene
		.engine
		.list_conflicts(scene.pair, 0, 10)
		.await
		.unwrap();
	assert_eq!(
		(conflicts.total, conflicts.conflicts.as_slice()),
		(1, [held].as_slice())
	);
	let count_only = scene.engine.list_conflicts(scene.pair, 0, 0).await.unwrap();
	assert_eq!(
		(count_only.total, count_only.conflicts.len()),
		(1, 0),
		"limit 0 gives only the total"
	);
	scene.engine.close().await.unwrap();
}

#[shared_test_runtime]
async fn removing_a_pair_forgets_it_and_leaves_both_sides_alone() {
	let scene = Scene::new().await;
	scene.write("docs/b.txt", b"bravo!");
	let report = scene.engine.sync_once(scene.pair).await.unwrap();
	assert_eq!(report.uploaded, 1, "{report:?}");

	scene.engine.remove_pair(scene.pair).await.unwrap();
	assert!(scene.engine.list_pairs().await.unwrap().is_empty());
	assert!(scene.local().join("docs/b.txt").exists());
	assert_eq!(scene.remote_names().await, strings(["docs"]));
	scene.engine.close().await.unwrap();
}

#[shared_test_runtime]
async fn a_watch_syncs_a_local_change_and_a_requested_pass_then_reports_stopped() {
	let scene = Scene::new().await;
	scene.write("first.txt", b"first");
	let watch = scene.engine.watch_pair(scene.pair, None).await.unwrap();
	assert_eq!(watch.pair_id(), scene.pair);
	let uploads_one = |event: &SyncEventMessage| matches!(event, SyncEventMessage::PassCompleted { report } if report.uploaded == 1);
	scene.listener.completed(0, uploads_one).await;

	// While watched, the pair takes no second watch and no one-shot pass.
	let second = scene.engine.watch_pair(scene.pair, None).await;
	assert_eq!(
		second.err().map(|error| error.kind()),
		Some(ErrorKind::InvalidState)
	);
	let once = scene.engine.sync_once(scene.pair).await;
	assert_eq!(
		once.err().map(|error| error.kind()),
		Some(ErrorKind::InvalidState)
	);

	// A file written into the local root is picked up by the watcher, with no call.
	let from = scene.listener.delivered();
	scene.write("second.txt", b"second");
	scene.listener.completed(from, uploads_one).await;
	assert_eq!(
		scene.remote_names().await,
		strings(["first.txt", "second.txt"])
	);

	// A requested pass reads both sides whole.
	let from = scene.listener.delivered();
	scene.engine.request_pass(scene.pair).await.unwrap();
	let requested = scene
		.listener
		.completed(from, |event| {
			matches!(event, SyncEventMessage::PassCompleted { report }
				if report.full_pass_reason == Some(FullPassReason::Requested.to_string()))
		})
		.await;
	let SyncEventMessage::PassCompleted { report } = &requested.event else {
		unreachable!("completed() returns a PassCompleted");
	};
	assert!(
		report.errors.is_empty() && report.uploaded == 0,
		"{report:?}"
	);

	let events = scene.listener.events();
	assert!(
		events.iter().all(|event| event.pair_id == scene.pair),
		"{events:?}"
	);
	let started: Vec<u64> = events
		.iter()
		.filter(|event| matches!(event.event, SyncEventMessage::PassStarted { .. }))
		.map(|event| event.pass_id)
		.collect();
	assert!(
		started.len() >= 3 && started.is_sorted_by(|a, b| a < b),
		"pass ids must increase: {started:?}"
	);

	// Stopped, the watch says so, and so does the last status the listener got.
	watch.stop().await.unwrap();
	assert_eq!(watch.status().state, WatchState::Stopped);
	let stopped = poll_until(CACHE_CONVERGE_TIMEOUT, || {
		scene
			.listener
			.statuses
			.lock()
			.unwrap()
			.last()
			.is_some_and(|(_, status)| status.state == WatchState::Stopped)
	})
	.await;
	let statuses = scene.listener.statuses.lock().unwrap().clone();
	assert!(stopped, "{statuses:?}");
	assert!(
		statuses.iter().all(|(pair_id, _)| *pair_id == scene.pair),
		"{statuses:?}"
	);
	assert_eq!(
		statuses.first().map(|(_, status)| &status.state),
		Some(&WatchState::Running)
	);
	assert_eq!(
		scene.engine.list_pairs().await.unwrap()[0].watch_status,
		None,
		"a stopped watch still shows on its pair"
	);
	scene.engine.close().await.unwrap();
}

/// Assert `result` is the refusal every verb of a closed engine gets.
fn assert_closed<T: std::fmt::Debug>(verb: &str, result: Result<T, Error>) {
	let error = result.expect_err(verb);
	assert_eq!(error.kind(), ErrorKind::InvalidState, "{verb}: {error}");
	assert!(
		error.to_string().contains("the sync engine is closed"),
		"{verb}: {error}"
	);
}

/// An engine open through the binding on a baseline in a fresh temp directory, which the caller
/// keeps alive, with no pairs.
async fn bare_engine(dir: &tempfile::TempDir) -> JsSyncEngine {
	let base = test_utils::RESOURCES.client().await;
	js_client(&base)
		.open_sync_engine(
			path_string(&dir.path().join("baseline.db")),
			Arc::new(Recorder::default()),
		)
		.await
		.unwrap()
}

#[shared_test_runtime]
async fn an_open_baseline_refuses_another_account() {
	let dir = tempfile::tempdir().unwrap();
	let engine = bare_engine(&dir).await;

	let share = test_utils::SHARE_RESOURCES.client().await;
	let db = path_string(&dir.path().join("baseline.db"));
	let refused = js_client(&share)
		.open_sync_engine(db.clone(), Arc::new(Recorder::default()))
		.await
		.err()
		.expect("a second account opened a baseline the first has open");
	assert_eq!(
		refused.sync_refusal(),
		Some(SyncRefusal::BaselineInUse { path: db })
	);
	engine.close().await.unwrap();
}

#[shared_test_runtime]
async fn a_closed_engine_refuses_every_verb() {
	let dir = tempfile::tempdir().unwrap();
	let engine = bare_engine(&dir).await;

	engine.close().await.unwrap();
	engine.close().await.expect("close is idempotent");
	let pair = 1;
	let local = path_string(dir.path());
	assert_closed("list_pairs", engine.list_pairs().await);
	assert_closed("last_report", engine.last_report(pair).await);
	assert_closed("recent_events", engine.recent_events().await);
	assert_closed(
		"add_pair",
		engine
			.add_pair(local, Uuid::from_u128(1), SyncMode::TwoWay)
			.await,
	);
	assert_closed("remove_pair", engine.remove_pair(pair).await);
	assert_closed(
		"watch_pair",
		engine.watch_pair(pair, None).await.map(|_| ()),
	);
	assert_closed("stop_watch", engine.stop_watch(pair).await);
	assert_closed("sync_once", engine.sync_once(pair).await);
	assert_closed("request_pass", engine.request_pass(pair).await);
	assert_closed(
		"reconfigure_pair",
		engine
			.reconfigure_pair(pair, SyncMode::TwoWay, Backlog::Propagate)
			.await,
	);
	assert_closed(
		"set_delete_guard",
		engine.set_delete_guard(pair, 10, 0.5).await,
	);
	assert_closed(
		"set_delete_guard_unlimited",
		engine.set_delete_guard_unlimited(pair).await,
	);
	assert_closed("pause_pair", engine.pause_pair(pair, None).await);
	assert_closed(
		"cancel_paused_actions",
		engine.cancel_paused_actions(pair).await,
	);
	assert_closed("resume_pair", engine.resume_pair(pair).await);
	assert_closed(
		"approve_deletions",
		engine.approve_deletions(pair, "token".to_owned()).await,
	);
	assert_closed("list_conflicts", engine.list_conflicts(pair, 0, 10).await);
	assert_closed(
		"resolve_conflict",
		engine
			.resolve_conflict(pair, "a.txt".to_owned(), ConflictResolution::KeepLocal)
			.await,
	);
	assert_closed(
		"retry_path",
		engine.retry_path(pair, "a.txt".to_owned()).await,
	);
	assert_closed("user_ignore", engine.user_ignore().await);
	assert_closed(
		"set_user_ignore",
		engine.set_user_ignore("*.tmp".to_owned()).await,
	);
	assert_closed("plan_pair", engine.plan_pair(pair).await.map(|_| ()));
}
