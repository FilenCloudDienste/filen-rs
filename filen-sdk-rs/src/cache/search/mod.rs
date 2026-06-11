//! Live, windowed search over the LOCAL cache (contrast [`crate::search`], the server-side
//! hashed-substring search of the whole account: network round-trip, no live updates).
//!
//! # How it works
//!
//! [`Client::create_search`] registers the searched directory as a sync root (the [`Search`]
//! owns the [`SyncRootHandle`] internally) and spawns a dedicated engine task (one OS thread +
//! tokio runtime on native, via [`runtime::spawn_async`](crate::runtime::spawn_async)) that owns
//! a READ-ONLY connection to the cache DB. All filtering, ordering, windowing, and hydration run
//! inside SQLite — a Rust `filen_name_matches` function registered on that connection provides
//! full-Unicode substring matching — so the engine holds no result set in memory and a search
//! over a huge drive costs query time, not resident memory. Windows ([`Search::get_range`]) are
//! RAII subscriptions: the call returns the window's current snapshot, and the callback fires
//! with a FRESH [`SearchSnapshot`] whenever a re-query changes the window's contents or the
//! total.
//!
//! Cache events reach the engine as bare refresh pings: any committed batch touching the
//! subtree schedules a debounced (250 ms) re-query of the count and every window, so bursts (a
//! resync flood) coalesce. With a name needle a refresh scans the scope — substring matching
//! cannot use an index; on the order of 100–300 ms for a 100k-item drive — which the debounce
//! amortizes.
//!
//! # Name matching & normalization
//!
//! Needles are trimmed + NFC-normalized, and matched case-insensitively with Unicode SIMPLE
//! case folding unless [`SearchConfig::case_sensitive`] (the matcher compiles the needle into
//! an escaped-literal `(?i)` regex once per query — per row that is a SIMD-prefiltered
//! substring scan with no allocation and no haystack transform). Simple folding matches Greek
//! Σ/σ/ς position-independently but does not equate multi-codepoint folds (ß≠ss, İ≠i). Cached
//! names are ASSUMED to already be NFC-normalized: every current client writes NFC, but pre-v4
//! drives can hold NFD names written by old clients, which may fail to match
//! visually-identical needles — a rare edge accepted for now and gone for good once the v4
//! re-encode normalizes the whole drive.
//!
//! # Consistency model
//!
//! Results are CACHE truth, which converges to server truth: a search starts from whatever is
//! already cached and streams further results in as the cache's convergence resync lands. There
//! is no per-search "converged" signal — watch the snapshots/total grow, or observe
//! [`CacheMessage::ResyncProgress`](crate::cache::CacheMessage::ResyncProgress) on the global
//! status callback: a `Finished { converged: true }` after this search's root was listed means
//! the cache now holds a complete listing of the subtree. Queries read the committed DB
//! directly, so a delivered snapshot is exactly cache truth at query time; after the last ping
//! of a burst it can trail by at most the debounce interval.
//!
//! # Termination
//!
//! When the cache stops feeding the search — the searched directory was deleted server-side, or
//! the worker stopped ([`flush_cache`](crate::auth::Client::flush_cache) / failure) — each
//! window fires ONE final snapshot with `live: false` carrying its last-delivered results, and
//! [`Search::is_live`] flips false. The cause is deliberately ambiguous; correlating
//! [`CacheMessage::SyncRootsDeleted`](crate::cache::CacheMessage::SyncRootsDeleted) with
//! [`Search::root_uuid`] is BEST-EFFORT (the status message can be dropped under load), and
//! re-creating the search is the definitive probe (a deleted root is rejected with
//! [`CacheError::InvalidSyncRoot`](crate::cache::CacheError::InvalidSyncRoot)). A terminal
//! search keeps answering [`Search::get_range`] with direct queries over the now-frozen cache;
//! for the deleted-root cause the subtree rows are already cascade-deleted, so fresh windows
//! come back empty.
//!
//! # Cost & callback discipline
//!
//! Each live [`Search`] costs one OS thread, one tokio runtime, one SQLite read connection, and
//! one worker registration; each ping burst costs one debounced re-query of the count plus
//! every open window. Hold a FEW concurrent searches, not one per visible folder. Callbacks run
//! on the engine task: keep them cheap, never block, and NEVER move the [`Search`] (or anything
//! that owns it) into a callback — the engine owns the callbacks, so a captured Search keeps
//! its own engine alive forever. Windowless consumers can poll [`Search::total`] /
//! [`Search::is_live`].

