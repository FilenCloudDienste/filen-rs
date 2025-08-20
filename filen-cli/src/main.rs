use anyhow::{Context, Result};
use clap::Parser;
use std::io::Write;

use crate::{
	commands::{Commands, execute_command},
	util::RemotePath,
};

mod auth;
mod commands;
mod util;

#[derive(Debug, Parser)]
pub struct Cli {
	/// Filen account email (requires --password)
	#[arg(short, long)]
	email: Option<String>,

	/// Filen account password (requires --email)
	#[arg(short, long)]
	password: Option<String>,

	#[command(subcommand)]
	command: Option<Commands>,
}

#[tokio::main]
async fn main() -> Result<()> {
	println!("Welcome to Filen CLI!");

	let cli = Cli::parse();
	let mut client = auth::LazyClient::new(cli.email, cli.password);

	let mut working_path = RemotePath::new("");

	if let Some(command) = cli.command {
		execute_command(&mut client, &working_path, &command).await?;
	} else {
		loop {
			let line = prompt(&format!("{} >", working_path))?;
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
			match execute_command(&mut client, &working_path, &cli.command.unwrap()).await {
				Ok(result) => {
					if result.exit {
						break;
					}
					working_path = result.working_path.unwrap_or(working_path);
				}
				Err(e) => {
					eprintln!("Error: {}", e);
				}
			}
		}
	}

	Ok(())
}

/// Information returned by a command execution.
#[derive(Default)]
pub struct CommandResult {
	/// Change the REPL's working path.
	working_path: Option<RemotePath>,
	/// Exit the REPL.
	exit: bool,
}

// util

fn prompt(msg: &str) -> Result<String> {
	write!(std::io::stdout(), "{} ", msg.trim()).context("Failed to write message for prompt")?;
	std::io::stdout()
		.flush()
		.context("Failed to write message for prompt")?;
	let mut buffer = String::new();
	std::io::stdin()
		.read_line(&mut buffer)
		.context("Failed to read line for prompt")?;
	Ok(buffer)
}

fn prompt_confirm(msg: &str, default: bool) -> Result<bool> {
	let y_n_str = if default { "Y/n" } else { "y/N" };
	let response = prompt(&format!("{} [{}] ", msg, y_n_str))?;
	if response.trim().is_empty() {
		return Ok(default);
	}
	Ok(response.trim().eq_ignore_ascii_case("y"))
}
