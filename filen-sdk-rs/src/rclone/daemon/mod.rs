//! One `rclone rcd` per host, configured over its authenticated remote control API (rc).
//!
//! Nothing secret reaches rclone through its argv, its environment or a file it reads. Its argv
//! is readable by every user through `ps` and `/proc/<pid>/cmdline`, and its environment is
//! inherited by every process it starts (the FUSE and NFS mount helpers) and readable through
//! `/proc/<pid>/environ`. So rclone starts with an in-memory config (`--config ""`) and an rc
//! that takes a random basic-auth password whose SHA-1 alone is in an htpasswd file (0600). rclone
//! re-reads that file on every rc request (and fails the request once it is gone), so it stays
//! until rclone has exited. The account's remotes are then created over rc (`config/create`),
//! where rclone obscures the password options itself.
//!
//! Every role (a mount, a serve) uses one of two remotes: `Filen`, whose name keeps the VFS cache
//! the old desktop client's drive left at `vfs/Filen` resumable, and `FilenRO` for read-only roles,
//! with a cache tree of its own, as rclone keys its VFS cache by remote.

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;
use std::{
	collections::VecDeque,
	ffi::{OsStr, OsString},
	path::{Path, PathBuf},
	process::Stdio,
	sync::{
		Arc, Mutex, PoisonError,
		atomic::{AtomicBool, Ordering},
	},
	time::Duration,
};

use base64::{Engine as _, prelude::BASE64_STANDARD};
use filen_macros::js_type;
use filen_types::auth::FilenSDKConfig;
use num_traits::ToPrimitive as _;
use port_check::free_local_ipv4_port;
use serde_json::{Map, Value};
use sha1::{Digest as _, Sha1};
use tokio::{
	io::{AsyncBufReadExt as _, BufReader},
	process::{Child, ChildStderr, Command},
	sync::{oneshot, watch},
	time::Instant,
};
use tracing::{info, warn};

#[cfg(windows)]
use crate::rclone::system::CREATE_NO_WINDOW;
#[cfg(unix)]
use crate::rclone::system::with_system_path;
use crate::{
	Error, ErrorKind,
	auth::Client,
	error::ResultExt as _,
	rclone::{
		RcloneBinary, RcloneRefusal,
		binary::{MIN_RCLONE, parse_version},
		private_file::write_private_file,
		rc::{CoreStatsTransfer, RcClient},
		recovery::{HTPASSWD_FILE, PIDFILE, PidRecord, force_unmount, recover_stale},
		serve::ServeInfo,
		system::identify,
		vfs::VfsTuning,
		watchdog::{self, Watchdog},
	},
};

/// The remote every read-write role uses.
pub(crate) const FILEN_REMOTE: &str = "Filen";
/// The remote every read-only role uses.
pub(crate) const FILEN_READ_ONLY_REMOTE: &str = "FilenRO";

/// What a call on a stopped daemon fails with.
pub(crate) fn stopped() -> Error {
	Error::custom(ErrorKind::InvalidState, "the rclone daemon is stopped")
}

/// rc's basic-auth user. Only the password is secret.
const RC_USER: &str = "filen";
/// How long rclone has to answer on rc after it is started.
const READY_TIMEOUT: Duration = Duration::from_secs(15);
const READY_POLL: Duration = Duration::from_millis(100);
/// How often rclone is started before giving up when it exits before its rc answers, as it does
/// when another process takes its rc port between the moment it is found free and rclone binding
/// it.
const SPAWN_ATTEMPTS: u32 = 3;
const DRAIN_POLL: Duration = Duration::from_millis(500);
/// How long the end of rclone's stderr is waited for once it has exited. A mount helper it started
/// may hold the pipe open longer; the tail is then what arrived by this time.
const STDERR_EOF_WAIT: Duration = Duration::from_secs(1);
const STDERR_TAIL_LINES: usize = 20;
const STDERR_TAIL_LINE_LEN: usize = 512;

/// Parallel uploads per file, the old desktop client's value.
const UPLOAD_CONCURRENCY: &str = "24";

