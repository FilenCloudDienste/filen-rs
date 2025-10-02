use anyhow::{Context, Result};
use clap::Parser;
use log::error;

use crate::{
	commands::{Commands, execute_command},
	util::RemotePath,
};

mod auth;
mod commands;
mod ui;
mod util;

#[derive(Debug, Parser)]
#[clap(name = "Filen CLI", version)]
pub(crate) struct Cli {
	/// Filen account email (requires --password)
	#[arg(short, long)]
	email: Option<String>,

	/// Filen account password (requires --email)
	#[arg(short, long)]
	password: Option<String>,

	/// Path to auth config file
	#[arg(long)]
	auth_config_path: Option<String>,

	#[command(subcommand)]
	command: Option<Commands>,
}

#[tokio::main]
async fn main() -> Result<()> {
	env_logger::init();

	let mut ui = ui::UI::new();

	let cli = Cli::parse();
	let mut client = auth::LazyClient::new(cli.email, cli.password, cli.auth_config_path);

	let mut working_path = RemotePath::new("");

	if let Some(command) = cli.command {
		if let Err(e) = execute_command(&mut ui, &mut client, &working_path, command).await {
			error!("{}", e);
			ui.print_failure(&format!("An error occurred: {}. If you believe this is a bug, please report it at https://github.com/FilenCloudDienste/filen-rs/issues", e));
		}
	} else {
		ui.print_banner();
		loop {
			match client.get(&mut ui).await {
				Ok(_) => {}
				Err(e) => {
					ui.print_err(&e);
					break;
				}
			}
			// authenticate, so the username is shown in the prompt.
			// this essentially defeats the purpose of LazyClient, so maybe scrapping it would be better?
			// it does make a difference so non-authenticated commands (e.g. logout) can still be run ..
			// .. without authentication when called directly (no REPL)
			// todo: improve LazyClient?

			let line = ui.prompt_repl(&working_path.to_string())?;
			let line = line.trim();
			if line.is_empty() {
				continue;
			}
			let mut args = shlex::split(line).context("Invalid quoting")?;
			args.insert(0, String::from("filen"));
			let cli = match Cli::try_parse_from(args).map_err(|e| e.to_string()) {
				Ok(cli) => cli,
				Err(e) => {
					eprintln!("{}", e);
					continue;
				}
			};
			if cli.command.is_none() {
				continue;
			}
			match execute_command(&mut ui, &mut client, &working_path, cli.command.unwrap()).await {
				Ok(result) => {
					if result.exit {
						break;
					}
					working_path = result.working_path.unwrap_or(working_path);
				}
				Err(e) => {
					error!("{}", e);
					ui.print_failure(&format!("An error occurred: {}. If you believe this is a bug, please report it at https://github.com/FilenCloudDienste/filen-rs/issues", e));
				}
			}
		}
	}

	Ok(())
}

/// Information returned by a command execution.
#[derive(Default)]
pub(crate) struct CommandResult {
	/// Change the REPL's working path.
	working_path: Option<RemotePath>,
	/// Exit the REPL.
	exit: bool,
}
