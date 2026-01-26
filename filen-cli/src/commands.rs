use std::borrow::Cow;

use anyhow::{Context, Result};
use clap::Subcommand;
use dialoguer::console::style;
use filen_rclone_wrapper::serve::BasicServerOptions;
use filen_sdk_rs::{
	auth::Client,
	fs::{
		FSObject, HasName as _, HasUUID,
		dir::{DirectoryType, HasContents},
		file::{enums::RemoteFileType, traits::HasFileInfo as _},
	},
	io::RemoteDirectory,
};
use filen_types::fs::ParentUuid;
use serde_json::json;

use crate::{
	CliConfig, CommandResult,
	auth::{self, LazyClient, export_auth_config},
	docs::print_in_app_docs,
	ui::{self, UI},
	util::RemotePath,
};

#[derive(Debug, Subcommand)]
pub(crate) enum Commands {
	/// Print help about a command or topic (default: general help)
	Help {
		/// Command or topic to show help about
		command_or_topic: Option<String>,
	},
	/// Change the working directory (in REPL)
	Cd {
		/// Directory to navigate into (supports "..")
		directory: String,
	},
	/// List files in a directory
	Ls {
		/// Directory to list files in (default: the current working directory)
		directory: Option<String>,
	},
	/// Print the contents of a file
	Cat {
		/// File to print
		file: String,
	},
	/// Print the first lines of a file
	Head {
		/// File to print
		file: String,
		/// Number of lines to print
		#[arg(short = 'n', long, default_value_t = 10)]
		lines: usize,
	},
	/// Print the last lines of a file
	Tail {
		/// File to print
		file: String,
		/// Number of lines to print
		#[arg(short = 'n', long, default_value_t = 10)]
		lines: usize,
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
		/// Recursively create parent directories
		#[arg(short, long)]
		recursive: bool,
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
	/// Execute an Rclone command using filen-rclone
	Rclone {
		/// The command to execute. Your Filen drive is available as the "filen" remote.
		#[arg(trailing_var_arg = true, allow_hyphen_values = true)]
		cmd: Vec<String>,
	},
	/// Mount Filen as a network drive
	Mount {
		/// Where to mount the network drive (default: system default)
		mount_point: Option<String>,
	},
	/// Runs a WebDAV, FTP, SFTP or HTTP server exposing your Filen drive
	Serve {
		/// The type of server to run: webdav, ftp, sftp, http
		server: String,
		/// IP and port for the server (`<ip>:<port>` or `:<port>`)
		#[arg(long = "addr", default_value = ":8080")]
		address: String,
		/// Directory that the server exposes (default: the entire Filen drive)
		#[arg(long)]
		root: Option<String>,
		/// Username for authentication to the server (default: no authentication).
		/// On S3 servers, this is the Access Key ID.
		#[arg(long)]
		user: Option<String>,
		/// Password for authentication to the server (default: no authentication).
		/// On S3 servers, this is the Secret Access Key.
		#[arg(long)]
		password: Option<String>,
		/// The server is read-only
		#[arg(long)]
		read_only: bool,
	},
	// todo: s3 server
	/// Exports your user API key (for use with non-managed Rclone)
	ExportApiKey,
	/// Delete saved credentials and exit
	Logout,
	/// Exit the REPL
	Exit,
}
// (!) every command needs to be mentioned in the docs outline

pub(crate) async fn execute_command(
	config: &CliConfig,
	ui: &mut UI,
	client: &mut LazyClient,
	working_path: &RemotePath,
	command: Commands,
) -> Result<CommandResult> {
	let result: Option<CommandResult> = match command {
		Commands::Help { command_or_topic } => {
			print_in_app_docs(ui, command_or_topic)?;
			None
		}
		Commands::Cd { directory } => {
			let working_path = cd(ui, client, working_path, &directory).await?;
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
			print_file(ui, client, working_path, &file, PrintFileLines::Head(lines)).await?;
			None
		}
		Commands::Tail { file, lines } => {
			print_file(ui, client, working_path, &file, PrintFileLines::Tail(lines)).await?;
			None
		}
		Commands::Stat { file_or_directory } => {
			print_file_or_directory_info(ui, client, working_path, &file_or_directory).await?;
			None
		}
		Commands::Mkdir {
			directory,
			recursive,
		} => {
			create_directory(ui, client, working_path, &directory, recursive).await?;
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
			let export_path = export_auth_config(
				client,
				&std::env::current_dir().context("Failed to get current working directory")?,
			)?;
			ui.print_success(&format!(
				"Exported auth config to {}",
				export_path.display()
			));
			None
		}
		Commands::Rclone { cmd } => {
			rclone::execute_rclone(config, ui, client, cmd).await?;
			None
		}
		Commands::Mount { mount_point } => {
			rclone::mount(config, ui, client, mount_point).await?;
			None
		}
		Commands::Serve {
			server,
			address,
			root,
			user,
			password,
			read_only,
		} => {
			let display_server_type = match server.as_str() {
				"webdav" => "WebDAV",
				"ftp" => "FTP",
				"sftp" => "SFTP",
				"http" => "HTTP",
				"s3" => "S3",
				_ => {
					return Err(UI::failure(&format!(
						"Unsupported server type: {}. Supported types are: webdav, ftp, sftp, http, s3",
						server
					)));
				}
			};
			rclone::start_server(
				config,
				ui,
				client,
				&server,
				display_server_type,
				BasicServerOptions {
					address,
					root,
					user,
					password,
					read_only,
				},
			)
			.await?;
			None
		}
		Commands::ExportApiKey => {
			let client = client.get(ui).await?.to_stringified();
			if ui.json {
				ui.print_json(json!({
					"email": client.email,
					"apiKey": client.api_key,
				}))?;
			} else {
				ui.print_warning("Keep your API key secret! Do not share it with anyone.");
				ui.print_key_value_table(&[(
					&format!("API Key for {}:", client.email),
					client.api_key.as_str(),
				)]);
			}
			None
		}
		Commands::Logout => {
			if auth::logout(config, ui)? {
				Some(CommandResult {
					exit: true,
					..Default::default()
				})
			} else {
				None
			}
		}
		Commands::Exit => Some(CommandResult {
			exit: true,
			..Default::default()
		}),
	};
	Ok(result.unwrap_or_default())
}

async fn cd(
	ui: &mut UI,
	client: &mut LazyClient,
	working_path: &RemotePath,
	directory: &str,
) -> Result<RemotePath> {
	let client = client.get(ui).await?;
	let directory = working_path.navigate(directory);
	match client
		.find_item_at_path(&directory.0)
		.await
		.context("Failed to find directory")?
	{
		Some(dir) => match dir {
			FSObject::Dir(_) | FSObject::Root(_) | FSObject::RootWithMeta(_) => Ok(directory),
			_ => Err(UI::failure(&format!("Not a directory: {}", directory.0))),
		},
		None => Err(UI::failure(&format!("No such directory: {}", directory.0))),
	}
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
		return Err(UI::failure(&format!(
			"No such directory: {}",
			directory_str
		)));
	};
	let directory = match directory {
		FSObject::Dir(dir) => DirectoryType::Dir(dir),
		FSObject::Root(root) => DirectoryType::Root(root),
		FSObject::RootWithMeta(root) => DirectoryType::RootWithMeta(root),
		_ => return Err(UI::failure(&format!("Not a directory: {}", directory_str))),
	};
	list_directory_by_uuid(ui, client, directory.uuid(), None).await
}

