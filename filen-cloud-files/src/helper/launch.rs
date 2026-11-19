//! Running the helper as a process: the refusals before it serves, the state directory's lock and
//! the runtime, over the process's stdin and stdout. The shipping binary and the VM harness both
//! start the helper through here, so the harness runs the launch that ships.

use std::{
	fs::{File, TryLockError},
	io::Write,
	sync::{Arc, Mutex, PoisonError},
	time::{Duration, Instant},
};

use tokio::runtime::Runtime;
use tracing::{info, warn};

use crate::{
	helper::{
		HelperConfig, StateDir,
		platform::{DriveOpener, Platform},
		protocol::{FatalCode, OutMessage},
		supervisor::run_helper,
		unregister::unregister_all,
	},
	thumbnails::{ThumbnailHandler, ThumbnailRefusal},
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

/// The argument the thumbnail class's local server command line starts the helper exe with
/// (`"<exe>" --thumbnail-server`): what tells an instance COM started, to which COM appends
/// `-Embedding`, from one the desktop app spawned.
pub const THUMBNAIL_SERVER_ARG: &str = "--thumbnail-server";

/// How long the instance COM started waits for a thumbnail request before it exits.
pub const THUMBNAIL_SERVER_IDLE: Duration = Duration::from_secs(10);

/// A handler that answers every request [`ThumbnailRefusal::SignedOut`], noting when it was last
/// asked in `asked`.
fn refusing_handler(asked: Arc<Mutex<Instant>>) -> ThumbnailHandler {
	Arc::new(move |_, _| {
		*asked.lock().unwrap_or_else(PoisonError::into_inner) = Instant::now();
		Err(ThumbnailRefusal::SignedOut)
	})
}

/// Serves the thumbnail class as the instance COM starts when no helper serves it, which is when
/// none runs: it answers every request [`ThumbnailRefusal::SignedOut`], as no root is served, and
/// exits once `idle` passed without a request. It never takes the state directory's lock, opens a
/// session or connects a root. The process's exit code: 0, also on a build that serves no
/// thumbnails, which exits at once; 1 when the server could not start. Refuses, with one `fatal`
/// line, to run elevated.
pub fn serve_refusing_thumbnails<P: Platform>(platform: P, idle: Duration) -> u8 {
	if platform.elevated() {
		return fatal(
			FatalCode::Elevated,
			"the thumbnail server runs elevated; run it as the signed-in user",
		);
	}
	let asked = Arc::new(Mutex::new(Instant::now()));
	let server = match platform.serve_thumbnails(refusing_handler(Arc::clone(&asked))) {
		Ok(Some(server)) => server,
		Ok(None) => {
			info!("this build serves no thumbnails; exiting");
			return 0;
		}
		Err(error) => {
			warn!(%error, "the thumbnail server could not start");
			return 1;
		}
	};
	loop {
		let quiet = asked
			.lock()
			.unwrap_or_else(PoisonError::into_inner)
			.elapsed();
		let Some(left) = idle.checked_sub(quiet).filter(|left| !left.is_zero()) else {
			break;
		};
		std::thread::sleep(left.min(Duration::from_secs(1)));
	}
	platform.stop_thumbnails(server);
	0
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
	use filen_types::fs::Uuid;

	use super::*;
	use crate::{
		identity::PlaceholderIdentity,
		test_support::{FakeLocal, FakePlatform},
	};

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
	fn the_instance_com_starts_refuses_every_request_and_notes_it() {
		let start = Instant::now();
		let asked = Arc::new(Mutex::new(start));
		let handler = refusing_handler(Arc::clone(&asked));
		assert_eq!(
			handler(std::path::Path::new(r"C:\Users\u\Filen\a.jpg"), 96),
			Err(ThumbnailRefusal::SignedOut)
		);
		assert!(*asked.lock().unwrap() >= start);
	}

	#[test]
	fn the_instance_com_starts_stops_its_server_once_idle() {
		let platform = FakePlatform::new(FakeLocal::new(PlaceholderIdentity::Root {
			root: Uuid::from_u128(1),
		}));
		platform.implement_thumbnails(true);
		assert_eq!(
			serve_refusing_thumbnails(platform.clone(), Duration::ZERO),
			0
		);
		assert_eq!(platform.thumbnail_servers(), (1, 1));
	}

	#[test]
	fn the_instance_com_starts_exits_at_once_on_a_build_serving_none_and_refuses_elevation() {
		let platform = FakePlatform::new(FakeLocal::new(PlaceholderIdentity::Root {
			root: Uuid::from_u128(1),
		}));
		assert_eq!(
			serve_refusing_thumbnails(platform.clone(), THUMBNAIL_SERVER_IDLE),
			0
		);
		assert_eq!(platform.thumbnail_servers(), (0, 0));
		platform.implement_thumbnails(true);
		platform.set_elevated(true);
		assert_eq!(
			serve_refusing_thumbnails(platform.clone(), THUMBNAIL_SERVER_IDLE),
			FatalCode::Elevated.exit_code()
		);
		assert_eq!(platform.thumbnail_servers(), (0, 0));
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
