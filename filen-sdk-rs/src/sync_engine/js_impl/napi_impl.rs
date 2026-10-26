//! The sync engine's napi exports the macros do not write: `JsClient.openSyncEngine` with its
//! listener, and `defaultSyncIgnorePatterns` (`FilenSdkError.syncRefusal` is with the class, in
//! `js::napi`).
//!
//! The listener is one weak threadsafe function per open, carrying whole chunks, so events and
//! statuses reach JS in the order the delivery thread made them. Only the delivery thread calls
//! it, in Blocking mode on a small queue: while the JS thread is busy the delivery thread waits
//! and the next drain coalesces what piled up, while the passes never wait on it. On the JS
//! thread it calls the listener's `onEvents` and `onWatchStatus`, bound to the listener and read
//! once when it is handed over. As under UniFFI, a listener method that throws is logged and loses
//! only the deliveries it carried: the rest of its chunk is still delivered.
//!
//! An engine belongs to the Node environment that opened it, and closes with it: each environment
//! that opens one gets one cleanup hook, which closes every engine it opened that is still open
//! (see [`close_env_engines`]). One hook per environment rather than per engine, so nothing is
//! left to unregister when an engine closes: napi-rs's `remove_env_cleanup_hook` would leak the
//! hook's boxed data on every open and close, and must be called on the JS thread, which an
//! engine's close does not run on.

use std::{
	collections::{HashMap, hash_map::Entry},
	panic::{AssertUnwindSafe, catch_unwind},
	sync::{
		Arc, LazyLock, Mutex, Weak,
		atomic::{AtomicBool, AtomicU64, Ordering},
		mpsc::{RecvTimeoutError, channel},
	},
	time::Duration,
};

use napi::{
	Env, Status,
	bindgen_prelude::{BigInt, FnArgs, Function, FunctionRef, JsObjectValue, Object, PromiseRaw},
	threadsafe_function::{ThreadsafeCallContext, ThreadsafeFunctionCallMode},
};

use super::{
	JsSyncEngine, SyncEngineCore,
	delivery::{Delivery, Listener, ListenerCall, listener_calls},
	lock,
	types::SyncPassEvent,
};
use crate::{
	auth,
	js::napi::{IntoWire, Wire, spawn_promise},
	sync_engine::WatchStatus,
};

/// How many chunks the delivery thread may run ahead of a busy JS thread: two waiting while
/// another is being delivered.
const LISTENER_QUEUE: usize = 2;

/// How long a Node environment's teardown waits for its engines to close. N-API cleanup hooks
/// are synchronous (napi-rs's async one reports itself done as soon as its callback returns), so
/// this blocks the environment's own thread: bounded, so a transfer in flight cannot hold up a
/// process exit or a worker's end for its whole length. An engine that has not closed by then
/// finishes closing on the commander runtime, keeping its baseline claimed until it has.
const ENV_CLOSE_WAIT: Duration = Duration::from_secs(5);

/// The engines each Node environment has opened, by the address of its `napi_env`.
static ENV_ENGINES: LazyLock<Mutex<HashMap<usize, EnvEngines>>> = LazyLock::new(Mutex::default);

/// The next environment's host id. 0 is UniFFI's, which never shares a build with napi.
static NEXT_HOST: AtomicU64 = AtomicU64::new(1);

/// One Node environment's engines.
struct EnvEngines {
	/// The host its opens name, unique in the process: its address is reused once it is gone.
	host: u64,
	/// Pruned of closed and dropped engines whenever one is added.
	engines: Vec<Weak<SyncEngineCore>>,
}

/// The address and host of `env`. The first open in an environment registers the cleanup hook
/// that closes its engines when it goes away.
fn env_host(env: &Env) -> napi::Result<(usize, u64)> {
	let addr = env.raw().addr();
	let mut envs = lock(&ENV_ENGINES);
	if let Some(known) = envs.get(&addr) {
		return Ok((addr, known.host));
	}
	let host = NEXT_HOST.fetch_add(1, Ordering::Relaxed);
	env.add_env_cleanup_hook((), move |()| close_env_engines(addr, host))?;
	envs.insert(
		addr,
		EnvEngines {
			host,
			engines: Vec::new(),
		},
	);
	Ok((addr, host))
}