/// rclone's transfer tuning (the old desktop client's values) and log level.
const TRANSFER_ARGS: [&str; 19] = [
	"--transfers",
	"8",
	"--multi-thread-streams",
	"4",
	"--multi-thread-cutoff",
	"64Mi",
	"--buffer-size",
	"32Mi",
	"--low-level-retries",
	"10",
	"--timeout",
	"5m",
	"--contimeout",
	"1m",
	"--disable-http2",
	"--no-gzip-encoding",
	"--use-mmap",
	"--log-level",
	"INFO",
];

/// How to start an [`RcloneDaemon`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RcloneDaemonConfig {
	/// The rclone binary to run.
	pub binary: RcloneBinary,
	/// A directory only the daemon uses, for its own state: the rc credentials while it starts.
	/// Created (0700 on Unix) if it is missing.
	pub state_dir: PathBuf,
	/// rclone's cache directory (`--cache-dir`). Its VFS cache holds what was written to a mount
	/// or a serve and is not uploaded yet, which the next daemon on the same directory uploads.
	/// Created (0700 on Unix) if it is missing.
	pub cache_dir: PathBuf,
	/// rclone's log file, rotated at 10 MB with three old ones kept. Without one, rclone logs to
	/// its stderr, which is logged here at `info`.
	pub log_file: Option<PathBuf>,
	/// The user agent rclone sends.
	pub user_agent: String,
	/// The VFS cache's limits, the same for every mount and serve.
	pub vfs: VfsTuning,
}

/// How an [`RcloneDaemon`]'s rclone ended.
#[js_type(export, no_deser, tagged, napi)]
pub enum RcloneExit {
	/// It was stopped ([`RcloneDaemon::stop`]) or the daemon was dropped.
	Stopped,
	/// It exited by itself. `code` is its exit code, `None` when a signal ended it, and
	/// `stderr_tail` the last lines it wrote to stderr, which a caller redacts before showing.
	Crashed {
		code: Option<i32>,
		stderr_tail: String,
	},
}

/// What the daemon's VFS caches and transfers are doing, summed over every remote in use.
#[js_type(export, no_deser, napi)]
pub struct RcloneStats {
	/// Files being uploaded from the cache now.
	pub uploads_in_progress: u32,
	/// Files waiting in the cache to be uploaded.
	pub uploads_queued: u32,
	/// Files whose upload failed; rclone retries them.
	pub errored_files: u32,
	/// Bytes the VFS caches hold on disk.
	pub cache_bytes_used: u64,
	/// Whether a VFS cache stopped taking writes because its disk is full.
	pub out_of_space: bool,
	/// The transfers running now.
	pub transfers: Vec<RcloneTransfer>,
	/// The mount points rclone has mounted.
	pub mounts: Vec<String>,
	/// The serves running now.
	pub serves: Vec<ServeInfo>,
}

/// One running transfer.
#[js_type(export, no_deser, napi)]
pub struct RcloneTransfer {
	/// The file's path in its remote.
	pub name: String,
	/// Bytes transferred so far.
	pub bytes: u64,
	/// The file's size in bytes, `None` while rclone does not know it.
	pub size: Option<u64>,
	/// Bytes per second.
	pub speed_bps: u64,
}

/// How [`RcloneDaemon::stop`] stops rclone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[js_type(import, no_ser, no_default, napi)]
pub struct StopOptions {
	/// With `Some`, how long, in milliseconds, uploads waiting in the VFS cache may finish before
	/// rclone is asked to quit. Whatever is still waiting then stays in the cache and is uploaded
	/// by the next daemon on it.
	pub drain_timeout_ms: Option<u32>,
	/// How long, in milliseconds, rclone has to quit once asked before it is killed.
	pub grace_ms: u32,
}

/// What [`RcloneDaemon::stop`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[js_type(export, no_deser, no_default, napi)]
pub struct StopReport {
	/// Uploads still waiting in the VFS cache when rclone was asked to quit (in progress and
	/// queued), 0 when they could not be read.
	pub pending_uploads: u32,
	/// Whether rclone did not quit within the grace period and was killed.
	pub forced: bool,
}

