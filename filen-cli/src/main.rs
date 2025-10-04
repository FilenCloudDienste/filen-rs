use std::{fs, path::PathBuf};

use anyhow::{Context, Result};
use clap::Parser;
use ftail::Ftail;
use log::{LevelFilter, info};

use crate::{
	commands::{Commands, execute_command},
	ui::CustomLogger,
	util::RemotePath,
};

mod auth;
mod commands;
mod ui;
mod util;

#[derive(Debug, Parser)]
#[clap(name = "Filen CLI", version)]
pub(crate) struct CliArgs {
	/// Increase verbosity (-v, -vv, -vvv)
	#[arg(short, long, action = clap::ArgAction::Count)]
	verbose: u8,

	/// Hide progress bars and other non-essential output (overrides -v)
	#[arg(short, long)]
	quiet: bool,

	/// Config directory
	#[arg(long)]
	config_dir: Option<PathBuf>,

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

pub(crate) struct CliConfig {
	pub(crate) config_dir: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
	let cli_args = CliArgs::parse();
	// todo: add colors and styling to clap help texts

	let is_dev = cfg!(debug_assertions);
	let config = CliConfig {
		config_dir: match cli_args.config_dir {
			Some(ref dir) => {
				if !dir.exists() {
					return Err(anyhow::anyhow!("Config dir does not exist"));
				}
				dir.clone()
			}
			None => {
				let dir = dirs::config_dir()
					.context("Failed to get config dir")?
					.join(match is_dev {
						true => "filen-cli-dev",
						false => "filen-cli",
					});
				fs::create_dir_all(&dir).context("Failed to create config dir")?;
				dir
			}
		},
	};

	// setup logging
	fs::create_dir_all(config.config_dir.join("logs")).context("Failed to create logs dir")?;
	let logging_level = if cli_args.quiet {
		LevelFilter::Off
	} else {
		match cli_args.verbose {
			0 => LevelFilter::Off,
			1 => LevelFilter::Info,
			2 => LevelFilter::Debug,
			_ => LevelFilter::Trace,
		}
	};
	let log_file = config.config_dir.join("logs").join("latest.log");
	Ftail::new()
		.custom(
			|config| Box::new(CustomLogger { config }) as Box<dyn log::Log + Send + Sync>,
			logging_level,
		)
		.single_file(&log_file, false, LevelFilter::Debug)
		.daily_file(&config.config_dir.join("logs"), LevelFilter::Debug)
		.max_file_size(10 * 1024 * 1024) // 10 MB
		.retention_days(3)
		.init()
		.context("Failed to initialize logger")?;
	info!("Logging level: {}", logging_level);
	info!("Full log file: {}", log_file.display());

	let mut ui = ui::UI::new(cli_args.quiet);

	let mut client =
		auth::LazyClient::new(cli_args.email, cli_args.password, cli_args.auth_config_path);

	let mut working_path = RemotePath::new("");

	if let Some(command) = cli_args.command {
		if let Err(e) = execute_command(&config, &mut ui, &mut client, &working_path, command).await
		{
			ui.print_failure_or_error(&e);
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
			let cli_args = match CliArgs::try_parse_from(args).map_err(|e| e.to_string()) {
				Ok(cli) => cli,
				Err(e) => {
					eprintln!("{}", e);
					continue;
				}
			};
			if cli_args.command.is_none() {
				continue;
			}
			match execute_command(
				&config,
				&mut ui,
				&mut client,
				&working_path,
				cli_args.command.unwrap(),
			)
			.await
			{
				Ok(result) => {
					if result.exit {
						break;
					}
					working_path = result.working_path.unwrap_or(working_path);
				}
				Err(e) => {
					ui.print_failure_or_error(&e);
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
