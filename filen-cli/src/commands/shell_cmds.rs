//! Commands about the CLI itself rather than the drive.

use std::path::Path;

use anyhow::{Context as _, Result};
use clap::{
	Arg, Command, CommandFactory as _, Subcommand,
	builder::{Resettable, Str},
};
use clap_complete::Shell;
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
}

/// The commands listed in the docs' usage section.
pub(crate) const DOCS: &[DocElement] = &[
	DocElement::CommandHelp("version"),
	DocElement::CommandHelp("install"),
	DocElement::CommandHelp("completions"),
	DocElement::DocFragment("shell-completions"),
];

impl ShellCommands {
	/// Whether the update check when the CLI starts is skipped for this command: it works on the
	/// CLI itself and needs no other release announced.
	pub(crate) fn skips_update_check(&self) -> bool {
		match self {
			Self::Version | Self::Install { .. } | Self::Completions { .. } => true,
		}
	}

	pub(super) async fn execute(self, ctx: CommandContext<'_>) -> Result<CommandResult> {
		let CommandContext { config, ui, .. } = ctx;
		match self {
			Self::Version => print_version(ui, &config.config_dir)?,
			Self::Install { release } => updater::install(ui, &release).await?,
			Self::Completions { shell } => print_completions(ui, shell)?,
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
