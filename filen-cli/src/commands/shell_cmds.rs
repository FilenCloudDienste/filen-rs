//! Commands about the CLI itself rather than the drive.

use std::{
	env, fs,
	path::{Path, PathBuf},
};

use anyhow::{Context as _, Result};
use clap::{
	Arg, Command, CommandFactory as _, Subcommand, ValueHint,
	builder::{Resettable, Str},
};
use clap_complete::Shell;
use console::style;
use serde_json::json;

use crate::{
	CliArgs, CommandResult,
	commands::CommandContext,
	docs::DocElement,
	ui::UI,
	updater::{self, BUILD_TARGET, IN_DOCKER_IMAGE, ReleaseRequest},
};

#[derive(Debug, Subcommand)]
pub(crate) enum ShellCommands {
	/// Print the version of the CLI, the platform it was built for and its config directory
	Version,
	/// Install a release of the CLI in place of this one, also an older one
	Install {
		/// "latest", or the version to install, such as v0.2.9
		#[arg(value_name = "latest|vX.Y.Z")]
		release: ReleaseRequest,
	},
	/// Print a script that completes the CLI's commands and options in a shell (see the install
	/// lines in `help shell-completions`)
	Completions {
		/// The shell to print the completion script for
		shell: Shell,
	},
	/// Change the local working directory, which local paths start from (in REPL)
	Lcd {
		/// Local directory to change into (supports "~")
		#[arg(value_hint = ValueHint::DirPath)]
		directory: PathBuf,
	},
	/// Print the local working directory
	Lpwd,
	/// List the files in a local directory, hidden ones too
	Lls {
		/// Local directory to list (default: the local working directory; supports "~")
		#[arg(value_hint = ValueHint::DirPath)]
		directory: Option<PathBuf>,
	},
}

/// The commands listed in the docs' usage section.
pub(crate) const DOCS: &[DocElement] = &[
	DocElement::CommandHelp("version"),
	DocElement::CommandHelp("install"),
	DocElement::CommandHelp("completions"),
	DocElement::DocFragment("shell-completions"),
	DocElement::CommandHelp("lcd"),
	DocElement::CommandHelp("lpwd"),
	DocElement::CommandHelp("lls"),
];

impl ShellCommands {
	/// Whether the update check when the CLI starts is skipped for this command: it works on the
	/// CLI itself and needs no other release announced.
	pub(crate) fn skips_update_check(&self) -> bool {
		match self {
			Self::Version | Self::Install { .. } | Self::Completions { .. } => true,
			Self::Lcd { .. } | Self::Lpwd | Self::Lls { .. } => false,
		}
	}

	pub(super) async fn execute(self, ctx: CommandContext<'_>) -> Result<CommandResult> {
		let CommandContext { config, ui, .. } = ctx;
		match self {
			Self::Version => print_version(ui, &config.config_dir)?,
			Self::Install { release } => updater::install(ui, &release).await?,
			Self::Completions { shell } => print_completions(ui, shell)?,
			Self::Lcd { directory } => change_local_dir(&directory)?,
			Self::Lpwd => print_local_dir(ui)?,
			Self::Lls { directory } => list_local_dir(ui, directory.as_deref())?,
		}
		Ok(CommandResult::default())
	}
}

/// Prints the CLI's version, the target it was built for, its config directory and whether it is
/// the Docker image's, or with `--json`, an object of them.
fn print_version(ui: &mut UI, config_dir: &Path) -> Result<()> {
	let version = env!("CARGO_PKG_VERSION");
	let config_dir = config_dir.to_string_lossy();
	if ui.json {
		return ui.print_json(json!({
			"version": version,
			"target": BUILD_TARGET,
			"configDir": config_dir,
			"docker": IN_DOCKER_IMAGE,
		}));
	}
	ui.print(&format!("Filen CLI v{version}"));
	ui.print_key_value_table(&[
		("Target:", BUILD_TARGET),
		("Config directory:", &config_dir),
		("Docker image:", if IN_DOCKER_IMAGE { "yes" } else { "no" }),
	]);
	Ok(())
}

