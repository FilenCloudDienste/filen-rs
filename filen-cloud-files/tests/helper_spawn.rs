//! The helper binary as the desktop app spawns it. The exe comes from `FILEN_CLOUD_FILES_HELPER`
//! when set, as the build machine's path is wrong in a Windows VM, and from Cargo otherwise.

#[cfg(windows)]
use std::{
	io::{BufRead, BufReader, Write},
	process::{Child, ChildStdout},
	sync::mpsc,
	thread,
	time::{Duration, Instant},
};
use std::{
	path::PathBuf,
	process::{Command, Stdio},
};

use serde_json::Value;

/// How long a helper may take to say its first line or to exit before the test fails: a hung
/// helper fails the test instead of blocking the run.
#[cfg(windows)]
const DEADLINE: Duration = Duration::from_secs(60);

fn helper() -> PathBuf {
	std::env::var_os("FILEN_CLOUD_FILES_HELPER").map_or_else(
		|| PathBuf::from(env!("CARGO_BIN_EXE_filen-cloud-files")),
		PathBuf::from,
	)
}

#[cfg(not(windows))]
#[test]
fn off_windows_the_helper_refuses_with_one_fatal_line() {
	let output = Command::new(helper())
		.args(["--state-dir", "/nonexistent"])
		.stdin(Stdio::null())
		.output()
		.unwrap();
	assert_eq!(output.status.code(), Some(14));
	let stdout = String::from_utf8(output.stdout).unwrap();
	let lines: Vec<&str> = stdout.lines().collect();
	assert_eq!(lines.len(), 1, "{stdout}");
	let line: Value = serde_json::from_str(lines[0]).unwrap();
	assert_eq!(line["t"], "fatal");
	assert_eq!(line["code"], "notWindows");
}

/// A spawned helper, killed once dropped, so a failed test leaves none running.
#[cfg(windows)]
struct Helper(Child);

#[cfg(windows)]
impl Helper {
	fn spawn(state: &std::path::Path) -> Self {
		Self(
			Command::new(helper())
				.arg("--state-dir")
				.arg(state)
				.stdin(Stdio::piped())
				.stdout(Stdio::piped())
				.stderr(Stdio::null())
				.spawn()
				.unwrap(),
		)
	}

	/// The next line the helper writes, within [`DEADLINE`]. Read on a thread of its own, which a
	/// kill of the helper ends; its stdout stays open after.
	fn first_line(&mut self) -> Value {
		let stdout = self.0.stdout.take().unwrap();
		let (sent, read) = mpsc::channel::<(String, ChildStdout)>();
		thread::spawn(move || {
			let mut reader = BufReader::new(stdout);
			let mut line = String::new();
			reader.read_line(&mut line).unwrap();
			// Fails only once the test stopped waiting, which has failed it already.
			let _ = sent.send((line, reader.into_inner()));
		});
		let (line, stdout) = read
			.recv_timeout(DEADLINE)
			.expect("the helper wrote no line");
		self.0.stdout = Some(stdout);
		serde_json::from_str(&line).unwrap()
	}

	fn write(&mut self, line: &[u8]) {
		self.0.stdin.as_mut().unwrap().write_all(line).unwrap();
	}

	fn close_stdin(&mut self) {
		drop(self.0.stdin.take());
	}

	/// The helper's exit code, once it exited within [`DEADLINE`].
	fn exit_code(&mut self) -> Option<i32> {
		let started = Instant::now();
		loop {
			if let Some(status) = self.0.try_wait().unwrap() {
				return status.code();
			}
			assert!(started.elapsed() < DEADLINE, "the helper never exited");
			thread::sleep(Duration::from_millis(10));
		}
	}
}

#[cfg(windows)]
impl Drop for Helper {
	fn drop(&mut self) {
		// Both fail only for a helper that exited already, which is what this wants.
		let _ = self.0.kill();
		let _ = self.0.wait();
	}
}

#[cfg(windows)]
#[test]
fn the_helper_says_hello_and_exits_cleanly_on_shutdown() {
	let temp = tempfile::tempdir().unwrap();
	let mut child = Helper::spawn(temp.path());
	assert_eq!(child.first_line()["t"], "hello");
	child.write(b"{\"t\":\"shutdown\",\"id\":1}\n");
	assert_eq!(child.exit_code(), Some(0));
}

#[cfg(windows)]
#[test]
fn a_second_helper_on_the_same_state_dir_is_refused() {
	let temp = tempfile::tempdir().unwrap();
	let mut first = Helper::spawn(temp.path());
	assert_eq!(first.first_line()["t"], "hello");
	let mut second = Helper::spawn(temp.path());
	let refused = second.first_line();
	assert_eq!(refused["code"], "alreadyRunning");
	assert_eq!(second.exit_code(), Some(12));
	first.close_stdin();
	assert_eq!(first.exit_code(), Some(0));
}

#[cfg(windows)]
#[test]
fn a_closed_stdin_exits_cleanly() {
	let temp = tempfile::tempdir().unwrap();
	let mut child = Helper::spawn(temp.path());
	assert_eq!(child.first_line()["t"], "hello");
	child.close_stdin();
	assert_eq!(child.exit_code(), Some(0));
}
