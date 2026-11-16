//! Stall probes and the time budgets built on them.
//!
//! Measured on the Mac that hosts the VM: UTM runs QEMU's threads at macOS's background priority
//! (4), so a host job that keeps the cores busy starves the guest of CPU and of disk. Beside a
//! ten-core host job QEMU got under 2 % of a core and pushing the harness took 16 minutes instead
//! of one; at lighter loads the provider's probes stopped for up to 13 s while a scenario process
//! kept running, and single dumps of the fake drive (a 47 KB file) took 27 s and 47 s. A budget
//! measured in wall time then runs out while the process waited for could not run, which failed
//! checks the provider passes; so budgets count only the time the waiting process ran.
//!
//! - A probe thread that only sleeps reports each wake-up later than [`LATE`]: in the provider as
//!   an event, in a scenario process on its stderr (its log). A probe late in every process at once
//!   is the guest stopping; the provider's runtime probe late alone is its async workers held up,
//!   which is a provider bug, and no budget forgives it.
//! - [`Budget`] counts a wait's steps, each at most [`LATE`], so a stretch the waiting process did
//!   not run costs one step; the wait still ends after a bounded number of steps.
//! - [`Timer`] measures a span less what the probe found stalled meanwhile, for checks of how
//!   long something took.

use std::{
	sync::atomic::{AtomicU64, Ordering},
	thread,
	time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use tracing::info;

/// How long each probe sleeps.
const TICK: Duration = Duration::from_millis(100);
/// How much later than its tick a probe may wake before it reports; the most a step of a
/// [`Budget`] counts.
const LATE: Duration = Duration::from_secs(1);

/// The time this process's probe found it stalled, in milliseconds.
static STALLED_MS: AtomicU64 = AtomicU64::new(0);

/// The wall clock in milliseconds, which lines up the reports of several processes.
pub(crate) fn unix_ms() -> u128 {
	SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.map_or(0, |since| since.as_millis())
}

/// How much later than `TICK` after `started` the probe woke, when that is at least [`LATE`].
fn late(started: Instant) -> Option<Duration> {
	started
		.elapsed()
		.checked_sub(TICK)
		.filter(|late| *late >= LATE)
}

fn stalled() -> Duration {
	Duration::from_millis(STALLED_MS.load(Ordering::Relaxed))
}

/// Starts a thread that reports each late wake-up, as a provider event when `events`, otherwise
/// on stderr (the process's log), and counts it for [`Timer`].
pub(crate) fn watch_thread(events: bool) {
	let spawned = thread::Builder::new()
		.name("stall-probe".to_owned())
		.spawn(move || {
			loop {
				let started = Instant::now();
				thread::sleep(TICK);
				if let Some(late) = late(started) {
					let late_ms = u64::try_from(late.as_millis()).unwrap_or(u64::MAX);
					STALLED_MS.fetch_add(late_ms, Ordering::Relaxed);
					let unix_ms = unix_ms();
					if events {
						info!(target: "vm_harness", probe = "thread", late_ms, %unix_ms, "stall");
					} else {
						eprintln!("STALL thread late {late_ms} ms at {unix_ms}");
					}
				}
			}
		});
	if let Err(error) = spawned {
		eprintln!("starting the stall probe failed: {error}");
	}
}

/// Starts a task on `runtime` that reports each late wake-up as a provider event.
pub(crate) fn watch_runtime(runtime: &tokio::runtime::Handle) {
	runtime.spawn(async {
		loop {
			let started = Instant::now();
			tokio::time::sleep(TICK).await;
			if let Some(late) = late(started) {
				let late_ms = u64::try_from(late.as_millis()).unwrap_or(u64::MAX);
				let unix_ms = unix_ms();
				info!(target: "vm_harness", probe = "runtime", late_ms, %unix_ms, "stall");
			}
		}
	});
}

/// A wait's budget, counted in steps of at most [`LATE`].
pub(crate) struct Budget {
	limit: Duration,
	used: Duration,
	last: Instant,
}

impl Budget {
	pub(crate) fn new(limit: Duration) -> Self {
		Self {
			limit,
			used: Duration::ZERO,
			last: Instant::now(),
		}
	}

	/// Counts the step since the last call, and says whether the budget is spent.
	pub(crate) fn spent(&mut self) -> bool {
		let now = Instant::now();
		self.used += now.duration_since(self.last).min(LATE);
		self.last = now;
		self.used >= self.limit
	}
}

/// A span's start, to measure it less the time the probe found this process stalled.
pub(crate) struct Timer {
	started: Instant,
	stalled: Duration,
}

impl Timer {
	pub(crate) fn start() -> Self {
		Self {
			started: Instant::now(),
			stalled: stalled(),
		}
	}

	/// The time since the start that this process ran. It first waits two ticks, so a stall that
	/// just ended is counted: the probe wakes up at most a tick after the guest runs again.
	pub(crate) fn running(&self) -> Duration {
		thread::sleep(TICK * 2);
		let elapsed = self.started.elapsed().saturating_sub(TICK * 2);
		elapsed.saturating_sub(stalled().saturating_sub(self.stalled))
	}
}
