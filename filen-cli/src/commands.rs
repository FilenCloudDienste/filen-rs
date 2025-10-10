use anyhow::{Context, Result};
use clap::Subcommand;
use dialoguer::console::style;
use filen_sdk_rs::{
	auth::Client,
	fs::{
		FSObject, HasName as _, HasUUID,
		dir::{DirectoryType, HasContents},
		file::{enums::RemoteFileType, traits::HasFileInfo as _},
	},
};
use filen_types::fs::ParentUuid;

use crate::{
	CliConfig, CommandResult,
	auth::{LazyClient, export_auth_config},
	ui::{self, UI},
	util::RemotePath,
};

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
	/// Show information about a file, a directory or the Filen drive
	Stat {
		/// File or directory to show information about ("/" for the Filen drive)
		file_or_directory: String,
	},
	/// Create a new directory
	Mkdir {
		/// Directory to create
		directory: String,
	},
	/// Remove a file or directory
	Rm {
		/// File or directory to remove
		file_or_directory: String,
		/// Permanently delete the file or directory (default: move to trash)
		#[arg(short, long)]
		permanent: bool,
	},
	/// Move a file or directory
	Mv {
		/// Source file or directory
		source: String,
		/// Destination parent directory
		destination: String,
	},
	/// Copy a file or directory
	Cp {
		/// Source file or directory
		source: String,
		/// Destination parent directory
		destination: String,
	},
	/// Favorite a file or directory
	Favorite {
		/// File or directory to favorite
		file_or_directory: String,
	},
	/// Unfavorite a file or directory
	Unfavorite {
		/// File or directory to unfavorite
		file_or_directory: String,
	},
	/// List trashed items with option to restore or permanently delete them
	ListTrash,
	/// Permanently delete all trashed items
	EmptyTrash,
	/// Export an auth config (to be used with --auth-config-path option)
	ExportAuthConfig,
	/// Mount Filen as a network drive
	Mount { mount_point: Option<String> },
	/// Delete saved credentials and exit
	Logout,
	/// Exit the REPL
	Exit,
}

pub(crate) async fn execute_command(
	config: &CliConfig,
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
		Commands::Mkdir { directory } => {
			create_directory(ui, client, working_path, &directory).await?;
			None
		}
		Commands::Rm {
			file_or_directory,
			permanent,
		} => {
			delete_file_or_directory(ui, client, working_path, &file_or_directory, permanent)
				.await?;
			None
		}
		Commands::Mv {
			source,
			destination,
		} => {
			move_or_copy_file_or_directory(
				ui,
				client,
				working_path,
				MoveOrCopy::Move,
				&source,
				&destination,
			)
			.await?;
			None
		}
		Commands::Cp {
			source,
			destination,
		} => {
			move_or_copy_file_or_directory(
				ui,
				client,
				working_path,
				MoveOrCopy::Copy,
				&source,
				&destination,
			)
			.await?;
			None
		}
		Commands::Favorite { file_or_directory } => {
			set_file_or_directory_favorite(ui, client, working_path, &file_or_directory, true)
				.await?;
			None
		}
		Commands::Unfavorite { file_or_directory } => {
			set_file_or_directory_favorite(ui, client, working_path, &file_or_directory, false)
				.await?;
			None
		}
		Commands::ListTrash => {
			list_trash(ui, client).await?;
			None
		}
		Commands::EmptyTrash => {
			empty_trash(ui, client).await?;
			None
		}
		Commands::ExportAuthConfig => {
			let client = client.get(ui).await?;
			let export_path = std::env::current_dir()
				.context("Failed to get current directory")?
				.join("filen-cli-auth-config");
			export_auth_config(client, &export_path)?;
			ui.print_success(&format!(
				"Exported auth config to {}",
				export_path.display()
			));
			None
		}
		Commands::Mount { mount_point } => {
			let client = client.get(ui).await?;
			let mut network_drive = filen_network_drive::mount_network_drive(
				client,
				&config.config_dir,
				mount_point.as_deref(),
				false,
			)
			.await
			.context("Failed to mount network drive")?;
			ui.print_success("Mounted network drive (press Ctrl+C to unmount and exit)"); // todo: change message, it might not be successful yet
			let code = network_drive
				.process
				.wait()
				.await
				.context("Failed to wait for mount process")?;
			if !code.success() {
				return Err(anyhow::anyhow!(match code.code() {
					Some(c) => format!("Mount process exited with code: {}", c),
					None => "Mount process exited with unknown code".to_string(),
				}));
			}
			None
		}
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
		.context("Failed to find parent directory")?
	else {
		return UI::failure(&format!("No such directory: {}", directory_str));
	};
	let directory = match directory {
		FSObject::Dir(dir) => DirectoryType::Dir(dir),
		FSObject::Root(root) => DirectoryType::Root(root),
		FSObject::RootWithMeta(root) => DirectoryType::RootWithMeta(root),
		_ => return UI::failure(&format!("Not a directory: {}", directory_str)),
	};
	list_directory_by_uuid(ui, client, directory.uuid()).await
	// todo: ls -l flag
}

