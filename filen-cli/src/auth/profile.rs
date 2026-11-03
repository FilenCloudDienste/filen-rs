//! [cli-doc] profile-option
//! `--profile <name>` (or `FILEN_CLI_PROFILE`) uses the profile `<name>`: its own config dir,
//! `profiles/<name>` in the config dir, which is created the first time it is used. A profile has
//! its own login (in the system keychain or an auth config file), cache, logs and sync pairs, so
//! that you can stay logged in to several accounts at once. A profile name consists of letters,
//! digits, `-` and `_`.
//!
//! `profiles` lists the profiles, marking the one in use with `*`. With `--json`, it prints them as
//! an array of `{"name", "active"}`.

use std::{
	fmt,
	io::ErrorKind,
	path::{Path, PathBuf},
	str::FromStr,
};

use anyhow::{Context as _, Result};
use serde_json::json;

use crate::ui::UI;

/// The directory in the config dir that holds a directory for each profile.
const PROFILES_DIR_NAME: &str = "profiles";

/// The name of a profile, which names its directory: letters, digits, `-` and `_` only, so that it
/// never names another directory, such as `..` or a path.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct ProfileName(String);

/// Why a profile name is invalid.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ProfileNameError {
	Empty,
	/// `ch`, at the byte index `at`, is not a letter, a digit, `-` or `_`.
	Forbidden {
		ch: char,
		at: usize,
	},
}

impl ProfileName {
	pub(crate) fn parse(name: &str) -> Result<Self, ProfileNameError> {
		if name.is_empty() {
			return Err(ProfileNameError::Empty);
		}
		if let Some((at, ch)) = name
			.char_indices()
			.find(|(_, ch)| !(ch.is_ascii_alphanumeric() || *ch == '-' || *ch == '_'))
		{
			return Err(ProfileNameError::Forbidden { ch, at });
		}
		Ok(Self(name.to_owned()))
	}

	pub(crate) fn as_str(&self) -> &str {
		&self.0
	}
}

impl FromStr for ProfileName {
	type Err = ProfileNameError;

	fn from_str(name: &str) -> Result<Self, Self::Err> {
		Self::parse(name)
	}
}

impl fmt::Display for ProfileNameError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::Empty => f.write_str("a profile name must not be empty"),
			Self::Forbidden { ch, at } => write!(
				f,
				"a profile name may only contain letters, digits, `-` and `_`, not {ch:?} (at \
				 byte {at})"
			),
		}
	}
}

impl std::error::Error for ProfileNameError {}

/// The profiles of a config dir, and the one in use.
#[derive(Debug, Clone)]
pub(crate) struct Profiles {
	/// The directory holding a directory for each profile.
	dir: PathBuf,
	active: Option<ProfileName>,
}

impl Profiles {
	/// The profiles kept in `base_config_dir`, the config dir without a profile, with `active` in
	/// use.
	pub(crate) fn new(base_config_dir: &Path, active: Option<ProfileName>) -> Self {
		Self {
			dir: base_config_dir.join(PROFILES_DIR_NAME),
			active,
		}
	}

	/// The config dir of the active profile, or `None` without one.
	fn active_dir(&self) -> Option<PathBuf> {
		self.active
			.as_ref()
			.map(|name| self.dir.join(name.as_str()))
	}

	/// The config dir to use: the active profile's, which is created if it does not exist yet, or
	/// `base_config_dir` without a profile.
	pub(crate) fn config_dir(&self, base_config_dir: &Path) -> Result<PathBuf> {
		let Some(dir) = self.active_dir() else {
			return Ok(base_config_dir.to_path_buf());
		};
		std::fs::create_dir_all(&dir)
			.with_context(|| format!("Failed to create the profile dir {}", dir.display()))?;
		Ok(dir)
	}

	/// The profiles there are, by name: the directories in the profiles dir with a valid name.
	fn list(&self) -> Result<Vec<ProfileName>> {
		let entries = match std::fs::read_dir(&self.dir) {
			Ok(entries) => entries,
			Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
			Err(e) => {
				return Err(e).with_context(|| {
					format!("Failed to list the profiles in {}", self.dir.display())
				});
			}
		};
		let mut names = Vec::new();
		for entry in entries {
			let entry = entry.with_context(|| {
				format!("Failed to list the profiles in {}", self.dir.display())
			})?;
			if !entry.path().is_dir() {
				continue;
			}
			match entry.file_name().to_str().map(ProfileName::parse) {
				Some(Ok(name)) => names.push(name),
				_ => log::info!(
					"Skipping {}, which is not named as a profile",
					entry.path().display()
				),
			}
		}
		names.sort();
		Ok(names)
	}
}

