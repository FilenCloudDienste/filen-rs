use anyhow::{Context, Result, anyhow};
use clap::Subcommand;
use filen_sdk_rs::fs::{
	FSObject, HasName as _, HasUUID as _,
	dir::{DirectoryType, traits::HasDirMeta},
	file::{enums::RemoteFileType, traits::HasFileInfo as _},
};

use crate::{CommandResult, auth::LazyClient, ui::UI, util::RemotePath};

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
	/// Show information about a file or directory
	Stat {
		/// File or directory to show information about
		file_or_directory: String,
	},
	/// Show information about the file system
	Statfs,
	/// Create a new directory
	Mkdir {
		/// Directory to create
		directory: String,
	},
	/// Remove a file or directory
	Rm {
		/// File or directory to remove
		file_or_directory: String,
	},
	/// Move a file or directory
	Mv {
		/// Source file or directory
		source: String,
		/// Destination file or directory
		destination: String,
	},
	/// Copy a file or directory
	Cp {
		/// Source file or directory
		source: String,
		/// Destination file or directory
		destination: String,
	},
	/// Delete saved credentials and exit
	Logout,
	/// Exit the REPL
	Exit,
}

pub(crate) async fn execute_command(
	ui: &mut UI,
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
			list_directory(ui, client, working_path, directory).await?;
			None
		}
		Commands::Cat { file } => {
			print_file(ui, client, working_path, &file, PrintFileLines::Full).await?;
			None
		}
		Commands::Head { file, lines } => {
			print_file(
				ui,
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
				ui,
				client,
				working_path,
				&file,
				PrintFileLines::Tail(lines.unwrap_or(10)),
			)
			.await?;
			None
		}
		Commands::Stat { file_or_directory } => {
			print_file_or_directory_info(ui, client, working_path, &file_or_directory).await?;
			None
		}
		Commands::Statfs => todo!(),
		Commands::Mkdir { directory: _ } => todo!(),
		Commands::Rm {
			file_or_directory: _,
		} => todo!(),
		Commands::Mv {
			source: _,
			destination: _,
		} => todo!(),
		Commands::Cp {
			source: _,
			destination: _,
		} => todo!(),
		Commands::Logout => {
			let deleted = crate::auth::delete_credentials()?;
			if deleted {
				ui.print_success("Credentials deleted");
			} else {
				ui.print_failure("No credentials found");
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
	ui: &mut UI,
	client: &mut LazyClient,
	working_path: &RemotePath,
	directory: Option<String>,
) -> Result<()> {
	let directory_str = working_path.navigate(directory.as_deref().unwrap_or("")).0;
	let client = client.get(ui).await?;
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
	ui: &mut UI,
	client: &mut LazyClient,
	working_path: &RemotePath,
	file_str: &str,
	lines: PrintFileLines,
) -> Result<()> {
	let file_str = working_path.navigate(file_str).0;
	let client = client.get(ui).await?;
	let Some(file) = client
		.find_item_at_path(file_str.clone())
		.await
		.context("Failed to find cat file")?
	else {
		return Err(anyhow::anyhow!("No such file: {}", file_str));
	};
	let file = match file {
		FSObject::File(file) => RemoteFileType::File(file),
		FSObject::SharedFile(file) => RemoteFileType::SharedFile(file),
		_ => return Err(anyhow::anyhow!("Not a file: {}", file_str)),
	};
	if file.size() < 1024
		|| ui.prompt_confirm("File is larger than 1KB, do you want to continue?", false)?
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

async fn print_file_or_directory_info(
	ui: &mut UI,
	client: &mut LazyClient,
	working_path: &RemotePath,
	file_or_directory_str: &str,
) -> Result<()> {
	let file_or_directory_str = working_path.navigate(file_or_directory_str).0;
	let client = client.get(ui).await?;
	let Some(item) = client
		.find_item_at_path(file_or_directory_str.clone())
		.await
		.context("Failed to find item")?
	else {
		return Err(anyhow::anyhow!(
			"No such file or directory: {}",
			file_or_directory_str
		));
	};
	match item {
		// todo: better date formatting?
		FSObject::File(file) => {
			ui.print_key_value_table(&[
				("Name", file.name().unwrap_or_else(|| file.uuid().as_ref())),
				("Type", "File"),
				(
					"Size",
					&humansize::format_size(file.size(), humansize::BINARY),
				),
				(
					"Modified",
					&file
						.last_modified()
						.map(|d| d.to_string())
						.unwrap_or("-".to_string()),
				),
				(
					"Created",
					&file
						.created()
						.map(|d| d.to_string())
						.unwrap_or("-".to_string()),
				),
			]);
		}
		FSObject::Dir(dir) => {
			ui.print_key_value_table(&[
				("Name", dir.name().unwrap_or_else(|| dir.uuid().as_ref())),
				("Type", "Directory"),
				(
					"Created",
					&dir.created()
						.map(|d| d.to_string())
						.unwrap_or("-".to_string()),
				),
				// todo: aggregate directory size, file count, ...?
			]);
		}
		FSObject::Root(_) => {
			ui.print_key_value_table(&[
				("Type", "Root"),
				// todo: print root info
			]);
		}
		FSObject::RootWithMeta(root) => {
			let dir = root.get_meta();
			ui.print_key_value_table(&[
				("Name", dir.name().unwrap_or("(root)")),
				("Type", "Root"),
				(
					"Created",
					&dir.created()
						.map(|d| d.to_string())
						.unwrap_or("-".to_string()),
				),
			]);
		}
		FSObject::SharedFile(_) => {
			ui.print_failure("Cannot show information for shared file");
		}
	}
	Ok(())
}
