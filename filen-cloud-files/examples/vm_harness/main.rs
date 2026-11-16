//! Drives the Windows layer against a real Windows, with no Filen account: a provider serving a
//! deterministic tree from the fake drive through cfapi, and readers in other processes checking
//! what Windows shows them. Run as the logged-on user, never elevated, with each subcommand in its
//! own process: the OS does not call a provider back for its own process's file access.
//!
//! - `provide <root> <out> <secs>`: registers and connects `<root>` with a dispatcher and a local
//!   engine but no watcher, serves until `<out>\stop` exists or `<secs>` pass, then shuts down,
//!   disconnects and unregisters.
//! - `provide-local <root> <out> <secs>`: as `provide`, also running the root's watcher, so local
//!   changes reach the fake drive; the summary lists its writes. It
//!   dumps the drive to `<out>\remote.json` every second and takes commands from files in
//!   `<out>` (`offline`, `ctl-<n>.txt`).
//! - `local <root> <out>`: the local-change scenarios, a user changing the root that
//!   `provide-local` serves, into `<out>\local.txt`.
//! - `saves <root> <out>`: the save and restart scenarios, a user copying, saving and holding
//!   files around remote changes and restarts of the provider, into `<out>\saves.txt`; after
//!   `local`, before `remote`.
//! - `remote <root> <out>`: the remote-change scenarios, another device changing the drive that
//!   `provide-local` serves (through its control commands) and a user checking the root, into
//!   `<out>\remote.txt`; after `local`, before `check`.
//! - `pins <root> <out>`: the pin scenarios, a user pinning and unpinning items of the root and
//!   asking the platform to dehydrate files, into `<out>\pins.txt`; after `remote`, before
//!   `check`.
//! - `check <root> <out>`: the reader's scenarios, into `<out>\check.txt`.
//! - `saves-loop <root> <out> <iterations>`: atomic saves in a loop (`File.Replace`, Office's
//!   save, a rename over a file) on a root `provide-local` serves, into `<out>\loop.txt`.
//! - `quiet <out>`: after `local` (and `remote`), `check` and the provider's stop, that `check`
//!   caused no drive write, into `<out>\quiet.txt`.
//! - `after <root> <out>`: what unregistering left behind, into `<out>\after.txt`.
//! - `keys <out>`: a Credential Manager round trip, into `<out>\keys.txt`.
//! - `helper --state-dir <dir> --rearm-secs <n>`: the helper's command loop on stdin and stdout,
//!   on the real Windows layer with the fake drive, replacing the watcher every `<n>` seconds.
//! - `helper-drive <exe> <out> fake|prod`: the desktop app's side of a helper, into
//!   `<out>\helper.txt`; `fake` drives `<exe> helper` (this harness), `prod` the shipping helper
//!   with no session.
//! - `user <op> <path> [text]`: one user-side file operation, for `helper-drive`.
//! - `read-one <path>`, `read-to <path> <offset> <len> <file>` and `keys-load <account> <file>`:
//!   the second processes `check` and `keys` start.
//!
//! Every process running scenarios, and the provider, reports a stall: a probe that only sleeps
//! waking a second late or more, which a loaded host's VM shows (see `stall`).
//!
//! Every result line is `PASS|FAIL|INFO <step> <detail>`; a run is green with no `FAIL` line. The
//! exit code is 0 when every check passed, 1 when one failed, 2 for bad arguments and 3 when the
//! harness itself failed.

#[cfg(windows)]
mod after;
#[cfg(windows)]
mod check;
#[cfg(windows)]
mod events;
#[cfg(windows)]
mod helper;
#[cfg(windows)]
mod helper_drive;
#[cfg(windows)]
mod keys;
#[cfg(windows)]
mod local;
#[cfg(windows)]
mod pins;
#[cfg(windows)]
mod provide;
#[cfg(windows)]
mod remote;
#[cfg(windows)]
mod report;
#[cfg(windows)]
mod saves;
#[cfg(windows)]
mod saves_loop;
#[cfg(windows)]
mod stall;
#[cfg(windows)]
mod tree;

