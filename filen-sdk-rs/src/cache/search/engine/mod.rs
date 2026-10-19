//! The per-search engine task: owns the read-only DB connection (with the `filen_name_matches`
//! function registered, see [`hydrate`]) and the window subscriptions. All filtering, ordering,
//! windowing, and hydration run inside SQLite — the engine keeps no result state beyond each
//! window's last-delivered snapshot (for equality suppression and the terminal fire). Cache
//! events reach it as bare refresh PINGS: any committed batch touching the subtree schedules a
//! debounced re-query of the count and every window.
//!
//! Hosted per platform: native spawns a dedicated thread via
//! [`runtime::spawn_async`](crate::runtime::spawn_async) (a current-thread tokio runtime, so
//! `tokio::time` drives the debounce and the engine's blocking SQLite reads are fine: nothing
//! else runs there); wasm runs the loop on the CALLING thread's local executor via
//! [`runtime::spawn_local`](crate::runtime::spawn_local) — the wasm engine never blocks (reads
//! round-trip through the worker connection ([`ReadConn`] on wasm), wasmtimer drives the
//! debounce), so a dedicated
//! per-search worker would be needless weight. FFI entry points call `create_search` on the
//! commander, so that is where the engine lives in practice.
//!
//! Channel topology: the PINGS channel's only sender lives inside the sync-root callback on the
//! cache worker — its disconnect is the TERMINAL signal (root deleted server-side, or the worker
//! stopped). The COMMANDS channel's senders are the `Search` (strong) and window handles (weak);
//! the engine exits on an explicit `Shutdown` or when commands disconnect, and keeps answering
//! queries between the terminal signal and shutdown (with the closed pings arm DISABLED —
//! polling a closed receiver in `select!` would spin hot).

use std::{
	ops::Range,
	sync::{
		Arc,
		atomic::{AtomicBool, AtomicUsize, Ordering},
	},
	time::Duration,
};

// Native-only: only the native `ReadSource` carries a path (wasm carries a `ReadTask` sender).
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
use std::path::PathBuf;

use uuid::Uuid;

use super::{
	config::{CompiledFilter, SearchConfig},
	hydrate::{self, ReadConn, Scope},
	result::SearchSnapshot,
};

// Only the gated wasm `ReadSource`/`ReadConn` newtypes carry a `ReadTask` sender.
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
use hydrate::ReadTask;

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
use tokio::time::{Instant as TimerInstant, sleep_until as timer_sleep_until};
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
use wasmtimer::{std::Instant as TimerInstant, tokio::sleep_until as timer_sleep_until};

/// Minimum refresh coalescing interval: bursts of pings (a resync flood) collapse into at most
/// one re-query per interval. A refresh re-runs every window's scan — with a needle that walks
/// the whole scope (order of 100–300 ms on a 100k-item drive), so this is deliberately coarser
/// than UI-frame latency. `GetRange`/`SetConfig` replies are exempt.
const DEBOUNCE: Duration = Duration::from_millis(250);

/// Ceiling for the ADAPTIVE debounce (see [`next_debounce`]) so a pathologically slow scan can
/// never push live updates out beyond UI-tolerable staleness.
const MAX_DEBOUNCE: Duration = Duration::from_secs(5);

/// The next ping-to-refresh delay, scaled to the last refresh's measured cost: refreshing spends
/// at most ~1/3 of the engine's time even while a resync apply pings continuously (a fixed 250ms
/// debounce under a multi-second scan re-queries back to back, churning the page cache the
/// writer needs). Clamped to [`DEBOUNCE`], [`MAX_DEBOUNCE`] — and because the deadline is always
/// armed by the next ping, the trailing refresh after a burst still fires unconditionally.
fn next_debounce(last_refresh: Duration) -> Duration {
	(last_refresh * 3).clamp(DEBOUNCE, MAX_DEBOUNCE)
}

/// Per-search cap on live windows. Every refresh re-queries ALL windows in one synchronous
/// closure on the read connection (one ReadTask on wasm — see [`Engine::refresh`]), so this
/// bounds that closure's worst-case stall (~hundreds of ms per window on a 100k-item scope).
const MAX_WINDOWS: usize = 16;

/// Per-window callback; receives a FRESH snapshot whenever the window's contents (or the total)
/// change. Runs on the engine task: keep it cheap and non-blocking, and never move the
/// [`Search`](super::Search) (or anything owning it) into one.
pub type SearchWindowCallback = Box<dyn Fn(SearchSnapshot) + Send + 'static>;

/// State mirrored out of the engine for cheap synchronous reads on [`Search`](super::Search).
pub(super) struct SearchShared {
	pub(super) total: AtomicUsize,
	pub(super) live: AtomicBool,
}

