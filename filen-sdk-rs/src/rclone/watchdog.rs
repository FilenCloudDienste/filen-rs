//! The watchdog that stops rclone when its host dies without stopping it (a crash, a kill), so
//! no rclone outlives the app and no mount is left hanging on a dead rclone.
//!
//! It is a separate process, in a process group of its own, so it survives the host: on Unix a
//! static shell script, given only pids and mount points as arguments and an empty environment;
//! on Windows a hidden PowerShell. While the host lives it is disarmed with the daemon.

#[cfg(windows)]
use std::path::Path;
use std::process::Stdio;

use tokio::{process::Command, sync::oneshot};
use tracing::warn;

use crate::{Error, rclone::RcloneRefusal};

#[cfg(windows)]
use crate::rclone::system::CREATE_NO_WINDOW;

/// The Unix watchdog. Its arguments are the host's pid, rclone's pid and the mount points.
///
/// It polls both every second and exits once rclone has. Once the host is gone it sends rclone
/// SIGTERM, then SIGKILL after 5 s, and force-unmounts every mount point. A zombie host (dead,
/// not reaped yet) still answers `kill -0`; its parent reaps it promptly.
#[cfg(unix)]
pub(crate) const WATCHDOG_SCRIPT: &str = r#"PATH=/usr/bin:/bin:/usr/sbin:/sbin
host=$1
rclone=$2
shift 2
while kill -0 "$rclone" 2>/dev/null; do
	if ! kill -0 "$host" 2>/dev/null; then
		kill -TERM "$rclone" 2>/dev/null
		waited=0
		while [ "$waited" -lt 5 ] && kill -0 "$rclone" 2>/dev/null; do
			sleep 1
			waited=$((waited + 1))
		done
		kill -KILL "$rclone" 2>/dev/null
		for mount in "$@"; do
			if [ "$(uname)" = Darwin ]; then
				umount -f "$mount" || diskutil unmount force "$mount"
			else
				fusermount3 -uz "$mount" || fusermount -uz "$mount" || umount -l "$mount"
			fi
		done >/dev/null 2>&1
		exit 0
	fi
	sleep 1
done
"#;

/// `$0` of the Unix watchdog, which `ps` shows.
#[cfg(unix)]
const WATCHDOG_NAME: &str = "filen-rclone-watchdog";

/// <https://learn.microsoft.com/en-us/windows/win32/procthread/process-creation-flags>
#[cfg(windows)]
const DETACHED_PROCESS: u32 = 0x0000_0008;

/// An armed watchdog. Dropping it disarms it: the watchdog process is killed.
pub(crate) struct Watchdog {
	_disarm: oneshot::Sender<()>,
}

/// Arms a watchdog that stops the rclone `rclone_pid`, started at `rclone_started_at` (seconds
/// since the Unix epoch), once the host `host_pid` is gone, and then force-unmounts `mounts`.
/// Refused with [`RcloneRefusal::WatchdogUnavailable`] when it cannot be started.
pub(crate) fn arm(
	host_pid: u32,
	rclone_pid: u32,
	rclone_started_at: u64,
	mounts: &[String],
) -> Result<Watchdog, Error> {
	let mut command = watchdog_command(host_pid, rclone_pid, rclone_started_at, mounts);
	let mut child = command.spawn().map_err(|e| {
		Error::from(RcloneRefusal::WatchdogUnavailable)
			.with_context(format!("failed to start the rclone watchdog: {e}"))
	})?;
	let (disarm, disarmed) = oneshot::channel::<()>();
	// The task reaps the watchdog when it exits by itself, and kills it once disarmed. When the
	// host dies the task dies with it, and the watchdog lives on.
	tokio::spawn(async move {
		tokio::select! {
			_ = child.wait() => {}
			_ = disarmed => {
				if let Err(e) = child.start_kill() {
					warn!("disarming the rclone watchdog failed: {e}");
				}
				if let Err(e) = child.wait().await {
					warn!("reaping the rclone watchdog failed: {e}");
				}
			}
		}
	});
	Ok(Watchdog { _disarm: disarm })
}