async fn list_directory_by_uuid(
	ui: &mut UI,
	client: &Client,
	directory: &dyn HasContents,
) -> Result<()> {
	dbg!(directory.uuid_as_parent());
	let items = client
		.list_dir(directory)
		.await
		.context("Failed to list directory")?;
	let mut directories = items
		.0
		.iter()
		.map(|f| f.name().unwrap_or_else(|| f.uuid().as_ref()))
		.collect::<Vec<&str>>();
	directories.sort();
	let mut files = items
		.1
		.iter()
		.map(|f| f.name().unwrap_or_else(|| f.uuid().as_ref()).to_string())
		.collect::<Vec<String>>();
	files.sort();
	// print directory names in blue
	let directories = directories
		.iter()
		.map(|s| style(s).blue().to_string())
		.collect::<Vec<String>>();
	let all_items = directories
		.iter()
		.chain(files.iter())
		.map(|s| s.as_ref())
		.collect::<Vec<&str>>();
	if all_items.is_empty() {
		ui.print_muted("Directory is empty");
		return Ok(());
	}
	ui.print_grid(&all_items);
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
		.find_item_at_path(&file_str)
		.await
		.context("Failed to find cat file")?
	else {
		return UI::failure(&format!("No such file: {}", file_str));
	};
	let file = match file {
		FSObject::File(file) => RemoteFileType::File(file),
		FSObject::SharedFile(file) => RemoteFileType::SharedFile(file),
		_ => return UI::failure(&format!("Not a file: {}", file_str)),
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
		.find_item_at_path(&file_or_directory_str)
		.await
		.context("Failed to find item")?
	else {
		return UI::failure(&format!(
			"No such file or directory: {}",
			file_or_directory_str
		));
	};
	match item {
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
						.map(|d| ui::format_date(&d))
						.unwrap_or("-".to_string()),
				),
				(
					"Created",
					&file
						.created()
						.map(|d| ui::format_date(&d))
						.unwrap_or("-".to_string()),
				),
				("UUID", file.uuid().as_ref()),
			]);
		}
		FSObject::Dir(dir) => {
			ui.print_key_value_table(&[
				("Name", dir.name().unwrap_or_else(|| dir.uuid().as_ref())),
				("Type", "Directory"),
				(
					"Created",
					&dir.created()
						.map(|d| ui::format_date(&d))
						.unwrap_or("-".to_string()),
				),
				("UUID", dir.uuid().as_ref()),
				// todo: aggregate directory size, file count, ...?
			]);
		}
		FSObject::Root(_) | FSObject::RootWithMeta(_) => {
			let user_info = client
				.get_user_info()
				.await
				.context("Failed to get user info")?;
			ui.print_key_value_table(&[
				("Type", "Drive"),
				("Used", &ui::format_size(user_info.storage_used)),
				("Total", &ui::format_size(user_info.max_storage)),
			]);
		}
		FSObject::SharedFile(_) => {
			return UI::failure("Cannot show information for shared file");
		}
	}
	Ok(())
}