pub(super) enum EngineMsg {
	GetRange {
		range: Range<usize>,
		callback: SearchWindowCallback,
		reply: tokio::sync::oneshot::Sender<Result<(SearchSnapshot, u64), String>>,
	},
	DropWindow(u64),
	SetConfig {
		config: SearchConfig,
		reply: tokio::sync::oneshot::Sender<Result<(), String>>,
	},
	Shutdown,
}

struct Window {
	requested_range: Range<usize>,
	callback: SearchWindowCallback,
	last_delivered: SearchSnapshot,
}

/// Sets `live = false` on ANY engine exit — including an unwind — so `is_live()` can never lie
/// about a dead engine. The guard only writes the flag; it never invokes callbacks (re-entering
/// a just-panicked callback during unwind would risk a double panic → abort).
struct LiveGuard(Arc<SearchShared>);

impl Drop for LiveGuard {
	fn drop(&mut self) {
		self.0.live.store(false, Ordering::Release);
	}
}

// How the engine reads the cache DB, resolved by `create_search` per platform. A separate newtype
// per target rather than an enum, since the read path is fixed at compile time: native opens its
// own dedicated READ-ONLY connection on the `PathBuf` (WAL concurrent reads); wasm ships query
// closures to the cache worker's single connection via the `ReadTask` sender (the wasm VFS
// supports neither WAL nor a second connection — see `ReadConn`).
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
pub(super) struct ReadSource(pub(super) PathBuf);
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
pub(super) struct ReadSource(pub(super) tokio::sync::mpsc::UnboundedSender<ReadTask>);

pub(super) struct EngineInit {
	pub(super) root: Uuid,
	/// `true` when `root` IS the account root: the scope queries then skip the recursive-CTE
	/// subtree walk entirely (everything cached lives under the account root).
	pub(super) is_account_root: bool,
	pub(super) config: SearchConfig,
	pub(super) read_source: ReadSource,
	pub(super) pings: tokio::sync::mpsc::UnboundedReceiver<()>,
	pub(super) commands: tokio::sync::mpsc::UnboundedReceiver<EngineMsg>,
	pub(super) shared: Arc<SearchShared>,
	/// Dropped (after the DB connection) when the loop exits — `Search::close()` awaits it.
	pub(super) finished: tokio::sync::oneshot::Sender<()>,
}

struct Engine {
	root: Uuid,
	is_account_root: bool,
	filter: CompiledFilter,
	conn: Option<ReadConn>,
	/// `Some` when opening the connection failed terminally — replied to gets/set_configs so the
	/// failure is visible.
	build_error: Option<String>,
	pings: Option<tokio::sync::mpsc::UnboundedReceiver<()>>,
	windows: std::collections::HashMap<u64, Window>,
	next_window_id: u64,
	shared: Arc<SearchShared>,
	live: bool,
	/// Measured cost of the most recent refresh — drives [`next_debounce`].
	last_refresh: Duration,
}

pub(super) async fn run(init: EngineInit) {
	let EngineInit {
		root,
		is_account_root,
		config,
		read_source,
		pings,
		mut commands,
		shared,
		finished,
	} = init;
	let _live_guard = LiveGuard(shared.clone());

	let mut engine = Engine {
		root,
		is_account_root,
		filter: CompiledFilter::compile(&config),
		conn: None,
		build_error: None,
		pings: Some(pings),
		windows: std::collections::HashMap::new(),
		next_window_id: 0,
		shared,
		live: true,
		last_refresh: Duration::ZERO,
	};
	engine.build(read_source).await;

	let mut debounce_deadline: Option<TimerInstant> = None;
	loop {
		tokio::select! {
			biased;
			command = commands.recv() => {
				match command {
					None | Some(EngineMsg::Shutdown) => break,
					Some(EngineMsg::GetRange { range, callback, reply }) => {
						let _ = reply.send(engine.add_window(range, callback).await);
					}
					Some(EngineMsg::DropWindow(id)) => {
						engine.windows.remove(&id);
					}
					Some(EngineMsg::SetConfig { config, reply }) => {
						let result = engine.set_config(&config);
						// Refresh every window against the new filter right away —
						// `set_config` semantics stay crisp, no debounce lag.
						if result.is_ok() {
							engine.refresh().await;
							debounce_deadline = None;
						}
						let _ = reply.send(result);
					}
				}
			}
			ping = recv_ping(&mut engine.pings), if engine.pings.is_some() => {
				match ping {
					Some(()) => {
						if debounce_deadline.is_none() {
							debounce_deadline =
								Some(TimerInstant::now() + next_debounce(engine.last_refresh));
						}
					}
					None => {
						// TERMINAL: worker dropped our registration (root deleted
						// server-side) or stopped entirely. Disable the pings arm so the
						// closed receiver does not spin the select loop hot.
						engine.pings = None;
						engine.go_terminal();
						debounce_deadline = None;
					}
				}
			}
			_ = sleep_until(debounce_deadline), if debounce_deadline.is_some() => {
				engine.refresh().await;
				debounce_deadline = None;
			}
		}
	}

	// Deterministic teardown order: close the read connection BEFORE signalling completion, so
	// a resolved `close()` means "callbacks ceased AND the connection is closed".
	drop(engine);
	drop(finished);
}

