//! The REPL's history, which is kept in the config directory across runs, without the lines that
//! give a secret.

use std::{
	fs,
	io::{self, Write as _},
	path::{Path, PathBuf},
	process,
};

use clap::{ArgMatches, Command, CommandFactory as _, parser::ValueSource};
use log::warn;

use crate::{
	CliArgs, ParsedArgs, parse_repl_line_matches,
	private_file::{open_private, open_private_append, private_file_options},
};

/// The file in the config directory that holds the history.
pub(crate) const REPL_HISTORY_FILE: &str = "repl_history";

/// The most lines the history keeps.
const MAX_ENTRIES: usize = 1000;

/// The lines entered at the REPL, which the arrow keys bring back.
#[derive(Default)]
pub(crate) struct ReplHistory {
	/// Newest first, with this run's lines, those not saved included.
	entries: Vec<String>,
	/// Where the lines are saved, if they are.
	file: Option<HistoryFile>,
}

/// The file the history is saved in: one line per entry, oldest first.
struct HistoryFile {
	path: PathBuf,
	/// How many lines it holds.
	lines: usize,
}

impl ReplHistory {
	/// The history saved in `path`, which the lines entered from now on are added to. A history
	/// that cannot be read is logged and starts empty.
	pub(crate) fn load(path: PathBuf) -> Self {
		let saved = match fs::read_to_string(&path) {
			Ok(saved) => saved,
			Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
			Err(e) => {
				warn!("Failed to read the REPL history {}: {e}", path.display());
				String::new()
			}
		};
		let lines = saved
			.lines()
			.filter(|line| !line.is_empty())
			.collect::<Vec<_>>();
		let kept = lines
			.get(lines.len().saturating_sub(MAX_ENTRIES)..)
			.unwrap_or_default();
		let mut history = Self {
			entries: kept.iter().rev().map(|line| (*line).to_owned()).collect(),
			file: Some(HistoryFile {
				path,
				lines: lines.len(),
			}),
		};
		if lines.len() > MAX_ENTRIES {
			history.rewrite_file();
		}
		history
	}

	/// The lines, newest first.
	pub(crate) fn entries(&self) -> &[String] {
		&self.entries
	}

	/// Adds a line entered at the REPL, unless it repeats the latest. It is saved unless it gives a
	/// secret or does not parse ([`is_safe_to_save`]).
	pub(crate) fn add(&mut self, line: &str) {
		if self.entries.first().is_some_and(|latest| latest == line) {
			return;
		}
		self.entries.insert(0, line.to_owned());
		self.entries.truncate(MAX_ENTRIES);
		if self.file.is_some() && is_safe_to_save(line) {
			self.append_to_file(line);
		}
	}

	fn append_to_file(&mut self, line: &str) {
		let Some(file) = &mut self.file else {
			return;
		};
		let appended =
			open_private_append(&file.path).and_then(|mut opened| writeln!(opened, "{line}"));
		match appended {
			Ok(()) => file.lines = file.lines.saturating_add(1),
			Err(e) => {
				warn!(
					"Failed to save the line to the REPL history {}: {e}",
					file.path.display()
				);
				return;
			}
		}
		if file.lines > MAX_ENTRIES {
			self.rewrite_file();
		}
	}

	/// Cuts the file down to its latest [`MAX_ENTRIES`] lines. Reads it again rather than writing
	/// [`ReplHistory::entries`], which holds lines that are not saved, and keeps what another REPL
	/// running at the same time appended before the read. The kept lines go to a sibling file that
	/// then replaces the history, so a run killed mid-write leaves the history whole; a line another
	/// REPL appends between the read and the replace is lost, which is the cost of a cut that takes
	/// no lock.
	fn rewrite_file(&mut self) {
		let Some(file) = &mut self.file else {
			return;
		};
		let rewritten = fs::read_to_string(&file.path).and_then(|saved| {
			let lines = saved
				.lines()
				.filter(|line| !line.is_empty())
				.collect::<Vec<_>>();
			let kept = lines
				.get(lines.len().saturating_sub(MAX_ENTRIES)..)
				.unwrap_or_default();
			replace_with_lines(&file.path, kept)?;
			Ok(kept.len())
		});
		match rewritten {
			Ok(lines) => file.lines = lines,
			Err(e) => warn!(
				"Failed to cut the REPL history {} down to {MAX_ENTRIES} lines: {e}",
				file.path.display()
			),
		}
	}
}