async fn list_directory_by_uuid(
	ui: &mut UI,
	client: &Client,
	directory: &dyn HasContents,
	directory_label: Option<&str>,
) -> Result<()> {
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
	if ui.json {
		ui.print_json(json!({
			"directories": directories,
			"files": files,
		}))?;
	} else {
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
			ui.print_muted(&format!(
				"{} is empty",
				directory_label.unwrap_or("Directory")
			));
			return Ok(());
		}
		ui.print_grid(&all_items);
	}
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
		return Err(UI::failure(&format!("No such file: {}", file_str)));
	};
	let file = match file {
		FSObject::File(file) => RemoteFileType::File(file),
		FSObject::SharedFile(file) => RemoteFileType::SharedFile(file),
		_ => return Err(UI::failure(&format!("Not a file: {}", file_str))),
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
		ui.print(&content);
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
		return Err(UI::failure(&format!(
			"No such file or directory: {}",
			file_or_directory_str
		)));
	};
	match item {
		FSObject::File(file) => {
			if ui.json {
				ui.print_json(json!({
					"name": file.name().unwrap_or_else(|| file.uuid().as_ref()),
					"type": "file",
					"size": file.size(),
					"modified": file.last_modified(),
					"created": file.created(),
					"uuid": file.uuid(),
				}))?;
			} else {
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
		}
		FSObject::Dir(dir) => {
			if ui.json {
				ui.print_json(json!({
					"name": dir.name().unwrap_or_else(|| dir.uuid().as_ref()),
					"type": "directory",
					"created": dir.created(),
					"uuid": dir.uuid(),
				}))?;
			} else {
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
		}
		FSObject::Root(_) | FSObject::RootWithMeta(_) => {
			let user_info = client
				.get_user_info()
				.await
				.context("Failed to get user info")?;
			if ui.json {
				ui.print_json(json!({
					"type": "drive",
					"usedStorage": user_info.storage_used,
					"totalStorage": user_info.max_storage,
				}))?;
			} else {
				ui.print_key_value_table(&[
					("Type", "Drive"),
					("Used", &ui::format_size(user_info.storage_used)),
					("Total", &ui::format_size(user_info.max_storage)),
				]);
			}
		}
		FSObject::SharedFile(_) => {
			return Err(UI::failure("Cannot show information for shared file"));
		}
	}
	Ok(())
}

async fn create_directory(
	ui: &mut UI,
	client: &mut LazyClient,
	working_path: &RemotePath,
	directory_str: &str,
	recursive: bool,
) -> Result<()> {
	let directory_str = working_path.navigate(directory_str);
	let parent_str = directory_str.navigate("..");
	let client = client.get(ui).await?;
	if parent_str.0 == directory_str.0 {
		return Err(UI::failure("Cannot create root directory"));
	}
	let _ = create_directory_(client, &directory_str, recursive).await?;
	ui.print_success(&format!("Directory created: {}", directory_str));
	Ok(())
}

async fn create_directory_(
	client: &Client,
	directory: &RemotePath,
	recursive: bool,
) -> Result<RemoteDirectory> {
	let parent = directory.navigate("..");
	let parent = match client.find_item_at_path(&parent.0).await {
		Err(e) => {
			if e.kind() == filen_sdk_rs::ErrorKind::InvalidType {
				return Err(UI::failure(&format!(
					"Path contains a file inbetween: {}",
					parent.0
				)));
			} else {
				return Err(e).context("Failed to find parent directory");
			}
		}
		Ok(Some(FSObject::Dir(parent_dir))) => Ok(DirectoryType::Dir(parent_dir)),
		Ok(Some(FSObject::Root(root))) => Ok(DirectoryType::Root(root)),
		Ok(Some(FSObject::RootWithMeta(root))) => Ok(DirectoryType::RootWithMeta(root)),
		Ok(Some(_)) => Err(UI::failure(&format!("Not a directory: {}", parent.0))),
		Ok(None) => {
			if recursive {
				Box::pin(create_directory_(client, &parent, true))
					.await
					.map(|d| DirectoryType::Dir(Cow::Owned(d)))
			} else {
				Err(UI::failure(&format!(
					"No such parent directory: {}",
					parent
				)))
			}
		}
	}?;
	client
		.create_dir(&parent, directory.basename().unwrap().to_string())
		.await
		.context("Failed to create directory")
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
		return Err(UI::failure(&format!(
			"No such file or directory: {}",
			file_or_directory_str
		)));
	};
	if permanent
		&& !ui.prompt_confirm(
			&format!("Permanently delete {}?", file_or_directory_str),
			false,
		)? {
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
			return Err(UI::failure("Cannot delete root directory"));
		}
		FSObject::SharedFile(_) => {
			return Err(UI::failure("Cannot delete shared file"));
		}
	}
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
		return Err(UI::failure(&format!(
			"No such source file or directory: {}",
			source_str.0
		)));
	};
	let Some(destination_dir) = client
		.find_item_at_path(&destination_str.0)
		.await
		.context("Failed to find destination directory")?
	else {
		return Err(UI::failure(&format!(
			"No such destination directory: {}",
			destination_str.0
		)));
	};
	let destination_dir = match destination_dir {
		FSObject::Dir(dir) => DirectoryType::Dir(dir),
		FSObject::Root(root) => DirectoryType::Root(root),
		FSObject::RootWithMeta(root) => DirectoryType::RootWithMeta(root),
		_ => {
			return Err(UI::failure(&format!(
				"Not a directory: {}",
				destination_str.0
			)));
		}
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
				return Err(UI::failure("Cannot move root directory"));
			}
			FSObject::SharedFile(_) => {
				return Err(UI::failure("Cannot move shared file"));
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
				return Err(UI::failure("Cannot copy root directory"));
			}
			FSObject::SharedFile(_) => {
				return Err(UI::failure("Cannot copy shared file"));
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
		return Err(UI::failure(&format!(
			"No such file or directory: {}",
			file_or_directory_str
		)));
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
			return Err(UI::failure(
				"Cannot change favorite status of root directory",
			));
		}
		FSObject::SharedFile(_) => {
			return Err(UI::failure("Cannot change favorite status of shared file"));
		}
	}
	Ok(())
}