/// A running `rclone rcd`, driven over rc. Dropping it kills rclone; [`RcloneDaemon::stop`] stops
/// it gracefully.
///
/// Every method takes `&self`, so a binding can share one daemon between calls: after
/// [`RcloneDaemon::stop`] the others fail, as rc no longer answers.
pub struct RcloneDaemon {
	pub(super) rc: RcClient,
	pid: u32,
	exit: watch::Receiver<Option<RcloneExit>>,
	/// Firing it, or dropping it with the daemon, makes the task supervising rclone kill it.
	kill: Mutex<Option<oneshot::Sender<()>>>,
	/// Set before rclone is stopped or killed on purpose, so its exit is not taken for a crash.
	stopping: Arc<AtomicBool>,
	htpasswd: Htpasswd,
	/// What a crash must be recovered from. `None` until rclone is configured and again once it
	/// is stopped. Held through every mount and unmount, which it records, so they run one at a
	/// time.
	pub(super) guard: tokio::sync::Mutex<Option<Guard>>,
	/// The pidfile, written once the watchdog is armed and removed once rclone is stopped.
	pub(super) pidfile: PathBuf,
	/// The VFS options every role gets.
	pub(super) vfs: VfsTuning,
}

/// The pidfile's record of the running rclone, and the watchdog armed for the same mounts.
pub(super) struct Guard {
	pub(super) record: PidRecord,
	/// Dropping it disarms it.
	watchdog: Watchdog,
}

impl Guard {
	/// Records the mounts in `self.record` as rclone's: arms a watchdog for them before the one
	/// for the old set is disarmed, so there is always one, and rewrites the pidfile.
	pub(super) fn record_mounts(&mut self, pidfile: &Path) -> Result<(), Error> {
		self.watchdog = watchdog::arm(
			std::process::id(),
			self.record.pid,
			self.record.started_at,
			&self.record.mounts,
		)?;
		self.record.write(pidfile)
	}
}

/// The backend and options both remotes are created with. Holds the account's keys, so it has no
/// `Debug`.
pub(crate) struct RemoteBackend {
	kind: &'static str,
	parameters: Map<String, Value>,
}

impl RemoteBackend {
	/// rclone's `filen` backend for the account `config` describes.
	pub(crate) fn filen(config: &FilenSDKConfig) -> Self {
		// Owned copies: the body is built once per start.
		let mut parameters = Map::new();
		let mut set =
			|key: &str, value: String| parameters.insert(key.to_owned(), Value::String(value));
		set("email", config.email.to_owned());
		// The backend logs in with the keys below, never with a password, but requires the option
		// to be set. This is the value it documents for that.
		// https://github.com/FilenCloudDienste/filen-rclone/blob/784979078ecd573af0d9809d2c5a07bafaad2c41/backend/filen/filen.go#L136
		set("password", "INTERNAL".to_owned());
		set("api_key", config.api_key.to_owned());
		set("master_keys", config.master_keys.join("|"));
		set("private_key", config.private_key.to_owned());
		set("public_key", config.public_key.to_owned());
		set("auth_version", (config.auth_version as u8).to_string());
		set("base_folder_uuid", config.base_folder_uuid.to_owned());
		set("upload_concurrency", UPLOAD_CONCURRENCY.to_owned());
		Self {
			kind: "filen",
			parameters,
		}
	}

	/// An alias of the local directory `dir`, so tests run a daemon without an account.
	#[cfg(feature = "test-seams")]
	pub(crate) fn local(dir: &Path) -> Self {
		let mut parameters = Map::new();
		parameters.insert(
			"remote".to_owned(),
			Value::String(dir.to_string_lossy().into_owned()),
		);
		Self {
			kind: "alias",
			parameters,
		}
	}
}

impl RcloneDaemon {
	/// Starts `rclone rcd` for `client`'s account.
	///
	/// Resolves the binary, starts rclone on a free loopback port with rc behind a random
	/// password, waits up to 15 s for rc to answer, refuses an rclone older than 1.74.3
	/// ([`RcloneRefusal::UnsupportedVersion`]), and creates the `Filen` and `FilenRO` remotes in its
	/// in-memory config. Fails, with rclone killed, if any step does.
	pub async fn start(client: &Client, config: RcloneDaemonConfig) -> Result<Self, Error> {
		Self::start_with(config, &RemoteBackend::filen(&client.to_sdk_config())).await
	}