/// [cli-doc] shell-completions
/// `filen completions <shell>` prints a script that completes the CLI's commands and options in
/// bash, zsh, fish, PowerShell or elvish. Remote paths are completed in the REPL only. Install the
/// script once, then open a new shell:
/// - bash: `filen completions bash > ~/.local/share/bash-completion/completions/filen`
/// - zsh: `filen completions zsh > ~/.zfunc/_filen`, with `fpath+=~/.zfunc` before `compinit` in
///   `~/.zshrc`
/// - fish: `filen completions fish > ~/.config/fish/completions/filen.fish`
/// - PowerShell: add `filen completions powershell | Out-String | Invoke-Expression` to `$PROFILE`
/// - elvish: add `eval (filen completions elvish | slurp)` to `~/.config/elvish/rc.elv`
///
/// PowerShell and elvish load the script each time a shell starts. For bash, zsh and fish, install
/// it again after an update, so that it completes the new release's commands.
fn print_completions(ui: &mut UI, shell: Shell) -> Result<()> {
	ui.print(&completion_script(shell)?);
	Ok(())
}

/// The completion script for `shell`, without the line break that ends it. It is static: clap's
/// completers for remote paths need a logged-in client, which only the REPL has.
fn completion_script(shell: Shell) -> Result<String> {
	let mut script = Vec::new();
	clap_complete::generate(
		shell,
		&mut without_hidden(&CliArgs::command()),
		"filen",
		&mut script,
	);
	let mut script = String::from_utf8(script).context("The completion script is not UTF-8")?;
	script.truncate(script.trim_end().len());
	Ok(script)
}

/// `command` without its hidden options and subcommands, recursively, except `-h`/`--help`.
/// clap_complete's static generators complete every option, hidden or not, and clap has no way to
/// take an option out of a command, so this builds a copy with what the generators read: the names,
/// visible aliases and descriptions, the completed options (their conflicts included) and the help
/// and version settings.
/// The copy has no argument groups: the derived ones only list their struct's fields, and a group
/// that names a left-out option would not build.
fn without_hidden(command: &Command) -> Command {
	Command::new(command.get_name().to_owned())
		.visible_aliases(command.get_visible_aliases().map(str::to_owned))
		.about(Resettable::from(command.get_about().cloned()))
		.version(Resettable::from(
			command
				.get_version()
				.map(|version| Str::from(version.to_owned())),
		))
		.disable_help_flag(command.is_disable_help_flag_set())
		.disable_version_flag(command.is_disable_version_flag_set())
		.disable_help_subcommand(command.is_disable_help_subcommand_set())
		.allow_external_subcommands(command.is_allow_external_subcommands_set())
		.args(
			command
				.get_arguments()
				.filter(|arg| is_completed(arg))
				.cloned(),
		)
		.subcommands(
			command
				.get_subcommands()
				.filter(|subcommand| !subcommand.is_hide_set())
				.map(without_hidden),
		)
}

/// Whether a completion script offers `arg`: the shown options, and `-h`/`--help`, which is hidden
/// only so that the help pages do not list it under every command, and works with all of them.
fn is_completed(arg: &Arg) -> bool {
	!arg.is_hide_set() || arg.get_id() == "help"
}

/// `directory` with a leading `~` component replaced by `home`, as a shell expands it. The REPL
/// splits its lines without expanding anything.
fn expand_home(directory: &Path, home: Option<&Path>) -> Result<PathBuf> {
	let Ok(rest) = directory.strip_prefix("~") else {
		return Ok(directory.to_owned());
	};
	let home = home.ok_or_else(|| UI::failure("Could not find your home directory"))?;
	Ok(if rest.as_os_str().is_empty() {
		home.to_owned()
	} else {
		home.join(rest)
	})
}