async fn recv_ping(pings: &mut Option<tokio::sync::mpsc::UnboundedReceiver<()>>) -> Option<()> {
	match pings {
		Some(receiver) => receiver.recv().await,
		// Unreachable: the select arm is guarded by `pings.is_some()`.
		None => None,
	}
}

async fn sleep_until(deadline: Option<TimerInstant>) {
	// The `None` case is unreachable: the select arm is guarded by `deadline.is_some()`.
	if let Some(deadline) = deadline {
		timer_sleep_until(deadline).await;
	}
}

impl Engine {
	/// The query scope, derived per use because [`set_config`](Self::set_config) can flip
	/// `recursive`.
	fn scope(&self) -> Scope {
		if !self.filter.recursive {
			Scope::Children(self.root)
		} else if self.is_account_root {
			Scope::Account(self.root)
		} else {
			Scope::Subtree(self.root)
		}
	}

	/// Resolve the read path and prime the shared total. There is no index to build: queries
	/// always read the CURRENT committed DB, so batches committed before this point are covered
	/// by the queries themselves and later ones arrive as refresh pings.
	async fn build(&mut self, read_source: ReadSource) {
		#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
		let conn = match hydrate::open_read_connection(&read_source.0) {
			Ok(conn) => ReadConn(conn),
			Err(e) => {
				tracing::error!("search engine failed to open its read connection: {e}");
				self.build_error = Some(e.to_string());
				return;
			}
		};
		#[cfg(all(target_family = "wasm", target_os = "unknown"))]
		let conn = ReadConn(read_source.0);
		let scope = self.scope();
		let filter = self.filter.clone();
		match conn
			.run(move |conn| hydrate::count_results(conn, scope, &filter))
			.await
		{
			Ok(total) => {
				self.shared.total.store(total, Ordering::Release);
			}
			Err(e) => {
				// TRANSIENT, not terminal: on the worker path the likeliest cause is a reply
				// timeout while the worker applies a heavy burst (first sync!) — latching
				// `build_error` here would brick the search on a one-off slow reply. Keep the
				// connection; `total` stays 0 until the first ping/getRange re-queries. (A
				// connection that cannot OPEN is the terminal case, handled above.)
				tracing::warn!("search engine failed its initial count (will retry on use): {e}");
			}
		}
		self.conn = Some(conn);
	}

	fn set_config(&mut self, config: &SearchConfig) -> Result<(), String> {
		if self.conn.is_none() {
			return Err(self
				.build_error
				.clone()
				.unwrap_or_else(|| "search connection unavailable".to_string()));
		}
		self.filter = CompiledFilter::compile(config);
		Ok(())
	}

	async fn snapshot(&self, range: &Range<usize>) -> rusqlite::Result<SearchSnapshot> {
		let Some(conn) = &self.conn else {
			return Ok(SearchSnapshot {
				results: Vec::new(),
				total: 0,
				live: self.live,
			});
		};
		let scope = self.scope();
		let filter = self.filter.clone();
		let range = range.clone();
		let (results, total) = conn
			.run(move |conn| {
				// One read transaction so a fallback count (empty page) reads the same DB state
				// as the window scan (see `refresh` for the torn-snapshot rationale).
				let tx = conn.unchecked_transaction()?;
				hydrate::window_and_count(&tx, scope, &filter, &range)
			})
			.await?;
		Ok(SearchSnapshot {
			results,
			total,
			live: self.live,
		})
	}

	async fn add_window(
		&mut self,
		range: Range<usize>,
		callback: SearchWindowCallback,
	) -> Result<(SearchSnapshot, u64), String> {
		if self.conn.is_none() {
			return Err(self
				.build_error
				.clone()
				.unwrap_or_else(|| "search connection unavailable".to_string()));
		}
		// Bounds the refresh batch: all windows re-query inside ONE synchronous closure on the
		// cache worker (wasm), so an unbounded window count would stall the worker — long
		// enough to starve the drive-lock keep-alive during a resync. A UI needs a handful.
		if self.windows.len() >= MAX_WINDOWS {
			return Err(format!(
				"too many live search windows (max {MAX_WINDOWS}); drop unused window handles"
			));
		}
		let snapshot = self.snapshot(&range).await.map_err(|e| e.to_string())?;
		let id = self.next_window_id;
		self.next_window_id += 1;
		self.windows.insert(
			id,
			Window {
				requested_range: range,
				callback,
				last_delivered: snapshot.clone(),
			},
		);
		Ok((snapshot, id))
	}

