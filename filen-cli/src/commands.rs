use anyhow::{Context, Result};
use clap::Subcommand;
use filen_sdk_rs::fs::{HasName as _, HasUUID};

use crate::{CommandResult, auth::LazyClient, util::RemotePath};

#[derive(Debug, Subcommand)]
pub enum Commands {
	/// Change the working directory (in REPL)
	Cd { directory: String },
	/// List files in a directory
	Ls {
		/// Directory to list files in, defaults to the current working directory.
		directory: Option<String>,
	},
	/// Delete saved credentials and exit
	Logout,
	/// Exit the REPL
	Exit,
}

pub async fn execute_command(
	client: &mut LazyClient,
	working_path: &RemotePath,
	command: &Commands,
) -> Result<CommandResult> {
	let result: Option<CommandResult> = match command {
		Commands::Cd { directory } => {
			let working_path = working_path.navigate(directory);
			Some(CommandResult {
				working_path: Some(working_path),
				..Default::default()
			})
		}
		Commands::Ls { directory } => {
			let directory_str = directory
				.as_ref()
				.map(|d| working_path.navigate(d))
				.unwrap_or(working_path.clone())
				.0;
			let client = client.get().await?;
			let Some(directory) = client
				.find_item_at_path(directory_str.clone())
				.await
				.context("Failed to find ls parent directory")?
			else {
				anyhow::bail!("No such directory: {}", directory_str);
			};
			let directory = match directory {
				filen_sdk_rs::fs::FSObject::Dir(dir) => dir.uuid,
				filen_sdk_rs::fs::FSObject::Root(root) => root.uuid(),
				filen_sdk_rs::fs::FSObject::RootWithMeta(root) => root.uuid(),
				_ => anyhow::bail!("Not a directory: {}", directory_str),
			};
			let items = client
				.list_dir(&directory)
				.await
				.expect("Failed to list root directory");
			let mut directories = items
				.0
				.iter()
				.map(|f| f.name().expect("Failed to get directory name"))
				.collect::<Vec<&str>>();
			directories.sort();
			let mut files = items
				.1
				.iter()
				.map(|f| f.name().expect("Failed to get file name"))
				.collect::<Vec<&str>>();
			files.sort();
			println!("{}", [directories, files].concat().join("  "));
			None
		}
		Commands::Logout => {
			let deleted = crate::auth::delete_credentials()?;
			if deleted {
				println!("Credentials deleted.");
			} else {
				println!("No credentials found.");
			}
			Some(CommandResult {
				exit: true,
				..Default::default()
			})
		}
		Commands::Exit => Some(CommandResult {
			exit: true,
			..Default::default()
		}),
	};
	Ok(result.unwrap_or_default())
}
