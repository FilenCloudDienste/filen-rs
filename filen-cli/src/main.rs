use std::{fs, path::PathBuf};

use anyhow::{Context, Result};
use clap::Parser;
use clap_verbosity_flag::{OffLevel, Verbosity};
use ftail::Ftail;
use log::{LevelFilter, error, info};

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
pub(crate) struct Cli {
	#[command(flatten)]
	verbose: Verbosity<OffLevel>, // todo: remove default help text for these options (--quiet cannot even be used)

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

#[tokio::main]
async fn main() -> Result<()> {
	let is_dev = cfg!(debug_assertions);

	let cli = Cli::parse();
	// todo: add colors and styling to clap help texts

	// determine config dir
	let config_dir = match cli.config_dir {
		Some(dir) => {
			if !dir.exists() {
				return Err(anyhow::anyhow!("Config dir does not exist"));
			}
			dir
		}
		None => {
			let dir = dirs::config_dir()
				.ok_or(anyhow::anyhow!("Failed to get config dir"))?
				.join(match is_dev {
					true => "filen-cli-dev",
					false => "filen-cli",
				});
			fs::create_dir_all(&dir).context("Failed to create config dir")?;
			dir
		}
	};

	fs::create_dir_all(config_dir.join("logs")).context("Failed to create logs dir")?;
	let logging_level = match cli.verbose.log_level_filter() {
		LevelFilter::Off => LevelFilter::Off,
		filter => filter.increment_severity().increment_severity(),
		// default logging level is off, -v = info, -vv = debug, -vvv = trace (error and warn are skipped)
	};
	Ftail::new()
		.custom(
			|config| Box::new(CustomLogger { config }) as Box<dyn log::Log + Send + Sync>,
			logging_level,
		)
		.single_file(
			&config_dir.join("logs").join("latest.log"),
			false,
			LevelFilter::Debug,
		)
		.daily_file(&config_dir.join("logs"), LevelFilter::Debug)
		.max_file_size(10 * 1024 * 1024) // 10 MB
		.retention_days(3)
		.init()
		.context("Failed to initialize logger")?;
	info!("Logging level: {}", logging_level);

	let mut ui = ui::UI::new();

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
					// todo: better error handling, e. g. "no such directory bla" should not be formatted with bug report link
					// there should be a user-facing error type or something that's stil an error
					// we can't only print the error via ui.print_failure() because we want non-zero exit codes
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