/// Record that the environment at `addr` opened or attached to `core`. An environment gone while
/// the open ran left no hook to close it, so it is closed here instead.
fn register(addr: usize, host: u64, core: &Arc<SyncEngineCore>) {
	let mut envs = lock(&ENV_ENGINES);
	match envs.get_mut(&addr).filter(|env| env.host == host) {
		Some(env) => {
			env.engines.retain(|engine| {
				engine
					.upgrade()
					.is_some_and(|engine| engine.is_open() && !Arc::ptr_eq(&engine, core))
			});
			env.engines.push(Arc::downgrade(core));
		}
		None => {
			drop(envs);
			core.runtime.spawn(close_with_env(Arc::clone(core)));
		}
	}
}

/// The cleanup hook of the environment at `addr`: close every engine it opened and wait for them,
/// for at most [`ENV_CLOSE_WAIT`]. It runs on that environment's thread as it is torn down, and
/// leaves every other environment's engines alone. Never unwinds: napi calls it from an
/// `extern "C"` function.
fn close_env_engines(addr: usize, host: u64) {
	let engines = match lock(&ENV_ENGINES).entry(addr) {
		Entry::Occupied(env) if env.get().host == host => env.remove().engines,
		_ => return,
	};
	// Every close holds a sender; the receiver hears `Disconnected` once all have returned.
	let (done, all_done) = channel::<()>();
	for core in engines.iter().filter_map(Weak::upgrade) {
		let done = done.clone();
		core.runtime.clone().spawn(async move {
			close_with_env(core).await;
			drop(done);
		});
	}
	drop(done);
	if all_done.recv_timeout(ENV_CLOSE_WAIT) == Err(RecvTimeoutError::Timeout) {
		tracing::warn!(
			"the sync engines of a Node environment going away did not close within {ENV_CLOSE_WAIT:?}; they finish closing in the background"
		);
	}
}

async fn close_with_env(core: Arc<SyncEngineCore>) {
	if let Err(e) = core.close().await {
		tracing::warn!("closing a sync engine with its Node environment failed: {e}");
	}
}

type EventsWire = Vec<<SyncPassEvent as Wire>::Wire>;
type WatchStatusWire = <WatchStatus as Wire>::Wire;

/// One listener call in wire form, made on the delivery thread.
enum CallWire {
	Events(EventsWire),
	Status(BigInt, WatchStatusWire),
}

impl From<ListenerCall> for CallWire {
	fn from(call: ListenerCall) -> Self {
		match call {
			ListenerCall::Events(events) => Self::Events(events.into_wire()),
			ListenerCall::Status { pair_id, status } => {
				Self::Status(pair_id.into_wire(), status.into_wire())
			}
		}
	}
}

/// The delivery thread's listener: each chunk becomes one `send` of its calls, in order. A send
/// answering `Closing` ends it, as the JS environment is gone; any other failure loses only that
/// chunk.
fn napi_listener(send: impl Fn(Vec<CallWire>) -> Status + Send + Sync + 'static) -> Listener {
	// Only the delivery thread reads and writes it.
	let closed = AtomicBool::new(false);
	Arc::new(move |deliveries: Vec<Delivery>| {
		if closed.load(Ordering::Relaxed) {
			return;
		}
		let calls = listener_calls(deliveries)
			.into_iter()
			.map(CallWire::from)
			.collect();
		match send(calls) {
			Status::Ok => {}
			Status::Closing => {
				closed.store(true, Ordering::Relaxed);
				tracing::debug!("the JS environment is gone; the sync engine listener stops");
			}
			status => tracing::warn!("sync engine deliveries were not handed to JS: {status}"),
		}
	})
}

/// The listener's methods, bound to it, for the JS thread to call.
struct BoundListener {
	on_events: FunctionRef<EventsWire, ()>,
	on_watch_status: FunctionRef<FnArgs<(BigInt, WatchStatusWire)>, ()>,
}

impl BoundListener {
	/// Make `calls` on the JS thread. Never fails and never unwinds: it runs inside napi's
	/// `extern "C"` callback, where an error would be raised as an uncaught exception and an
	/// unwind would abort the process.
	fn deliver(&self, env: &Env, calls: Vec<CallWire>) {
		match catch_unwind(AssertUnwindSafe(|| self.dispatch(env, calls))) {
			Ok(Ok(())) => {}
			Ok(Err(e)) => tracing::warn!("sync engine deliveries were not handed to JS: {e}"),
			Err(_) => tracing::error!(
				"the sync engine listener's dispatch panicked; the rest of its chunk was lost"
			),
		}
	}