use std::{
	ops::Range,
	sync::{
		Arc,
		atomic::{AtomicBool, AtomicUsize, Ordering},
	},
};

use uuid::Uuid;

use crate::{
	Error, ErrorKind,
	auth::Client,
	cache::{
		SyncRootHandle,
		state::{CacheEventType, GlobalEvent, SyncRootCallback},
	},
	fs::HasUUID,
};

mod config;
mod engine;
mod hydrate;
mod result;

pub use config::{SearchConfig, SearchItemType};
pub use engine::SearchWindowCallback;
pub use result::{SearchResult, SearchSnapshot};

use engine::{EngineInit, EngineMsg, SearchShared};

impl Client {
	/// Create a live, cache-backed search over the subtree rooted at `uuid` (see the
	/// [module docs](self) for the consistency model and cost).
	///
	/// Registers `uuid` as a sync root — CHEAP (zero network, zero resync) when `uuid` is
	/// already an active sync root or is covered by one (cached under it); otherwise the worker
	/// validates it remotely and runs a convergence resync of the registered roots, which can
	/// take a while on large accounts — its progress (per-root listing byte ticks) is reported
	/// as [`CacheMessage::ResyncProgress`](crate::cache::CacheMessage::ResyncProgress) on the
	/// [`configure_cache`](Client::configure_cache) status callback, keyed by root uuid. Change
	/// filters with [`Search::set_config`] — do NOT drop + recreate the search per filter
	/// change.
	///
	/// Errors if [`configure_cache`](Client::configure_cache) was never called, or if
	/// validation rejects `uuid` — downcast to [`CacheError`](crate::cache::CacheError) to
	/// branch: [`InvalidSyncRoot`](crate::cache::CacheError::InvalidSyncRoot) means the
	/// directory definitively no longer exists (do not retry);
	/// [`SyncRootUnavailable`](crate::cache::CacheError::SyncRootUnavailable) means the check
	/// itself failed (network — retry the same uuid). Must be called from within the app's
	/// Tokio runtime.
	pub async fn create_search(
		self: Arc<Self>,
		uuid: Uuid,
		config: SearchConfig,
	) -> Result<Search, Error> {
		// Register FIRST: the worker (and the DB file) is guaranteed up, and from the ack on,
		// every committed batch touching the subtree pings the engine — whose queries read the
		// committed DB directly, so batches committed before the engine starts are covered by
		// the queries themselves.
		let (ping_sender, ping_receiver) = tokio::sync::mpsc::unbounded_channel();
		let callback: SyncRootCallback = Box::new(move |events| {
			// The engine re-queries the DB on refresh, so the payloads are irrelevant — all
			// this forwards is "something in the subtree changed", and only for batches that
			// can affect results (the skipped globals never touch item rows).
			for event in events {
				if matches!(
					event.event,
					CacheEventType::NoOp
						| CacheEventType::Global(GlobalEvent::TrashEmpty)
						| CacheEventType::Global(GlobalEvent::DeleteVersioned)
				) {
					continue;
				}
				// A closed channel is the NORMAL state after `Search::close()` (or an engine
				// failure) until the worker processes the registration removal — never panic
				// here.
				let _ = ping_sender.send(());
				return;
			}
		});
		let sync_root_handle = self.clone().add_sync_root(uuid, callback).await?;

		// A live registration implies `configure_cache` ran, so the path is present; it is
		// stable for the search's lifetime (reconfiguration is rejected while a worker lives).
		let db_path = self.cache_slot.lock().await.db_path().ok_or_else(|| {
			Error::custom(
				ErrorKind::InvalidState,
				"cache is not configured; call configure_cache first",
			)
		})?;

		let (command_sender, command_receiver) = tokio::sync::mpsc::unbounded_channel();
		let shared = Arc::new(SearchShared {
			total: AtomicUsize::new(0),
			live: AtomicBool::new(true),
		});
		let (finished_sender, finished_receiver) = tokio::sync::oneshot::channel();
		let account_root: Uuid = self.root().uuid().into();
		let init = EngineInit {
			root: uuid,
			is_account_root: uuid == account_root,
			config,
			db_path,
			pings: ping_receiver,
			commands: command_receiver,
			shared: shared.clone(),
			finished: finished_sender,
		};
		crate::runtime::spawn_async(move || engine::run(init));

		Ok(Search {
			commands: command_sender,
			shared,
			root: uuid,
			_sync_root_handle: sync_root_handle,
			finished: Some(finished_receiver),
		})
	}
}

