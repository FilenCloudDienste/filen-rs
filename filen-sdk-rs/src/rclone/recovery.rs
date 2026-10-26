//! What a daemon leaves behind for the next one: its pidfile, and the recovery from a daemon whose
//! host crashed (its rclone may still run, its mounts may be stale) or from the old desktop
//! client's rclone.

#[cfg(unix)]
use std::os::unix::fs::MetadataExt as _;
use std::{
	io,
	path::{Path, PathBuf},
	time::Duration,
};

use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::{
	Error,
	error::ResultExt as _,
	rclone::{
		private_file::write_private_file,
		system::{ProcessIdentity, identify, processes_under, stop_processes},
	},
};

#[cfg(unix)]
use crate::{ErrorKind, rclone::system::system_command};

/// The pidfile in a daemon's state directory.
pub(crate) const PIDFILE: &str = "rcd.json";
/// rc's credentials file in a daemon's state directory.
pub(crate) const HTPASSWD_FILE: &str = "rc.htpasswd";
/// How long a stale rclone has to quit before it is killed.
const STALE_GRACE: Duration = Duration::from_secs(5);
/// How long a mount point may take to answer `stat` before it is taken for a stale mount: a
/// network mount whose server is gone blocks it indefinitely.
#[cfg(unix)]
const STAT_TIMEOUT: Duration = Duration::from_secs(3);
/// How long one unmount command may run.
#[cfg(unix)]
const UNMOUNT_TIMEOUT: Duration = Duration::from_secs(10);

/// A running daemon's rclone, as its pidfile records it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PidRecord {
	pub(crate) pid: u32,
	/// rclone's start time in seconds since the Unix epoch, which tells it from a later process
	/// that got its pid.
	pub(crate) started_at: u64,
	/// The rclone binary, canonical.
	pub(crate) binary: PathBuf,
	/// The mount points rclone has mounted.
	pub(crate) mounts: Vec<String>,
}

impl PidRecord {
	/// Writes the record to the pidfile at `path`, replacing it in one rename.
	pub(crate) fn write(&self, path: &Path) -> Result<(), Error> {
		let mut partial = path.as_os_str().to_os_string();
		partial.push(".part");
		let json =
			serde_json::to_string(self).expect("a pid record is plain data (should be impossible)");
		write_private_file(Path::new(&partial), &json)
			.and_then(|()| std::fs::rename(&partial, path))
			.context(format!("failed to write {}", path.display()))
	}

	/// The record in `state_dir`'s pidfile, if there is one. One that cannot be read as a record
	/// is logged and taken as none.
	pub(crate) fn read(state_dir: &Path) -> Result<Option<Self>, Error> {
		let path = state_dir.join(PIDFILE);
		let json = match std::fs::read(&path) {
			Ok(json) => json,
			Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
			Err(e) => {
				return Err(
					Error::from(e).with_context(format!("failed to read {}", path.display()))
				);
			}
		};
		match serde_json::from_slice(&json) {
			Ok(record) => Ok(Some(record)),
			Err(e) => {
				warn!(
					"ignoring the unreadable rclone pidfile {}: {e}",
					path.display()
				);
				Ok(None)
			}
		}
	}
}

/// What [`recover_stale`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaleRecovery {
	/// Whether the rclone a crashed host left running was stopped.
	pub stopped: bool,
	/// The stale mounts that were force-unmounted.
	pub unmounted: Vec<String>,
}

