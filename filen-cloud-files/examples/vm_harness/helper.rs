//! `helper`: the helper as the shipping binary launches it (the same refusals, state directory
//! lock and runtime, through the library's `serve_stdio`), on the real Windows layer (registering
//! as Filen, so the shipping binary's `--unregister-all` takes its roots), with the fake drive
//! serving the tree in place of the cache. Stdout carries the protocol alone; logs go to stderr.
//! While a file `offline` sits beside the state directory, every drive call fails as the server
//! unreachable; while a file `slow` sits there, every thumbnail the drive makes waits until it is
//! removed. A directory `images` beside the state directory is served too, as the root's `images`
//! folder, with the files and folders it holds when the helper starts.

use std::{io, path::Path, sync::Arc, thread, time::Duration};

use filen_cloud_files::{
	FatalCode, HelperConfig, StateDir, WinPlatform, fatal, install_stderr_logging, serve_stdio,
	test_support::{DriveCall, FakeDrive, FakeOpener},
};
use tracing::{info, level_filters::LevelFilter, warn};

use crate::tree;

/// Serves on stdin and stdout with the state directory `state_dir`, replacing the root's watcher
/// every `rearm` (the shipping default is 10 minutes): the process's exit code.
pub(crate) fn run(state_dir: &Path, rearm: Duration) -> u8 {
	install_stderr_logging(LevelFilter::DEBUG);
	let offline = state_dir.with_file_name("offline");
	let slow = state_dir.with_file_name("slow");
	let images = state_dir.with_file_name("images");
	let state_dir = match StateDir::new(state_dir.to_path_buf()) {
		Ok(dir) => dir,
		Err(error) => return fatal(FatalCode::StateDirInvalid, &error.to_string()),
	};
	let mut config = HelperConfig::new(state_dir);
	config.watch_rearm = rearm;
	// Rebuilt on every start, as a restarted helper finds a cache with nothing pending.
	let drive = Arc::new(tree::build());
	if images.is_dir() {
		let root = tree::ROOT.to_string();
		let made = drive.add_remote_dir(&root, "images");
		match mirror(&drive, &format!("stable/{}", made.uuid), &images) {
			Ok(count) => info!(count, "serving the images directory"),
			Err(error) => warn!(%error, "could not serve the images directory"),
		}
	}
	let switched = Arc::clone(&drive);
	thread::spawn(move || {
		let mut held = None;
		loop {
			switched.set_offline(offline.exists());
			match (slow.exists(), held.take()) {
				(true, None) => held = Some(switched.gate_calls(DriveCall::Thumbnail)),
				(true, Some(gate)) => held = Some(gate),
				(false, Some(gate)) => {
					switched.stop_gating(DriveCall::Thumbnail);
					gate.add_permits(100_000);
				}
				(false, None) => {}
			}
			thread::sleep(Duration::from_millis(250));
		}
	});
	let opener = FakeOpener::new(drive);
	serve_stdio(WinPlatform, opener, config)
}

/// Lists what `source` holds in the drive's directory `dir`, recursively: the number of files.
fn mirror(drive: &FakeDrive, dir: &str, source: &Path) -> io::Result<usize> {
	let mut count = 0;
	for entry in std::fs::read_dir(source)? {
		let entry = entry?;
		let name = entry.file_name().to_string_lossy().into_owned();
		if entry.file_type()?.is_dir() {
			let made = drive.add_remote_dir(dir, &name);
			count += mirror(drive, &format!("stable/{}", made.uuid), &entry.path())?;
		} else {
			drive.add_remote_file(dir, &name, &std::fs::read(entry.path())?);
			count += 1;
		}
	}
	Ok(count)
}
