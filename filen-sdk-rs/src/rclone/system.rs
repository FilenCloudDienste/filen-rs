//! Running the system's own tools (mount helpers, the watchdog's shell), and finding and stopping
//! processes by identity: a pid together with its start time, as a pid alone may have been reused.

#[cfg(unix)]
use std::ffi::OsString;
use std::{
	path::{Path, PathBuf},
	time::Duration,
};

use sysinfo::{
	Pid, ProcessRefreshKind, ProcessStatus, ProcessesToUpdate, Signal, System, UpdateKind,
};
use tokio::process::Command;

use crate::{Error, ErrorKind};

/// The directories a mount needs on `PATH`: nfsmount runs `mount` and `umount` from `/sbin`, and
/// fuse3 `fusermount3` from `/usr/bin`. An app started from a desktop launcher may get a `PATH`
/// without them.
#[cfg(unix)]
pub(crate) const SYSTEM_PATH: [&str; 4] = ["/usr/bin", "/bin", "/usr/sbin", "/sbin"];

/// <https://learn.microsoft.com/en-us/windows/win32/procthread/process-creation-flags>
#[cfg(windows)]
pub(crate) const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// How often a process being stopped is looked at.
const STOP_POLL: Duration = Duration::from_millis(100);
/// How long a killed process has to disappear.
const KILL_WAIT: Duration = Duration::from_secs(2);

/// `path` with every one of [`SYSTEM_PATH`] it lacks appended.
#[cfg(unix)]
pub(crate) fn with_system_path(path: Option<OsString>) -> OsString {
	let mut dirs: Vec<PathBuf> = path
		.as_deref()
		.map(|path| std::env::split_paths(path).collect())
		.unwrap_or_default();
	for dir in SYSTEM_PATH {
		if !dirs.iter().any(|known| known == Path::new(dir)) {
			dirs.push(dir.into());
		}
	}
	// The entries came from splitting a PATH, or are the constants above, so none holds the
	// separator that would make joining them fail.
	std::env::join_paths(dirs).unwrap_or_else(|_| path.unwrap_or_default())
}

/// A command for the system tool `program`, found on `PATH` with the system directories added,
/// and without a console window on Windows.
pub(crate) fn system_command(program: &str) -> Command {
	let mut command = Command::new(program);
	#[cfg(unix)]
	command.env("PATH", with_system_path(std::env::var_os("PATH")));
	#[cfg(windows)]
	command.creation_flags(CREATE_NO_WINDOW);
	command.kill_on_drop(true);
	command
}

/// A process as it was seen: its pid, its start time in seconds since the Unix epoch, and its
/// executable's path where the system reports one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProcessIdentity {
	pub(crate) pid: u32,
	pub(crate) started_at: u64,
	pub(crate) exe: Option<PathBuf>,
}

/// Without tasks: sysinfo otherwise lists each thread of a process on Linux as a process of its own,
/// with the process's executable, so a multi-threaded rclone would be found and counted once per
/// thread.
fn refresh_kind() -> ProcessRefreshKind {
	ProcessRefreshKind::nothing()
		.without_tasks()
		.with_exe(UpdateKind::OnlyIfNotSet)
}

/// The running process `pid`, or `None` when there is none, or only a zombie.
pub(crate) async fn identify(pid: u32) -> Result<Option<ProcessIdentity>, Error> {
	blocking(move || {
		let mut system = System::new();
		let pid = Pid::from_u32(pid);
		system.refresh_processes_specifics(ProcessesToUpdate::Some(&[pid]), true, refresh_kind());
		system
			.process(pid)
			.filter(|process| process.status() != ProcessStatus::Zombie)
			.map(|process| ProcessIdentity {
				pid: pid.as_u32(),
				started_at: process.start_time(),
				exe: process.exe().map(Path::to_path_buf),
			})
	})
	.await
}

/// Every running process whose executable is inside one of `dirs`, other than this one.
pub(crate) async fn processes_under(dirs: Vec<PathBuf>) -> Result<Vec<ProcessIdentity>, Error> {
	blocking(move || {
		let mut system = System::new();
		system.refresh_processes_specifics(ProcessesToUpdate::All, true, refresh_kind());
		let own = std::process::id();
		system
			.processes()
			.values()
			.filter(|process| {
				process.pid().as_u32() != own && process.status() != ProcessStatus::Zombie
			})
			.filter_map(|process| {
				let exe = process.exe()?;
				dirs.iter()
					.any(|dir| exe.starts_with(dir))
					.then(|| ProcessIdentity {
						pid: process.pid().as_u32(),
						started_at: process.start_time(),
						exe: Some(exe.to_path_buf()),
					})
			})
			.collect()
	})
	.await
}

/// Stops each of `processes` that is still the process it was seen as: asks it to terminate
/// (SIGTERM; where the platform has no such signal it is killed right away), and kills those still
/// running after `grace`. Returns how many it found running. Runs on a blocking thread for up to
/// `grace` and [`KILL_WAIT`].
pub(crate) async fn stop_processes(
	processes: &[ProcessIdentity],
	grace: Duration,
) -> Result<u32, Error> {
	let pids = processes
		.iter()
		.map(|process| (Pid::from_u32(process.pid), process.started_at))
		.collect::<Vec<_>>();
	blocking(move || {
		let running = signal_all(&pids, Signal::Term);
		if !wait_gone(&pids, grace) {
			signal_all(&pids, Signal::Kill);
			wait_gone(&pids, KILL_WAIT);
		}
		running
	})
	.await
}

/// Sends `signal` to each of `pids` still running with its start time, counting them.
fn signal_all(pids: &[(Pid, u64)], signal: Signal) -> u32 {
	let system = refreshed(pids);
	let mut signalled = 0u32;
	for process in pids
		.iter()
		.filter_map(|(pid, started_at)| running(&system, *pid, *started_at))
	{
		// `None`: the platform has no such signal (Windows has no SIGTERM), so it is killed.
		if process.kill_with(signal).unwrap_or_else(|| process.kill()) {
			signalled = signalled.saturating_add(1);
		}
	}
	signalled
}

/// Whether every one of `pids` is gone within `timeout`.
fn wait_gone(pids: &[(Pid, u64)], timeout: Duration) -> bool {
	let deadline = std::time::Instant::now() + timeout;
	loop {
		let system = refreshed(pids);
		if pids
			.iter()
			.all(|(pid, started_at)| running(&system, *pid, *started_at).is_none())
		{
			return true;
		}
		if std::time::Instant::now() >= deadline {
			return false;
		}
		std::thread::sleep(STOP_POLL);
	}
}

fn refreshed(pids: &[(Pid, u64)]) -> System {
	let mut system = System::new();
	let pids = pids.iter().map(|(pid, _)| *pid).collect::<Vec<_>>();
	system.refresh_processes_specifics(
		ProcessesToUpdate::Some(&pids),
		true,
		ProcessRefreshKind::nothing().without_tasks(),
	);
	system
}

fn running(system: &System, pid: Pid, started_at: u64) -> Option<&sysinfo::Process> {
	system
		.process(pid)
		.filter(|process| process.status() != ProcessStatus::Zombie)
		.filter(|process| process.start_time() == started_at)
}

/// Runs `f`, which reads the process table, off the async threads.
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Result<T, Error> {
	tokio::task::spawn_blocking(f).await.map_err(|e| {
		Error::custom_with_source(
			ErrorKind::Internal,
			e,
			Some("reading the process table failed"),
		)
	})
}