	fn dispatch(&self, env: &Env, calls: Vec<CallWire>) -> napi::Result<()> {
		let on_events = self.on_events.borrow_back(env)?;
		let on_watch_status = self.on_watch_status.borrow_back(env)?;
		for call in calls {
			// `call`, not `apply`: it takes a throw out of the environment and returns it, where
			// a pending one would end the process as an uncaught exception. `this` is bound.
			let called = match call {
				CallWire::Events(events) => on_events.call(events),
				CallWire::Status(pair_id, status) => {
					on_watch_status.call(FnArgs::from((pair_id, status)))
				}
			};
			if let Err(e) = called {
				tracing::warn!("sync engine listener threw: {e}");
			}
		}
		Ok(())
	}
}

/// `openSyncEngine`'s body, outside the module whose `#[napi] impl` needs the `unsafe_code`
/// allow. A listener without a callable `onEvents` and `onWatchStatus` throws napi's own error
/// synchronously.
// napi-derive registers the method calling this only outside `cfg(test)`.
#[cfg_attr(test, allow(dead_code))]
fn open_sync_engine<'env>(
	client: &auth::JsClient,
	env: &'env Env,
	db_path: String,
	listener: Object<'env>,
	async_opts: Option<Object<'env>>,
) -> napi::Result<PromiseRaw<'env, <JsSyncEngine as Wire>::Wire>> {
	let on_events: Function<'env, EventsWire, ()> = listener.get_named_property("onEvents")?;
	let on_watch_status: Function<'env, FnArgs<(BigInt, WatchStatusWire)>, ()> =
		listener.get_named_property("onWatchStatus")?;
	// ubrn calls both as methods of the listener, so `this` is bound to it.
	let bound = BoundListener {
		on_events: on_events.bind(listener)?.create_ref()?,
		on_watch_status: on_watch_status.bind(listener)?.create_ref()?,
	};
	// The callback makes the calls itself, so the function the threadsafe function calls after it
	// does nothing. Weak: an engine never keeps Node's event loop alive.
	let tsfn = env
		.create_function_from_closure::<(), (), _>("syncEngineListener", |_| Ok(()))?
		.build_threadsafe_function::<Vec<CallWire>>()
		.weak::<true>()
		.max_queue_size::<LISTENER_QUEUE>()
		.build_callback(move |ctx: ThreadsafeCallContext<Vec<CallWire>>| {
			bound.deliver(&ctx.env, ctx.value);
			Ok(())
		})?;
	// Blocking is safe here: only the delivery thread, a plain thread of its own, calls it.
	let listener =
		napi_listener(move |calls| tsfn.call(calls, ThreadsafeFunctionCallMode::Blocking));
	let (addr, host) = env_host(env)?;
	let client = client.inner();
	spawn_promise(env, async_opts, async move {
		let engine = JsSyncEngine::open(client, db_path, host, listener).await?;
		register(addr, host, &engine.core);
		Ok(engine)
	})
}

// napi-derive's generated FFI glue (the raw N-API calls of each item's callback) lands in this
// module, which holds nothing but the two napi items forwarding to plain Rust.
#[allow(unsafe_code)]
mod class {
	use napi::{
		Env,
		bindgen_prelude::{Object, PromiseRaw},
	};
	use napi_derive::napi;

	use super::open_sync_engine;
	use crate::{
		js::napi::{JsClient, JsSyncEngine},
		sync_engine::DEFAULT_IGNORE_PATTERNS,
	};