/// Replaces the file at `path` with `lines`, one per line: writes them to a sibling file named
/// after this process, so that two REPLs do not write the same one, and renames it over `path`.
fn replace_with_lines(path: &Path, lines: &[&str]) -> io::Result<()> {
	let mut temp_name = path.file_name().unwrap_or_default().to_owned();
	temp_name.push(format!(".{}.tmp", process::id()));
	let temp_path = path.with_file_name(temp_name);
	let written = open_private(
		private_file_options()
			.create(true)
			.write(true)
			.truncate(true),
		&temp_path,
	)
	.and_then(|mut opened| {
		for line in lines {
			writeln!(opened, "{line}")?;
		}
		opened.sync_all()
	})
	.and_then(|()| fs::rename(&temp_path, path));
	if written.is_err()
		&& let Err(e) = fs::remove_file(&temp_path)
		&& e.kind() != io::ErrorKind::NotFound
	{
		warn!(
			"Failed to remove the unfinished REPL history {}: {e}",
			temp_path.display()
		);
	}
	written
}

/// The path of the history in `config_dir`.
pub(crate) fn history_path(config_dir: &Path) -> PathBuf {
	config_dir.join(REPL_HISTORY_FILE)
}

/// Whether `line` can be saved in the history: it parses as a REPL line, and gives no secret
/// (see [`gives_a_secret`]). A line that does not parse could hold anything, and one with a line
/// break would not read back as one line.
fn is_safe_to_save(line: &str) -> bool {
	if line.trim().is_empty() || line.contains(['\n', '\r']) {
		return false;
	}
	match parse_repl_line_matches(line) {
		Ok(ParsedArgs { matches, .. }) => !gives_a_secret(&CliArgs::command(), &matches),
		Err(_) => false,
	}
}

/// Whether `matches`, which `command` parsed, or those of its subcommand give a value on the
/// command line for an option that holds a secret ([`is_secret`]), or for one that passes
/// arguments on to another program (`rclone`'s), which could hold one.
fn gives_a_secret(command: &Command, matches: &ArgMatches) -> bool {
	let given = matches
		.ids()
		.filter(|id| matches.value_source(id.as_str()) == Some(ValueSource::CommandLine));
	let gives = given.into_iter().any(|id| {
		is_secret(id.as_str())
			|| command
				.get_arguments()
				.find(|arg| arg.get_id() == id)
				.is_some_and(|arg| arg.is_trailing_var_arg_set())
	});
	gives
		|| matches.subcommand().is_some_and(|(name, sub_matches)| {
			// A subcommand clap parsed is one of the command's; were it not, it is safer to assume
			// it gives a secret.
			command
				.find_subcommand(name)
				.is_none_or(|sub_command| gives_a_secret(sub_command, sub_matches))
		})
}

