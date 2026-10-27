//! The rclone daemon against a real rclone, with its remotes an alias of a local directory, so
//! no account or network is needed.
//!
//! The binary is `FILEN_TEST_RCLONE`, or with `rclone-managed` the managed download (kept in the
//! user's cache directory across runs). Without either the tests fail rather than skip.

use std::{
	path::{Path, PathBuf},
	time::Duration,
};

use base64::{Engine as _, prelude::BASE64_STANDARD};
use filen_sdk_rs::rclone::{
	MountOptions, RcloneBinary, RcloneDaemon, RcloneDaemonConfig, RcloneExit, RcloneRefusal,
	ServeAuth, ServeOptions, ServeProtocol, ServeTls, StopOptions, StopReport, VfsTuning,
	stop_rclone_under,
};
use serde_json::json;
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};

const GRACEFUL: StopOptions = StopOptions {
	drain_timeout_ms: None,
	grace_ms: 10_000,
};

fn binary() -> RcloneBinary {
	let path = match std::env::var_os("FILEN_TEST_RCLONE") {
		Some(path) => PathBuf::from(path),
		None => managed_rclone().to_path_buf(),
	};
	RcloneBinary::Bundled { path, sha256: None }
}

/// The managed download, resolved once per test binary: the tests run in parallel, and on an
/// empty cache each would download the release itself.
#[cfg(feature = "rclone-managed")]
fn managed_rclone() -> &'static Path {
	static PATH: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
	PATH.get_or_init(|| {
		let managed = RcloneBinary::Managed {
			dir: dirs::cache_dir()
				.expect("no user cache directory for the managed rclone")
				.join("filen-rs-rclone-daemon-tests"),
		};
		// On a runtime of its own: the calling test's runtime is blocked until it is resolved.
		std::thread::spawn(move || {
			tokio::runtime::Runtime::new()
				.unwrap()
				.block_on(managed.resolve())
				.unwrap()
		})
		.join()
		.unwrap()
	})
}

#[cfg(not(feature = "rclone-managed"))]
fn managed_rclone() -> &'static Path {
	panic!("set FILEN_TEST_RCLONE to an rclone binary, or enable `rclone-managed`")
}

/// A daemon's directories, removed with it.
struct Dirs {
	root: TempDir,
	/// Forced off before the directories are removed, so a failed test leaves no mount behind
	/// (removing a stale mount's directory blocks).
	#[cfg(unix)]
	mount_points: std::sync::Mutex<Vec<String>>,
}

impl Dirs {
	fn new() -> Self {
		let root = tempfile::tempdir().unwrap();
		std::fs::create_dir(root.path().join("remote")).unwrap();
		Self {
			root,
			#[cfg(unix)]
			mount_points: std::sync::Mutex::default(),
		}
	}

	fn remote(&self) -> PathBuf {
		self.root.path().join("remote")
	}

	/// An empty mount point, by its resolved path, which is what the system lists mounts by.
	#[cfg(unix)]
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

#[cfg(unix)]
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
	let mut listed = daemon
		.rc_call_for_test("config/listremotes", &json!({}))
		.await
		.unwrap();
	// rclone lists them in no fixed order.
	let mut remotes = serde_json::from_value::<Vec<String>>(listed["remotes"].take()).unwrap();
	remotes.sort();

