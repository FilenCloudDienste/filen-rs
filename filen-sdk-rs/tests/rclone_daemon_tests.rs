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
	RcloneBinary, RcloneDaemon, RcloneDaemonConfig, RcloneExit, RcloneRefusal, StopOptions,
	StopReport, stop_rclone_under,
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
}

impl Dirs {
	fn new() -> Self {
		let root = tempfile::tempdir().unwrap();
		std::fs::create_dir(root.path().join("remote")).unwrap();
		Self { root }
	}

	fn remote(&self) -> PathBuf {
		self.root.path().join("remote")
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
		}
	}

	async fn start(&self) -> RcloneDaemon {
		RcloneDaemon::start_local_for_test(self.config(), &self.remote())
			.await
			.unwrap()
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

fn files_named_conf(dir: &Path) -> Vec<PathBuf> {
	walkdir::WalkDir::new(dir)
		.into_iter()
		.map(|entry| entry.unwrap().into_path())
		.filter(|path| path.extension().is_some_and(|ext| ext == "conf"))
		.collect()
}