	pub(crate) async fn start_with(
		config: RcloneDaemonConfig,
		backend: &RemoteBackend,
	) -> Result<Self, Error> {
		let binary = config.binary.resolve().await?;
		create_private_dir(&config.state_dir).await?;
		create_private_dir(&config.cache_dir).await?;
		let recovered = recover_stale(&config.state_dir).await?;
		if recovered.stopped || !recovered.unmounted.is_empty() {
			info!("recovered from a crashed rclone daemon: {recovered:?}");
		}

		let password = hex::encode(rand::random::<[u8; 32]>());
		let (daemon, version) = Self::spawn_until_ready(&binary, &config, &password).await?;
		check_version(version)?;
		for name in [FILEN_REMOTE, FILEN_READ_ONLY_REMOTE] {
			daemon
				.rc
				.config_create(name, backend.kind, &backend.parameters)
				.await
				.context(format!("failed to create rclone's {name} remote"))?;
		}
		daemon.guard(&binary).await?;
		Ok(daemon)
	}

	/// Arms the watchdog for the running rclone and writes the pidfile that names it.
	async fn guard(&self, binary: &Path) -> Result<(), Error> {
		let rclone = identify(self.pid).await?.ok_or_else(|| {
			Error::custom(ErrorKind::IO, "rclone rcd exited while it was configured")
		})?;
		let record = PidRecord {
			pid: self.pid,
			started_at: rclone.started_at,
			// What the system reports, which is what a later recovery compares it with.
			binary: rclone.exe.unwrap_or_else(|| binary.to_path_buf()),
			mounts: Vec::new(),
		};
		let watchdog = watchdog::arm(std::process::id(), self.pid, record.started_at, &[])?;
		record.write(&self.pidfile)?;
		*self.guard.lock().await = Some(Guard { record, watchdog });
		Ok(())
	}

	/// Starts rclone until one answers on rc, returning it with the version it reported. An rclone
	/// that exits before it answers is started again on a new port, up to [`SPAWN_ATTEMPTS`] times:
	/// it may have lost its port to a process that held it only for a moment, which is gone by the
	/// time it could be checked for.
	async fn spawn_until_ready(
		binary: &Path,
		config: &RcloneDaemonConfig,
		password: &str,
	) -> Result<(Self, String), Error> {
		let mut attempt = 1;
		loop {
			let port = free_local_ipv4_port()
				.ok_or_else(|| Error::custom(ErrorKind::IO, "no free port for rclone's rc"))?;
			let daemon = Self::spawn(binary, config, port, password)?;
			match daemon.wait_ready().await {
				Ok(version) => return Ok((daemon, version)),
				Err(e) if attempt < SPAWN_ATTEMPTS && daemon.has_exited() => {
					warn!(
						"rclone exited before its rc answered on port {port}, starting it again: {e}"
					);
					attempt += 1;
				}
				Err(e) => return Err(e),
			}
		}
	}

