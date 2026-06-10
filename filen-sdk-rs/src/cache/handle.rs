use std::{collections::HashMap, path::PathBuf, sync::Arc, thread::JoinHandle};

use crate::{
	Error, ErrorKind,
	auth::Client,
	fs::HasUUID,
	io::{RemoteDirectory, RemoteFile},
	socket::ListenerHandle,
};
use crossbeam::channel::Sender;
use uuid::Uuid;

use crate::cache::{
	CacheControlMessage, CacheError, CacheState, SyncRootCallback,
	state::{CacheThreadEvent, ManualEvent},
};

pub struct CacheHandle {
	/// `Some` until [`shutdown`](CacheHandle::shutdown) takes it to join the worker; `Drop` only
	/// best-effort signals when it is still present.
	task_handle: Option<JoinHandle<()>>,
	control_sender: Sender<CacheControlMessage>,
	manual_event_sender: Sender<CacheThreadEvent>,
	_listener_handle: ListenerHandle,
}

#[derive(Debug)]
pub enum CacheMessage {
	/// Non-fatal errors surfaced by the worker. The worker keeps running after emitting these — they are
	/// informational, not a shutdown signal — though repeated errors may warrant the app restarting it.
	Error(Vec<CacheError>),
	/// One or more configured sync roots were deleted server-side (a `Removed` of the root node, or a
	/// cascade when an ancestor was deleted or moved out). They have been dropped from the active set; the
	/// app must re-issue [`add_sync_root`](CacheHandle::add_sync_root) to resume syncing them.
	SyncRootsDeleted(Vec<Uuid>),
}

impl CacheHandle {
	/// Open the cache and start the worker. `sync_roots` configures which directories are synced and the
	/// per-root notification callbacks — an EMPTY list caches nothing until
	/// [`add_sync_root`](CacheHandle::add_sync_root); pass `(account_root_uuid, callback)` to sync the
	/// whole account. The worker resyncs the configured roots on startup (the gap-check).
	pub async fn new(
		client: Arc<Client>,
		cache_path: PathBuf,
		sync_roots: Vec<(Uuid, SyncRootCallback)>,
		status_event_callback: impl Fn(Vec<CacheMessage>) + Send + 'static,
	) -> Result<Self, Error> {
		let (res_sender, res_receiver) = tokio::sync::oneshot::channel();
		let (msg_sender, mut msg_receiver) = tokio::sync::mpsc::channel(100);

		let root_uuid = client.root().uuid().into();
		let sync_roots: HashMap<Uuid, SyncRootCallback> = sync_roots.into_iter().collect();
		// Capture the app's runtime handle here (this fn runs inside it) so the worker `std::thread` can
		// `block_on` the async resync (which needs an async runtime, but the worker is a plain thread).
		// The worker owns its own `Arc<Client>` clone; the original stays on this task for
		// `add_event_listener` below.
		let rt_handle = tokio::runtime::Handle::current();
		let worker_client = client.clone();
		let handle = std::thread::spawn(move || {
			let state = match CacheState::new(
				&cache_path,
				root_uuid,
				sync_roots,
				msg_sender,
				worker_client,
				rt_handle,
			) {
				Ok((state, callback, control_sender, event_sender)) => {
					if res_sender
						.send(Ok((callback, control_sender, event_sender)))
						.is_err()
					{
						// The `CacheHandle::new` future was dropped (e.g. cancelled) before it received the
						// init result, so nobody is waiting. Exit the worker cleanly instead of panicking.
						log::warn!(
							"cache init result receiver dropped before init completed; worker exiting"
						);
						return;
					}
					state
				}
				Err(e) => {
					if res_sender.send(Err(e)).is_err() {
						log::warn!(
							"cache init result receiver dropped before init failed; worker exiting"
						);
					}
					return;
				}
			};

			state.run();
		});

		// Bridge the worker's status channel to the app callback. The JoinHandle is intentionally
		// dropped (detached): the loop ends on its own when the worker drops `msg_sender` — on shutdown
		// or if the worker thread panics — at which point `recv()` returns `None`.
		tokio::task::spawn(async move {
			while let Some(msg) = msg_receiver.recv().await {
				status_event_callback(msg);
			}
		});

		let (callback, control_sender, manual_event_sender) =
			res_receiver.await.map_err(|_| {
				Error::custom(
					ErrorKind::Internal,
					"cache worker thread exited before initialization completed",
				)
			})??;

		// need to track all event types to make sure we don't miss any so we can increment global_message_id correctly
		let listener_handle = client.add_event_listener(callback, None).await?;

		Ok(Self {
			task_handle: Some(handle),
			_listener_handle: listener_handle,
			control_sender,
			manual_event_sender,
		})
	}

