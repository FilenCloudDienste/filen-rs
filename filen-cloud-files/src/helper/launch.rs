//! Running the helper as a process: the refusals before it serves, the state directory's lock and
//! the runtime, over the process's stdin and stdout. The shipping binary and the VM harness both
//! start the helper through here, so the harness runs the launch that ships.

use std::{
	fs::{File, TryLockError},
	io::Write,
};

use tokio::runtime::Runtime;

use crate::helper::{
	HelperConfig, StateDir,
	platform::{DriveOpener, Platform},
	protocol::{FatalCode, OutMessage},
	supervisor::run_helper,
	unregister::unregister_all,
};

/// The oldest Windows build the provider runs on: 19041 (Windows 10 2004) is the oldest base still
/// serviced, and nothing older was measured.
pub const MIN_BUILD: u32 = 19041;

/// Writes the `fatal` line for `code`, saying `message`, to stdout: the process's exit code.
pub fn fatal(code: FatalCode, message: &str) -> u8 {
	let mut stdout = std::io::stdout().lock();
	// The parent may be gone already; the exit code says it all then.
	let _ = writeln!(
		stdout,
		"{}",
		OutMessage::Fatal {
			code,
			message: message.to_owned(),
		}
		.to_line()
	);
	let _ = stdout.flush();
	code.exit_code()
}

/// The state directory's single-instance lock, held while it lives; the OS lets go of it when the
/// process dies.
#[derive(Debug)]
pub struct StateLock {
	_file: File,
}

impl StateLock {
	/// Creates the state directory `dir` and takes its lock, or why not: `alreadyRunning` while
	/// another process holds it, `stateDirInvalid` when the directory or its lock file cannot be
	/// made.
	pub fn acquire(dir: &StateDir) -> Result<Self, (FatalCode, String)> {
		let invalid = |error: std::io::Error| (FatalCode::StateDirInvalid, error.to_string());
		std::fs::create_dir_all(dir.path()).map_err(invalid)?;
		let file = File::options()
			.create(true)
			.write(true)
			.truncate(false)
			.open(dir.lock_file())
			.map_err(invalid)?;
		match file.try_lock() {
			Ok(()) => Ok(Self { _file: file }),
			Err(TryLockError::WouldBlock) => Err((
				FatalCode::AlreadyRunning,
				"another helper holds the state directory".to_owned(),
			)),
			Err(TryLockError::Error(error)) => Err(invalid(error)),
		}
	}
}

/// Refuses a Cloud Files platform older than [`MIN_BUILD`], or one whose build is not known.
fn check_build<P: Platform>(platform: &P) -> Result<(), (FatalCode, String)> {
	match platform.platform_build() {
		Some(build) if build >= MIN_BUILD => Ok(()),
		build => Err((
			FatalCode::Unsupported,
			format!("Windows build {build:?} is older than {MIN_BUILD}"),
		)),
	}
}

/// The multi-thread runtime the helper runs on.
fn runtime() -> Result<Runtime, (FatalCode, String)> {
	tokio::runtime::Builder::new_multi_thread()
		// As the cache's own runtime: the SDK's futures are deep.
		.thread_stack_size(2 * 1024 * 1024)
		.enable_all()
		.build()
		.map_err(|error| (FatalCode::RuntimeFailed, error.to_string()))
}

/// What is set up before the helper runs: the lock and the runtime, or the refusal's exit code
/// after its `fatal` line.
fn prepare<P: Platform>(platform: &P, state_dir: &StateDir) -> Result<(StateLock, Runtime), u8> {
	let refused = |(code, message): (FatalCode, String)| fatal(code, &message);
	check_build(platform).map_err(refused)?;
	let lock = StateLock::acquire(state_dir).map_err(refused)?;
	let runtime = runtime().map_err(refused)?;
	Ok((lock, runtime))
}

/// Serves the protocol on stdin and stdout with `config` until the parent stops the helper: the
/// process's exit code. Refuses, with one `fatal` line, to run elevated, on a build older than
/// [`MIN_BUILD`], or while another helper holds the state directory.
pub fn serve_stdio<P: Platform, O: DriveOpener>(
	platform: P,
	opener: O,
	config: HelperConfig,
) -> u8 {
	if platform.elevated() {
		return fatal(
			FatalCode::Elevated,
			"the helper runs elevated; run it as the signed-in user",
		);
	}
	let (lock, runtime) = match prepare(&platform, &config.state_dir) {
		Ok(prepared) => prepared,
		Err(exit) => return exit,
	};
	let reason = runtime.block_on(run_helper(
		platform,
		opener,
		config,
		tokio::io::stdin(),
		tokio::io::stdout(),
	));
	// Not waiting for stdin's reader, which blocks until the parent closes its end.
	runtime.shutdown_background();
	drop(lock);
	reason.exit_code()
}

/// Unregisters every root the index in `state_dir` names, as the uninstaller does, and writes the
/// `unregisterAll` line: the process's exit code (see [`UnregisterAll::exit_code`]). Elevated or
/// not; refuses, with one `fatal` line, what [`serve_stdio`] refuses but elevation.
///
/// [`UnregisterAll::exit_code`]: crate::helper::UnregisterAll::exit_code
pub fn unregister_all_stdio<P: Platform>(
	platform: P,
	state_dir: &StateDir,
	delete_hydrated: bool,
) -> u8 {
	let (lock, runtime) = match prepare(&platform, state_dir) {
		Ok(prepared) => prepared,
		Err(exit) => return exit,
	};
	let done = runtime.block_on(unregister_all(platform, state_dir, delete_hydrated));
	let mut stdout = std::io::stdout().lock();
	// The uninstaller reads the exit code if it cannot read the line.
	let _ = writeln!(stdout, "{}", done.line().to_line());
	let _ = stdout.flush();
	runtime.shutdown_background();
	drop(lock);
	done.exit_code()
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn a_second_lock_on_the_state_directory_is_refused_until_the_first_goes() {
		let temp = tempfile::tempdir().unwrap();
		let dir = StateDir::new(temp.path().join("state")).unwrap();
		let first = StateLock::acquire(&dir).unwrap();
		assert_eq!(
			StateLock::acquire(&dir).unwrap_err().0,
			FatalCode::AlreadyRunning
		);
		drop(first);
		StateLock::acquire(&dir).unwrap();
	}

	#[test]
	fn a_state_directory_that_cannot_be_made_is_invalid() {
		let temp = tempfile::tempdir().unwrap();
		let file = temp.path().join("file");
		std::fs::write(&file, b"x").unwrap();
		let dir = StateDir::new(file.join("state")).unwrap();
		assert_eq!(
			StateLock::acquire(&dir).unwrap_err().0,
			FatalCode::StateDirInvalid
		);
	}

	#[test]
	fn every_refusal_has_its_own_exit_code() {
		let codes = [
			FatalCode::Usage,
			FatalCode::Elevated,
			FatalCode::Unsupported,
			FatalCode::AlreadyRunning,
			FatalCode::StateDirInvalid,
			FatalCode::NotWindows,
			FatalCode::RuntimeFailed,
		]
		.map(FatalCode::exit_code);
		assert_eq!(codes, [2, 10, 11, 12, 13, 14, 15]);
	}
}