	fn spawn(
		binary: &Path,
		config: &RcloneDaemonConfig,
		port: u16,
		password: &str,
	) -> Result<Self, Error> {
		// Written for each rclone started: a failed one's daemon removes it when it drops.
		let htpasswd = Htpasswd::write(&config.state_dir, password)?;
		let rc_addr = format!("127.0.0.1:{port}");
		let mut command = Command::new(binary);
		command
			.args(rcd_args(config, &rc_addr, &htpasswd.path))
			.stdin(Stdio::null())
			.stdout(Stdio::null())
			.stderr(Stdio::piped())
			.kill_on_drop(true);
		for (key, value) in environment_overrides(std::env::vars_os()) {
			match value {
				Some(value) => command.env(key, value),
				None => command.env_remove(key),
			};
		}
		// Its own process group, so a terminal's Ctrl-C reaches only the host, which stops rclone
		// gracefully.
		#[cfg(unix)]
		command.process_group(0);
		#[cfg(windows)]
		command.creation_flags(CREATE_NO_WINDOW);

		let rc = RcClient::new(port, RC_USER, password)?;
		let mut child = command.spawn().context("failed to start rclone rcd")?;
		let pid = child.id().ok_or_else(|| {
			Error::custom(
				ErrorKind::Internal,
				"rclone rcd was reaped before its pid was read",
			)
		})?;
		let stderr = child.stderr.take().ok_or_else(|| {
			Error::custom(ErrorKind::Internal, "rclone rcd's stderr was not piped")
		})?;
		let (exit_tx, exit) = watch::channel(None);
		let (kill, kill_rx) = oneshot::channel();
		let stopping = Arc::new(AtomicBool::new(false));
		tokio::spawn(supervise(
			child,
			stderr,
			kill_rx,
			Arc::clone(&stopping),
			exit_tx,
		));
		Ok(Self {
			rc,
			pid,
			exit,
			kill: Mutex::new(Some(kill)),
			stopping,
			htpasswd,
			guard: tokio::sync::Mutex::new(None),
			pidfile: config.state_dir.join(PIDFILE),
			vfs: config.vfs,
		})
	}

	/// The version rclone reports once its rc answers.
	async fn wait_ready(&self) -> Result<String, Error> {
		let deadline = Instant::now() + READY_TIMEOUT;
		let mut exit = self.exit.clone();
		loop {
			let not_ready = match self.rc.core_version().await {
				Ok(version) => return Ok(version.version),
				Err(e) => e,
			};
			if let Some(ended) = exit.borrow().as_ref() {
				return Err(Error::custom(
					ErrorKind::IO,
					format!("rclone rcd exited before its rc answered: {ended:?}"),
				));
			}
			if Instant::now() >= deadline {
				return Err(not_ready.with_context(format!(
					"rclone rcd's rc did not answer within {READY_TIMEOUT:?}"
				)));
			}
			tokio::select! {
				() = tokio::time::sleep(READY_POLL) => {}
				_ = exit.changed() => {}
			}
		}
	}

	/// Whether rclone has exited.
	pub(crate) fn has_exited(&self) -> bool {
		self.exit.borrow().is_some()
	}

	/// rclone's process id.
	pub fn pid(&self) -> u32 {
		self.pid
	}

	/// Resolves once rclone has exited, with how it ended.
	pub async fn exited(&self) -> RcloneExit {
		let mut exit = self.exit.clone();
		match exit.wait_for(Option::is_some).await {
			// Every caller gets its own copy of the one exit.
			Ok(ended) => ended.clone().unwrap_or(RcloneExit::Stopped),
			// The supervising task is gone without saying how rclone ended: it was cancelled with
			// the runtime, which kills rclone with it.
			Err(_) => RcloneExit::Stopped,
		}
	}

	/// What the VFS caches and transfers are doing now.
	pub async fn stats(&self) -> Result<RcloneStats, Error> {
		let mut stats = RcloneStats {
			uploads_in_progress: 0,
			uploads_queued: 0,
			errored_files: 0,
			cache_bytes_used: 0,
			out_of_space: false,
			transfers: Vec::new(),
			mounts: Vec::new(),
			serves: Vec::new(),
		};
		for fs in self.rc.vfs_list().await?.vfses {
			let Some(cache) = self.rc.vfs_stats(&fs).await?.disk_cache else {
				continue;
			};
			stats.uploads_in_progress = stats
				.uploads_in_progress
				.saturating_add(count(cache.uploads_in_progress));
			stats.uploads_queued = stats
				.uploads_queued
				.saturating_add(count(cache.uploads_queued));
			stats.errored_files = stats
				.errored_files
				.saturating_add(count(cache.errored_files));
			stats.cache_bytes_used = stats
				.cache_bytes_used
				.saturating_add(u64::try_from(cache.bytes_used).unwrap_or(0));
			stats.out_of_space |= cache.out_of_space;
		}
		stats.transfers = self
			.rc
			.core_stats()
			.await?
			.transferring
			.into_iter()
			.map(RcloneTransfer::from)
			.collect();
		stats.mounts = self
			.rc
			.mount_listmounts()
			.await?
			.mount_points
			.into_iter()
			.map(|mount| mount.mount_point)
			.collect();
		stats.serves = self
			.rc
			.serve_list()
			.await?
			.list
			.into_iter()
			.filter_map(|serve| {
				ServeInfo::try_from(serve)
					.inspect_err(|kind| {
						warn!("rclone runs a {kind} serve the daemon did not start")
					})
					.ok()
			})
			.collect();
		Ok(stats)
	}