	/// Start syncing `uuid` (with its notification `callback`) and converge it. The worker resyncs the
	/// new root before resuming live event application. Re-adding an already-configured `uuid` REPLACES
	/// its callback and triggers another full resync — there is currently no lighter callback-only update.
	pub fn add_sync_root(&self, uuid: Uuid, callback: SyncRootCallback) -> Result<(), Error> {
		self.control_sender
			.try_send(CacheControlMessage::AddSyncRoot { uuid, callback })
			.map_err(|_| {
				Error::custom(
					ErrorKind::Internal,
					"cache control channel closed (AddSyncRoot); worker has shut down",
				)
			})
	}

	/// Stop syncing `uuid`; if `evict`, also delete its cached subtree (protecting any still-active
	/// nested root).
	pub fn remove_sync_root(&self, uuid: Uuid, evict: bool) -> Result<(), Error> {
		self.control_sender
			.try_send(CacheControlMessage::RemoveSyncRoot { uuid, evict })
			.map_err(|_| {
				Error::custom(
					ErrorKind::Internal,
					"cache control channel closed (RemoveSyncRoot); worker has shut down",
				)
			})
	}

	/// Cleanly stop the cache worker and wait for it to finish.
	///
	/// Signals shutdown, then blocks (off the async executor, via `spawn_blocking`) until the worker
	/// has drained its buffered events into the durable `events` store, applied them, and CLOSED the
	/// SQLite connection. Call this on app close/suspend — before the process pauses or another
	/// [`CacheHandle`] reopens the same DB file — so the DB is fully flushed and there is never a window
	/// with two workers writing one file.
	///
	/// This is NOT required for correctness: an un-joined drop (or an outright process kill) is
	/// recovered on the next startup by the gap-check — the watermark was never advanced for any
	/// un-drained event, so the remote drive id reads ahead of it and triggers a catch-up resync. It
	/// only makes shutdown deterministic.
	pub async fn shutdown(mut self) {
		let control = self.control_sender.clone();
		let task_handle = self.task_handle.take();
		// Send + join together off the async executor; a crossbeam join is blocking.
		let joined = tokio::task::spawn_blocking(move || {
			// If the worker already exited, the send errors and the join returns immediately.
			let _ = control.send(CacheControlMessage::Shutdown);
			if let Some(task_handle) = task_handle {
				return task_handle.join();
			}
			Ok(())
		})
		.await;
		match joined {
			Ok(Ok(())) => {}
			Ok(Err(_)) => log::error!("cache worker thread panicked during shutdown"),
			Err(e) => log::error!("cache shutdown join task failed: {e}"),
		}
	}

	/// Inject a recursive directory listing into the cache.
	///
	/// LEGACY initial-population path: it is upsert-only (it never deletes vanished items) and is
	/// applied WITHOUT watermark gating, so using it as a *live* refresh can resurrect items that
	/// socket events already deleted. The resync diff supersedes it — prefer it for initial population
	/// only.
	pub async fn update_list_dir_recursive(
		&self,
		dirs: Vec<RemoteDirectory>,
		files: Vec<RemoteFile>,
	) -> Result<(), Error> {
		let event = CacheThreadEvent::Manual(ManualEvent::ListDirRecursive(dirs, files));
		// The worker's event channel is UNBOUNDED, so `send` never blocks — it only errors if the worker
		// has shut down (receiver dropped). It is therefore safe to call directly on the async executor
		// (no need to offload to a blocking task). `map_err(|_| ...)` drops the
		// (large) un-sent event held by `SendError` so the `Err` stays small.
		self.manual_event_sender.send(event).map_err(|_| {
			Error::custom(
				ErrorKind::Internal,
				"Failed to send manual event to cache thread (channel closed)",
			)
		})
	}
}

impl Drop for CacheHandle {
	fn drop(&mut self) {
		// If `shutdown` already ran it took `task_handle` and joined the worker, so there is nothing to
		// do. Otherwise best-effort signal the worker to stop (we do NOT join here — `Drop` is sync and
		// must not block an async executor; an un-joined worker is recovered by the next startup
		// gap-check). The control channel is unbounded, so `send` never blocks and only fails if the
		// worker already exited (receiver dropped) — which is fine, nothing left to stop.
		if self.task_handle.is_some() {
			let _ = self.control_sender.send(CacheControlMessage::Shutdown);
		}
	}
}