/// Changes the process's working directory to `directory`. Every local path a command takes, and
/// every local default, starts from it, so `upload`, `download` and `sync` follow it.
fn change_local_dir(directory: &Path) -> Result<()> {
	let directory = expand_home(directory, dirs::home_dir().as_deref())?;
	env::set_current_dir(&directory).map_err(|e| {
		UI::failure(&format!(
			"Failed to change into {}: {e}",
			directory.display()
		))
	})
}

/// Prints the process's working directory, or with `--json`, an object with its path.
fn print_local_dir(ui: &mut UI) -> Result<()> {
	let directory = env::current_dir()
		.map_err(|e| UI::failure(&format!("Failed to read the local working directory: {e}")))?;
	let directory = directory.to_string_lossy();
	if ui.json {
		return ui.print_json(json!({ "path": directory }));
	}
	ui.print(&directory);
	Ok(())
}

/// The names in a local directory, each list sorted.
#[derive(Debug, PartialEq)]
struct LocalListing {
	directories: Vec<String>,
	files: Vec<String>,
}

/// Lists `directory`. An entry that is a link to a directory counts as a directory, and one that
/// cannot be followed as a file.
fn read_local_dir(directory: &Path) -> Result<LocalListing> {
	let cannot_list =
		|e: std::io::Error| UI::failure(&format!("Failed to list {}: {e}", directory.display()));
	let mut listing = LocalListing {
		directories: Vec::new(),
		files: Vec::new(),
	};
	for entry in fs::read_dir(directory).map_err(cannot_list)? {
		let entry = entry.map_err(cannot_list)?;
		let name = entry.file_name().to_string_lossy().into_owned();
		if entry.path().is_dir() {
			listing.directories.push(name);
		} else {
			listing.files.push(name);
		}
	}
	listing.directories.sort();
	listing.files.sort();
	Ok(listing)
}

/// Prints the names in `directory` (default: the working directory), directories first and
/// ending in a `/`, or with `--json`, an object of the two lists as `ls` prints one.
fn list_local_dir(ui: &mut UI, directory: Option<&Path>) -> Result<()> {
	let directory = match directory {
		Some(directory) => expand_home(directory, dirs::home_dir().as_deref())?,
		None => PathBuf::from("."),
	};
	let LocalListing { directories, files } = read_local_dir(&directory)?;
	if ui.json {
		return ui.print_json(json!({
			"directories": directories,
			"files": files,
		}));
	}
	if directories.is_empty() && files.is_empty() {
		ui.print_muted(&format!("{} is empty", directory.display()));
		return Ok(());
	}
	let directories = directories
		.iter()
		.map(|name| style(format!("{name}/")).blue().to_string())
		.collect::<Vec<_>>();
	let items = directories
		.iter()
		.chain(&files)
		.map(String::as_str)
		.collect::<Vec<_>>();
	ui.print_grid(&items);
	Ok(())
}