	/// Uploads in progress and queued in the VFS caches.
	async fn pending_uploads(&self) -> Result<u32, Error> {
		let stats = self.stats().await?;
		Ok(stats
			.uploads_in_progress
			.saturating_add(stats.uploads_queued))
	}

	/// Stops rclone: stops every serve, lets pending uploads finish for up to
	/// `drain_timeout_ms`, unmounts every mount, asks rclone to quit and kills it if it has not
	/// within `grace_ms`. A step that fails is logged and the next one runs; the daemon is
	/// stopped when this returns.
	pub async fn stop(&self, options: StopOptions) -> Result<StopReport, Error> {
		// Waits for a mount or unmount in progress, and keeps new ones out.
		let mut guard = self.guard.lock().await;
		self.stopping.store(true, Ordering::Release);
		if let Err(e) = self.rc.serve_stopall().await {
			warn!("stopping rclone's serves failed: {e}");
		}
		let pending = match options.drain_timeout_ms {
			Some(timeout_ms) => self.drain(Duration::from_millis(timeout_ms.into())).await,
			None => self.pending_uploads().await,
		};
		let pending = pending.unwrap_or_else(|e| {
			warn!("reading rclone's pending uploads failed: {e}");
			0
		});
		if let Err(e) = self.rc.mount_unmountall().await {
			warn!("unmounting rclone's mounts failed: {e}");
		}
		if let Err(e) = self.rc.core_quit().await {
			warn!("asking rclone to quit failed: {e}");
		}
		let grace = Duration::from_millis(options.grace_ms.into());
		let forced = tokio::time::timeout(grace, self.exited()).await.is_err();
		if forced {
			warn!("rclone did not quit within {grace:?}, killing it");
			self.kill();
			self.exited().await;
		}
		self.htpasswd.remove();
		// rclone is gone: the watchdog has nothing left to guard, nor the pidfile to name. A mount
		// rclone did not take down with it is stale now.
		if let Some(Guard { record, watchdog }) = guard.take() {
			drop(watchdog);
			for mount in record.mounts {
				if let Err(e) = force_unmount(Path::new(&mount)).await {
					warn!("force-unmounting {mount} after rclone stopped failed: {e}");
				}
			}
		}
		if let Err(e) = std::fs::remove_file(&self.pidfile)
			&& e.kind() != std::io::ErrorKind::NotFound
		{
			warn!("removing {} failed: {e}", self.pidfile.display());
		}
		Ok(StopReport {
			pending_uploads: pending,
			forced,
		})
	}

	/// Waits up to `timeout` for the VFS caches' pending uploads to finish, returning how many are
	/// left.
	async fn drain(&self, timeout: Duration) -> Result<u32, Error> {
		let deadline = Instant::now() + timeout;
		loop {
			let pending = self.pending_uploads().await?;
			if pending == 0 || Instant::now() >= deadline {
				return Ok(pending);
			}
			tokio::time::sleep(DRAIN_POLL).await;
		}
	}

	fn kill(&self) {
		self.stopping.store(true, Ordering::Release);
		let kill = self
			.kill
			.lock()
			.unwrap_or_else(PoisonError::into_inner)
			.take();
		if let Some(kill) = kill {
			// The supervising task ends with rclone; if it has, there is nothing left to kill.
			let _ = kill.send(());
		}
	}
}

impl Drop for RcloneDaemon {
	fn drop(&mut self) {
		// The supervising task kills rclone once the kill sender drops with the daemon.
		self.stopping.store(true, Ordering::Release);
	}
}

