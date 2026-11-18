//! `verb_stub`: stands in for the desktop app as the program File Explorer's context menu commands
//! start, in the VM harness's checks. A GUI-subsystem program, as the app is, so starting it shows
//! no console window. Writes the command line it was given, one argument per line, each as its
//! UTF-16 code units in hex (so a name Explorer passes is compared exactly, whatever it holds), to
//! `%LOCALAPPDATA%\filen-verb-stub\<pid>.txt`, then exits.

#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
fn main() -> std::process::ExitCode {
	use std::{env, fs, os::windows::ffi::OsStrExt, path::PathBuf, process::ExitCode};

	let Some(base) = env::var_os("LOCALAPPDATA") else {
		return ExitCode::from(2);
	};
	let dir = PathBuf::from(base).join("filen-verb-stub");
	let lines: Vec<String> = env::args_os()
		.map(|arg| {
			arg.encode_wide()
				.map(|unit| format!("{unit:04x}"))
				.collect::<Vec<_>>()
				.join(" ")
		})
		.collect();
	let written = fs::create_dir_all(&dir).and_then(|()| {
		let temp = dir.join(format!("{}.tmp", std::process::id()));
		fs::write(&temp, lines.join("\n"))?;
		// Renamed into place, so a reader never sees half a file.
		fs::rename(&temp, dir.join(format!("{}.txt", std::process::id())))
	});
	if written.is_ok() {
		ExitCode::SUCCESS
	} else {
		ExitCode::from(1)
	}
}

#[cfg(not(windows))]
fn main() {
	eprintln!("verb_stub runs on Windows only");
}
