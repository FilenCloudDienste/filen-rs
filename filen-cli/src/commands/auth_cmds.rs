use std::path::PathBuf;

use anyhow::Result;
use clap::Subcommand;
use serde_json::json;

use crate::{
	CommandResult,
	auth::{self, LogoutOutcome},
	commands::CommandContext,
	docs::DocElement,
	ui::UI,
};

#[derive(Debug, Subcommand)]
pub(crate) enum AuthCommands {
	/// Print the email of the account you are logged in as
	Whoami,
	/// Export an auth config (to be used with --auth-config-path option), after confirming the
	/// risk (--yes answers)
	ExportAuthConfig {
		/// The file to write, or a directory to write filen-cli-auth-config.txt into
		/// (defaults to the current working directory)
		path: Option<PathBuf>,
	},
	/// Delete saved credentials, including the --auth-config-path file, and exit, asking before
	/// each (--yes deletes them all); then offer to delete the local cache
	Logout,
	/// Enable or disable two-factor authentication
	#[command(name = "2fa")]
	TwoFactor {
		#[command(subcommand)]
		command: TwoFactorCommand,
	},
	/// Change your password, and update the kept login (asks for the current and the new password)
	ChangePassword,
	/// Request to change your email, which Filen asks you to confirm by email (asks for your
	/// password)
	ChangeEmail {
		/// The new email
		new_email: String,
	},
	/// Print your master keys, which decrypt all your files, after confirming it (--yes answers)
	ExportMasterKeys,
	/// List the profiles (see --profile), marking the one in use
	Profiles,
}

#[derive(Debug, Subcommand)]
pub(crate) enum TwoFactorCommand {
	/// Enable two-factor authentication: print the secret for your authenticator app, ask for a
	/// code from it and print the recovery key (asks to confirm, which --yes answers)
	Enable {
		/// A current code from your authenticator app, instead of asking for it
		#[arg(long)]
		code: Option<String>,
	},
	/// Disable two-factor authentication (asks to confirm, which --yes answers)
	Disable {
		/// A current code or the recovery key, instead of asking for it
		#[arg(long)]
		code: Option<String>,
	},
}

/// The authentication section of the docs.
pub(crate) const DOCS: &[DocElement] = &[
	DocElement::DocFragment("auth-methods"),
	DocElement::CommandHelp("whoami"),
	DocElement::CommandHelp("export-auth-config"),
	DocElement::CommandHelp("logout"),
	DocElement::DocFragment("account-credentials"),
	DocElement::CommandHelp("2fa"),
	DocElement::CommandHelp("change-password"),
	DocElement::CommandHelp("change-email"),
	DocElement::CommandHelp("export-master-keys"),
	DocElement::DocFragment("profile-option"),
	DocElement::CommandHelp("profiles"),
];

impl AuthCommands {
	/// Whether this command uses the drive: see [`super::Commands::uses_drive`]. `logout` and
	/// `profiles` work on the local credentials and config only.
	pub(super) fn uses_drive(&self) -> bool {
		match self {
			Self::Logout | Self::Profiles => false,
			Self::Whoami
			| Self::ExportAuthConfig { .. }
			| Self::TwoFactor { .. }
			| Self::ChangePassword
			| Self::ChangeEmail { .. }
			| Self::ExportMasterKeys => true,
		}
	}

	pub(super) async fn execute(self, ctx: CommandContext<'_>) -> Result<CommandResult> {
		let CommandContext {
			config, ui, client, ..
		} = ctx;
		match self {
			Self::Whoami => {
				let client = client.get(ui).await?;
				print_whoami(ui, client.email())?;
			}
			Self::ExportAuthConfig { path } => {
				if let Some(export_path) =
					auth::export_auth_config_command(client, ui, path.as_deref()).await?
				{
					ui.print_success(&format!(
						"Exported auth config to {}",
						export_path.display()
					));
				}
			}
			Self::Logout => {
				// stay in the REPL when every deletion was declined
				if auth::logout(config, ui, client)? == LogoutOutcome::Deleted {
					return Ok(CommandResult {
						exit: true,
						..Default::default()
					});
				}
			}
			Self::TwoFactor { command } => match command {
				TwoFactorCommand::Enable { code } => {
					auth::enable_two_factor(ui, client, code.as_deref()).await?;
				}
				TwoFactorCommand::Disable { code } => {
					auth::disable_two_factor(ui, client, code.as_deref()).await?;
				}
			},
			Self::ChangePassword => auth::change_password(config, ui, client).await?,
			Self::ChangeEmail { new_email } => auth::change_email(ui, client, &new_email).await?,
			Self::ExportMasterKeys => auth::export_master_keys(ui, client).await?,
			Self::Profiles => auth::print_profiles(ui, &config.profiles)?,
		}
		Ok(CommandResult::default())
	}
}

/// Prints `email`, the account logged in, or `{"email": ...}` with `--json`.
fn print_whoami(ui: &mut UI, email: &str) -> Result<()> {
	if ui.json {
		ui.print_json(json!({ "email": email }))
	} else {
		ui.print(email);
		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::ui::test_support::Printed;

	#[test]
	fn whoami_prints_the_email() {
		let printed = Printed::default();
		print_whoami(&mut printed.ui(), "user@example.com").unwrap();
		assert_eq!(printed.lines(), "user@example.com");
	}

	#[test]
	fn whoami_with_json_prints_an_object_with_the_email() {
		let printed = Printed::default();
		let mut ui = printed.ui();
		ui.set_quiet_and_json(false, true);
		print_whoami(&mut ui, "user@example.com").unwrap();
		assert_eq!(printed.lines(), "{\n  \"email\": \"user@example.com\"\n}");
	}
}