#[cfg(feature = "test-seams")]
impl RcloneDaemon {
	/// [`RcloneDaemon::start`] with both remotes an alias of the local directory `dir`, so a test
	/// runs a daemon without an account.
	pub async fn start_local_for_test(
		config: RcloneDaemonConfig,
		dir: &Path,
	) -> Result<Self, Error> {
		Self::start_with(config, &RemoteBackend::local(dir)).await
	}

	/// The loopback port rclone's rc listens on.
	pub fn rc_port_for_test(&self) -> u16 {
		self.rc.port()
	}

	/// Calls the rc method `path` with `body`.
	pub async fn rc_call_for_test(&self, path: &'static str, body: &Value) -> Result<Value, Error> {
		self.rc.call(path, body).await
	}
}

impl From<CoreStatsTransfer> for RcloneTransfer {
	fn from(transfer: CoreStatsTransfer) -> Self {
		Self {
			name: transfer.name,
			bytes: u64::try_from(transfer.bytes).unwrap_or(0),
			size: u64::try_from(transfer.size).ok(),
			speed_bps: transfer.speed.to_u64().unwrap_or(0),
		}
	}
}

/// Refuses an rclone that reports `version` if it is older than [`MIN_RCLONE`] or unreadable.
fn check_version(version: String) -> Result<(), Error> {
	match parse_version(&version) {
		Some(found) if found >= MIN_RCLONE => Ok(()),
		_ => Err(RcloneRefusal::UnsupportedVersion { found: version }.into()),
	}
}

/// A count rclone reports, which is never negative.
fn count(n: i64) -> u32 {
	u32::try_from(n.max(0)).unwrap_or(u32::MAX)
}

/// rcd's argv. Holds no secret: see the module docs.
fn rcd_args<'a>(
	config: &'a RcloneDaemonConfig,
	rc_addr: &'a str,
	htpasswd: &'a Path,
) -> Vec<&'a OsStr> {
	let mut args: Vec<&OsStr> = ["rcd", "--config", "", "--rc-addr", rc_addr, "--rc-htpasswd"]
		.map(OsStr::new)
		.into();
	args.push(htpasswd.as_os_str());
	args.push(OsStr::new("--cache-dir"));
	args.push(config.cache_dir.as_os_str());
	args.push(OsStr::new("--user-agent"));
	args.push(OsStr::new(&config.user_agent));
	if let Some(log_file) = &config.log_file {
		args.push(OsStr::new("--log-file"));
		args.push(log_file.as_os_str());
		args.extend(["--log-file-max-size", "10M", "--log-file-max-backups", "3"].map(OsStr::new));
	}
	args.extend(TRANSFER_ARGS.map(OsStr::new));
	#[cfg(windows)]
	args.push(OsStr::new("--no-console"));
	args
}

/// The changes rclone's environment gets from the host's `vars`: every `RCLONE_*` variable is
/// removed, as rclone reads its options and remotes from them (`RCLONE_RC_NO_AUTH`,
/// `RCLONE_CONFIG_<remote>_*`) and they would override what the daemon sets; and on Unix, the
/// system directories a mount needs are added to `PATH`.
fn environment_overrides(
	vars: impl Iterator<Item = (OsString, OsString)>,
) -> Vec<(OsString, Option<OsString>)> {
	let mut overrides = Vec::new();
	#[cfg(unix)]
	let mut path = None;
	for (key, value) in vars {
		if key
			.to_string_lossy()
			.to_ascii_uppercase()
			.starts_with("RCLONE_")
		{
			overrides.push((key, None));
		} else {
			#[cfg(unix)]
			if key == "PATH" {
				path = Some(value);
			}
			#[cfg(not(unix))]
			drop(value);
		}
	}
	#[cfg(unix)]
	overrides.push((OsString::from("PATH"), Some(with_system_path(path))));
	overrides
}