async fn create_directory(
	ui: &mut UI,
	client: &mut LazyClient,
	working_path: &RemotePath,
	directory_str: &str,
) -> Result<()> {
	let directory_str = working_path.navigate(directory_str);
	let parent_str = directory_str.navigate("..");
	let client = client.get(ui).await?;
	if parent_str.0 == directory_str.0 {
		return UI::failure("Cannot create root directory");
	}
	let Some(parent) = client
		.find_item_at_path(&parent_str.0)
		.await
		.context("Failed to find parent directory")?
	else {
		return UI::failure(&format!("No such parent directory: {}", parent_str));
	};
	let parent = match parent {
		FSObject::Dir(dir) => DirectoryType::Dir(dir),
		FSObject::Root(root) => DirectoryType::Root(root),
		FSObject::RootWithMeta(root) => DirectoryType::RootWithMeta(root),
		_ => return UI::failure(&format!("Not a directory: {}", parent_str)),
	};
	client
		.create_dir(&parent, directory_str.basename().unwrap().to_string())
		.await
		.context("Failed to create directory")?;
	ui.print_success(&format!("Directory created: {}", directory_str));
	Ok(())
	// todo: recursive flag
}

async fn delete_file_or_directory(
	ui: &mut UI,
	client: &mut LazyClient,
	working_path: &RemotePath,
	file_or_directory_str: &str,
	permanent: bool,
) -> Result<()> {
	let file_or_directory_str = working_path.navigate(file_or_directory_str).0;
	let client = client.get(ui).await?;
	let Some(item) = client
		.find_item_at_path(&file_or_directory_str)
		.await
		.context("Failed to find file or directory")?
	else {
		return UI::failure(&format!(
			"No such file or directory: {}",
			file_or_directory_str
		));
	};
	if permanent
		&& !ui.prompt_confirm(
			&format!("Permanently delete {}?", file_or_directory_str),
			false,
		)? {
		// todo: make formatting of ui.prompt and ui.print_success more consistent
		return Ok(());
	}
	match item {
		FSObject::File(mut file) => {
			if permanent {
				client
					.delete_file_permanently(file.into_owned())
					.await
					.context("Failed to permanently delete file")?;
				ui.print_success(&format!(
					"Permanently deleted file: {}",
					file_or_directory_str
				));
			} else {
				client
					.trash_file(file.to_mut())
					.await
					.context("Failed to trash file")?;
				ui.print_success(&format!("Trashed file: {}", file_or_directory_str));
			}
		}
		FSObject::Dir(mut dir) => {
			if permanent {
				client
					.delete_dir_permanently(dir.into_owned())
					.await
					.context("Failed to permanently delete directory")?;
				ui.print_success(&format!(
					"Permanently deleted directory: {}",
					file_or_directory_str
				));
			} else {
				client
					.trash_dir(dir.to_mut())
					.await
					.context("Failed to trash directory")?;
				ui.print_success(&format!("Trashed directory: {}", file_or_directory_str));
			}
		}
		FSObject::Root(_) | FSObject::RootWithMeta(_) => {
			return UI::failure("Cannot delete root directory");
		}
		FSObject::SharedFile(_) => {
			return UI::failure("Cannot delete shared file");
		}
	} // todo: simplify this match statement?
	Ok(())
}

