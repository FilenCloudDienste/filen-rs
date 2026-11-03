//! Files the older, TypeScript Filen CLI left in its config directory. Users who switch are told
//! once what they are and what to do about them.

use std::{
	fs,
	path::{Path, PathBuf},
};

use log::warn;

use crate::ui::UI;

/// The file whose existence in the config directory says the hint was shown.
const HINT_SHOWN_STAMP: &str = "ts_migration_hint_shown";

/// What a leftover file held, which decides what the user is told to do about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LeftoverKind {
	/// A login it saved, which logging in tries to import (see `auth::old_cli`).
	SavedLogin,
	/// Its plaintext credentials file, which this CLI does not read.
	Credentials,
	/// Sync pairs or their sync state.
	Sync,
	/// The update check's state.
	UpdateCache,
}

/// The names the old CLI used in its config directory, and what each held.
const LEFTOVER_NAMES: &[(&str, LeftoverKind)] = &[
	(".filen-cli-keep-me-logged-in", LeftoverKind::SavedLogin),
	(".filen-cli-auth-config", LeftoverKind::SavedLogin),
	(".filen-cli-credentials", LeftoverKind::Credentials),
	("syncPairs.json", LeftoverKind::Sync),
	("sync", LeftoverKind::Sync),
	("updateCache.json", LeftoverKind::UpdateCache),
];

/// A file or directory the old CLI left.
#[derive(Debug, PartialEq, Eq)]
struct Leftover {
	path: PathBuf,
	kind: LeftoverKind,
}

/// The directories the old CLI may have used: `config_dir`, and if that is the default one, also
/// `~/.filen-cli` (which the old CLI preferred when it existed) and `filen-cli` in the platform's
/// config directory (its default). A config directory the user chose is the only one looked in,
/// as the user pointed both CLIs at it.
fn candidate_dirs(config_dir: &Path, config_dir_is_default: bool) -> Vec<PathBuf> {
	let mut dirs = vec![config_dir.to_owned()];
	if config_dir_is_default {
		let others = [
			dirs::home_dir().map(|home| home.join(".filen-cli")),
			dirs::config_dir().map(|config| config.join("filen-cli")),
		];
		for dir in others.into_iter().flatten() {
			if !dirs.contains(&dir) {
				dirs.push(dir);
			}
		}
	}
	dirs
}

/// The old CLI's files in `dirs`. Only their existence is checked: nothing is read.
fn find_leftovers(dirs: &[PathBuf]) -> Vec<Leftover> {
	dirs.iter()
		.flat_map(|dir| {
			LEFTOVER_NAMES.iter().map(move |(name, kind)| Leftover {
				path: dir.join(name),
				kind: *kind,
			})
		})
		.filter(|leftover| leftover.path.symlink_metadata().is_ok())
		.collect()
}

/// The hint for `leftovers`, which are not empty.
fn hint(leftovers: &[Leftover]) -> String {
	let mut hint = String::from("Found files of the older Filen CLI:\n");
	for leftover in leftovers {
		hint.push_str(&format!("  {}\n", leftover.path.display()));
	}
	let has = |kind| leftovers.iter().any(|leftover| leftover.kind == kind);
	if has(LeftoverKind::SavedLogin) {
		hint.push_str(
			"Logging in without a login of this CLI tries to import its saved login. If it asks \
			 for your credentials instead, log in again, and run `filen export-auth-config` to \
			 save an auth config for scripts.\n",
		);
	}
	if has(LeftoverKind::Credentials) {
		hint.push_str(
			"Its saved login does not carry over: log in again, and run `filen \
			 export-auth-config` to save an auth config for scripts.\n",
		);
	}
	if has(LeftoverKind::Sync) {
		hint.push_str(
			"Its sync pairs do not carry over: define them again in syncPairs.yaml (see `filen \
			 help sync`).\n",
		);
	}
	hint.push_str(match has(LeftoverKind::SavedLogin) {
		true => {
			"You can delete these files once this CLI is logged in. This hint is not shown again."
		}
		false => "You can delete these files. This hint is not shown again.",
	});
	hint
}

