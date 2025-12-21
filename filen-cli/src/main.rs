//! [cli-doc] main-usage
//!
//! Welcome to the Filen CLI!
//!
//! Invoke the Filen CLI with no command specified to enter interactive mode (REPL).
//! There, you can specify absolute paths (starting with "/") or relative paths (supports "." and "..").

use std::{fs, path::PathBuf};

use anyhow::{Context, Result};
use clap::Parser;
use ftail::Ftail;
use log::{LevelFilter, info};

use crate::{
	commands::{Commands, execute_command},
	docs::{generate_markdown_docs, print_in_app_docs},
	ui::CustomLogger,
	updater::check_for_updates,
	util::RemotePath,
};

mod auth;
mod commands;
mod docs;
mod ui;
mod updater;
mod util;

#[derive(Debug, Parser)]
#[clap(
	name = "Filen CLI",
	version,
	disable_help_flag = true,
	disable_help_subcommand = true
)]
pub(crate) struct CliArgs {
	/// Print help about a command or topic
	#[arg(short, long, num_args = 0..=1, default_missing_value = "", hide = true)]
	help: Option<String>,

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

	/// Filen account two-factor code (optional, requires --email and --password)
	#[arg(short, long)]
	two_factor_code: Option<String>,

	/// Path to auth config file (exported via `filen export-auth-config`)
	#[arg(long)]
	auth_config_path: Option<String>,

	/// Skip checking for updates
	#[arg(long)]
	skip_update: bool,

	/// Force checking for updates
	#[arg(long)]
	force_update_check: bool,

	/// Format command output as machine-readable JSON (where applicable)
	#[arg(long)]
	json: bool,

	#[command(subcommand)]
	command: Option<Commands>,

	#[arg(long, hide = true)]
	export_markdown_docs: bool,
}

pub(crate) struct CliConfig {
	pub(crate) config_dir: PathBuf,
}

#[tokio::main]
async fn main() {
	// translate errors to non-zero exit code
	match inner_main().await {
		Ok(_) => {}
		Err(_) => std::process::exit(1),
	}
}

async fn inner_main() -> Result<()> {
	let cli_args = CliArgs::parse();

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

	let mut ui = ui::UI::new(cli_args.quiet, cli_args.json, None);

	// --export-markdown-docs
	if cli_args.export_markdown_docs {
		generate_markdown_docs().inspect_err(|e| {
			ui.print_failure_or_error(e);
		})?;
	}

	// --help
	if let Some(help_topic) = cli_args.help {
		if let Err(e) = print_in_app_docs(
			&mut ui,
			if help_topic.is_empty() {
				None
			} else {
				Some(help_topic)
			},
		) {
			ui.print_failure_or_error(&e);
		}
		return Ok(());
	}

	if !cli_args.skip_update {
		check_for_updates(&mut ui, cli_args.force_update_check, &config.config_dir).await?;
	}

	let mut client = auth::LazyClient::new(
		cli_args.email,
		cli_args.password,
		cli_args.two_factor_code,
		cli_args.auth_config_path,
	);

	let mut working_path = RemotePath::new("");

	if let Some(command) = cli_args.command {
		match execute_command(&config, &mut ui, &mut client, &working_path, command).await {
			Ok(_) => Ok(()),
			Err(e) => {
				ui.print_failure_or_error(&e);
				Err(e)
			}
		}
	} else {
		ui.print_banner();
		loop {
			match client.get(&mut ui).await {
				Ok(_) => {}
				Err(e) => {
					ui.print_failure_or_error(&e);
					break;
				}
			}
			// authenticate, so the username is shown in the prompt.
			// this essentially defeats the purpose of LazyClient, but:
			// it does make a difference so non-authenticated commands (e.g. logout) can still be run ..
			// .. without authentication when called directly (no REPL)

			let line = ui.prompt_repl(&working_path.to_string())?;
			let line = line.trim();
			if line.is_empty() {
				continue;
			}
			let mut args = shlex::split(line).context("Invalid quoting")?;
			args.insert(0, String::from("filen"));
			let cli_args = match CliArgs::try_parse_from(args) {
				Ok(cli) => cli,
				Err(e) => {
					ui.print_failure_or_error(&anyhow::anyhow!(e));
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
		Ok(())
	}
}

/// Information returned by a command execution.
#[derive(Default)]
pub(crate) struct CommandResult {
	/// Change the REPL's working path.
	working_path: Option<RemotePath>,
	/// Exit the REPL.
	exit: bool,
}
