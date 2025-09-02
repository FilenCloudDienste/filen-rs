use anyhow::{Context, Result, anyhow};
use clap::Subcommand;
use filen_sdk_rs::fs::{
	FSObject, HasName as _, HasUUID as _,
	dir::DirectoryType,
	file::{enums::RemoteFileType, traits::HasFileInfo as _},
};

use crate::{CommandResult, auth::LazyClient, prompt_confirm, util::RemotePath};

#[derive(Debug, Subcommand)]
pub(crate) enum Commands {
	/// Change the working directory (in REPL)
	Cd { directory: String },
	/// List files in a directory
	Ls {
		/// Directory to list files in (default: the current working directory)
		directory: Option<String>,
	},
	/// Print the contents of a file
	Cat { file: String },
	/// Print the first lines of a file
	Head {
		file: String,
		/// Number of lines to print (default: 10)
		lines: Option<usize>,
	},
	/// Print the last lines of a file
	Tail {
		file: String,
		/// Number of lines to print (default: 10)
		lines: Option<usize>,
	},
	/// Delete saved credentials and exit
	Logout,
	/// Exit the REPL
	Exit,
}

pub(crate) async fn execute_command(
	client: &mut LazyClient,
	working_path: &RemotePath,
	command: Commands,
) -> Result<CommandResult> {
	let result: Option<CommandResult> = match command {
		Commands::Cd { directory } => {
			let working_path = working_path.navigate(&directory);
			Some(CommandResult {
				working_path: Some(working_path),
				..Default::default()
			})
		}
		Commands::Ls { directory } => {
			list_directory(client, working_path, directory).await?;
			None
		}
		Commands::Cat { file } => {
			print_file(client, working_path, &file, PrintFileLines::Full).await?;
			None
		}
		Commands::Head { file, lines } => {
			print_file(
				client,
				working_path,
				&file,
				PrintFileLines::Head(lines.unwrap_or(10)),
			)
			.await?;
			None
		}
		Commands::Tail { file, lines } => {
			print_file(
				client,
				working_path,
				&file,
				PrintFileLines::Tail(lines.unwrap_or(10)),
			)
			.await?;
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

async fn list_directory(
	client: &mut LazyClient,
	working_path: &RemotePath,
	directory: Option<String>,
) -> Result<()> {
	let directory_str = working_path.navigate(directory.as_deref().unwrap_or("")).0;
	let client = client.get().await?;
	let Some(directory) = client
		.find_item_at_path(&directory_str)
		.await
		.context("Failed to find ls parent directory")?
	else {
		return Err(anyhow!("No such directory: {}", directory_str));
	};
	let directory = match directory {
		FSObject::Dir(dir) => DirectoryType::Dir(dir),
		FSObject::Root(root) => DirectoryType::Root(root),
		FSObject::RootWithMeta(root) => DirectoryType::RootWithMeta(root),
		_ => return Err(anyhow!("Not a directory: {}", directory_str)),
	};
	let items = client
		.list_dir(&directory)
		.await
		.context("Failed to list root directory")?;
	let mut directories = items
		.0
		.iter()
		.map(|f| f.name().unwrap_or_else(|| f.uuid().as_ref()))
		.collect::<Vec<&str>>();
	directories.sort();
	let mut files = items
		.1
		.iter()
		.map(|f| f.name().unwrap_or_else(|| f.uuid().as_ref()))
		.collect::<Vec<&str>>();
	files.sort();
	println!("{}", [directories, files].concat().join("  "));
	Ok(())
}

enum PrintFileLines {
	Full,
	Head(usize),
	Tail(usize),
}
async fn print_file(
	client: &mut LazyClient,
	working_path: &RemotePath,
	file: &str,
	lines: PrintFileLines,
) -> Result<()> {
	let file_str = working_path.navigate(file).0;
	let client = client.get().await?;
	let Some(file) = client
		.find_item_at_path(file)
		.await
		.context("Failed to find cat file")?
	else {
		return Err(anyhow::anyhow!("No such file: {}", file));
	};
	let file = match file {
		FSObject::File(file) => RemoteFileType::File(file),
		FSObject::SharedFile(file) => RemoteFileType::SharedFile(file),
		_ => return Err(anyhow::anyhow!("Not a file: {}", file_str)),
	};
	if file.size() < 1024
		|| prompt_confirm("File is larger than 1KB, do you want to continue?", false)?
	{
		let content = client.download_file(&file).await?;
		let content = String::from_utf8_lossy(&content);
		let content = match lines {
			PrintFileLines::Full => content.to_string(),
			PrintFileLines::Head(n) => content.lines().take(n).collect::<Vec<&str>>().join("\n"),
			PrintFileLines::Tail(n) => content
				.lines()
				.rev()
				.take(n)
				.collect::<Vec<&str>>()
				.join("\n"),
		};
		println!("{}", content);
	}
	Ok(())
}