	#[napi]
	impl JsClient {
		/// Open the sync engine on the baseline DB at `db_path`, whose directory must exist; call
		/// `configure_cache` first, or remote changes are not announced. `listener` receives every
		/// pair's events and watch statuses.
		///
		/// If this Node environment already has the DB open for the same account, this attaches to
		/// that engine instead: its watches keep running, and `listener` replaces the old one from
		/// the next delivery round. The old one still receives the rest of the round in progress:
		/// the chunks already handed to the JS thread and those not yet handed over. If it has it
		/// open for another account, or another environment of this process has it open, or is
		/// still shutting it down, or another process has it open, this fails with the
		/// `BaselineInUse` sync refusal. A DB at a schema version this build does not read fails
		/// with the `BaselineSchemaMismatch` one, and is left as it was.
		///
		/// The engine closes with this Node environment: when the environment goes away (process
		/// exit, a worker ending, a reload), its teardown closes every engine it opened that is
		/// still open, waiting up to 5 s for them.
		#[napi(
			js_name = "openSyncEngine",
			ts_args_type = "dbPath: string, listener: SyncEngineListener, asyncOpts_?: { signal: AbortSignal }",
			ts_return_type = "Promise<JsSyncEngine>"
		)]
		pub fn __napi_open_sync_engine<'env>(
			&self,
			env: &'env Env,
			db_path: String,
			listener: Object<'env>,
			async_opts: Option<Object<'env>>,
		) -> napi::Result<PromiseRaw<'env, JsSyncEngine>> {
			open_sync_engine(&self.0, env, db_path, listener, async_opts)
		}
	}

	/// The ignore patterns every sync pair applies below its own, for an ignore editor to show as
	/// built in.
	#[napi(
		js_name = "defaultSyncIgnorePatterns",
		ts_args_type = "",
		ts_return_type = "Array<string>"
	)]
	pub fn __napi_default_sync_ignore_patterns() -> Vec<String> {
		DEFAULT_IGNORE_PATTERNS
			.iter()
			.map(|pattern| (*pattern).to_owned())
			.collect()
	}
}

#[cfg(test)]
mod tests {
	use std::sync::Mutex;

	use super::*;
	use crate::{js::napi::FromWire, sync_engine::js_impl::types::SyncEventMessage};

	/// What one listener call was, read back from its wire form.
	#[derive(Debug, PartialEq)]
	enum Shape {
		Events(usize),
		Status(u64),
	}

	fn event() -> Delivery {
		Delivery::Event(SyncPassEvent {
			pair_id: 1,
			pass_id: 1,
			emitted_at_ms: 0,
			event: SyncEventMessage::Planned { actions: 0 },
		})
	}

	fn status(pair_id: u64) -> Delivery {
		Delivery::Status {
			pair_id,
			status: WatchStatus::default(),
		}
	}

	/// A listener whose sends answer `answers` in turn (then `Ok`), and the shapes of every send.
	fn recording(answers: Vec<Status>) -> (Listener, Arc<Mutex<Vec<Vec<Shape>>>>) {
		let sent = Arc::new(Mutex::new(Vec::new()));
		let answers = Mutex::new(answers.into_iter());
		let record = Arc::clone(&sent);
		let listener = napi_listener(move |calls| {
			let shapes = calls
				.into_iter()
				.map(|call| match call {
					CallWire::Events(events) => Shape::Events(events.len()),
					CallWire::Status(pair_id, _) => {
						Shape::Status(u64::from_wire(pair_id).expect("a pair id fits"))
					}
				})
				.collect();
			record.lock().unwrap().push(shapes);
			answers.lock().unwrap().next().unwrap_or(Status::Ok)
		});
		(listener, sent)
	}

	#[test]
	fn a_chunk_is_one_send_with_its_calls_in_order() {
		let (listener, sent) = recording(Vec::new());

		listener(vec![event(), event(), status(3), status(4), event()]);

		assert_eq!(
			*sent.lock().unwrap(),
			vec![vec![
				Shape::Events(2),
				Shape::Status(3),
				Shape::Status(4),
				Shape::Events(1)
			]]
		);
	}

	#[test]
	fn nothing_is_sent_after_the_environment_closes() {
		let (listener, sent) = recording(vec![Status::Closing]);

		listener(vec![event()]);
		listener(vec![status(2)]);
		listener(vec![event()]);

		assert_eq!(*sent.lock().unwrap(), vec![vec![Shape::Events(1)]]);
	}

	#[test]
	fn a_failed_send_loses_only_its_own_chunk() {
		let (listener, sent) = recording(vec![Status::GenericFailure]);

		listener(vec![event()]);
		listener(vec![status(2)]);

		assert_eq!(
			*sent.lock().unwrap(),
			vec![vec![Shape::Events(1)], vec![Shape::Status(2)]]
		);
	}
}