/// Recovers from a daemon on `state_dir` whose host ended without stopping it.
///
/// When its pidfile names a process that is still the rclone it started (same pid, start time
/// and executable), that rclone is asked to terminate and killed after 5 s. A process that only
/// reuses the pid is left alone. The mounts it recorded are then force-unmounted where they are
/// still mounted, and the pidfile and rc credentials file are removed. [`RcloneDaemon::start`]
/// runs this first; a host may also run it on its own at startup.
///
/// [`RcloneDaemon::start`]: crate::rclone::RcloneDaemon::start
pub async fn recover_stale(state_dir: &Path) -> Result<StaleRecovery, Error> {
	let mut recovery = StaleRecovery {
		stopped: false,
		unmounted: Vec::new(),
	};
	if let Some(record) = PidRecord::read(state_dir)? {
		if let Some(process) = identify(record.pid).await?
			&& is_recorded_rclone(&process, &record)
		{
			info!(
				"stopping the rclone (pid {}) a previous host left running",
				record.pid
			);
			recovery.stopped = stop_processes(&[process], STALE_GRACE).await? > 0;
		}
		for mount in record.mounts {
			match force_unmount(Path::new(&mount)).await {
				Ok(true) => recovery.unmounted.push(mount),
				Ok(false) => {}
				Err(e) => warn!("force-unmounting the stale mount {mount} failed: {e}"),
			}
		}
	}
	for file in [PIDFILE, HTPASSWD_FILE] {
		remove_if_present(&state_dir.join(file))?;
	}
	Ok(recovery)
}

fn is_recorded_rclone(process: &ProcessIdentity, record: &PidRecord) -> bool {
	process.started_at == record.started_at
		&& process
			.exe
			.as_deref()
			.is_some_and(|exe| exe == record.binary)
}

/// Stops every process whose executable is inside `dir`, such as the old desktop client's rclone
/// under `<userData>/rclone/bin`: asks it to terminate and kills it after `grace_ms`. Returns how
/// many it stopped; a missing `dir` stops none.
pub async fn stop_rclone_under(dir: &Path, grace_ms: u32) -> Result<u32, Error> {
	let canonical = match tokio::fs::canonicalize(dir).await {
		Ok(canonical) => canonical,
		Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
		Err(e) => {
			return Err(Error::from(e).with_context(format!("failed to resolve {}", dir.display())));
		}
	};
	// A process's executable is reported as the path it was started by, which on macOS is not
	// resolved (`/var/...` for `/private/var/...`), so both spellings count.
	let processes = processes_under(vec![dir.to_path_buf(), canonical]).await?;
	if processes.is_empty() {
		return Ok(0);
	}
	stop_processes(&processes, Duration::from_millis(grace_ms.into())).await
}

/// Whether something is mounted at a path, as far as `stat` can tell.
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MountState {
	/// The path is on its parent's device, or does not exist.
	NotMounted,
	/// The path is on a device of its own.
	Mounted,
	/// `stat` fails as it does on a mount whose server is gone (`ENOTCONN`, `EIO`, `ENXIO`), or
	/// does not answer within [`STAT_TIMEOUT`].
	Stale,
}

/// What is at `path`, without hanging on a stale mount: `stat` runs on a blocking thread, which a
/// mount that never answers keeps.
#[cfg(unix)]
pub(crate) async fn mount_state(path: &Path) -> Result<MountState, Error> {
	let owned = path.to_path_buf();
	let stat = tokio::task::spawn_blocking(move || stat_mount(&owned));
	match tokio::time::timeout(STAT_TIMEOUT, stat).await {
		Err(_) => Ok(MountState::Stale),
		Ok(Err(e)) => Err(Error::custom_with_source(
			ErrorKind::Internal,
			e,
			Some("the stat task failed"),
		)),
		Ok(Ok(state)) => state.context(format!("failed to stat {}", path.display())),
	}
}

/// `EIO` and `ENXIO`, the same on Linux and macOS.
#[cfg(unix)]
const EIO: i32 = 5;
#[cfg(unix)]
const ENXIO: i32 = 6;

#[cfg(unix)]
fn stat_mount(path: &Path) -> io::Result<MountState> {
	let metadata = match std::fs::metadata(path) {
		Ok(metadata) => metadata,
		Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(MountState::NotMounted),
		Err(e)
			if e.kind() == io::ErrorKind::NotConnected
				|| matches!(e.raw_os_error(), Some(EIO | ENXIO)) =>
		{
			return Ok(MountState::Stale);
		}
		Err(e) => return Err(e),
	};
	let parent = path.parent().unwrap_or(path);
	Ok(if std::fs::metadata(parent)?.dev() == metadata.dev() {
		MountState::NotMounted
	} else {
		MountState::Mounted
	})
}

