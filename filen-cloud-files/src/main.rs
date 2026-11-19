//! The Cloud Files provider's helper: the desktop app spawns it, non-elevated, as the signed-in
//! user, and drives it over stdin and stdout (see the library's protocol). Stdout carries the
//! protocol alone; logs go to stderr.
//!
//! `filen-cloud-files --state-dir <dir> [--log-level <level>]` serves; `filen-cloud-files
//! --unregister-all --state-dir <dir> [--keep-hydrated]` unregisters every root, as the
//! uninstaller does; `filen-cloud-files --thumbnail-server [-Embedding]` is what COM starts for File
//! Explorer's thumbnail class when no helper serves it: it refuses every request and exits once
//! idle (see `serve_refusing_thumbnails`). A refusal to start is one `fatal` line and its code's exit code
//! (`FatalCode::exit_code`): 2 a wrong command line, 10 elevated, 11 a Windows build older than
//! supported, 12 another helper holds the state directory, 13 the state directory is unusable, 14
//! not Windows, 15 the async runtime could not be built. `--unregister-all` exits 0 when every root is gone, 20 when something was kept or
//! left, 21 when nothing could be done.

#![deny(clippy::print_stdout)]
// The helper's future, over the cache and the SDK's drive calls and lock, nests deeper than the
// default limit of 128 queries the compiler takes to lay it out on Windows.
#![recursion_limit = "256"]

use std::{ffi::OsString, path::PathBuf, process::ExitCode, str::FromStr};

#[cfg(windows)]
use filen_cloud_files::{
	CacheOpener, HelperConfig, StateDir, THUMBNAIL_SERVER_IDLE, WinPlatform,
	install_stderr_logging, serve_refusing_thumbnails, serve_stdio, unregister_all_stdio,
};
use filen_cloud_files::{FatalCode, THUMBNAIL_SERVER_ARG, fatal};
use tracing::level_filters::LevelFilter;

/// What the command line asks.
#[derive(Debug, PartialEq, Eq)]
struct Args {
	log_level: LevelFilter,
	mode: Mode,
}

#[derive(Debug, PartialEq, Eq)]
enum Mode {
	Serve {
		state_dir: PathBuf,
	},
	UnregisterAll {
		state_dir: PathBuf,
		delete_hydrated: bool,
	},
	ThumbnailServer,
}

/// The command line `args`, or why it is wrong.
fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Args, String> {
	let mut args = args.into_iter();
	let mut state_dir = None;
	let mut log_level = LevelFilter::INFO;
	let mut unregister_all = false;
	let mut keep_hydrated = false;
	let mut thumbnail_server = false;
	let mut embedding = false;
	while let Some(arg) = args.next() {
		match arg.to_str() {
			Some("--state-dir") => {
				state_dir = Some(PathBuf::from(
					args.next().ok_or("--state-dir needs a directory")?,
				));
			}
			Some("--log-level") => {
				let level = args.next().ok_or("--log-level needs a level")?;
				log_level = level
					.to_str()
					.and_then(|level| LevelFilter::from_str(level).ok())
					.ok_or("--log-level takes error, warn, info, debug or trace")?;
			}
			Some("--unregister-all") => unregister_all = true,
			Some("--keep-hydrated") => keep_hydrated = true,
			Some(THUMBNAIL_SERVER_ARG) => thumbnail_server = true,
			// What COM appends to a local server's command line.
			Some("-Embedding" | "/Embedding") => embedding = true,
			_ => return Err(format!("unknown argument {arg:?}")),
		}
	}
	if thumbnail_server {
		if state_dir.is_some() || unregister_all || keep_hydrated {
			return Err(format!("{THUMBNAIL_SERVER_ARG} takes only --log-level"));
		}
		return Ok(Args {
			log_level,
			mode: Mode::ThumbnailServer,
		});
	}
	if embedding {
		return Err(format!("-Embedding goes with {THUMBNAIL_SERVER_ARG}"));
	}
	let state_dir = state_dir.ok_or("--state-dir is required")?;
	if keep_hydrated && !unregister_all {
		return Err("--keep-hydrated goes with --unregister-all".to_owned());
	}
	Ok(Args {
		log_level,
		mode: if unregister_all {
			Mode::UnregisterAll {
				state_dir,
				delete_hydrated: !keep_hydrated,
			}
		} else {
			Mode::Serve { state_dir }
		},
	})
}

