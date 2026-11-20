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
		platform::{DriveOpener, Platform, PlatformError},
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

/// How long the instance COM started waits for a thumbnail request before it exits: it keeps the
/// exe file open that long after Explorer's last request, so an updater or uninstaller replacing
/// the exe ends `--thumbnail-server` processes first or retries (see
/// [`serve_refusing_thumbnails`]).
pub const THUMBNAIL_SERVER_IDLE: Duration = Duration::from_secs(10);

/// A handler that answers every request [`ThumbnailRefusal::SignedOut`], noting when it was last
/// asked in `asked`.
fn refusing_handler(asked: Arc<Mutex<Instant>>) -> ThumbnailHandler {
	Arc::new(move |_, _, _| {
		*asked.lock().unwrap_or_else(PoisonError::into_inner) = Instant::now();
		Err(ThumbnailRefusal::SignedOut)
	})
}

/// Serves the thumbnail class as the instance COM starts when no helper serves it, which is when
/// none runs: it answers every request [`ThumbnailRefusal::SignedOut`], as no root is served, and
/// exits once `idle` passed without a request. It never takes the state directory's lock, opens a
/// session or connects a root. The process's exit code: 0, also on a platform that serves no
/// thumbnails, which exits at once; 1 when the server could not start, or the process's thumbnail
/// security ([`Platform::secure_thumbnails`], set before anything but the elevation check) could
/// not be set. Refuses,
/// with one `fatal` line, to run elevated.
///
/// Once a helper starts serving the class too, Explorer still sends this instance its requests
/// for a while (measured: about 4 s), each refused, until it idles out: an item asked for then
/// keeps its icon until Explorer asks for it again (a refresh, or scrolled back into view), as it
/// does not remember a refusal (measured), and nothing measured has it ask again by itself. Left
/// so: this instance cannot tell a serving helper exists without a channel to it: it knows no
/// state directory, so no helper's lock (and asking COM for the class from here would likely
/// find its own registration first: not measured).
///
/// Nothing ends this instance but `idle`, and each request starts that again: it keeps the exe
/// open for up to `idle` (about 10 s with [`THUMBNAIL_SERVER_IDLE`]) after Explorer's last request,
/// also once the class is removed, so an updater or uninstaller that replaces or deletes the exe
/// ends its `--thumbnail-server` processes first, or retries until the file is free.
/// Only a `GetThumbnail` starts it again, so an activation COM began just before the idle ran out
/// fails in its `CreateInstance` or `Initialize` (`CO_E_SERVER_STOPPING`), or once this instance
/// is gone: one more refused request, as it would have been answered anyway.
pub fn serve_refusing_thumbnails<P: Platform>(platform: P, idle: Duration) -> u8 {
	if platform.elevated() {
		return fatal(
			FatalCode::Elevated,
			"the thumbnail server runs elevated; run it as the signed-in user",
		);
	}
	// First, before any COM or shell use: see `Platform::secure_thumbnails`.
	let security = match platform.secure_thumbnails() {
		Ok(Some(security)) => security,
		Ok(None) => {
			info!("this build serves no thumbnails; exiting");
			return 0;
		}
		Err(error) => {
			warn!(%error, "the thumbnail server could not start");
			return 1;
		}
	};
	let asked = Arc::new(Mutex::new(Instant::now()));
	let server = match platform.serve_thumbnails(security, refusing_handler(Arc::clone(&asked))) {
		Ok(server) => server,
		Err(error) => {
			warn!(%error, "the thumbnail server could not start");
			return 1;
		}
	};
	// Idle from now: COM started this process for a request it may not have delivered yet, however
	// long the start took.
	*asked.lock().unwrap_or_else(PoisonError::into_inner) = Instant::now();
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

/// What is set up before the helper runs: the lock and the runtime `build` makes, or the refusal's
/// exit code after its `fatal` line.
fn prepare<P: Platform>(
	platform: &P,
	state_dir: &StateDir,
	build: impl FnOnce() -> Result<Runtime, (FatalCode, String)>,
) -> Result<(StateLock, Runtime), u8> {
	let refused = |(code, message): (FatalCode, String)| fatal(code, &message);
	check_build(platform).map_err(refused)?;
	let lock = StateLock::acquire(state_dir).map_err(refused)?;
	let runtime = build().map_err(refused)?;
	Ok((lock, runtime))
}

/// What [`serve_stdio`] sets up before the helper runs.
struct Started<S> {
	/// What [`Platform::secure_thumbnails`] answered, handed on to [`run_helper`].
	security: Result<Option<S>, PlatformError>,
	lock: StateLock,
	runtime: Runtime,
}

/// [`serve_stdio`]'s start, in order: refuses to run elevated, sets the process's thumbnail
/// security, then takes the lock and the runtime `build` makes ([`prepare`]). The refusal's exit
/// code after its `fatal` line otherwise.
///
/// The security comes before anything that may use COM or the shell (`check_build` asks the Cloud
/// Files platform) and before the runtime's threads. It is bounded by the COM layer's own wait
/// (10 s), well under the 30 s the desktop app waits for `hello`. A helper refused after it, as one
/// started while another holds the state directory, pays that start and exits.
fn start_stdio<P: Platform>(
	platform: &P,
	state_dir: &StateDir,
	build: impl FnOnce() -> Result<Runtime, (FatalCode, String)>,
) -> Result<Started<P::ThumbnailSecurity>, u8> {
	if platform.elevated() {
		return Err(fatal(
			FatalCode::Elevated,
			"the helper runs elevated; run it as the signed-in user",
		));
	}
	// First, before any COM or shell use: see `Platform::secure_thumbnails`.
	let security = platform.secure_thumbnails();
	let (lock, runtime) = prepare(platform, state_dir, build)?;
	Ok(Started {
		security,
		lock,
		runtime,
	})
}

/// Serves the protocol on stdin and stdout with `config` until the parent stops the helper: the
/// process's exit code. Refuses, with one `fatal` line, to run elevated, on a build older than
/// [`MIN_BUILD`], or while another helper holds the state directory.
///
/// Sets the process's thumbnail security ([`Platform::secure_thumbnails`]) once it knows it is not
/// elevated, before any COM or shell use and before it builds the runtime: a helper whose security
/// was not set serves no thumbnails, and withdraws the class from every root (see [`run_helper`]).
pub fn serve_stdio<P: Platform, O: DriveOpener>(
	platform: P,
	opener: O,
	config: HelperConfig,
) -> u8 {
	let Started {
		security,
		lock,
		runtime,
	} = match start_stdio(&platform, &config.state_dir, runtime) {
		Ok(started) => started,
		Err(exit) => return exit,
	};
	let reason = runtime.block_on(run_helper(
		platform,
		security,
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
	let (lock, runtime) = match prepare(&platform, state_dir, runtime) {
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
		thumbnails::ThumbnailCancel,
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
			handler(
				std::path::Path::new(r"C:\Users\u\Filen\a.jpg"),
				96,
				&ThumbnailCancel::default()
			),
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
		assert_eq!(platform.thumbnail_securings(), 1);
		assert_eq!(platform.thumbnail_servers(), (1, 1));
	}

	#[test]
	fn the_instance_com_starts_idles_from_its_servers_start_not_its_own() {
		let platform = FakePlatform::new(FakeLocal::new(PlaceholderIdentity::Root {
			root: Uuid::from_u128(1),
		}));
		platform.implement_thumbnails(true);
		let hold = platform.hold_thumbnail_starts();
		let idle = Duration::from_millis(300);
		let serving = std::thread::spawn({
			let platform = platform.clone();
			move || serve_refusing_thumbnails(platform, idle)
		});
		let waited = Instant::now();
		while platform.thumbnail_starts_held() == 0 {
			assert!(
				waited.elapsed() < Duration::from_secs(60),
				"the start never reached the hold"
			);
			std::thread::yield_now();
		}
		// A start slower than the idle time.
		std::thread::sleep(idle + Duration::from_millis(100));
		let released = Instant::now();
		hold.release();
		assert_eq!(serving.join().unwrap(), 0);
		assert!(
			released.elapsed() >= idle,
			"stopped {:?} after its start",
			released.elapsed()
		);
		assert_eq!(platform.thumbnail_servers(), (1, 1));
	}

	#[test]
	fn the_instance_com_starts_leaves_the_roots_alone_when_its_server_fails() {
		let platform = FakePlatform::new(FakeLocal::new(PlaceholderIdentity::Root {
			root: Uuid::from_u128(1),
		}));
		platform.implement_thumbnails(true);
		platform.fail_thumbnail_servers();
		assert_eq!(
			serve_refusing_thumbnails(platform.clone(), THUMBNAIL_SERVER_IDLE),
			1
		);
		assert_eq!(platform.thumbnail_servers(), (0, 0));
		assert_eq!(platform.thumbnail_withdrawals(), 0);
	}

	#[test]
	fn the_instance_com_starts_starts_no_server_when_its_security_fails() {
		let platform = FakePlatform::new(FakeLocal::new(PlaceholderIdentity::Root {
			root: Uuid::from_u128(1),
		}));
		platform.implement_thumbnails(true);
		platform.fail_thumbnail_security();
		assert_eq!(
			serve_refusing_thumbnails(platform.clone(), THUMBNAIL_SERVER_IDLE),
			1
		);
		assert_eq!(platform.thumbnail_securings(), 1);
		assert_eq!(platform.thumbnail_servers(), (0, 0));
		// The roots are a helper's to withdraw the class from, as for a server that failed.
		assert_eq!(platform.thumbnail_withdrawals(), 0);
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
		// Refused before it set anything.
		assert_eq!(platform.thumbnail_securings(), 0);
		assert_eq!(platform.thumbnail_servers(), (0, 0));
	}

	/// A fake platform serving thumbnails, and a state directory in `temp`.
	fn helper_start(temp: &tempfile::TempDir) -> (FakePlatform, StateDir) {
		let platform = FakePlatform::new(FakeLocal::new(PlaceholderIdentity::Root {
			root: Uuid::from_u128(1),
		}));
		platform.implement_thumbnails(true);
		(platform, StateDir::new(temp.path().join("state")).unwrap())
	}

	#[test]
	fn the_helper_secures_its_thumbnails_before_its_runtime_and_hands_the_proof_on() {
		let temp = tempfile::tempdir().unwrap();
		let (platform, dir) = helper_start(&temp);
		let mut built = false;
		let started = start_stdio(&platform, &dir, || {
			assert_eq!(
				platform.thumbnail_securings(),
				1,
				"secured after the runtime"
			);
			built = true;
			runtime()
		});
		let Ok(started) = started else {
			panic!("the helper was refused");
		};
		assert!(built);
		assert!(matches!(started.security, Ok(Some(_))));
		assert_eq!(platform.thumbnail_securings(), 1);
		// Secured before the build is read, the start's first use of the Cloud Files platform.
		assert_eq!(platform.securings_at_build_reads(), [1]);
		// The lock is held while the helper runs.
		assert_eq!(
			StateLock::acquire(&dir).unwrap_err().0,
			FatalCode::AlreadyRunning
		);
	}

	#[test]
	fn the_helper_hands_on_a_failed_or_absent_security_and_still_runs() {
		let temp = tempfile::tempdir().unwrap();
		let (platform, dir) = helper_start(&temp);
		platform.fail_thumbnail_security();
		let Ok(started) = start_stdio(&platform, &dir, runtime) else {
			panic!("the helper was refused");
		};
		// `run_helper` withdraws the class from every root then.
		assert!(started.security.is_err());
		assert_eq!(platform.thumbnail_securings(), 1);
		drop(started);

		let temp = tempfile::tempdir().unwrap();
		let (platform, dir) = helper_start(&temp);
		platform.implement_thumbnails(false);
		let Ok(started) = start_stdio(&platform, &dir, runtime) else {
			panic!("the helper was refused");
		};
		assert!(matches!(started.security, Ok(None)));
		assert_eq!(platform.thumbnail_securings(), 0);
	}

	#[test]
	fn the_helper_refuses_elevation_before_it_secures_anything() {
		let temp = tempfile::tempdir().unwrap();
		let (platform, dir) = helper_start(&temp);
		platform.set_elevated(true);
		assert_eq!(
			start_stdio(&platform, &dir, || panic!("built a runtime")).err(),
			Some(FatalCode::Elevated.exit_code())
		);
		assert_eq!(platform.thumbnail_securings(), 0);
		// Nor took the lock.
		StateLock::acquire(&dir).unwrap();
	}

	#[test]
	fn a_helper_on_an_unsupported_build_secures_first_and_is_refused_before_its_lock() {
		for build in [Some(MIN_BUILD - 1), None] {
			let temp = tempfile::tempdir().unwrap();
			let (platform, dir) = helper_start(&temp);
			platform.set_platform_build(build);
			assert_eq!(
				start_stdio(&platform, &dir, || panic!("built a runtime")).err(),
				Some(FatalCode::Unsupported.exit_code())
			);
			// Secured before the build was read.
			assert_eq!(platform.securings_at_build_reads(), [1]);
			assert_eq!(platform.thumbnail_securings(), 1);
			StateLock::acquire(&dir).unwrap();
		}
	}

	#[test]
	fn a_second_helper_secures_first_and_is_refused_before_its_runtime() {
		let temp = tempfile::tempdir().unwrap();
		let (platform, dir) = helper_start(&temp);
		let _first = StateLock::acquire(&dir).unwrap();
		assert_eq!(
			start_stdio(&platform, &dir, || panic!("built a runtime")).err(),
			Some(FatalCode::AlreadyRunning.exit_code())
		);
		assert_eq!(platform.thumbnail_securings(), 1);
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
