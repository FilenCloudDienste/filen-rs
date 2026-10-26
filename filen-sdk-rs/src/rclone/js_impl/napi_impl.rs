//! The rclone daemon's napi exports the macros do not write: `JsClient.startRcloneDaemon` and the
//! free functions (`FilenSdkError.rcloneRefusal` is with the class, in `js::napi`).
//!
//! A daemon belongs to the Node environment that started it, and stops with it: each environment
//! that starts one gets one cleanup hook, which stops every daemon it started that still runs (see
//! [`stop_env_daemons`]), as the sync engine's hook closes its engines. Should a stop outlast the
//! wait, the process exits anyway and rclone's watchdog stops it, as it would after a crash.

use std::{
	collections::{HashMap, hash_map::Entry},
	sync::{
		Arc, LazyLock, Mutex, MutexGuard, PoisonError, Weak,
		atomic::{AtomicU64, Ordering},
		mpsc::{RecvTimeoutError, channel},
	},
	time::Duration,
};

use napi::{
	Env,
	bindgen_prelude::{Object, PromiseRaw},
};

use super::{JsRcloneDaemon, RcloneDaemonOptions};
use crate::{
	auth,
	js::napi::{FromWire as _, Wire, spawn_promise},
	rclone::{RcloneDaemon, StopOptions},
};

/// How an environment going away stops its daemons: serves stopped and drives unmounted, then
/// rclone asked to quit and killed after 3 s. Uploads still waiting stay in the cache, and the
/// next daemon on it uploads them.
const ENV_STOP: StopOptions = StopOptions {
	drain_timeout_ms: None,
	grace_ms: 3_000,
};

/// How long a Node environment's teardown waits for its daemons to stop. N-API cleanup hooks are
/// synchronous, so this blocks the environment's own thread: bounded, as the sync engine's
/// `ENV_CLOSE_WAIT` is.
const ENV_STOP_WAIT: Duration = Duration::from_secs(5);

/// The daemons each Node environment has started, by the address of its `napi_env`.
static ENV_DAEMONS: LazyLock<Mutex<HashMap<usize, EnvDaemons>>> = LazyLock::new(Mutex::default);

/// The next environment's host id, which tells it from an earlier one at the same address.
static NEXT_HOST: AtomicU64 = AtomicU64::new(1);

/// One Node environment's daemons.
struct EnvDaemons {
	host: u64,
	/// Pruned of dropped and exited daemons whenever one is added.
	daemons: Vec<EnvDaemon>,
}

/// A daemon an environment started, and the runtime to stop it on.
struct EnvDaemon {
	daemon: Weak<RcloneDaemon>,
	runtime: tokio::runtime::Handle,
}

/// `mutex`'s guard, poison or not: every section under it leaves the map consistent.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
	mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The address and host of `env`. The first start in an environment registers the cleanup hook
/// that stops its daemons when it goes away.
fn env_host(env: &Env) -> napi::Result<(usize, u64)> {
	let addr = env.raw().addr();
	let mut envs = lock(&ENV_DAEMONS);
	if let Some(known) = envs.get(&addr) {
		return Ok((addr, known.host));
	}
	let host = NEXT_HOST.fetch_add(1, Ordering::Relaxed);
	env.add_env_cleanup_hook((), move |()| stop_env_daemons(addr, host))?;
	envs.insert(
		addr,
		EnvDaemons {
			host,
			daemons: Vec::new(),
		},
	);
	Ok((addr, host))
}

/// Record that the environment at `addr` started `daemon`. An environment gone while the start ran
/// left no hook to stop it, so it is stopped here instead.
fn register(addr: usize, host: u64, daemon: &JsRcloneDaemon) {
	let mut envs = lock(&ENV_DAEMONS);
	match envs.get_mut(&addr).filter(|env| env.host == host) {
		Some(env) => {
			env.daemons.retain(|known| {
				known
					.daemon
					.upgrade()
					.is_some_and(|known| !known.has_exited())
			});
			env.daemons.push(EnvDaemon {
				daemon: Arc::downgrade(&daemon.daemon),
				runtime: daemon.runtime.clone(),
			});
		}
		None => {
			drop(envs);
			daemon
				.runtime
				.spawn(stop_with_env(Arc::clone(&daemon.daemon)));
		}
	}
}