enum MoveOrCopy {
	Move,
	Copy,
}
async fn move_or_copy_file_or_directory(
	ui: &mut UI,
	client: &mut LazyClient,
	working_path: &RemotePath,
	action: MoveOrCopy,
	source_str: &str,
	destination_str: &str,
) -> Result<()> {
	let source_str = working_path.navigate(source_str);
	let destination_str = working_path.navigate(destination_str);
	let client = client.get(ui).await?;
	let Some(source_file_or_directory) = client
		.find_item_at_path(&source_str.0)
		.await
		.context("Failed to find source file or directory")?
	else {
		return UI::failure(&format!(
			"No such source file or directory: {}",
			source_str.0
		));
	};
	let Some(destination_dir) = client
		.find_item_at_path(&destination_str.0)
		.await
		.context("Failed to find destination directory")?
	else {
		return UI::failure(&format!(
			"No such destination directory: {}",
			destination_str.0
		));
	};
	let destination_dir = match destination_dir {
		FSObject::Dir(dir) => DirectoryType::Dir(dir),
		FSObject::Root(root) => DirectoryType::Root(root),
		FSObject::RootWithMeta(root) => DirectoryType::RootWithMeta(root),
		_ => return UI::failure(&format!("Not a directory: {}", destination_str.0)),
	};
	match action {
		MoveOrCopy::Move => match source_file_or_directory {
			FSObject::File(file) => {
				client
					.move_file(&mut file.into_owned(), &destination_dir)
					.await
					.context("Failed to move file or directory")?;
			}
			FSObject::Dir(dir) => {
				client
					.move_dir(&mut dir.into_owned(), &destination_dir)
					.await
					.context("Failed to move directory")?;
			}
			FSObject::Root(_) | FSObject::RootWithMeta(_) => {
				return UI::failure("Cannot move root directory");
			}
			FSObject::SharedFile(_) => {
				return UI::failure("Cannot move shared file");
			}
		},
		MoveOrCopy::Copy => match source_file_or_directory {
			FSObject::File(_) => {
				todo!("Implement file copy"); // filen-sdk-rs does not support file copy yet
			}
			FSObject::Dir(_) => {
				todo!("Implement directory copy"); // filen-sdk-rs does not support directory copy yet
			}
			FSObject::Root(_) | FSObject::RootWithMeta(_) => {
				return UI::failure("Cannot copy root directory");
			}
			FSObject::SharedFile(_) => {
				return UI::failure("Cannot copy shared file");
			}
		},
	}
	ui.print_success(&format!(
		"{} {} into {}",
		match action {
			MoveOrCopy::Move => "Moved",
			MoveOrCopy::Copy => "Copied",
		},
		source_str.0,
		destination_str.0
	));
	Ok(())
}

async fn set_file_or_directory_favorite(
	ui: &mut UI,
	client: &mut LazyClient,
	working_path: &RemotePath,
	file_or_directory_str: &str,
	favorite: bool,
) -> Result<()> {
	let file_or_directory_str = working_path.navigate(file_or_directory_str).0;
	let client = client.get(ui).await?;
	let Some(file_or_directory) = client
		.find_item_at_path(&file_or_directory_str)
		.await
		.context("Failed to find file or directory")?
	else {
		return UI::failure(&format!(
			"No such file or directory: {}",
			file_or_directory_str
		));
	};
	match file_or_directory {
		FSObject::File(mut file) => {
			client
				.set_favorite(file.to_mut(), favorite)
				.await
				.context("Failed to set file favorite status")?;
			ui.print_success(&format!(
				"{} file: {}",
				if favorite { "Favorited" } else { "Unfavorited" },
				file_or_directory_str
			));
		}
		FSObject::Dir(mut dir) => {
			client
				.set_favorite(dir.to_mut(), favorite)
				.await
				.context("Failed to set directory favorite status")?;
			ui.print_success(&format!(
				"{} directory: {}",
				if favorite { "Favorited" } else { "Unfavorited" },
				file_or_directory_str
			));
		}
		FSObject::Root(_) | FSObject::RootWithMeta(_) => {
			return UI::failure("Cannot change favorite status of root directory");
		}
		FSObject::SharedFile(_) => {
			return UI::failure("Cannot change favorite status of shared file");
		}
	}
	Ok(())
}

async fn list_trash(ui: &mut UI, client: &mut LazyClient) -> Result<()> {
	let client = client.get(ui).await?;
	list_directory_by_uuid(ui, client, &ParentUuid::Trash).await
	// todo: this should work, maybe it is an underlying issue?
}

async fn empty_trash(ui: &mut UI, client: &mut LazyClient) -> Result<()> {
	let client = client.get(ui).await?;
	client.empty_trash().await?;
	ui.print_success("Emptied trash");
	Ok(())
}
