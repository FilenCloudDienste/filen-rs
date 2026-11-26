//! Keeps the sync root's watcher running. `notify` reports some failures and, on Windows, stops
//! watching silently on others and ignores a lost-events overflow; so a failed watcher is restarted
//! after the sync engine's watch backoff, and every [`HelperConfig::watch_rearm`](crate::HelperConfig::watch_rearm) a new
//! watcher replaces the old one, started before the old one is dropped so no change falls between
//! them. Each new watcher is followed by a walk of the whole root, which finds what the old one
//! missed; the engine's own safety-net walk runs besides.
//!
//! A watcher start blocks in the OS and cannot be called off, so the keeper is never aborted: it is
//! asked to stop, and stopping waits for a start in flight, then drops what it started.

use std::{
	path::PathBuf,
	sync::{
		Arc,
		atomic::{AtomicBool, Ordering},
	},
	time::Duration,
};

use filen_sdk_rs::sync_engine::watch_backoff;
use tokio::{
	sync::{mpsc, watch},
	task::JoinHandle,
	time::{Instant, sleep, sleep_until},
};
use tracing::warn;

use crate::{
	helper::{
		blocking,
		events::Out,
		platform::Platform,
		protocol::{OutMessage, PhaseName, StatusEvent},
	},
	os::{LocalEvent, WatchSignal},
};

/// How long a watcher runs before its failure counts as the first of a new run of failures.
const HEALTHY: Duration = Duration::from_secs(60);

/// Returns once the keeper is asked to stop, or its handle is gone.
async fn stop_asked(stop: &mut watch::Receiver<bool>) {
	// An error means the handle is gone, which stops the keeper too.
	let _ = stop.wait_for(|stop| *stop).await;
}

/// Waits out the backoff after one more failure in a row, counted in `failures`; `false` when the
/// keeper was asked to stop meanwhile.
async fn back_off(failures: &mut u32, stop: &mut watch::Receiver<bool>) -> bool {
	*failures = failures.saturating_add(1);
	let wait = watch_backoff(*failures).expect("a failure was just counted");
	tokio::select! {
		() = sleep(wait) => true,
		() = stop_asked(stop) => false,
	}
}

/// What the keeper works with.
pub(crate) struct Keeper<P> {
	// Reason for the Arc: each blocking start takes a handle.
	pub(crate) platform: Arc<P>,
	pub(crate) root: PathBuf,
	/// Where each event goes: the local engine.
	pub(crate) on_event: Arc<dyn Fn(LocalEvent) + Send + Sync>,
	/// Whether a watcher runs, for `getState`.
	pub(crate) watching: Arc<AtomicBool>,
	/// The status sent when the watcher goes up or down, its `watching` set then, and its state
	/// [`PhaseName::Held`] while `held` is set.
	pub(crate) status: StatusEvent,
	/// Whether the served root is held: the command loop sets it as it holds the root or lifts the
	/// hold, and sends a status of its own then.
	pub(crate) held: Arc<AtomicBool>,
	pub(crate) out: Out,
	pub(crate) rearm: Duration,
}

/// A running keeper. Dropping it asks the keeper to stop without waiting; [`Keeping::stop`] waits.
pub(crate) struct Keeping {
	stop: watch::Sender<bool>,
	task: JoinHandle<()>,
}

impl Keeping {
	/// Stops the keeper and returns once every watcher it started, one still starting included, is
	/// dropped.
	pub(crate) async fn stop(self) {
		self.stop.send_replace(true);
		if let Err(error) = self.task.await {
			warn!(%error, "the sync root's watch keeper ended abnormally");
		}
	}
}

/// Starts keeping the root watched.
pub(crate) fn keep_watching<P: Platform>(keeper: Keeper<P>) -> Keeping {
	let (stop, stopping) = watch::channel(false);
	Keeping {
		stop,
		task: tokio::spawn(run(keeper, stopping)),
	}
}

impl<P: Platform> Keeper<P> {
	/// Starts a watcher whose signals carry `generation`. Runs to its end once started: the OS's
	/// start cannot be called off, and what it starts must be dropped by the keeper.
	async fn start(
		&self,
		generation: u64,
		signals: &mpsc::UnboundedSender<(u64, WatchSignal)>,
	) -> Option<P::Watcher> {
		let platform = Arc::clone(&self.platform);
		let root = self.root.clone();
		let signals = signals.clone();
		let started = blocking(move || {
			platform.watch(
				&root,
				Arc::new(move |signal| {
					// The keeper is gone only once the root is no longer served.
					let _ = signals.send((generation, signal));
				}),
			)
		})
		.await;
		started
			.inspect_err(|error| warn!(%error, "the sync root's watcher did not start"))
			.ok()
	}

	fn set_watching(&self, watching: bool) {
		if self.watching.swap(watching, Ordering::Relaxed) != watching {
			// The writer is gone only once the helper stops.
			let state = if self.held.load(Ordering::Relaxed) {
				PhaseName::Held
			} else {
				self.status.state
			};
			let _ = self.out.send(OutMessage::Status(StatusEvent {
				state,
				watching,
				..self.status.clone()
			}));
		}
	}
}