	assert_eq!(paths["config"], json!(""));
	assert_eq!(remotes, ["Filen", "FilenRO"]);
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
	let old_rclone = old_bin.join(format!("rclone{}", std::env::consts::EXE_SUFFIX));
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

/// Downloads of the managed release into one empty directory at once all resolve to one binary,
/// which runs, and leave nothing else behind.
#[cfg(feature = "rclone-managed")]
#[tokio::test(flavor = "multi_thread")]
async fn concurrent_managed_downloads_install_one_runnable_binary() {
	let dir = tempfile::tempdir().unwrap();
	let managed = RcloneBinary::Managed {
		dir: dir.path().to_path_buf(),
	};
	let mut downloads = tokio::task::JoinSet::new();
	for _ in 0..3 {
		let managed = managed.clone();
		downloads.spawn(async move { managed.resolve().await.unwrap() });
	}

	let paths = downloads.join_all().await;

	assert!(paths.iter().all(|path| *path == paths[0]));
	let left = std::fs::read_dir(dir.path())
		.unwrap()
		.map(|entry| entry.unwrap().path())
		.collect::<Vec<_>>();
	assert_eq!(left, [paths[0].clone()]);
	let version = tokio::process::Command::new(&paths[0])
		.arg("version")
		.output()
		.await
		.unwrap();
	assert!(version.status.success(), "{version:?}");
	assert!(version.stdout.starts_with(b"rclone v"), "{version:?}");
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

#[cfg(unix)]
fn kill_9(pid: u32) {
	let killed = std::process::Command::new("kill")
		.args(["-9", &pid.to_string()])
		.status()
		.unwrap();
	assert!(killed.success());
}

// Unix: the mount point is a directory, where Windows mounts on a drive letter.
#[cfg(unix)]
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

// Unix: the mount point is a directory, where Windows mounts on a drive letter.
#[cfg(unix)]
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

// Unix: the mount point is a directory, where Windows mounts on a drive letter.
#[cfg(unix)]
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

// Unix: the mount point is a directory, where Windows mounts on a drive letter.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_mount_and_a_serve_share_one_vfs() {
	let dirs = Dirs::new();
	let daemon = dirs.start().await;
	let mount_point = dirs.mount_point("drive");
	daemon.mount(read_write(&mount_point)).await.unwrap();

	daemon
		.start_serve(serve(ServeProtocol::Webdav, basic_auth()))
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

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread")]
async fn a_drive_letter_mount_writes_through_to_the_remote_and_unmounts() {
	let dirs = Dirs::new();
	let daemon = dirs.start().await;
	let letter = filen_sdk_rs::rclone::available_drive_letters()
		.await
		.pop()
		.expect("no free drive letter");
	let drive = format!("{letter}:");

	let info = daemon.mount(read_write(&letter)).await.unwrap();

	assert_eq!(info.mount_point, drive);
	tokio::fs::write(format!(r"{drive}\file.txt"), "content")
		.await
		.unwrap();
	// Uploaded once the VFS cache's write-back delay (5 s) has passed.
	assert!(
		eventually_holds(
			&dirs.remote().join("file.txt"),
			"content",
			Duration::from_secs(20)
		)
		.await
	);
	daemon.unmount(&drive).await.unwrap();
	assert!(!Path::new(&format!(r"{drive}\")).exists());
	daemon.stop(GRACEFUL).await.unwrap();
}

const USER: &str = "filen-0a1b2c3d";
const PASSWORD: &str = "correct-horse-battery";

fn basic_auth() -> ServeAuth {
	ServeAuth::Basic {
		user: USER.to_owned(),
		password: PASSWORD.to_owned(),
	}
}

/// A serve of `protocol` on a free loopback port.
fn serve(protocol: ServeProtocol, auth: ServeAuth) -> ServeOptions {
	ServeOptions {
		protocol,
		host: "127.0.0.1".to_owned(),
		port: port_check::free_local_ipv4_port().unwrap(),
		auth,
		tls: None,
		read_only: false,
	}
}

fn http_client() -> reqwest::Client {
	reqwest::Client::builder().no_proxy().build().unwrap()
}

/// The FTP replies to logging in as `user` with `password`: the greeting, `USER` and `PASS`.
async fn ftp_login(addr: &str, user: &str, password: &str) -> Vec<String> {
	let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
	let (read, mut write) = stream.into_split();
	let mut lines = tokio::io::BufReader::new(read).lines();
	let mut replies = vec![lines.next_line().await.unwrap().unwrap()];
	for command in [format!("USER {user}\r\n"), format!("PASS {password}\r\n")] {
		write.write_all(command.as_bytes()).await.unwrap();
		replies.push(lines.next_line().await.unwrap().unwrap());
	}
	replies
}

fn reply_codes(replies: &[String]) -> Vec<&str> {
	replies
		.iter()
		.map(|reply| reply.get(..3).unwrap())
		.collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn every_authenticated_protocol_refuses_an_unauthenticated_request() {
	let dirs = Dirs::new();
	let daemon = dirs.start().await;
	let http = http_client();

	for protocol in [
		ServeProtocol::Webdav,
		ServeProtocol::Http,
		ServeProtocol::Restic,
	] {
		let info = daemon
			.start_serve(serve(protocol, basic_auth()))
			.await
			.unwrap();
		let url = format!("http://{}/", info.addr);
		let anonymous = http.get(&url).send().await.unwrap().status().as_u16();
		let wrong = http
			.get(&url)
			.basic_auth(USER, Some("wrong"))
			.send()
			.await
			.unwrap()
			.status()
			.as_u16();
		assert_eq!((anonymous, wrong), (401, 401), "{protocol:?}");
	}
	// restic answers its repository paths, not `/`, so only these two list the drive there.
	for protocol in [ServeProtocol::Webdav, ServeProtocol::Http] {
		let info = daemon
			.start_serve(serve(protocol, basic_auth()))
			.await
			.unwrap();
		let authenticated = http
			.get(format!("http://{}/", info.addr))
			.basic_auth(USER, Some(PASSWORD))
			.send()
			.await
			.unwrap()
			.status()
			.as_u16();
		assert_eq!(authenticated, 200, "{protocol:?}");
	}

	let s3 = daemon
		.start_serve(serve(
			ServeProtocol::S3,
			ServeAuth::S3 {
				access_key_id: "AKIA0123456789ABCDEF".to_owned(),
				secret_access_key: "c2VjcmV0LXNlY3JldC1zZWNyZXQtc2VjcmV0LXNl".to_owned(),
			},
		))
		.await
		.unwrap();
	let anonymous = http
		.get(format!("http://{}/", s3.addr))
		.send()
		.await
		.unwrap()
		.status();
	assert!(anonymous.is_client_error(), "{anonymous}");

	let ftp = daemon
		.start_serve(serve(ServeProtocol::Ftp, basic_auth()))
		.await
		.unwrap();
	assert_eq!(
		reply_codes(&ftp_login(&ftp.addr, "anonymous", "anonymous").await),
		["220", "331", "530"]
	);
	assert_eq!(
		reply_codes(&ftp_login(&ftp.addr, USER, "wrong").await),
		["220", "331", "530"]
	);
	assert_eq!(
		reply_codes(&ftp_login(&ftp.addr, USER, PASSWORD).await),
		["220", "331", "230"]
	);

	// sftp is left out: there is no SSH client here. Its options never let in a key
	// (`sftp_never_trusts_authorized_keys`) and need a password.
	daemon.stop(GRACEFUL).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn stats_list_the_running_serves() {
	let dirs = Dirs::new();
	let daemon = dirs.start().await;
	let webdav = daemon
		.start_serve(serve(ServeProtocol::Webdav, basic_auth()))
		.await
		.unwrap();
	#[cfg(not(windows))]
	let nfs = daemon
		.start_serve(serve(ServeProtocol::Nfs, ServeAuth::Unauthenticated))
		.await
		.unwrap();

	let mut serves = daemon.stats().await.unwrap().serves;
	serves.sort_by(|a, b| a.id.cmp(&b.id));

	#[cfg(not(windows))]
	assert_eq!(serves, [nfs, webdav.clone()]);
	#[cfg(windows)]
	assert_eq!(serves, [webdav.clone()]);
	daemon.stop_serve(&webdav.id).await.unwrap();
	assert!(
		!daemon
			.stats()
			.await
			.unwrap()
			.serves
			.iter()
			.any(|serve| serve.id == webdav.id)
	);
	daemon.stop(GRACEFUL).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn serve_types_list_every_protocol_the_daemon_serves() {
	let dirs = Dirs::new();
	let daemon = dirs.start().await;

	let mut types = daemon.serve_types().await.unwrap();
	types.sort_by_key(|protocol| format!("{protocol:?}"));

	assert_eq!(
		types,
		[
			ServeProtocol::Ftp,
			ServeProtocol::Http,
			#[cfg(not(windows))]
			ServeProtocol::Nfs,
			ServeProtocol::Restic,
			ServeProtocol::S3,
			ServeProtocol::Sftp,
			ServeProtocol::Webdav,
		]
	);
	daemon.stop(GRACEFUL).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn port_in_use_is_a_typed_refusal() {
	let dirs = Dirs::new();
	let daemon = dirs.start().await;
	let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
	let port = taken.local_addr().unwrap().port();

	let error = daemon
		.start_serve(ServeOptions {
			port,
			..serve(ServeProtocol::Webdav, basic_auth())
		})
		.await
		.unwrap_err();

	assert_eq!(
		error.downcast_ref::<RcloneRefusal>(),
		Some(&RcloneRefusal::PortInUse { port })
	);
	assert_eq!(daemon.stats().await.unwrap().serves, []);
	daemon.stop(GRACEFUL).await.unwrap();
}

/// A self-signed certificate for `localhost` and its key, as PEM files in `dir`, generated with
/// the system's `openssl`; returns their paths and the certificate's DER.
fn self_signed_certificate(dir: &Path) -> (PathBuf, PathBuf, Vec<u8>) {
	let cert = dir.join("serve.crt");
	let key = dir.join("serve.key");
	let status = std::process::Command::new("openssl")
		.args([
			"req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1",
		])
		.args(["-subj", "/CN=localhost", "-keyout"])
		.arg(&key)
		.arg("-out")
		.arg(&cert)
		.stderr(std::process::Stdio::null())
		.status()
		.expect("openssl is needed to generate the test certificate");
	assert!(status.success());
	let pem = std::fs::read_to_string(&cert).unwrap();
	let body = pem
		.lines()
		.filter(|line| !line.starts_with("-----"))
		.collect::<String>();
	let der = BASE64_STANDARD.decode(body).unwrap();
	(cert, key, der)
}

#[tokio::test(flavor = "multi_thread")]
async fn https_serve_presents_the_given_certificate() {
	let dirs = Dirs::new();
	let daemon = dirs.start().await;
	let (cert, key, der) = self_signed_certificate(dirs.root.path());

	let info = daemon
		.start_serve(ServeOptions {
			tls: Some(ServeTls {
				cert_path: cert.to_str().unwrap().to_owned(),
				key_path: key.to_str().unwrap().to_owned(),
			}),
			..serve(ServeProtocol::Webdav, basic_auth())
		})
		.await
		.unwrap();

	let https = reqwest::Client::builder()
		.no_proxy()
		.danger_accept_invalid_certs(true)
		.tls_info(true)
		.build()
		.unwrap();
	let response = https
		.get(format!("https://{}/", info.addr))
		.basic_auth(USER, Some(PASSWORD))
		.send()
		.await
		.unwrap();
	assert_eq!(response.status().as_u16(), 200);
	let presented = response
		.extensions()
		.get::<reqwest::tls::TlsInfo>()
		.and_then(reqwest::tls::TlsInfo::peer_certificate)
		.unwrap();
	assert_eq!(presented, der.as_slice());
	daemon.stop(GRACEFUL).await.unwrap();
}

fn files_named_conf(dir: &Path) -> Vec<PathBuf> {
	walkdir::WalkDir::new(dir)
		.into_iter()
		.map(|entry| entry.unwrap().into_path())
		.filter(|path| path.extension().is_some_and(|ext| ext == "conf"))
		.collect()
}