use std::process::ExitCode;
#[cfg(windows)]
use std::time::Duration;

#[cfg(not(windows))]
fn main() -> ExitCode {
	eprintln!("the VM harness runs on Windows only");
	ExitCode::from(2)
}

#[cfg(windows)]
fn main() -> ExitCode {
	let args: Vec<String> = std::env::args().skip(1).collect();
	let args: Vec<&str> = args.iter().map(String::as_str).collect();
	if let Some(&("local" | "saves" | "saves-loop" | "remote" | "pins" | "check" | "read-to")) =
		args.first()
	{
		stall::watch_thread(false);
	}
	let result = match args.as_slice() {
		["provide", root, out, secs] => match secs.parse() {
			Ok(secs) => provide::run(root.as_ref(), out.as_ref(), secs, false),
			Err(_) => return usage(),
		},
		["provide-local", root, out, secs] => match secs.parse() {
			Ok(secs) => provide::run(root.as_ref(), out.as_ref(), secs, true),
			Err(_) => return usage(),
		},
		["check", root, out] => check::run(root.as_ref(), out.as_ref()),
		["saves-loop", root, out, iterations] => match iterations.parse() {
			Ok(iterations) => saves_loop::run(root.as_ref(), out.as_ref(), iterations),
			Err(_) => return usage(),
		},
		["local", root, out] => local::run(root.as_ref(), out.as_ref()),
		["saves", root, out] => saves::run(root.as_ref(), out.as_ref()),
		["remote", root, out] => remote::run(root.as_ref(), out.as_ref()),
		["pins", root, out] => pins::run(root.as_ref(), out.as_ref()),
		["quiet", out] => local::quiet(out.as_ref()),
		["after", root, out] => after::run(root.as_ref(), out.as_ref()),
		["keys", out] => keys::run(out.as_ref()),
		["helper", "--state-dir", state, "--rearm-secs", secs] => match secs.parse() {
			Ok(secs) => {
				return ExitCode::from(helper::run(state.as_ref(), Duration::from_secs(secs)));
			}
			Err(_) => return usage(),
		},
		["helper-drive", exe, out, mode] => helper_drive::run(exe.as_ref(), out.as_ref(), mode),
		["user", op, path] => helper_drive::user_op(op, path.as_ref(), None),
		["user", op, path, text] => helper_drive::user_op(op, path.as_ref(), Some(text)),
		["keys-load", account, file] => keys::load_in_child(account, file.as_ref()),
		["read-one", path] => check::read_one(path.as_ref()),
		["read-to", path, offset, len, file] => match (offset.parse(), len.parse()) {
			(Ok(offset), Ok(len)) => check::read_to(path.as_ref(), offset, len, file.as_ref()),
			_ => return usage(),
		},
		_ => return usage(),
	};
	match result {
		Ok(true) => ExitCode::SUCCESS,
		Ok(false) => ExitCode::from(1),
		Err(error) => {
			eprintln!("vm_harness failed: {error}");
			ExitCode::from(3)
		}
	}
}

#[cfg(windows)]
fn usage() -> ExitCode {
	eprintln!(
		"usage: vm_harness provide <root> <out> <secs> | provide-local <root> <out> <secs> | local \
		 <root> <out> | saves <root> <out> | remote <root> <out> | pins <root> <out> | check <root> <out> | saves-loop <root> <out> <iterations> | quiet <out> | after <root> <out> | \
		 keys <out> | helper --state-dir <dir> --rearm-secs <n> | helper-drive <exe> <out> \
		 fake|prod | user <op> <path> [text] | read-one <path> | read-to <path> <offset> <len> <file> | keys-load <account> \
		 <file>"
	);
	ExitCode::from(2)
}