/// Print the profiles of `profiles`, marking the active one, or an array of `{"name", "active"}`
/// with `--json`.
pub(crate) fn print_profiles(ui: &mut UI, profiles: &Profiles) -> Result<()> {
	let names = profiles.list()?;
	let is_active = |name: &ProfileName| profiles.active.as_ref() == Some(name);
	if ui.json {
		return ui.print_json(
			names
				.iter()
				.map(|name| json!({ "name": name.as_str(), "active": is_active(name) }))
				.collect::<Vec<_>>(),
		);
	}
	if names.is_empty() {
		ui.print_muted("No profiles: pass --profile <name> to create one");
		return Ok(());
	}
	for name in &names {
		let marker = if is_active(name) { "*" } else { " " };
		ui.print(&format!("{marker} {}", name.as_str()));
	}
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::ui::test_support::Printed;

	fn name(name: &str) -> ProfileName {
		ProfileName::parse(name).unwrap()
	}

	#[test]
	fn letters_digits_dashes_and_underscores_make_a_profile_name() {
		assert_eq!(name("Work_2-old").as_str(), "Work_2-old");
	}

	#[test]
	fn an_empty_profile_name_is_invalid() {
		assert_eq!(ProfileName::parse(""), Err(ProfileNameError::Empty));
	}

	#[test]
	fn a_profile_name_with_a_path_separator_is_invalid() {
		assert_eq!(
			ProfileName::parse("work/old"),
			Err(ProfileNameError::Forbidden { ch: '/', at: 4 })
		);
		assert_eq!(
			ProfileName::parse("work\\old"),
			Err(ProfileNameError::Forbidden { ch: '\\', at: 4 })
		);
	}

	#[test]
	fn a_profile_name_cannot_leave_the_profiles_dir() {
		assert_eq!(
			ProfileName::parse(".."),
			Err(ProfileNameError::Forbidden { ch: '.', at: 0 })
		);
	}

	#[test]
	fn a_profile_name_with_a_space_or_a_non_ascii_letter_is_invalid() {
		assert_eq!(
			ProfileName::parse("my work"),
			Err(ProfileNameError::Forbidden { ch: ' ', at: 2 })
		);
		assert_eq!(
			ProfileName::parse("büro"),
			Err(ProfileNameError::Forbidden { ch: 'ü', at: 1 })
		);
	}

	#[test]
	fn an_invalid_profile_name_says_what_is_allowed() {
		assert_eq!(
			ProfileName::parse("a/b").unwrap_err().to_string(),
			"a profile name may only contain letters, digits, `-` and `_`, not '/' (at byte 1)"
		);
	}

	#[test]
	fn the_profile_flag_goes_before_or_after_the_command() {
		for args in [
			["filen", "--profile", "work", "ls"],
			["filen", "ls", "--profile", "work"],
		] {
			let parsed = crate::parse_args(args).unwrap();
			assert_eq!(parsed.args.profile, Some(name("work")));
		}
	}

	#[test]
	fn the_profile_flag_refuses_a_path() {
		let err = crate::parse_args(["filen", "--profile", "../other", "ls"])
			.err()
			.expect("a path is no profile name");
		assert_eq!(err.kind(), clap::error::ErrorKind::ValueValidation);
		assert!(
			err.to_string().contains(
				"a profile name may only contain letters, digits, `-` and `_`, not '.' (at byte 0)"
			),
			"{err}"
		);
	}

	#[test]
	fn without_a_profile_the_config_dir_is_the_base_one() {
		let base = assert_fs::TempDir::new().unwrap();
		let profiles = Profiles::new(base.path(), None);

		assert_eq!(profiles.config_dir(base.path()).unwrap(), base.path());
		assert!(!base.path().join(PROFILES_DIR_NAME).exists());
	}

	#[test]
	fn a_profile_has_its_config_dir_in_the_profiles_dir_created_on_first_use() {
		let base = assert_fs::TempDir::new().unwrap();
		let profiles = Profiles::new(base.path(), Some(name("work")));

		let dir = profiles.config_dir(base.path()).unwrap();

		assert_eq!(dir, base.path().join("profiles").join("work"));
		assert!(dir.is_dir());
	}

	#[test]
	fn the_profiles_are_listed_by_name_with_the_active_one_marked() {
		let base = assert_fs::TempDir::new().unwrap();
		for profile in ["work", "home", "archive"] {
			std::fs::create_dir_all(base.path().join("profiles").join(profile)).unwrap();
		}
		// neither a profile: a file, and a directory with a name no profile can have
		std::fs::write(base.path().join("profiles").join("notes"), "").unwrap();
		std::fs::create_dir_all(base.path().join("profiles").join("not a profile")).unwrap();
		let printed = Printed::default();

		print_profiles(
			&mut printed.ui(),
			&Profiles::new(base.path(), Some(name("home"))),
		)
		.unwrap();

		assert_eq!(printed.lines(), "  archive\n* home\n  work");
	}

	#[test]
	fn the_profiles_with_json_are_an_array() {
		let base = assert_fs::TempDir::new().unwrap();
		for profile in ["work", "home"] {
			std::fs::create_dir_all(base.path().join("profiles").join(profile)).unwrap();
		}
		let printed = Printed::default();
		let mut ui = printed.ui();
		ui.set_quiet_and_json(false, true);

		print_profiles(&mut ui, &Profiles::new(base.path(), Some(name("work")))).unwrap();

		let printed: serde_json::Value = serde_json::from_str(&printed.lines()).unwrap();
		assert_eq!(
			printed,
			json!([
				{ "name": "home", "active": false },
				{ "name": "work", "active": true },
			])
		);
	}

	#[test]
	fn without_profiles_the_listing_says_how_to_create_one() {
		let base = assert_fs::TempDir::new().unwrap();
		let printed = Printed::default();

		print_profiles(&mut printed.ui(), &Profiles::new(base.path(), None)).unwrap();

		assert_eq!(
			console::strip_ansi_codes(&printed.lines()),
			"No profiles: pass --profile <name> to create one"
		);
	}

	#[test]
	fn without_profiles_the_listing_with_json_is_an_empty_array() {
		let base = assert_fs::TempDir::new().unwrap();
		let printed = Printed::default();
		let mut ui = printed.ui();
		ui.set_quiet_and_json(false, true);

		print_profiles(&mut ui, &Profiles::new(base.path(), None)).unwrap();

		assert_eq!(printed.lines(), "[]");
	}
}