async fn list_trash(ui: &mut UI, client: &mut LazyClient) -> Result<()> {
	let client = client.get(ui).await?;
	list_directory_by_uuid(ui, client, &ParentUuid::Trash, Some("Trash")).await
}

async fn empty_trash(ui: &mut UI, client: &mut LazyClient) -> Result<()> {
	let client = client.get(ui).await?;
	client.empty_trash().await?;
	ui.print_success("Emptied trash");
	Ok(())
}

mod rclone {
	//! [cli-doc] managed-rclone
	//! The Filen CLI includes a managed installation of [filen-rclone](https://github.com/FilenCloudDienste/filen-rclone).
	//! It is automatically downloaded and configured (authenticated) when you run the commands like `rclone`, `mount`, etc.

	use anyhow::{Context as _, Result};
	use filen_rclone_wrapper::serve::BasicServerOptions;

	use crate::{CliConfig, auth::LazyClient, ui::UI};

	pub(crate) async fn mount(
		config: &CliConfig,
		ui: &mut UI,
		client: &mut LazyClient,
		mount_point: Option<String>,
	) -> Result<()> {
		let client = client.get(ui).await?;
		let config_dir = config.config_dir.join("rclone");
		check_already_downloaded(ui, &config_dir).await;
		let mut network_drive = filen_rclone_wrapper::network_drive::NetworkDrive::mount(
			client,
			&config_dir,
			mount_point.as_deref(),
			false,
		)
		.await
		.context("Failed to mount network drive")?;
		network_drive
			.wait_until_active()
			.await
			.context("Failed to mount network drive")?;
		ui.print_success("Mounted network drive (kill the CLI to unmount and exit)");
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
		Ok(())
	}