/// Whether the option with this id holds a secret: a password, a two-factor code or recovery key
/// (`2fa`'s `--code`), a token or a key.
fn is_secret(id: &str) -> bool {
	id.starts_with("two_factor")
		|| id.split('_').any(|word| {
			matches!(
				word,
				"password" | "passphrase" | "pass" | "secret" | "token" | "key" | "otp" | "code"
			)
		})
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn a_plain_command_is_saved() {
		assert!(is_safe_to_save("ls /some/dir"));
	}

	#[test]
	fn a_serve_password_is_not_saved() {
		assert!(!is_safe_to_save(
			"serve webdav --user me --password hunter2"
		));
	}

	#[test]
	fn a_serve_without_a_password_is_saved() {
		assert!(is_safe_to_save("serve webdav --user me"));
	}

	#[test]
	fn a_line_the_repl_rejects_is_not_saved() {
		assert!(!is_safe_to_save("ls --auth-config-path /some/file"));
	}

	#[test]
	fn an_rclone_command_is_not_saved() {
		assert!(!is_safe_to_save(
			"rclone config create remote webdav pass=secret"
		));
	}

	#[test]
	fn a_line_that_does_not_parse_is_not_saved() {
		assert!(!is_safe_to_save("ls \"unclosed"));
		assert!(!is_safe_to_save("unknowncommand hunter2"));
	}

	#[test]
	fn a_line_giving_an_account_password_is_not_saved() {
		assert!(!is_safe_to_save("-p hunter2 ls"));
	}

	#[test]
	fn a_two_factor_code_or_recovery_key_is_not_saved() {
		assert!(!is_safe_to_save("2fa enable --code 123456"));
		assert!(!is_safe_to_save("2fa disable --code recovery-key"));
	}

	#[test]
	fn a_line_with_a_line_break_is_not_saved() {
		assert!(!is_safe_to_save("ls\nls"));
	}

	#[test]
	fn secret_option_ids_are_recognised() {
		for id in [
			"password",
			"two_factor_code",
			"api_key",
			"token",
			"client_secret",
			"code",
		] {
			assert!(is_secret(id), "{id}");
		}
		for id in ["auth_config_path", "email", "keep", "directory", "user"] {
			assert!(!is_secret(id), "{id}");
		}
	}

	#[test]
	fn lines_are_saved_and_loaded_newest_first() {
		let dir = tempfile::tempdir().unwrap();
		let path = history_path(dir.path());
		let mut history = ReplHistory::load(path.clone());
		history.add("ls");
		history.add("cd /dir");
		history.add("cd /dir");

		assert_eq!(fs::read_to_string(&path).unwrap(), "ls\ncd /dir\n");
		assert_eq!(ReplHistory::load(path).entries(), ["cd /dir", "ls"]);
	}

	#[test]
	fn a_secret_line_stays_in_this_runs_history_only() {
		let dir = tempfile::tempdir().unwrap();
		let path = history_path(dir.path());
		let mut history = ReplHistory::load(path.clone());
		history.add("ls");
		history.add("serve webdav --password hunter2");

		assert_eq!(history.entries(), ["serve webdav --password hunter2", "ls"]);
		assert_eq!(fs::read_to_string(&path).unwrap(), "ls\n");
	}

	#[test]
	fn the_history_keeps_the_latest_entries_up_to_the_cap() {
		let dir = tempfile::tempdir().unwrap();
		let path = history_path(dir.path());
		let mut history = ReplHistory::load(path.clone());
		for i in 0..=MAX_ENTRIES {
			history.add(&format!("ls /{i}"));
		}

		assert_eq!(history.entries().len(), MAX_ENTRIES);
		assert_eq!(history.entries().first().unwrap(), "ls /1000");
		assert_eq!(history.entries().last().unwrap(), "ls /1");
		let saved = fs::read_to_string(&path).unwrap();
		assert_eq!(saved.lines().count(), MAX_ENTRIES);
		assert_eq!(saved.lines().next(), Some("ls /1"));
		assert_eq!(saved.lines().last(), Some("ls /1000"));
	}

	#[test]
	fn a_history_file_over_the_cap_is_cut_down_on_load() {
		let dir = tempfile::tempdir().unwrap();
		let path = history_path(dir.path());
		let lines = (0..MAX_ENTRIES + 5)
			.map(|i| format!("ls /{i}\n"))
			.collect::<String>();
		fs::write(&path, lines).unwrap();

		let history = ReplHistory::load(path.clone());
		assert_eq!(history.entries().len(), MAX_ENTRIES);
		assert_eq!(history.entries().last().unwrap(), "ls /5");
		let saved = fs::read_to_string(&path).unwrap();
		assert_eq!(saved.lines().count(), MAX_ENTRIES);
		assert_eq!(saved.lines().next(), Some("ls /5"));
	}

	#[test]
	fn cutting_the_history_leaves_no_other_file_behind() {
		let dir = tempfile::tempdir().unwrap();
		let path = history_path(dir.path());
		let lines = (0..=MAX_ENTRIES)
			.map(|i| format!("ls /{i}\n"))
			.collect::<String>();
		fs::write(&path, lines).unwrap();

		ReplHistory::load(path);
		let names = fs::read_dir(dir.path())
			.unwrap()
			.map(|entry| entry.unwrap().file_name())
			.collect::<Vec<_>>();
		assert_eq!(names, [REPL_HISTORY_FILE]);
	}

	#[test]
	fn a_missing_history_starts_empty() {
		let dir = tempfile::tempdir().unwrap();
		let history = ReplHistory::load(history_path(dir.path()));
		assert!(history.entries().is_empty());
	}

	#[test]
	fn an_unwritable_history_keeps_the_line_for_this_run() {
		let dir = tempfile::tempdir().unwrap();
		// a directory where the file would be
		let path = history_path(dir.path());
		fs::create_dir(&path).unwrap();
		let mut history = ReplHistory::load(path);
		history.add("ls");
		assert_eq!(history.entries(), ["ls"]);
	}

	#[cfg(unix)]
	#[test]
	fn the_history_file_is_readable_by_its_owner_only() {
		use std::os::unix::fs::PermissionsExt as _;

		let dir = tempfile::tempdir().unwrap();
		let path = history_path(dir.path());
		let mut history = ReplHistory::load(path.clone());
		history.add("ls");
		let mode = fs::metadata(&path).unwrap().permissions().mode();
		assert_eq!(mode & 0o777, 0o600);
	}
}
