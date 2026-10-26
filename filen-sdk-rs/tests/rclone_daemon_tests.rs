//! The rclone daemon against a real rclone, with its remotes an alias of a local directory, so
//! no account or network is needed.
//!
//! The binary is `FILEN_TEST_RCLONE`, or with `rclone-managed` the managed download (kept in the
//! user's cache directory across runs). Without either the tests fail rather than skip.

use std::{
	path::{Path, PathBuf},
	time::Duration,
};

use filen_sdk_rs::rclone::{
	MountOptions, RcloneBinary, RcloneDaemon, RcloneDaemonConfig, RcloneExit, RcloneRefusal,
	StopOptions, StopReport, VfsTuning, stop_rclone_under,
};
use serde_json::json;
use tempfile::TempDir;

const GRACEFUL: StopOptions = StopOptions {
	drain_timeout_ms: None,
	grace_ms: 10_000,
};

fn binary() -> RcloneBinary {
	if let Some(path) = std::env::var_os("FILEN_TEST_RCLONE") {
		return RcloneBinary::Bundled {
			path: PathBuf::from(path),
			sha256: None,
		};
	}
	managed_binary()
}

#[cfg(feature = "rclone-managed")]
fn managed_binary() -> RcloneBinary {
	RcloneBinary::Managed {
		dir: dirs::cache_dir()
			.expect("no user cache directory for the managed rclone")
			.join("filen-rs-rclone-daemon-tests"),
	}
}

#[cfg(not(feature = "rclone-managed"))]
fn managed_binary() -> RcloneBinary {
	panic!("set FILEN_TEST_RCLONE to an rclone binary, or enable `rclone-managed`")
}

/// A daemon's directories, removed with it.
struct Dirs {
	root: TempDir,
	/// Forced off before the directories are removed, so a failed test leaves no mount behind
	/// (removing a stale mount's directory blocks).
	mount_points: std::sync::Mutex<Vec<String>>,
}

impl Dirs {
	fn new() -> Self {
		let root = tempfile::tempdir().unwrap();
		std::fs::create_dir(root.path().join("remote")).unwrap();
		Self {
			root,
			mount_points: std::sync::Mutex::default(),
		}
	}

	fn remote(&self) -> PathBuf {
		self.root.path().join("remote")
	}

	/// An empty mount point, by its resolved path, which is what the system lists mounts by.
	fn mount_point(&self, name: &str) -> String {
		let path = self.root.path().join(name);
		std::fs::create_dir(&path).unwrap();
		let mount_point = std::fs::canonicalize(path)
			.unwrap()
			.into_os_string()
			.into_string()
			.unwrap();
		self.mount_points.lock().unwrap().push(mount_point.clone());
		mount_point
	}

	fn state(&self) -> PathBuf {
		self.root.path().join("state")
	}

	fn config(&self) -> RcloneDaemonConfig {
		RcloneDaemonConfig {
			binary: binary(),
			state_dir: self.state(),
			cache_dir: self.root.path().join("cache"),
			log_file: Some(self.root.path().join("rclone.log")),
			user_agent: "filen-rs-tests".to_owned(),
			vfs: VfsTuning::default(),
		}
	}

	async fn start(&self) -> RcloneDaemon {
		RcloneDaemon::start_local_for_test(self.config(), &self.remote())
			.await
			.unwrap()
	}
}

impl Drop for Dirs {
	fn drop(&mut self) {
		for mount_point in self.mount_points.lock().unwrap().iter() {
			let (program, args): (&str, &[&str]) = if cfg!(target_os = "macos") {
				("/sbin/umount", &["-f"])
			} else {
				("fusermount3", &["-uz"])
			};
			// Nothing mounted there is the usual case, and its failure is expected.
			let _ = std::process::Command::new(program)
				.args(args)
				.arg(mount_point)
				.stderr(std::process::Stdio::null())
				.status();
		}
	}
}

fn process_is_running(pid: u32) -> bool {
	let system = sysinfo::System::new_with_specifics(
		sysinfo::RefreshKind::nothing().with_processes(sysinfo::ProcessRefreshKind::nothing()),
	);
	system.process(sysinfo::Pid::from_u32(pid)).is_some()
}

