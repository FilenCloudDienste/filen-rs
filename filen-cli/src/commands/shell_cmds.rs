//! Commands about the CLI itself rather than the drive.

use std::path::Path;

use anyhow::Result;
use clap::Subcommand;
use serde_json::json;

use crate::{
	CommandResult,
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
}

/// The commands listed in the docs' usage section.
pub(crate) const DOCS: &[DocElement] = &[
	DocElement::CommandHelp("version"),
	DocElement::CommandHelp("install"),
];

impl ShellCommands {
	/// Whether the update check when the CLI starts is skipped for this command: it works on the
	/// CLI itself and needs no other release announced.
	pub(crate) fn skips_update_check(&self) -> bool {
		match self {
			Self::Version | Self::Install { .. } => true,
		}
	}

	pub(super) async fn execute(self, ctx: CommandContext<'_>) -> Result<CommandResult> {
		let CommandContext { config, ui, .. } = ctx;
		match self {
			Self::Version => print_version(ui, &config.config_dir)?,
			Self::Install { release } => updater::install(ui, &release).await?,
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
