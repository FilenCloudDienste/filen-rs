use anyhow::{Context, Result, anyhow};
use clap::Subcommand;
use filen_sdk_rs::fs::{FSObject, HasName as _, HasUUID, file::traits::File};

use crate::{CommandResult, auth::LazyClient, prompt_confirm, util::RemotePath};

#[derive(Debug, Subcommand)]
pub(crate) enum Commands {
	/// Change the working directory (in REPL)
	Cd { directory: String },
	/// List files in a directory
	Ls {
		/// Directory to list files in, defaults to the current working directory.
		directory: Option<String>,
	},
	/// Print the contents of a file
	Cat { file: String },
	/// Delete saved credentials and exit
	Logout,
	/// Exit the REPL
	Exit,
}

pub(crate) async fn execute_command(
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
				return Err(anyhow!("No such directory: {}", directory_str));
			};
			let directory = match directory {
				FSObject::Dir(dir) => dir.uuid,
				FSObject::Root(root) => *root.uuid(),
				FSObject::RootWithMeta(root) => *root.uuid(),
				_ => return Err(anyhow!("Not a directory: {}", directory_str)),
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
		Commands::Cat { file } => {
			let file_str = working_path.navigate(file).0;
			let client = client.get().await?;
			let Some(file) = client
				.find_item_at_path(file_str.clone())
				.await
				.context("Failed to find cat file")?
			else {
				return Err(anyhow::anyhow!("No such file: {}", file_str));
			};
			let file: Box<dyn File> = match file {
				FSObject::File(file) => Box::new(file.into_owned()),
				FSObject::SharedFile(file) => Box::new(file.into_owned()),
				_ => return Err(anyhow::anyhow!("Not a file: {}", file_str)),
			};
			if file.size() < 1024
				|| prompt_confirm("File is larger than 1KB, do you want to continue?", false)?
			{
				let content = client.download_file(file.as_ref()).await?;
				let content = String::from_utf8_lossy(&content);
				println!("{}", content);
			}
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