	pub(crate) async fn start_server(
		config: &CliConfig,
		ui: &mut UI,
		client: &mut LazyClient,
		server_type: &str,
		display_server_type: &str,
		options: BasicServerOptions,
	) -> Result<()> {
		let client = client.get(ui).await?;
		let config_dir = config.config_dir.join("rclone");
		check_already_downloaded(ui, &config_dir).await;
		let mut server = filen_rclone_wrapper::serve::start_basic_server(
			client,
			&config_dir,
			server_type,
			options,
		)
		.await
		.with_context(|| format!("Failed to start {} server", display_server_type))?;
		ui.print_success(&format!(
			"Started {} server on http://{} {} (kill the CLI to stop)",
			display_server_type,
			server.address,
			if let Some(auth) = &server.auth {
				format!(
					"with {} \"{}\" and {} \"{}\"",
					if server_type == "s3" {
						"Access Key ID"
					} else {
						"username"
					},
					auth.user,
					if server_type == "s3" {
						"Secret Access Key"
					} else {
						"password"
					},
					auth.password
				)
			} else {
				"without authentication".to_string()
			}
		));
		let code = server.process.wait().await.with_context(|| {
			format!("Failed to wait for {} server process", display_server_type)
		})?;
		if !code.success() {
			return Err(anyhow::anyhow!(match code.code() {
				Some(c) => format!(
					"{} server process exited with code: {} (use --verbose for more info)",
					display_server_type, c
				),
				None => format!(
					"{} server process exited with unknown code",
					display_server_type
				),
			}));
		}
		Ok(())
	}

	pub(crate) async fn execute_rclone(
		config: &CliConfig,
		ui: &mut UI,
		client: &mut LazyClient,
		cmd: Vec<String>,
	) -> Result<()> {
		let config_dir = config.config_dir.join("rclone");
		check_already_downloaded(ui, &config_dir).await;
		let rclone = filen_rclone_wrapper::rclone_installation::RcloneInstallation::initialize(
			client.get(ui).await?,
			&config_dir,
		)
		.await
		.context("Failed to initialize rclone installation")?;
		let exit_code = rclone
			.execute(&cmd.iter().map(|s| s.as_str()).collect::<Vec<&str>>())
			.await?
			.code();
		if let Some(exit_code) = exit_code
			&& exit_code != 0
		{
			return Err(crate::construct_exit_code_error(exit_code));
		}
		Ok(())
	}

	async fn check_already_downloaded(ui: &mut UI, config_dir: &std::path::Path) {
		if !filen_rclone_wrapper::rclone_installation::RcloneInstallation::check_already_downloaded(
			config_dir,
		)
		.await
		{
			ui.print_muted("Downloading filen-rclone...");
		}
	}
}