#[tokio::test(flavor = "multi_thread")]
async fn daemon_starts_and_stops_gracefully() {
	let dirs = Dirs::new();
	let daemon = dirs.start().await;
	let pid = daemon.pid();
	assert!(process_is_running(pid));

	let stats = daemon.stats().await.unwrap();
	assert_eq!(
		(
			stats.uploads_in_progress,
			stats.uploads_queued,
			stats.transfers.len()
		),
		(0, 0, 0)
	);

	let report = daemon.stop(GRACEFUL).await.unwrap();

	assert_eq!(
		report,
		StopReport {
			pending_uploads: 0,
			forced: false
		}
	);
	assert_eq!(daemon.exited().await, RcloneExit::Stopped);
	assert!(!process_is_running(pid));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_rc_credentials_file_is_gone_once_stopped() {
	let dirs = Dirs::new();
	let daemon = dirs.start().await;
	assert!(dirs.state().join("rc.htpasswd").exists());

	daemon.stop(GRACEFUL).await.unwrap();

	assert!(!dirs.state().join("rc.htpasswd").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn rc_refuses_without_credentials() {
	let dirs = Dirs::new();
	let daemon = dirs.start().await;
	let url = format!(
		"http://127.0.0.1:{}/core/version",
		daemon.rc_port_for_test()
	);
	let http = reqwest::Client::builder().no_proxy().build().unwrap();

	let anonymous = http.post(&url).json(&json!({})).send().await.unwrap();
	let wrong = http
		.post(&url)
		.basic_auth("filen", Some("wrong"))
		.json(&json!({}))
		.send()
		.await
		.unwrap();

	assert_eq!(anonymous.status().as_u16(), 401);
	assert_eq!(wrong.status().as_u16(), 401);
	daemon.stop(GRACEFUL).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn no_config_file_is_written() {
	let dirs = Dirs::new();
	let daemon = dirs.start().await;

	let paths = daemon
		.rc_call_for_test("config/paths", &json!({}))
		.await
		.unwrap();
	let remotes = daemon
		.rc_call_for_test("config/listremotes", &json!({}))
		.await
		.unwrap();

	assert_eq!(paths["config"], json!(""));
	assert_eq!(remotes, json!({ "remotes": ["Filen", "FilenRO"] }));
	daemon.stop(GRACEFUL).await.unwrap();
	assert_eq!(files_named_conf(dirs.root.path()), Vec::<PathBuf>::new());
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_killed_rclone_is_reported_as_crashed() {
	let dirs = Dirs::new();
	let daemon = dirs.start().await;

	let killed = std::process::Command::new("kill")
		.args(["-9", &daemon.pid().to_string()])
		.status()
		.unwrap();
	assert!(killed.success());

	let exit = tokio::time::timeout(Duration::from_secs(10), daemon.exited())
		.await
		.unwrap();
	assert!(
		matches!(exit, RcloneExit::Crashed { code: None, .. }),
		"{exit:?}"
	);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_missing_bundled_binary_starts_nothing() {
	let dirs = Dirs::new();
	let config = RcloneDaemonConfig {
		binary: RcloneBinary::Bundled {
			path: dirs.root.path().join("rclone"),
			sha256: None,
		},
		..dirs.config()
	};

	let error = RcloneDaemon::start_local_for_test(config, &dirs.remote())
		.await
		.err()
		.unwrap();

	assert_eq!(
		error.downcast_ref::<RcloneRefusal>(),
		Some(&RcloneRefusal::BinaryMissing)
	);
	assert!(!dirs.state().exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn the_pidfile_names_rclone_until_it_is_stopped() {
	let dirs = Dirs::new();
	let daemon = dirs.start().await;
	let pidfile = dirs.state().join("rcd.json");

	let record: serde_json::Value =
		serde_json::from_slice(&std::fs::read(&pidfile).unwrap()).unwrap();
	assert_eq!(record["pid"], json!(daemon.pid()));
	assert_eq!(record["mounts"], json!([]));

	daemon.stop(GRACEFUL).await.unwrap();
	assert!(!pidfile.exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_new_daemon_stops_the_rclone_a_crashed_host_left() {
	let dirs = Dirs::new();
	let crashed = dirs.start().await;
	let stale_pid = crashed.pid();
	// What a crashed host leaves: rclone running, its pidfile in place, the watchdog armed.
	std::mem::forget(crashed);

	let daemon = dirs.start().await;

	assert!(!process_is_running(stale_pid));
	assert_ne!(daemon.pid(), stale_pid);
	daemon.stop(GRACEFUL).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_rclone_under_ignores_other_binaries() {
	let dirs = Dirs::new();
	let rclone = binary().resolve().await.unwrap();
	let old_bin = dirs.root.path().join("old-client").join("bin");
	std::fs::create_dir_all(&old_bin).unwrap();
	let old_rclone = old_bin.join("rclone");
	std::fs::copy(&rclone, &old_rclone).unwrap();
	let mut old = idle_rcd(&old_rclone);
	let mut other = idle_rcd(&rclone);
	wait_for_exec(&old, &old_rclone).await;

	let stopped = stop_rclone_under(&old_bin, 5_000).await.unwrap();

	assert_eq!(stopped, 1);
	tokio::time::timeout(Duration::from_secs(5), old.wait())
		.await
		.expect("the old client's rclone is still running")
		.unwrap();
	assert!(
		tokio::time::timeout(Duration::from_millis(500), other.wait())
			.await
			.is_err(),
		"an rclone outside the directory was stopped"
	);
}

/// An `rclone rcd` that does nothing until it is stopped.
fn idle_rcd(program: &Path) -> tokio::process::Child {
	tokio::process::Command::new(program)
		.args([
			"rcd",
			"--config",
			"",
			"--rc-addr",
			"127.0.0.1:0",
			"--rc-no-auth",
		])
		.stdout(std::process::Stdio::null())
		.stderr(std::process::Stdio::null())
		.kill_on_drop(true)
		.spawn()
		.unwrap()
}

/// Waits until `child`'s executable is `program`: right after the spawn it may still be a fork of
/// the test binary.
async fn wait_for_exec(child: &tokio::process::Child, program: &Path) {
	let pid = sysinfo::Pid::from_u32(child.id().unwrap());
	for _ in 0..100 {
		let mut system = sysinfo::System::new();
		system.refresh_processes_specifics(
			sysinfo::ProcessesToUpdate::Some(&[pid]),
			true,
			sysinfo::ProcessRefreshKind::nothing().with_exe(sysinfo::UpdateKind::Always),
		);
		if system.process(pid).and_then(sysinfo::Process::exe) == Some(program) {
			return;
		}
		tokio::time::sleep(Duration::from_millis(20)).await;
	}
	panic!("{} did not start", program.display());
}

fn read_write(mount_point: &str) -> MountOptions {
	MountOptions {
		mount_point: mount_point.to_owned(),
		read_only: false,
	}
}

/// Waits up to `timeout` for `path` to hold `content`.
async fn eventually_holds(path: &Path, content: &str, timeout: Duration) -> bool {
	let deadline = tokio::time::Instant::now() + timeout;
	while tokio::time::Instant::now() < deadline {
		if tokio::fs::read_to_string(path)
			.await
			.is_ok_and(|read| read == content)
		{
			return true;
		}
		tokio::time::sleep(Duration::from_millis(200)).await;
	}
	false
}

fn kill_9(pid: u32) {
	let killed = std::process::Command::new("kill")
		.args(["-9", &pid.to_string()])
		.status()
		.unwrap();
	assert!(killed.success());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_mount_writes_through_to_the_remote_and_unmounts() {
	let dirs = Dirs::new();
	let daemon = dirs.start().await;
	let mount_point = dirs.mount_point("drive");

	let info = daemon.mount(read_write(&mount_point)).await.unwrap();

	assert_eq!(info.mount_point, mount_point);
	assert_eq!(
		daemon.stats().await.unwrap().mounts,
		std::slice::from_ref(&mount_point)
	);
	tokio::fs::write(Path::new(&mount_point).join("file.txt"), "content")
		.await
		.unwrap();
	assert_eq!(
		tokio::fs::read_to_string(Path::new(&mount_point).join("file.txt"))
			.await
			.unwrap(),
		"content"
	);
	// Uploaded once the VFS cache's write-back delay (5 s) has passed.
	assert!(
		eventually_holds(
			&dirs.remote().join("file.txt"),
			"content",
			Duration::from_secs(20)
		)
		.await
	);

	daemon.unmount(&mount_point).await.unwrap();

	assert_eq!(daemon.stats().await.unwrap().mounts, Vec::<String>::new());
	assert_eq!(std::fs::read_dir(&mount_point).unwrap().count(), 0);
	daemon.stop(GRACEFUL).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_mount_point_in_use_is_refused() {
	let dirs = Dirs::new();
	let daemon = dirs.start().await;
	let mount_point = dirs.mount_point("drive");
	daemon.mount(read_write(&mount_point)).await.unwrap();

	let error = daemon.mount(read_write(&mount_point)).await.unwrap_err();

	assert_eq!(
		error.downcast_ref::<RcloneRefusal>(),
		Some(&RcloneRefusal::MountPointInUse)
	);
	daemon.stop(GRACEFUL).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_drains_pending_uploads_within_the_timeout() {
	let dirs = Dirs::new();
	let daemon = dirs.start().await;
	let mount_point = dirs.mount_point("drive");
	daemon.mount(read_write(&mount_point)).await.unwrap();
	tokio::fs::write(Path::new(&mount_point).join("file.txt"), "content")
		.await
		.unwrap();

	let report = daemon
		.stop(StopOptions {
			drain_timeout_ms: Some(30_000),
			grace_ms: 10_000,
		})
		.await
		.unwrap();

	assert_eq!(
		report,
		StopReport {
			pending_uploads: 0,
			forced: false
		}
	);
	assert_eq!(
		std::fs::read_to_string(dirs.remote().join("file.txt")).unwrap(),
		"content"
	);
	assert_eq!(std::fs::read_dir(&mount_point).unwrap().count(), 0);
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_dirty_cache_item_is_uploaded_on_the_next_start() {
	let dirs = Dirs::new();
	let mount_point = dirs.mount_point("drive");
	let crashed = dirs.start().await;
	crashed.mount(read_write(&mount_point)).await.unwrap();
	tokio::fs::write(Path::new(&mount_point).join("file.txt"), "content")
		.await
		.unwrap();
	// Well inside the 5 s write-back delay: the file is only in the cache.
	kill_9(crashed.pid());
	tokio::time::timeout(Duration::from_secs(10), crashed.exited())
		.await
		.unwrap();
	drop(crashed);
	assert!(!dirs.remote().join("file.txt").exists());

	// The new daemon finds the crashed one's mount stale and forces it off, and the mount
	// creates the VFS that finds the file in the cache.
	let daemon = dirs.start().await;
	daemon.mount(read_write(&mount_point)).await.unwrap();

	assert!(
		eventually_holds(
			&dirs.remote().join("file.txt"),
			"content",
			Duration::from_secs(30)
		)
		.await
	);
	daemon.stop(GRACEFUL).await.unwrap();
}

// Linux only: macOS's NFS client keeps answering `stat` from its attribute cache for a few seconds
// after rclone dies, so there a stale mount without a pidfile first reads as in use. The pidfile's
// recovery and the watchdog are what take it down on macOS.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
async fn a_stale_mount_is_force_unmounted_before_mounting() {
	let dirs = Dirs::new();
	let mount_point = dirs.mount_point("drive");
	let crashed = dirs.start().await;
	crashed.mount(read_write(&mount_point)).await.unwrap();
	kill_9(crashed.pid());
	tokio::time::timeout(Duration::from_secs(10), crashed.exited())
		.await
		.unwrap();
	drop(crashed);
	// Without the pidfile no recovery runs at start, so the mount itself meets the stale mount.
	std::fs::remove_file(dirs.state().join("rcd.json")).unwrap();
	let daemon = dirs.start().await;

	daemon.mount(read_write(&mount_point)).await.unwrap();

	assert_eq!(daemon.stats().await.unwrap().mounts, [mount_point]);
	daemon.stop(GRACEFUL).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_mount_and_a_serve_share_one_vfs() {
	let dirs = Dirs::new();
	let daemon = dirs.start().await;
	let mount_point = dirs.mount_point("drive");
	daemon.mount(read_write(&mount_point)).await.unwrap();
	let port = port_check::free_local_ipv4_port().unwrap();

	// The command-line spelling of `VfsTuning::default()` and the fixed VFS options mounts get.
	daemon
		.rc_call_for_test(
			"serve/start",
			&json!({
				"type": "webdav",
				"fs": "Filen:",
				"addr": format!("127.0.0.1:{port}"),
				"user": "user",
				"pass": "pass",
				"vfs_cache_mode": "full",
				"vfs_cache_max_size": "34359738368B",
				"vfs_cache_min_free_space": "17179869184B",
				"vfs_cache_max_age": "720h",
				"dir_cache_time": "15s",
				"vfs_write_back": "5s",
				"vfs_read_chunk_size": "64Mi",
				"vfs_read_chunk_streams": "8",
				"vfs_read_ahead": "128Mi",
				"file_perms": "0666",
				"dir_perms": "0777",
				"no_checksum": "true",
				"read_only": "false",
			}),
		)
		.await
		.unwrap();

	let vfses = daemon
		.rc_call_for_test("vfs/list", &json!({}))
		.await
		.unwrap();
	let fs = vfses["vfses"][0].as_str().unwrap().to_owned();
	let stats = daemon
		.rc_call_for_test("vfs/stats", &json!({ "fs": fs }))
		.await
		.unwrap();
	assert_eq!(vfses["vfses"].as_array().unwrap().len(), 1, "{vfses}");
	assert_eq!(stats["inUse"], json!(2), "{stats}");
	daemon.stop(GRACEFUL).await.unwrap();
}

fn files_named_conf(dir: &Path) -> Vec<PathBuf> {
	walkdir::WalkDir::new(dir)
		.into_iter()
		.map(|entry| entry.unwrap().into_path())
		.filter(|path| path.extension().is_some_and(|ext| ext == "conf"))
		.collect()
}