/// The cleanup hook of the environment at `addr`: stop every daemon it started that still runs,
/// and wait for them, for at most [`ENV_STOP_WAIT`]. It runs on that environment's thread as it is
/// torn down. Never unwinds: napi calls it from an `extern "C"` function.
fn stop_env_daemons(addr: usize, host: u64) {
	let daemons = match lock(&ENV_DAEMONS).entry(addr) {
		Entry::Occupied(env) if env.get().host == host => env.remove().daemons,
		_ => return,
	};
	// Every stop holds a sender; the receiver hears `Disconnected` once all have returned.
	let (done, all_done) = channel::<()>();
	for EnvDaemon { daemon, runtime } in daemons {
		let Some(daemon) = daemon.upgrade().filter(|daemon| !daemon.has_exited()) else {
			continue;
		};
		let done = done.clone();
		runtime.spawn(async move {
			stop_with_env(daemon).await;
			drop(done);
		});
	}
	drop(done);
	if all_done.recv_timeout(ENV_STOP_WAIT) == Err(RecvTimeoutError::Timeout) {
		tracing::warn!(
			"the rclone daemons of a Node environment going away did not stop within {ENV_STOP_WAIT:?}; their watchdogs stop them once the process is gone"
		);
	}
}

async fn stop_with_env(daemon: Arc<RcloneDaemon>) {
	if let Err(e) = daemon.stop(ENV_STOP).await {
		tracing::warn!("stopping an rclone daemon with its Node environment failed: {e}");
	}
}

/// `startRcloneDaemon`'s body, outside the module whose `#[napi] impl` needs the `unsafe_code`
/// allow.
// napi-derive registers the method calling this only outside `cfg(test)`.
#[cfg_attr(test, allow(dead_code))]
fn start_rclone_daemon<'env>(
	client: &auth::JsClient,
	env: &'env Env,
	options: <RcloneDaemonOptions as Wire>::Wire,
	async_opts: Option<Object<'env>>,
) -> napi::Result<PromiseRaw<'env, <JsRcloneDaemon as Wire>::Wire>> {
	let (addr, host) = env_host(env)?;
	let client = client.inner();
	spawn_promise(env, async_opts, async move {
		let options = RcloneDaemonOptions::from_wire(options)?;
		let daemon = JsRcloneDaemon::start(client, options).await?;
		register(addr, host, &daemon);
		Ok(daemon)
	})
}

// napi-derive's generated FFI glue (the raw N-API calls of each item's callback) lands in this
// module, which holds nothing but napi items forwarding to plain Rust.
#[allow(unsafe_code)]
mod class {
	use napi::{
		Env,
		bindgen_prelude::{Object, PromiseRaw},
	};
	use napi_derive::napi;

	use super::start_rclone_daemon;
	use crate::{
		js::napi::{JsClient, JsRcloneDaemon, Wire, spawn_promise},
		rclone::{
			FuseStatus, RcloneDaemonOptions, StaleRecovery,
			js_impl::{
				rclone_available_drive_letters, rclone_clear_cache, rclone_force_unmount,
				rclone_fuse_status, rclone_recover_stale, rclone_stop_under,
			},
		},
	};