/// Unmounts whatever is mounted at `path`, even when it is stale or busy: on macOS
/// `umount -f`, then `diskutil unmount force`; on Linux `fusermount3 -uz`, then `fusermount -uz`
/// and `umount -l`. Returns whether something was mounted there. Off Unix there is nothing to do:
/// a WinFsp drive goes with the rclone that served it.
pub async fn force_unmount(path: &Path) -> Result<bool, Error> {
	#[cfg(unix)]
	{
		if mount_state(path).await? == MountState::NotMounted {
			return Ok(false);
		}
		for (program, args) in unmount_commands() {
			let mut command = system_command(program);
			command.args(args).arg(path);
			match tokio::time::timeout(UNMOUNT_TIMEOUT, command.output()).await {
				Ok(Ok(output)) if output.status.success() => break,
				Ok(Ok(output)) => warn!(
					"{program} {} failed: {}",
					path.display(),
					String::from_utf8_lossy(&output.stderr).trim()
				),
				Ok(Err(e)) => warn!("running {program} failed: {e}"),
				Err(_) => warn!(
					"{program} {} did not finish within {UNMOUNT_TIMEOUT:?}",
					path.display()
				),
			}
		}
		match mount_state(path).await? {
			MountState::NotMounted => Ok(true),
			MountState::Mounted | MountState::Stale => Err(Error::custom(
				ErrorKind::IO,
				format!("could not unmount {}", path.display()),
			)),
		}
	}
	#[cfg(not(unix))]
	{
		let _ = path;
		Ok(false)
	}
}