	/// Re-query the count plus EVERY window and fire the callbacks whose snapshots changed
	/// (identical snapshots are suppressed). There is no dirt tracking — a ping just means
	/// "something in the subtree changed" and the queries re-read committed truth. A failed
	/// query (e.g. a busy timeout) keeps that window's last-delivered snapshot; the next ping
	/// retries. A panicking callback is caught (mirroring the cache worker's dispatch) and its
	/// window dropped — it never kills the engine.
	async fn refresh(&mut self) {
		let Some(conn) = &self.conn else {
			return;
		};
		let refresh_started = TimerInstant::now();
		let scope = self.scope();
		let filter = self.filter.clone();
		let window_ranges: Vec<(u64, Range<usize>)> = self
			.windows
			.iter()
			.map(|(id, window)| (*id, window.requested_range.clone()))
			.collect();
		// ONE round-trip for the count plus every window, inside one read transaction: on the
		// wasm worker path the connection commits event batches BETWEEN tasks, so split queries
		// could pair a `total` from one DB state with `results` from another (and the equality
		// suppression below would then sit on torn data); the transaction gives the same
		// guarantee against the concurrent WAL writer on the native direct path.
		let batch = conn
			.run(move |conn| {
				let tx = conn.unchecked_transaction()?;
				// Each window scan carries the match total for free (`count(*) OVER ()`), so a
				// refresh costs W scans instead of W+1 — and ONE instead of two for the common
				// single-window search. A dedicated count runs only when no window exists.
				let mut total: Option<usize> = None;
				let mut windows = Vec::with_capacity(window_ranges.len());
				for (id, range) in window_ranges {
					let results = match hydrate::window_and_count(&tx, scope, &filter, &range) {
						Ok((results, window_total)) => {
							total.get_or_insert(window_total);
							Ok(results)
						}
						Err(e) => Err(e),
					};
					windows.push((id, results));
				}
				let total = match total {
					Some(total) => total,
					None => hydrate::count_results(&tx, scope, &filter)?,
				};
				Ok((total, windows))
			})
			.await;
		let (total, window_results) = match batch {
			Ok(batch) => batch,
			Err(e) => {
				tracing::warn!("search refresh failed: {e}");
				self.last_refresh = refresh_started.elapsed();
				return;
			}
		};
		self.shared.total.store(total, Ordering::Release);
		let mut poisoned: Vec<u64> = Vec::new();
		for (id, results) in window_results {
			let results = match results {
				Ok(results) => results,
				Err(e) => {
					tracing::warn!("search refresh failed querying window {id}: {e}");
					continue;
				}
			};
			let snapshot = SearchSnapshot {
				results,
				total,
				live: self.live,
			};
			let window = self.windows.get_mut(&id).expect("window present");
			if snapshot.results == window.last_delivered.results
				&& snapshot.total == window.last_delivered.total
				&& snapshot.live == window.last_delivered.live
			{
				continue;
			}
			window.last_delivered = snapshot.clone();
			let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
				(window.callback)(snapshot);
			}));
			if result.is_err() {
				tracing::error!("search window {id} callback panicked; dropping the window");
				poisoned.push(id);
			}
		}
		for id in poisoned {
			self.windows.remove(&id);
		}
		self.last_refresh = refresh_started.elapsed();
	}

	/// Re-sends LAST-DELIVERED results (no re-query) with `live: false` — in the deleted-root
	/// case the subtree rows are already cascade-deleted, so a re-query would deliver a false
	/// empty "final" view.
	fn go_terminal(&mut self) {
		if !self.live {
			return;
		}
		self.live = false;
		self.shared.live.store(false, Ordering::Release);
		let mut poisoned: Vec<u64> = Vec::new();
		for (id, window) in &mut self.windows {
			window.last_delivered.live = false;
			let snapshot = window.last_delivered.clone();
			let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
				(window.callback)(snapshot);
			}));
			if result.is_err() {
				tracing::error!("search window {id} callback panicked; dropping the window");
				poisoned.push(*id);
			}
		}
		for id in poisoned {
			self.windows.remove(&id);
		}
	}
}

#[cfg(test)]
mod tests;