/// Waits for rclone to exit, killing it first when `kill` fires or its sender is dropped, and
/// publishes how it ended.
async fn supervise(
	mut child: Child,
	stderr: ChildStderr,
	mut kill: oneshot::Receiver<()>,
	stopping: Arc<AtomicBool>,
	exit: watch::Sender<Option<RcloneExit>>,
) {
	// Shared with the reader, which may outlive rclone while a process it started holds the pipe.
	let tail = Arc::new(Mutex::new(StderrTail::default()));
	let reader = tokio::spawn(read_stderr(stderr, Arc::clone(&tail)));
	let status = tokio::select! {
		status = child.wait() => status,
		_ = &mut kill => {
			if let Err(e) = child.start_kill() {
				warn!("killing rclone failed: {e}");
			}
			child.wait().await
		}
	};
	if tokio::time::timeout(STDERR_EOF_WAIT, reader).await.is_err() {
		warn!("rclone's stderr is still open {STDERR_EOF_WAIT:?} after it exited");
	}
	let ended = if stopping.load(Ordering::Acquire) {
		RcloneExit::Stopped
	} else {
		let code = match status {
			Ok(status) => status.code(),
			Err(e) => {
				warn!("waiting for rclone failed: {e}");
				None
			}
		};
		RcloneExit::Crashed {
			code,
			stderr_tail: tail.lock().unwrap_or_else(PoisonError::into_inner).text(),
		}
	};
	exit.send_replace(Some(ended));
}

/// Logs rclone's stderr line by line and keeps its last lines in `tail`.
async fn read_stderr(stderr: ChildStderr, tail: Arc<Mutex<StderrTail>>) {
	let mut lines = BufReader::new(stderr).split(b'\n');
	loop {
		match lines.next_segment().await {
			Ok(Some(line)) => {
				let line = String::from_utf8_lossy(&line);
				let line = line.trim_end();
				info!("rclone: {line}");
				tail.lock()
					.unwrap_or_else(PoisonError::into_inner)
					.push(line);
			}
			Ok(None) => return,
			Err(e) => {
				warn!("reading rclone's stderr failed: {e}");
				return;
			}
		}
	}
}

/// The last [`STDERR_TAIL_LINES`] lines rclone wrote to stderr, each cut at
/// [`STDERR_TAIL_LINE_LEN`] bytes.
#[derive(Default)]
struct StderrTail(VecDeque<String>);

impl StderrTail {
	fn push(&mut self, line: &str) {
		if self.0.len() == STDERR_TAIL_LINES {
			self.0.pop_front();
		}
		let end = line.floor_char_boundary(STDERR_TAIL_LINE_LEN);
		self.0
			.push_back(line.get(..end).unwrap_or_default().to_owned());
	}

	fn text(&self) -> String {
		self.0
			.iter()
			.map(String::as_str)
			.collect::<Vec<_>>()
			.join("\n")
	}
}

/// rc's htpasswd file, holding only the SHA-1 of its password. Removed when dropped, with the
/// daemon that uses it.
struct Htpasswd {
	path: PathBuf,
}

impl Htpasswd {
	fn write(state_dir: &Path, password: &str) -> Result<Self, Error> {
		let path = state_dir.join(HTPASSWD_FILE);
		write_private_file(&path, &htpasswd_line(RC_USER, password))
			.context(format!("failed to write {}", path.display()))?;
		Ok(Self { path })
	}

	fn remove(&self) {
		if let Err(e) = std::fs::remove_file(&self.path)
			&& e.kind() != std::io::ErrorKind::NotFound
		{
			warn!("removing {} failed: {e}", self.path.display());
		}
	}
}

impl Drop for Htpasswd {
	fn drop(&mut self) {
		self.remove();
	}
}

/// An htpasswd line in its `{SHA}` form: `user:{SHA}<base64 of the password's SHA-1>`. SHA-1 of
/// 256 random bits cannot be reversed.
fn htpasswd_line(user: &str, password: &str) -> String {
	format!(
		"{user}:{{SHA}}{}\n",
		BASE64_STANDARD.encode(Sha1::digest(password.as_bytes()))
	)
}

/// Creates `dir` and its missing parents, and makes it owner-only (0700) on Unix.
async fn create_private_dir(dir: &Path) -> Result<(), Error> {
	tokio::fs::create_dir_all(dir)
		.await
		.context(format!("failed to create {}", dir.display()))?;
	#[cfg(unix)]
	tokio::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
		.await
		.context(format!("failed to make {} private", dir.display()))?;
	Ok(())
}

#[cfg(test)]
mod tests;