/// What the CLI runs as, for the log: the target it was built for, its config directory and
/// whether it is the Docker image's.
pub(crate) fn environment_summary(config_dir: &Path) -> String {
	format!(
		"Target: {BUILD_TARGET}, config directory: {}, Docker image: {}",
		config_dir.display(),
		if IN_DOCKER_IMAGE { "yes" } else { "no" }
	)
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::ui::test_support::Printed;

	/// What the CLI prints for whether it is the Docker image's, which the features it is built
	/// with decide.
	fn docker() -> &'static str {
		if IN_DOCKER_IMAGE { "yes" } else { "no" }
	}

	#[test]
	fn version_prints_what_the_cli_runs_as() {
		let printed = Printed::default();

		print_version(&mut printed.ui(), Path::new("/home/user/.config/filen-cli")).unwrap();

		assert_eq!(
			console::strip_ansi_codes(&printed.lines()),
			format!(
				"Filen CLI v{}\n          Target: {BUILD_TARGET}\nConfig directory: \
				 /home/user/.config/filen-cli\n    Docker image: {}",
				env!("CARGO_PKG_VERSION"),
				docker()
			)
		);
	}

	#[test]
	fn version_with_json_prints_an_object() {
		let printed = Printed::default();
		let mut ui = printed.ui();
		ui.json = true;

		print_version(&mut ui, Path::new("/home/user/.config/filen-cli")).unwrap();

		let printed: serde_json::Value = serde_json::from_str(&printed.lines()).unwrap();
		assert_eq!(
			printed,
			json!({
				"version": env!("CARGO_PKG_VERSION"),
				"target": BUILD_TARGET,
				"configDir": "/home/user/.config/filen-cli",
				"docker": IN_DOCKER_IMAGE,
			})
		);
	}

	#[test]
	fn the_bash_completion_script_completes_the_commands() {
		let script = completion_script(Shell::Bash).unwrap();

		for command in ["ls", "upload", "sync", "completions"] {
			assert!(script.contains(command), "{command}");
		}
		assert!(script.contains("complete -F _filen"));
	}

	#[test]
	fn no_completion_script_runs_the_repl_completers() {
		for shell in [
			Shell::Bash,
			Shell::Zsh,
			Shell::Fish,
			Shell::PowerShell,
			Shell::Elvish,
		] {
			let script = completion_script(shell).unwrap();
			assert!(
				!script.contains("UNINITIALIZED_COMPLETER_OUTPUT"),
				"{shell}"
			);
		}
	}

	#[test]
	fn no_completion_script_offers_the_hidden_options() {
		for shell in [
			Shell::Bash,
			Shell::Zsh,
			Shell::Fish,
			Shell::PowerShell,
			Shell::Elvish,
		] {
			let script = completion_script(shell).unwrap();
			for hidden in [
				"export-markdown-docs",
				"make-output-environment-agnostic-for-replay-testing",
			] {
				assert!(!script.contains(hidden), "{shell}: {hidden}");
			}
		}
	}

	#[test]
	fn the_completion_scripts_keep_the_shown_options() {
		for shell in [
			Shell::Bash,
			Shell::Zsh,
			Shell::Fish,
			Shell::PowerShell,
			Shell::Elvish,
		] {
			let script = completion_script(shell).unwrap();
			for shown in [
				"help",
				"json",
				"data-dir",
				"version",
				"no-log-file",
				"completions",
			] {
				assert!(script.contains(shown), "{shell}: {shown}");
			}
		}
	}

	#[test]
	fn completions_prints_the_script() {
		let printed = Printed::default();

		print_completions(&mut printed.ui(), Shell::Fish).unwrap();

		assert_eq!(printed.lines(), completion_script(Shell::Fish).unwrap());
	}

	#[test]
	fn a_leading_tilde_is_the_home_directory() {
		let home = Path::new("/home/user");

		assert_eq!(
			expand_home(Path::new("~"), Some(home)).unwrap(),
			PathBuf::from("/home/user")
		);
		assert_eq!(
			expand_home(Path::new("~/Downloads/x"), Some(home)).unwrap(),
			PathBuf::from("/home/user/Downloads/x")
		);
	}

	#[test]
	fn a_tilde_elsewhere_is_part_of_the_name() {
		let home = Path::new("/home/user");

		for directory in ["~user", "a/~", "./~", "~~"] {
			assert_eq!(
				expand_home(Path::new(directory), Some(home)).unwrap(),
				PathBuf::from(directory)
			);
		}
	}

	#[test]
	fn a_tilde_without_a_home_directory_fails() {
		let err = expand_home(Path::new("~/x"), None).unwrap_err();

		assert_eq!(err.to_string(), "Could not find your home directory");
		assert_eq!(crate::error::exit_code(&err), 1);
	}

	#[test]
	fn changing_into_a_missing_directory_fails_and_names_it() {
		let dir = tempfile::tempdir().unwrap();
		let missing = dir.path().join("missing");

		let err = change_local_dir(&missing).unwrap_err();

		assert!(
			err.to_string()
				.starts_with(&format!("Failed to change into {}: ", missing.display())),
			"{err}"
		);
		assert_eq!(crate::error::exit_code(&err), 1);
	}

	/// A directory holding `b.txt`, `a.txt`, `.hidden`, and the directories `z` and `c`.
	fn local_dir() -> tempfile::TempDir {
		let dir = tempfile::tempdir().unwrap();
		for file in ["b.txt", "a.txt", ".hidden"] {
			fs::write(dir.path().join(file), "x").unwrap();
		}
		for directory in ["z", "c"] {
			fs::create_dir(dir.path().join(directory)).unwrap();
		}
		dir
	}

	#[test]
	fn a_local_directory_is_listed_sorted_with_its_directories_apart() {
		let dir = local_dir();

		assert_eq!(
			read_local_dir(dir.path()).unwrap(),
			LocalListing {
				directories: vec!["c".to_owned(), "z".to_owned()],
				files: vec![".hidden".to_owned(), "a.txt".to_owned(), "b.txt".to_owned()],
			}
		);
	}

	#[test]
	fn lls_prints_directories_first_with_a_slash() {
		let dir = local_dir();
		let printed = Printed::default();
		let mut ui = printed.ui();
		// one name per line
		ui.initialize(false, false, Some(1), false, false);

		list_local_dir(&mut ui, Some(dir.path())).unwrap();

		assert_eq!(
			console::strip_ansi_codes(&printed.lines()),
			"c/\nz/\n.hidden\na.txt\nb.txt"
		);
	}

	#[test]
	fn lls_with_json_prints_the_lists() {
		let dir = local_dir();
		let printed = Printed::default();
		let mut ui = printed.ui();
		ui.json = true;

		list_local_dir(&mut ui, Some(dir.path())).unwrap();

		let printed: serde_json::Value = serde_json::from_str(&printed.lines()).unwrap();
		assert_eq!(
			printed,
			json!({
				"directories": ["c", "z"],
				"files": [".hidden", "a.txt", "b.txt"],
			})
		);
	}

	#[test]
	fn lls_of_an_empty_directory_says_so() {
		let dir = tempfile::tempdir().unwrap();
		let printed = Printed::default();

		list_local_dir(&mut printed.ui(), Some(dir.path())).unwrap();

		assert_eq!(
			console::strip_ansi_codes(&printed.lines()),
			format!("{} is empty", dir.path().display())
		);
	}

	#[test]
	fn lls_of_a_file_fails_and_names_it() {
		let dir = local_dir();
		let file = dir.path().join("a.txt");

		let err = list_local_dir(&mut Printed::default().ui(), Some(&file)).unwrap_err();

		assert!(
			err.to_string()
				.starts_with(&format!("Failed to list {}: ", file.display())),
			"{err}"
		);
	}

	#[test]
	fn local_directory_arguments_complete_as_local_paths() {
		let cli = CliArgs::command();
		for command in ["lcd", "lls"] {
			let directory = cli
				.find_subcommand(command)
				.unwrap()
				.get_arguments()
				.find(|arg| arg.get_id() == "directory")
				.unwrap();
			assert_eq!(directory.get_value_hint(), ValueHint::DirPath, "{command}");
			assert!(
				directory
					.get::<clap_complete::ArgValueCompleter>()
					.is_none(),
				"{command}"
			);
		}
	}

	#[test]
	fn the_environment_summary_names_the_target_and_the_config_directory() {
		assert_eq!(
			environment_summary(Path::new("/home/user/.config/filen-cli")),
			format!(
				"Target: {BUILD_TARGET}, config directory: /home/user/.config/filen-cli, Docker \
				 image: {}",
				docker()
			)
		);
	}
}