/// Prints a hint once if the old CLI left files in its config directories (see
/// [`candidate_dirs`]), then writes [`HINT_SHOWN_STAMP`] into `config_dir`. Not with `--quiet` or
/// `--json`, or while stderr is not a terminal (`stderr_is_terminal`), as in a script or a cron
/// job, where no one may read it: it waits for a later run. Nothing is moved or deleted.
pub(crate) fn hint_once(
	ui: &mut UI,
	config_dir: &Path,
	config_dir_is_default: bool,
	stderr_is_terminal: bool,
) {
	let stamp = config_dir.join(HINT_SHOWN_STAMP);
	if stamp.symlink_metadata().is_ok() || !stderr_is_terminal || ui.is_quiet() || ui.json {
		return;
	}
	let leftovers = find_leftovers(&candidate_dirs(config_dir, config_dir_is_default));
	if leftovers.is_empty() {
		return;
	}
	let hint = hint(&leftovers);
	warn!("{hint}");
	ui.print_warning(&hint);
	if let Err(e) = fs::write(&stamp, "") {
		warn!(
			"Failed to write {}, so the hint about the older CLI's files shows again: {e}",
			stamp.display()
		);
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::ui::test_support::Printed;

	#[test]
	fn every_kind_of_leftover_is_found_and_nothing_else() {
		let dir = tempfile::tempdir().unwrap();
		for name in [
			".filen-cli-keep-me-logged-in",
			".filen-cli-auth-config",
			".filen-cli-credentials",
			"syncPairs.json",
			"updateCache.json",
			"filen-cli-auth-config.txt",
			"syncPairs.yaml",
		] {
			fs::write(dir.path().join(name), "").unwrap();
		}
		fs::create_dir(dir.path().join("sync")).unwrap();
		fs::create_dir(dir.path().join("logs")).unwrap();

		let found = find_leftovers(&[dir.path().to_owned()]);
		let leftover = |name: &str, kind| Leftover {
			path: dir.path().join(name),
			kind,
		};
		assert_eq!(
			found,
			[
				leftover(".filen-cli-keep-me-logged-in", LeftoverKind::SavedLogin),
				leftover(".filen-cli-auth-config", LeftoverKind::SavedLogin),
				leftover(".filen-cli-credentials", LeftoverKind::Credentials),
				leftover("syncPairs.json", LeftoverKind::Sync),
				leftover("sync", LeftoverKind::Sync),
				leftover("updateCache.json", LeftoverKind::UpdateCache),
			]
		);
	}

	#[test]
	fn leftovers_are_found_in_every_directory_given() {
		let first = tempfile::tempdir().unwrap();
		let second = tempfile::tempdir().unwrap();
		fs::write(first.path().join("updateCache.json"), "").unwrap();
		fs::write(second.path().join(".filen-cli-credentials"), "").unwrap();

		let found = find_leftovers(&[first.path().to_owned(), second.path().to_owned()]);
		assert_eq!(
			found,
			[
				Leftover {
					path: first.path().join("updateCache.json"),
					kind: LeftoverKind::UpdateCache,
				},
				Leftover {
					path: second.path().join(".filen-cli-credentials"),
					kind: LeftoverKind::Credentials,
				},
			]
		);
	}

	#[test]
	fn a_chosen_config_dir_is_the_only_one_looked_in() {
		let dir = Path::new("/chosen");
		assert_eq!(candidate_dirs(dir, false), [PathBuf::from("/chosen")]);
	}

	#[test]
	fn the_default_config_dir_comes_first_and_is_not_repeated() {
		let Some(default) = dirs::config_dir().map(|config| config.join("filen-cli")) else {
			panic!("no platform config directory");
		};
		let dirs = candidate_dirs(&default, true);
		assert_eq!(dirs.first(), Some(&default));
		assert_eq!(dirs.iter().filter(|dir| **dir == default).count(), 1);
	}

	#[test]
	fn the_hint_says_what_to_do_about_each_kind() {
		let leftovers = [
			Leftover {
				path: PathBuf::from("/c/.filen-cli-credentials"),
				kind: LeftoverKind::Credentials,
			},
			Leftover {
				path: PathBuf::from("/c/sync"),
				kind: LeftoverKind::Sync,
			},
		];
		assert_eq!(
			hint(&leftovers),
			"Found files of the older Filen CLI:\n  /c/.filen-cli-credentials\n  /c/sync\nIts saved login does not carry over: log in \
			 again, and run `filen export-auth-config` to save an auth config for scripts.\nIts \
			 sync pairs do not carry over: define them again in syncPairs.yaml (see `filen help \
			 sync`).\nYou can delete these files. This hint is not shown again."
		);
	}

	#[test]
	fn the_hint_says_logging_in_imports_a_saved_login() {
		let leftovers = [Leftover {
			path: PathBuf::from("/c/.filen-cli-keep-me-logged-in"),
			kind: LeftoverKind::SavedLogin,
		}];
		assert_eq!(
			hint(&leftovers),
			"Found files of the older Filen CLI:\n  /c/.filen-cli-keep-me-logged-in\nLogging in \
			 without a login of this CLI tries to import its saved login. If it asks for your \
			 credentials instead, log in again, and run `filen export-auth-config` to save an auth \
			 config for scripts.\nYou can delete these files once this CLI is logged in. This hint \
			 is not shown again."
		);
	}

	#[test]
	fn the_hint_for_an_update_cache_only_says_to_delete_it() {
		let leftovers = [Leftover {
			path: PathBuf::from("/c/updateCache.json"),
			kind: LeftoverKind::UpdateCache,
		}];
		assert_eq!(
			hint(&leftovers),
			"Found files of the older Filen CLI:\n  /c/updateCache.json\nYou can delete these files. This hint is not shown again."
		);
	}

	#[test]
	fn the_hint_is_shown_once() {
		let dir = tempfile::tempdir().unwrap();
		fs::write(dir.path().join("syncPairs.json"), "").unwrap();
		let printed = Printed::default();
		let mut ui = printed.ui();

		hint_once(&mut ui, dir.path(), false, true);
		let first = console::strip_ansi_codes(&printed.lines()).into_owned();
		assert!(
			first.contains(&dir.path().join("syncPairs.json").display().to_string()),
			"{first}"
		);
		assert!(dir.path().join(HINT_SHOWN_STAMP).exists());

		hint_once(&mut ui, dir.path(), false, true);
		assert_eq!(console::strip_ansi_codes(&printed.lines()), first);
		// nothing was moved or deleted
		assert!(dir.path().join("syncPairs.json").exists());
	}

	#[test]
	fn no_hint_and_no_stamp_without_leftovers() {
		let dir = tempfile::tempdir().unwrap();
		let printed = Printed::default();
		let mut ui = printed.ui();

		hint_once(&mut ui, dir.path(), false, true);
		assert_eq!(printed.lines_or_empty(), "");
		assert!(!dir.path().join(HINT_SHOWN_STAMP).exists());
	}

	#[test]
	fn the_hint_waits_while_the_output_is_json() {
		let dir = tempfile::tempdir().unwrap();
		fs::write(dir.path().join("updateCache.json"), "").unwrap();
		let printed = Printed::default();
		let mut ui = printed.ui();
		ui.set_quiet_and_json(false, true);

		hint_once(&mut ui, dir.path(), false, true);
		assert_eq!(printed.lines_or_empty(), "");
		assert!(!dir.path().join(HINT_SHOWN_STAMP).exists());
	}

	#[test]
	fn the_hint_waits_while_stderr_is_not_a_terminal() {
		let dir = tempfile::tempdir().unwrap();
		fs::write(dir.path().join("updateCache.json"), "").unwrap();
		let printed = Printed::default();
		let mut ui = printed.ui();

		hint_once(&mut ui, dir.path(), false, false);
		assert_eq!(printed.lines_or_empty(), "");
		assert!(!dir.path().join(HINT_SHOWN_STAMP).exists());
	}

	#[test]
	fn the_hint_waits_while_quiet() {
		let dir = tempfile::tempdir().unwrap();
		fs::write(dir.path().join("updateCache.json"), "").unwrap();
		let printed = Printed::default();
		let mut ui = printed.ui();
		ui.set_quiet_and_json(true, false);

		hint_once(&mut ui, dir.path(), false, true);
		assert_eq!(printed.lines_or_empty(), "");
		assert!(!dir.path().join(HINT_SHOWN_STAMP).exists());
	}
}
