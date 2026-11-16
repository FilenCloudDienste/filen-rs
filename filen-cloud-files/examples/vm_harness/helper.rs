//! `helper`: the helper as the shipping binary launches it (the same refusals, state directory
//! lock and runtime, through the library's `serve_stdio`), on the real Windows layer (registering
//! as Filen, so the shipping binary's `--unregister-all` takes its roots), with the fake drive
//! serving the tree in place of the cache. Stdout carries the protocol alone; logs go to stderr.
//! While a file `offline` sits beside the state directory, every drive call fails as the server
//! unreachable.

use std::{path::Path, sync::Arc, thread, time::Duration};

use filen_cloud_files::{
	FatalCode, HelperConfig, StateDir, WinPlatform, fatal, install_stderr_logging, serve_stdio,
	test_support::FakeOpener,
};
use tracing::level_filters::LevelFilter;

use crate::tree;

/// Serves on stdin and stdout with the state directory `state_dir`, replacing the root's watcher
/// every `rearm` (the shipping default is 10 minutes): the process's exit code.
pub(crate) fn run(state_dir: &Path, rearm: Duration) -> u8 {
	install_stderr_logging(LevelFilter::DEBUG);
	let offline = state_dir.with_file_name("offline");
	let state_dir = match StateDir::new(state_dir.to_path_buf()) {
		Ok(dir) => dir,
		Err(error) => return fatal(FatalCode::StateDirInvalid, &error.to_string()),
	};
	let mut config = HelperConfig::new(state_dir);
	config.watch_rearm = rearm;
	// Rebuilt on every start, as a restarted helper finds a cache with nothing pending.
	let drive = Arc::new(tree::build());
	let switched = Arc::clone(&drive);
	thread::spawn(move || {
		loop {
			switched.set_offline(offline.exists());
			thread::sleep(Duration::from_millis(250));
		}
	});
	let opener = FakeOpener::new(drive);
	serve_stdio(WinPlatform, opener, config)
}