async fn run<P: Platform>(keeper: Keeper<P>, mut stop: watch::Receiver<bool>) {
	let (signals, mut received) = mpsc::unbounded_channel();
	let mut generation = 0;
	// Failures in a row: of starts, and of watchers that failed before they ran for [`HEALTHY`].
	let mut failures = 0_u32;
	let mut watcher: Option<P::Watcher> = None;
	let mut started_at = Instant::now();
	let mut rearm_at = Instant::now() + keeper.rearm;
	loop {
		if watcher.is_none() {
			generation += 1;
			let started = keeper.start(generation, &signals).await;
			if *stop.borrow() {
				return;
			}
			match started {
				Some(started) => {
					watcher = Some(started);
					keeper.set_watching(true);
					if generation > 1 {
						(keeper.on_event)(LocalEvent::Rescan);
					}
					started_at = Instant::now();
					rearm_at = started_at + keeper.rearm;
				}
				None => {
					keeper.set_watching(false);
					if !back_off(&mut failures, &mut stop).await {
						return;
					}
					continue;
				}
			}
		}
		tokio::select! {
			() = stop_asked(&mut stop) => return,
			Some((from, signal)) = received.recv() => match signal {
				WatchSignal::Event(event) => (keeper.on_event)(event),
				WatchSignal::Failed(error) if from == generation => {
					warn!(%error, "the sync root's watcher failed; starting it again");
					drop(watcher.take());
					keeper.set_watching(false);
					if started_at.elapsed() >= HEALTHY {
						failures = 0;
					}
					if !back_off(&mut failures, &mut stop).await {
						return;
					}
				}
				// A watcher already replaced.
				WatchSignal::Failed(_) => {}
			},
			() = sleep_until(rearm_at) => {
				generation += 1;
				let fresh = keeper.start(generation, &signals).await;
				if *stop.borrow() {
					return;
				}
				match fresh {
					Some(fresh) => {
						// The new one runs before the old one stops: a change in between is seen
						// twice at worst, which is harmless.
						drop(watcher.replace(fresh));
						(keeper.on_event)(LocalEvent::Rescan);
						failures = 0;
						started_at = Instant::now();
					}
					None => warn!("re-arming the sync root's watcher failed; keeping the old one"),
				}
				rearm_at = Instant::now() + keeper.rearm;
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use std::sync::Mutex;

	use filen_types::fs::Uuid;

	use super::*;
	use crate::{
		helper::protocol::PhaseName,
		identity::PlaceholderIdentity,
		test_support::{FakeLocal, FakePlatform, PlatformCall, root_path},
	};

	struct Watched {
		platform: FakePlatform,
		events: Arc<Mutex<Vec<LocalEvent>>>,
		lines: mpsc::UnboundedReceiver<OutMessage>,
		keeping: Keeping,
	}

	fn watched(rearm: Duration) -> Watched {
		let platform = FakePlatform::new(FakeLocal::new(PlaceholderIdentity::Root {
			root: Uuid::from_u128(0x100),
		}));
		watched_on(platform, rearm)
	}

	fn watched_on(platform: FakePlatform, rearm: Duration) -> Watched {
		let events = Arc::new(Mutex::new(Vec::new()));
		let recorded = Arc::clone(&events);
		let (out, lines) = mpsc::unbounded_channel();
		let keeping = keep_watching(Keeper {
			platform: Arc::new(platform.clone()),
			root: PathBuf::from("/r"),
			on_event: Arc::new(move |event| recorded.lock().unwrap().push(event)),
			watching: Arc::new(AtomicBool::new(false)),
			status: StatusEvent {
				state: PhaseName::Serving,
				user: None,
				root: None,
				watching: false,
			},
			held: Arc::new(AtomicBool::new(false)),
			out,
			rearm,
		});
		Watched {
			platform,
			events,
			lines,
			keeping,
		}
	}

	impl Watched {
		fn events(&self) -> Vec<LocalEvent> {
			self.events.lock().unwrap().clone()
		}

		fn watching_lines(&mut self) -> Vec<bool> {
			let mut lines = Vec::new();
			while let Ok(OutMessage::Status(status)) = self.lines.try_recv() {
				lines.push(status.watching);
			}
			lines
		}
	}

	const LONG: Duration = Duration::from_secs(3600);

	#[tokio::test(start_paused = true)]
	async fn a_failed_watcher_is_started_again_after_the_backoff_and_the_root_walked() {
		let watched = watched(LONG);
		sleep(Duration::from_millis(10)).await;
		assert_eq!(watched.platform.watch_starts(), 1);
		watched.platform.fail_watcher();
		sleep(Duration::from_millis(1900)).await;
		assert_eq!(watched.platform.watch_starts(), 1);
		assert_eq!(watched.platform.live_watchers(), 0);
		sleep(Duration::from_millis(200)).await;
		assert_eq!(watched.platform.watch_starts(), 2);
		assert_eq!(watched.platform.live_watchers(), 1);
		assert_eq!(watched.events(), [LocalEvent::Rescan]);
	}

	#[tokio::test(start_paused = true)]
	async fn a_watcher_that_went_deaf_is_replaced_at_the_rearm() {
		let watched = watched(Duration::from_secs(5));
		sleep(Duration::from_millis(10)).await;
		watched.platform.deafen_watchers();
		watched.platform.local().user_create(&root_path("a"), b"a");
		watched.platform.deliver();
		assert!(watched.events().is_empty());
		sleep(Duration::from_secs(5)).await;
		assert_eq!(watched.platform.live_watchers(), 1);
		assert_eq!(watched.events(), [LocalEvent::Rescan]);
		watched.platform.local().user_create(&root_path("b"), b"b");
		watched.platform.deliver();
		sleep(Duration::from_millis(10)).await;
		assert_eq!(
			watched.events()[1..],
			[
				LocalEvent::Created(root_path("b")),
				LocalEvent::Modified(root_path("b")),
			]
		);
	}

	#[tokio::test(start_paused = true)]
	async fn a_watcher_that_will_not_start_is_retried_with_a_doubling_backoff() {
		let platform = FakePlatform::new(FakeLocal::new(PlaceholderIdentity::Root {
			root: Uuid::from_u128(0x100),
		}));
		platform.fail_watch_starts(3);
		let mut watched = watched_on(platform, LONG);
		// 2 s, 4 s, 8 s.
		sleep(Duration::from_millis(13_900)).await;
		assert_eq!(watched.platform.live_watchers(), 0);
		sleep(Duration::from_millis(200)).await;
		assert_eq!(watched.platform.live_watchers(), 1);
		assert_eq!(watched.watching_lines(), [true]);
		assert_eq!(watched.events(), [LocalEvent::Rescan]);
	}

	#[tokio::test(start_paused = true)]
	async fn a_watcher_going_down_and_up_is_reported() {
		let mut watched = watched(LONG);
		sleep(Duration::from_millis(10)).await;
		watched.platform.fail_watcher();
		sleep(Duration::from_secs(3)).await;
		assert_eq!(watched.watching_lines(), [true, false, true]);
	}

	#[tokio::test(start_paused = true)]
	async fn a_watcher_that_ran_a_while_restarts_after_the_first_backoff_again() {
		let watched = watched(LONG);
		sleep(Duration::from_millis(10)).await;
		// Three quick failures in a row: 2 s, 4 s, 8 s.
		for wait in [2, 4, 8] {
			watched.platform.fail_watcher();
			sleep(Duration::from_millis(wait * 1000 + 100)).await;
		}
		assert_eq!(watched.platform.watch_starts(), 4);
		sleep(HEALTHY).await;
		watched.platform.fail_watcher();
		sleep(Duration::from_millis(2100)).await;
		assert_eq!(watched.platform.watch_starts(), 5);
	}

	#[tokio::test(start_paused = true)]
	async fn stopping_the_keeper_drops_its_watcher() {
		let watched = watched(LONG);
		sleep(Duration::from_millis(10)).await;
		assert_eq!(watched.platform.live_watchers(), 1);
		watched.keeping.stop().await;
		assert_eq!(watched.platform.live_watchers(), 0);
	}

	// Real time, not paused: a start held in the blocking pool keeps paused time from advancing.
	// Nothing here waits on time.
	#[tokio::test]
	async fn stopping_the_keeper_waits_for_a_start_in_flight_and_drops_what_it_started() {
		let platform = FakePlatform::new(FakeLocal::new(PlaceholderIdentity::Root {
			root: Uuid::from_u128(0x100),
		}));
		let gate = platform.hold_watch_starts();
		let watched = watched_on(platform.clone(), LONG);
		{
			let gate = gate.clone();
			blocking(move || gate.wait_entered()).await;
		}
		let mut stopping = tokio::spawn(watched.keeping.stop());
		// The runtime has one thread: the stop runs as far as it can while this task yields.
		for _ in 0..100 {
			tokio::task::yield_now().await;
		}
		let returned_early = stopping.is_finished();
		let calls_then = platform.calls();
		// Opened before any assertion, so a failing one cannot leave the start blocking the
		// runtime's shutdown.
		gate.open();
		(&mut stopping).await.unwrap();
		assert!(
			!returned_early,
			"the stop returned with a watcher still starting"
		);
		assert!(calls_then.is_empty());
		assert_eq!(
			platform.calls(),
			[PlatformCall::Watch, PlatformCall::Unwatch]
		);
		assert_eq!(platform.live_watchers(), 0);
	}

	#[tokio::test(start_paused = true)]
	async fn a_keeper_backing_off_stops_at_once() {
		let platform = FakePlatform::new(FakeLocal::new(PlaceholderIdentity::Root {
			root: Uuid::from_u128(0x100),
		}));
		platform.fail_watch_starts(1);
		let watched = watched_on(platform, LONG);
		sleep(Duration::from_millis(10)).await;
		let started = Instant::now();
		watched.keeping.stop().await;
		assert_eq!(started.elapsed(), Duration::ZERO);
		assert_eq!(watched.platform.watch_starts(), 0);
	}
}