/// A live, cache-backed search: an RAII bundle of a sync-root registration (keeping the subtree
/// synced) and an engine task (owning the read-only DB connection and the window
/// subscriptions). See the [module docs](self).
///
/// Dropping the Search shuts the engine down (outstanding [`SearchWindowHandle`]s become inert)
/// and removes the sync-root registration — if it was the last registration overall, the cache
/// worker itself stops. Use [`close`](Search::close) when you need to WAIT for the teardown.
pub struct Search {
	commands: tokio::sync::mpsc::UnboundedSender<EngineMsg>,
	shared: Arc<SearchShared>,
	root: Uuid,
	/// Owned here — never inside a callback (see the module-doc footgun).
	_sync_root_handle: SyncRootHandle,
	/// `Some` until [`close`](Search::close) takes it; resolves when the engine loop has exited
	/// (its read connection already closed).
	finished: Option<tokio::sync::oneshot::Receiver<()>>,
}

impl std::fmt::Debug for Search {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("Search")
			.field("root", &self.root)
			.field("total", &self.total())
			.field("live", &self.is_live())
			.finish_non_exhaustive()
	}
}

impl Search {
	/// The searched directory's uuid — correlate with
	/// [`CacheMessage::SyncRootsDeleted`](crate::cache::CacheMessage::SyncRootsDeleted) to
	/// (best-effort) disambiguate a terminal signal.
	pub fn root_uuid(&self) -> Uuid {
		self.root
	}

	/// Total matches currently in the result set. Advisory: a cheap atomic read that may LEAD
	/// the last delivered snapshot by up to the debounce interval — each [`SearchSnapshot`]
	/// carries its own coherent total.
	pub fn total(&self) -> usize {
		self.shared.total.load(Ordering::Acquire)
	}

	/// `false` once the search went terminal (see the module docs) — it flips BEFORE the final
	/// window callbacks run, and is also forced false if the engine itself dies.
	pub fn is_live(&self) -> bool {
		self.shared.live.load(Ordering::Acquire)
	}