fn main() -> ExitCode {
	let args = match parse(std::env::args_os().skip(1)) {
		Ok(args) => args,
		Err(message) => return ExitCode::from(fatal(FatalCode::Usage, &message)),
	};
	ExitCode::from(run(args))
}

#[cfg(not(windows))]
fn run(_args: Args) -> u8 {
	fatal(
		FatalCode::NotWindows,
		"the Cloud Files provider runs on Windows only",
	)
}

#[cfg(windows)]
fn run(args: Args) -> u8 {
	// Before anything else logs: the cache installs a logger on stdout when none is.
	install_stderr_logging(args.log_level);
	let state_dir = |dir: PathBuf| StateDir::new(dir).map_err(|error| error.to_string());
	match args.mode {
		Mode::Serve { state_dir: dir } => match state_dir(dir) {
			Ok(dir) => serve_stdio(WinPlatform, CacheOpener, HelperConfig::new(dir)),
			Err(message) => fatal(FatalCode::StateDirInvalid, &message),
		},
		Mode::UnregisterAll {
			state_dir: dir,
			delete_hydrated,
		} => match state_dir(dir) {
			Ok(dir) => unregister_all_stdio(WinPlatform, &dir, delete_hydrated),
			Err(message) => fatal(FatalCode::StateDirInvalid, &message),
		},
		Mode::ThumbnailServer => serve_refusing_thumbnails(WinPlatform, THUMBNAIL_SERVER_IDLE),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn args(line: &[&str]) -> Result<Args, String> {
		parse(line.iter().map(OsString::from))
	}

	#[test]
	fn the_state_dir_is_required_and_serving_is_the_default() {
		assert_eq!(
			args(&["--state-dir", "C:\\s"]).unwrap(),
			Args {
				log_level: LevelFilter::INFO,
				mode: Mode::Serve {
					state_dir: PathBuf::from("C:\\s"),
				},
			}
		);
		assert!(args(&[]).is_err());
		assert!(args(&["--state-dir"]).is_err());
	}

	#[test]
	fn unregister_all_deletes_downloaded_files_unless_told_to_keep_them() {
		assert_eq!(
			args(&["--unregister-all", "--state-dir", "s"])
				.unwrap()
				.mode,
			Mode::UnregisterAll {
				state_dir: PathBuf::from("s"),
				delete_hydrated: true
			}
		);
		assert_eq!(
			args(&["--unregister-all", "--keep-hydrated", "--state-dir", "s"])
				.unwrap()
				.mode,
			Mode::UnregisterAll {
				state_dir: PathBuf::from("s"),
				delete_hydrated: false
			}
		);
		assert!(args(&["--keep-hydrated", "--state-dir", "s"]).is_err());
	}

	#[test]
	fn the_thumbnail_server_com_starts_needs_no_state_dir_and_takes_embedding() {
		for line in [
			&["--thumbnail-server"][..],
			&["--thumbnail-server", "-Embedding"],
			&["--thumbnail-server", "/Embedding"],
		] {
			assert_eq!(args(line).unwrap().mode, Mode::ThumbnailServer, "{line:?}");
		}
		assert!(args(&["--thumbnail-server", "--state-dir", "s"]).is_err());
		assert!(args(&["--thumbnail-server", "--unregister-all"]).is_err());
		assert!(args(&["--state-dir", "s", "-Embedding"]).is_err());
	}

	#[test]
	fn a_log_level_is_one_tracing_knows_and_unknown_arguments_are_refused() {
		assert_eq!(
			args(&["--state-dir", "s", "--log-level", "debug"])
				.unwrap()
				.log_level,
			LevelFilter::DEBUG
		);
		assert!(args(&["--state-dir", "s", "--log-level", "loud"]).is_err());
		assert!(args(&["--state-dir", "s", "--frobnicate"]).is_err());
	}
}