#[cfg(unix)]
fn watchdog_command(
	host_pid: u32,
	rclone_pid: u32,
	_rclone_started_at: u64,
	mounts: &[String],
) -> Command {
	let mut command = Command::new("/bin/sh");
	command
		.arg("-c")
		.arg(WATCHDOG_SCRIPT)
		.arg(WATCHDOG_NAME)
		.arg(host_pid.to_string())
		.arg(rclone_pid.to_string())
		.args(mounts);
	detach(&mut command);
	command
}

/// Waits for the host, then stops rclone if it is still the process started at
/// `rclone_started_at` (a later process may have its pid). WinFsp drives go with rclone, so there
/// is nothing to unmount.
#[cfg(windows)]
fn watchdog_command(
	host_pid: u32,
	rclone_pid: u32,
	rclone_started_at: u64,
	_mounts: &[String],
) -> Command {
	let script = format!(
		"$ErrorActionPreference = 'SilentlyContinue'; \
		 Wait-Process -Id {host_pid}; \
		 $rclone = Get-Process -Id {rclone_pid}; \
		 if ($rclone -and [Math]::Abs(([DateTimeOffset]$rclone.StartTime).ToUnixTimeSeconds() - {rclone_started_at}) -le 1) \
		 {{ Stop-Process -Id {rclone_pid} -Force }}"
	);
	let system_root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
	let powershell =
		Path::new(&system_root).join(r"System32\WindowsPowerShell\v1.0\powershell.exe");
	let mut command = Command::new(powershell);
	command
		.args(["-NoProfile", "-NonInteractive", "-Command"])
		.arg(script);
	detach(&mut command);
	// PowerShell does not start without it. It names a directory, nothing secret.
	command.env("SystemRoot", system_root);
	command
}

/// Cuts the watchdog loose from the host: an empty environment, no stdio, and on Unix a process
/// group of its own (a terminal's Ctrl-C or a group kill of the host does not reach it); on
/// Windows no console.
fn detach(command: &mut Command) {
	command
		.env_clear()
		.stdin(Stdio::null())
		.stdout(Stdio::null())
		.stderr(Stdio::null())
		.kill_on_drop(false);
	#[cfg(unix)]
	command.process_group(0);
	#[cfg(windows)]
	command.creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS);
}

#[cfg(all(test, unix))]
mod tests {
	use std::time::Duration;

	use tokio::process::{Child, Command};

	use super::{arm, watchdog_command};

	/// A stand-in process: `sleep` for long enough that only a signal ends it.
	fn sleeper() -> Child {
		Command::new("sleep")
			.arg("60")
			.kill_on_drop(true)
			.spawn()
			.unwrap()
	}

	#[test]
	fn watchdog_command_clears_the_environment() {
		let command = watchdog_command(10, 20, 30, &["/home/user/Filen".to_owned()]);

		let printed = format!("{:?}", command.as_std());

		assert!(printed.starts_with("env -i "), "{printed}");
		assert!(
			printed.ends_with(r#""filen-rclone-watchdog" "10" "20" "/home/user/Filen""#),
			"{printed}"
		);
	}

	#[tokio::test(flavor = "multi_thread")]
	async fn killing_the_host_stops_rclone() {
		let mut host = sleeper();
		let mut rclone = sleeper();
		let watchdog = arm(host.id().unwrap(), rclone.id().unwrap(), 0, &[]).unwrap();

		host.kill().await.unwrap();

		// The watchdog polls every second, then sends SIGTERM, which ends `sleep`.
		let status = tokio::time::timeout(Duration::from_secs(10), rclone.wait())
			.await
			.expect("the watchdog did not stop rclone within 10 s")
			.unwrap();
		assert!(!status.success());
		drop(watchdog);
	}

	#[tokio::test(flavor = "multi_thread")]
	async fn a_disarmed_watchdog_leaves_rclone_alone() {
		let mut host = sleeper();
		let mut rclone = sleeper();
		let watchdog = arm(host.id().unwrap(), rclone.id().unwrap(), 0, &[]).unwrap();

		drop(watchdog);
		// Give the disarm a moment to reach the watchdog before the host goes.
		tokio::time::sleep(Duration::from_millis(500)).await;
		host.kill().await.unwrap();

		let waited = tokio::time::timeout(Duration::from_secs(3), rclone.wait()).await;
		assert!(waited.is_err(), "rclone was stopped: {waited:?}");
	}
}