	/// Subscribe a window over `range` of the sorted result set. Returns the window's CURRENT
	/// snapshot plus an RAII handle; from then on `callback` fires with a fresh snapshot
	/// whenever the window's contents or the total change (the initial snapshot is NOT
	/// delivered through the callback). `range` is CLAMPED to the available results — never an
	/// error — and the window keeps its REQUESTED range, refilling as the result set grows.
	/// Errs only if the engine is gone (closed search), its DB connection failed to open, or
	/// the initial window query failed.
	pub async fn get_range(
		&self,
		range: Range<usize>,
		callback: SearchWindowCallback,
	) -> Result<(SearchSnapshot, SearchWindowHandle), Error> {
		let (reply_sender, reply_receiver) = tokio::sync::oneshot::channel();
		self.commands
			.send(EngineMsg::GetRange {
				range: range.clone(),
				callback,
				reply: reply_sender,
			})
			.map_err(|_| Error::custom(ErrorKind::Internal, "search engine has shut down"))?;
		match reply_receiver.await {
			Ok(Ok((snapshot, id))) => Ok((
				snapshot,
				SearchWindowHandle {
					id,
					requested_range: range,
					commands: self.commands.downgrade(),
				},
			)),
			Ok(Err(message)) => Err(Error::custom(
				ErrorKind::Internal,
				format!("search engine error: {message}"),
			)),
			Err(_) => Err(Error::custom(
				ErrorKind::Internal,
				"search engine exited before replying",
			)),
		}
	}

	/// Replace the filter configuration: the engine swaps its compiled filter and immediately
	/// re-queries every window on its OWN read connection — no re-registration, no network, no
	/// cache-worker interaction. This is THE way to change what a search matches; recreating
	/// the search would re-register its sync root.
	pub async fn set_config(&self, config: SearchConfig) -> Result<(), Error> {
		let (reply_sender, reply_receiver) = tokio::sync::oneshot::channel();
		self.commands
			.send(EngineMsg::SetConfig {
				config,
				reply: reply_sender,
			})
			.map_err(|_| Error::custom(ErrorKind::Internal, "search engine has shut down"))?;
		match reply_receiver.await {
			Ok(Ok(())) => Ok(()),
			Ok(Err(message)) => Err(Error::custom(
				ErrorKind::Internal,
				format!("search engine error: {message}"),
			)),
			Err(_) => Err(Error::custom(
				ErrorKind::Internal,
				"search engine exited before replying",
			)),
		}
	}

	/// Deterministic teardown: shuts the engine down (regardless of outstanding window handles
	/// — their callbacks cease and their drops become no-ops) and waits until the engine loop
	/// has exited with its read connection closed. Plain `drop` is equally CORRECT (it signals
	/// the same shutdown best-effort), just not awaitable. NOTE this resolves on engine-LOOP
	/// completion; the engine's OS thread unwinds immediately after (a true thread join would
	/// require extending the runtime module, which nothing needs today).
	pub async fn close(mut self) {
		let _ = self.commands.send(EngineMsg::Shutdown);
		if let Some(finished) = self.finished.take() {
			// Resolves Err when the engine dropped the sender — either way it has exited.
			let _ = finished.await;
		}
	}
}

impl Drop for Search {
	fn drop(&mut self) {
		// `close()` already shut the engine down if `finished` was taken. The unbounded send
		// never blocks; a send error means the engine is already gone.
		if self.finished.is_some() {
			let _ = self.commands.send(EngineMsg::Shutdown);
		}
	}
}

/// RAII window subscription returned by [`Search::get_range`]: dropping it unsubscribes the
/// window (its callback never fires again). Holds only a WEAK engine reference, so an outliving
/// handle never keeps a closed [`Search`]'s engine alive — it just becomes inert.
pub struct SearchWindowHandle {
	id: u64,
	requested_range: Range<usize>,
	commands: tokio::sync::mpsc::WeakUnboundedSender<EngineMsg>,
}

impl SearchWindowHandle {
	/// The window's REQUESTED (un-clamped) range.
	pub fn range(&self) -> Range<usize> {
		self.requested_range.clone()
	}
}

impl std::fmt::Debug for SearchWindowHandle {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("SearchWindowHandle")
			.field("id", &self.id)
			.field("requested_range", &self.requested_range)
			.finish_non_exhaustive()
	}
}

impl Drop for SearchWindowHandle {
	fn drop(&mut self) {
		if let Some(commands) = self.commands.upgrade() {
			// Best-effort: a failed send means the engine already exited.
			let _ = commands.send(EngineMsg::DropWindow(self.id));
		}
	}
}