/// The commands that force a mount off, in the order they are tried.
#[cfg(target_os = "macos")]
fn unmount_commands() -> [(&'static str, &'static [&'static str]); 2] {
	[("umount", &["-f"]), ("diskutil", &["unmount", "force"])]
}

#[cfg(all(unix, not(target_os = "macos")))]
fn unmount_commands() -> [(&'static str, &'static [&'static str]); 3] {
	[
		("fusermount3", &["-uz"]),
		("fusermount", &["-uz"]),
		("umount", &["-l"]),
	]
}

fn remove_if_present(path: &Path) -> Result<(), Error> {
	match std::fs::remove_file(path) {
		Ok(()) => Ok(()),
		Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
		Err(e) => Err(Error::from(e).with_context(format!("failed to remove {}", path.display()))),
	}
}

#[cfg(test)]
mod tests {
	use std::path::Path;

	use super::{HTPASSWD_FILE, PIDFILE, PidRecord, StaleRecovery, recover_stale};

	#[test]
	fn a_pid_record_reads_back_as_written() {
		let dir = tempfile::tempdir().unwrap();
		let record = PidRecord {
			pid: 4242,
			started_at: 1_790_000_000,
			binary: "/opt/filen/rclone".into(),
			mounts: vec!["/home/user/Filen".to_owned()],
		};

		record.write(&dir.path().join(PIDFILE)).unwrap();

		assert_eq!(PidRecord::read(dir.path()).unwrap(), Some(record));
		assert_eq!(
			std::fs::read_to_string(dir.path().join(PIDFILE)).unwrap(),
			r#"{"pid":4242,"startedAt":1790000000,"binary":"/opt/filen/rclone","mounts":["/home/user/Filen"]}"#
		);
		assert!(!dir.path().join(format!("{PIDFILE}.part")).exists());
	}

	#[tokio::test]
	async fn an_unreadable_pidfile_is_removed_as_none() {
		let dir = tempfile::tempdir().unwrap();
		std::fs::write(dir.path().join(PIDFILE), "{").unwrap();

		let recovery = recover_stale(dir.path()).await.unwrap();

		assert_eq!(recovery, nothing_done());
		assert!(!dir.path().join(PIDFILE).exists());
	}

	#[tokio::test]
	async fn without_a_pidfile_nothing_is_recovered() {
		let dir = tempfile::tempdir().unwrap();
		std::fs::write(dir.path().join(HTPASSWD_FILE), "filen:{SHA}x\n").unwrap();

		let recovery = recover_stale(dir.path()).await.unwrap();

		assert_eq!(recovery, nothing_done());
		assert!(!dir.path().join(HTPASSWD_FILE).exists());
	}

	fn nothing_done() -> StaleRecovery {
		StaleRecovery {
			stopped: false,
			unmounted: Vec::new(),
		}
	}

	#[cfg(unix)]
	mod processes {
		use std::time::Duration;

		use tokio::process::{Child, Command};

		use super::{super::stop_rclone_under, nothing_done};
		use crate::rclone::{
			recovery::{PIDFILE, PidRecord, StaleRecovery, recover_stale},
			system::identify,
		};

		fn sleeper() -> Child {
			Command::new("sleep")
				.arg("60")
				.kill_on_drop(true)
				.spawn()
				.unwrap()
		}

		/// The pid record of `child`, as a daemon would have written it for its rclone.
		async fn record_of(child: &Child) -> PidRecord {
			let process = identify(child.id().unwrap()).await.unwrap().unwrap();
			PidRecord {
				pid: process.pid,
				started_at: process.started_at,
				binary: process.exe.unwrap(),
				mounts: Vec::new(),
			}
		}

		async fn exits_within(child: &mut Child, timeout: Duration) -> bool {
			tokio::time::timeout(timeout, child.wait()).await.is_ok()
		}

		#[tokio::test(flavor = "multi_thread")]
		async fn stale_pidfile_of_a_live_rclone_is_stopped() {
			let dir = tempfile::tempdir().unwrap();
			let mut rclone = sleeper();
			record_of(&rclone)
				.await
				.write(&dir.path().join(PIDFILE))
				.unwrap();

			let recovery = recover_stale(dir.path()).await.unwrap();

			assert_eq!(
				recovery,
				StaleRecovery {
					stopped: true,
					unmounted: Vec::new()
				}
			);
			assert!(exits_within(&mut rclone, Duration::from_secs(5)).await);
			assert!(!dir.path().join(PIDFILE).exists());
		}

		#[tokio::test(flavor = "multi_thread")]
		async fn pidfile_of_a_reused_pid_is_left_alone() {
			let dir = tempfile::tempdir().unwrap();
			let mut other = sleeper();
			let mut record = record_of(&other).await;
			// The process the pidfile named started earlier; this one got its pid later.
			record.started_at -= 1000;
			record.write(&dir.path().join(PIDFILE)).unwrap();

			let recovery = recover_stale(dir.path()).await.unwrap();

			assert_eq!(recovery, nothing_done());
			assert!(!exits_within(&mut other, Duration::from_millis(500)).await);
			assert!(!dir.path().join(PIDFILE).exists());
		}

		#[tokio::test(flavor = "multi_thread")]
		async fn stale_pidfile_of_a_dead_process_is_removed() {
			let dir = tempfile::tempdir().unwrap();
			let mut gone = sleeper();
			let record = record_of(&gone).await;
			gone.kill().await.unwrap();
			record.write(&dir.path().join(PIDFILE)).unwrap();

			let recovery = recover_stale(dir.path()).await.unwrap();

			assert_eq!(recovery, nothing_done());
			assert!(!dir.path().join(PIDFILE).exists());
		}

		#[tokio::test]
		async fn stop_rclone_under_a_missing_directory_stops_nothing() {
			let dir = tempfile::tempdir().unwrap();

			let stopped = stop_rclone_under(&dir.path().join("missing"), 5000)
				.await
				.unwrap();

			assert_eq!(stopped, 0);
		}
	}

	#[cfg(unix)]
	#[tokio::test]
	async fn an_unmounted_directory_is_not_force_unmounted() {
		let dir = tempfile::tempdir().unwrap();

		let unmounted = super::force_unmount(dir.path()).await.unwrap();

		assert!(!unmounted);
		assert!(Path::new(dir.path()).exists());
	}
}