	#[napi]
	impl JsClient {
		/// Start an `rclone rcd` for this account, with rclone's remotes in its memory and its rc
		/// behind a random password: no secret in its argv, environment or files.
		///
		/// The daemon stops with this Node environment: when the environment goes away (process
		/// exit, a worker ending, a reload), its teardown stops every daemon it started that still
		/// runs, waiting up to 5 s for them.
		#[napi(
			js_name = "startRcloneDaemon",
			ts_args_type = "options: RcloneDaemonOptions, asyncOpts_?: { signal: AbortSignal }",
			ts_return_type = "Promise<JsRcloneDaemon>"
		)]
		pub fn __napi_start_rclone_daemon<'env>(
			&self,
			env: &'env Env,
			options: <RcloneDaemonOptions as Wire>::Wire,
			async_opts: Option<Object<'env>>,
		) -> napi::Result<PromiseRaw<'env, JsRcloneDaemon>> {
			start_rclone_daemon(&self.0, env, options, async_opts)
		}
	}

	/// The mount backend this system has.
	#[napi(
		js_name = "rcloneFuseStatus",
		ts_args_type = "asyncOpts_?: { signal: AbortSignal }",
		ts_return_type = "Promise<FuseStatus>"
	)]
	pub fn __napi_rclone_fuse_status<'env>(
		env: &'env Env,
		async_opts: Option<Object<'env>>,
	) -> napi::Result<PromiseRaw<'env, <FuseStatus as Wire>::Wire>> {
		spawn_promise(env, async_opts, rclone_fuse_status())
	}

	/// Recovers from a daemon on `stateDir` whose host ended without stopping it: stops the rclone
	/// its pidfile names if that still runs, and force-unmounts its stale mounts.
	#[napi(
		js_name = "rcloneRecoverStale",
		ts_args_type = "stateDir: string, asyncOpts_?: { signal: AbortSignal }",
		ts_return_type = "Promise<StaleRecovery>"
	)]
	pub fn __napi_rclone_recover_stale<'env>(
		env: &'env Env,
		state_dir: String,
		async_opts: Option<Object<'env>>,
	) -> napi::Result<PromiseRaw<'env, <StaleRecovery as Wire>::Wire>> {
		spawn_promise(env, async_opts, rclone_recover_stale(state_dir))
	}

	/// Stops every process whose executable is inside `dir`, killing it after `graceMs`; resolves
	/// with how many it stopped.
	#[napi(
		js_name = "rcloneStopUnder",
		ts_args_type = "dir: string, graceMs: number, asyncOpts_?: { signal: AbortSignal }",
		ts_return_type = "Promise<number>"
	)]
	pub fn __napi_rclone_stop_under<'env>(
		env: &'env Env,
		dir: String,
		grace_ms: u32,
		async_opts: Option<Object<'env>>,
	) -> napi::Result<PromiseRaw<'env, u32>> {
		spawn_promise(env, async_opts, rclone_stop_under(dir, grace_ms))
	}

	/// Unmounts whatever is mounted at `path`, stale or busy; resolves with whether something was.
	#[napi(
		js_name = "rcloneForceUnmount",
		ts_args_type = "path: string, asyncOpts_?: { signal: AbortSignal }",
		ts_return_type = "Promise<boolean>"
	)]
	pub fn __napi_rclone_force_unmount<'env>(
		env: &'env Env,
		path: String,
		async_opts: Option<Object<'env>>,
	) -> napi::Result<PromiseRaw<'env, bool>> {
		spawn_promise(env, async_opts, rclone_force_unmount(path))
	}

	/// The drive letters, from D to Z, no drive uses, such as `X`. Empty off Windows.
	#[napi(
		js_name = "rcloneAvailableDriveLetters",
		ts_args_type = "asyncOpts_?: { signal: AbortSignal }",
		ts_return_type = "Promise<Array<string>>"
	)]
	pub fn __napi_rclone_available_drive_letters<'env>(
		env: &'env Env,
		async_opts: Option<Object<'env>>,
	) -> napi::Result<PromiseRaw<'env, Vec<String>>> {
		spawn_promise(env, async_opts, rclone_available_drive_letters())
	}

	/// Deletes the VFS caches in `cacheDir`, unless the daemon on `stateDir` runs (`CacheInUse`)
	/// or the cache holds files not uploaded yet (`UploadsPending`).
	#[napi(
		js_name = "rcloneClearCache",
		ts_args_type = "stateDir: string, cacheDir: string, asyncOpts_?: { signal: AbortSignal }",
		ts_return_type = "Promise<void>"
	)]
	pub fn __napi_rclone_clear_cache<'env>(
		env: &'env Env,
		state_dir: String,
		cache_dir: String,
		async_opts: Option<Object<'env>>,
	) -> napi::Result<PromiseRaw<'env, ()>> {
		spawn_promise(env, async_opts, rclone_clear_cache(state_dir, cache_dir))
	}
}
