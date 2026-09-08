use anyhow::{Context, Result};
use clap::Subcommand;
use clap_complete::engine::{ArgValueCompleter, PathCompleter};
use console::style;
use filen_rclone_wrapper::serve::BasicServerOptions;
use filen_sdk_rs::{
	auth::Client,
	fs::{
		HasName as _, HasParent as _, HasUUID,
		categories::{DirType, NonRootFileType, Normal, fs::CategoryFS},
		dir::meta::DirectoryMetaChanges,
		file::{meta::FileMetaChanges, traits::HasFileInfo as _},
	},
	io::{RemoteDirectory, RemoteFile, client_impl::IoSharedClientExt},
};
use serde_json::json;

use crate::{
	CliConfig, CommandResult,
	auth::{self, LazyClient, export_auth_config},
	completion::FilenCompleter,
	docs::{print_in_app_docs, serve_markdown_docs_as_html},
	ui::{self, UI},
	util::RemotePath,
};

#[derive(Debug, Subcommand)]
pub(crate) enum Commands {
	/// Print help about a command or topic (default: general help)
	Help {
		/// Command or topic to show help about
		#[arg(add = FilenCompleter::help_topic())]
		command_or_topic: Option<String>,
	},
	/// Change the working directory (in REPL)
	Cd {
		/// Directory to navigate into (supports "..")
		#[arg(add = FilenCompleter::directory())]
		directory: String,
	},
	/// List files in a directory
	Ls {
		/// Directory to list files in (default: the current working directory)
		#[arg(add = FilenCompleter::directory())]
		directory: Option<String>,
		/// Use a long listing format with file sizes
		#[arg(short = 'l', long)]
		long: bool,
	},
	/// Print the contents of a file
	Cat {
		/// File to print
		#[arg(add = FilenCompleter::file())]
		file: String,
	},
	/// Print the first lines of a file
	Head {
		/// File to print
		#[arg(add = FilenCompleter::file())]
		file: String,
		/// Number of lines to print
		#[arg(short = 'n', long, default_value_t = 10)]
		lines: usize,
	},
	/// Print the last lines of a file
	Tail {
		/// File to print
		#[arg(add = FilenCompleter::file())]
		file: String,
		/// Number of lines to print
		#[arg(short = 'n', long, default_value_t = 10)]
		lines: usize,
	},
	/// Show information about a file, a directory or the Filen drive
	Stat {
		/// File or directory to show information about ("/" for the Filen drive)
		#[arg(add = FilenCompleter::file_or_directory())]
		file_or_directory: String,
	},
	/// Create a new directory
	Mkdir {
		/// Directory to create
		#[arg(add = FilenCompleter::directory())]
		directory: String,
		/// Recursively create parent directories
		#[arg(short, long)]
		recursive: bool,
	},
	/// Remove a file or directory
	Rm {
		/// File or directory to remove
		#[arg(add = FilenCompleter::file_or_directory())]
		file_or_directory: String,
		/// Permanently delete the file or directory (default: move to trash)
		#[arg(short, long)]
		permanent: bool,
	},
	/// Move and/or rename a file or directory
	Mv {
		/// Source file or directory
		#[arg(add = FilenCompleter::file_or_directory())]
		source: String,
		/// Destination: an existing directory to move the source into,
		/// or the new path of the source (to move and/or rename it)
		#[arg(add = FilenCompleter::file_or_directory())]
		destination: String,
	},
	/// Copy a file or directory
	Cp {
		/// Source file or directory
		#[arg(add = FilenCompleter::file_or_directory())]
		source: String,
		/// Destination parent directory
		#[arg(add = FilenCompleter::directory())]
		destination: String,
	},
	/// Upload a local file or directory (recursively) into a directory in the Filen drive
	Upload {
		/// Local file or directory to upload
		#[arg(add = ArgValueCompleter::new(PathCompleter::any()))]
		source: String,
		/// Destination directory in the Filen drive (default: the current working directory)
		#[arg(add = FilenCompleter::directory())]
		destination: Option<String>,
	},
	/// Download a file or directory (recursively) from the Filen drive into a local directory
	Download {
		/// File or directory to download ("/" for the entire Filen drive)
		#[arg(add = FilenCompleter::file_or_directory())]
		source: String,
		/// Local destination directory (default: the current local directory)
		#[arg(add = ArgValueCompleter::new(PathCompleter::dir()))]
		destination: Option<String>,
	},
	/// Search for a file or directory interactively
	Search,
	/// Favorite a file or directory
	Favorite {
		/// File or directory to favorite
		#[arg(add = FilenCompleter::file_or_directory())]
		file_or_directory: String,
	},
	/// Unfavorite a file or directory
	Unfavorite {
		/// File or directory to unfavorite
		#[arg(add = FilenCompleter::file_or_directory())]
		file_or_directory: String,
	},
	/// List trashed items
	ListTrash,
	/// Restore a trashed item interactively
	TrashRestore,
	/// Permanently delete a trashed item interactively
	TrashDelete,
	/// Permanently delete all trashed items
	EmptyTrash,
	/// Export an auth config (to be used with --auth-config-path option)
	ExportAuthConfig,
	/// Execute an Rclone command using the managed installation
	Rclone {
		/// The command to execute. Your Filen drive is available as the "filen" remote.
		#[arg(trailing_var_arg = true, allow_hyphen_values = true)]
		cmd: Vec<String>,
	},
	/// Mount Filen as a network drive
	Mount {
		/// Where to mount the network drive (default: system default)
		mount_point: Option<String>,
		/// The maximum cache size (e.g. "500Mi", "10Gi") (default: calculated from available disk space)
		#[arg(long)]
		cache_size: Option<String>,
		/// The number of parallel transfers
		#[arg(long)]
		transfers: Option<usize>,
		/// Additional arguments to Rclone
		rclone_args: Vec<String>,
	},
	/// Runs a WebDAV, FTP, SFTP or HTTP server exposing your Filen drive
	Serve {
		/// The type of server to run: webdav, ftp, sftp, http
		server: String,
		/// IP and port for the server (`<ip>:<port>` or `:<port>`)
		#[arg(long = "addr", default_value = ":80")]
		address: String,
		/// Directory that the server exposes (default: the entire Filen drive)
		#[arg(long, add = FilenCompleter::directory())]
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
		/// The maximum cache size (e.g. "500Mi", "10Gi") (default: calculated from available disk space)
		#[arg(long)]
		cache_size: Option<String>,
		/// The number of parallel transfers
		#[arg(long)]
		transfers: Option<usize>,
		/// Additional arguments to Rclone
		rclone_args: Vec<String>,
	},
	// todo: s3 server
	/// Exports your user API key (for use with non-managed Rclone)
	ExportApiKey,
	/// View the documentation (same as --help) locally in a browser rendered as HTML
	ViewHtmlDocs,
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
		Commands::Ls { directory, long } => {
			list_directory(ui, client, working_path, directory, long).await?;
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
			move_file_or_directory(ui, client, working_path, &source, &destination).await?;
			None
		}
		Commands::Cp {
			source,
			destination,
		} => {
			copy_file_or_directory(ui, client, working_path, &source, &destination).await?;
			None
		}
		Commands::Upload {
			source,
			destination,
		} => {
			crate::transfer_cmds::upload(ui, client, working_path, &source, destination.as_deref())
				.await?;
			None
		}
		Commands::Download {
			source,
			destination,
		} => {
			crate::transfer_cmds::download(
				ui,
				client,
				working_path,
				&source,
				destination.as_deref(),
			)
			.await?;
			None
		}
		Commands::Search => crate::search_cmd::search_cmd(ui, client, working_path).await?,
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
		Commands::TrashRestore => {
			select_trash_item(ui, client, TrashAction::Restore).await?;
			None
		}
		Commands::TrashDelete => {
			select_trash_item(ui, client, TrashAction::Delete).await?;
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
		Commands::Mount {
			mount_point,
			cache_size,
			transfers,
			rclone_args,
		} => {
			rclone::mount(
				config,
				ui,
				client,
				mount_point,
				cache_size,
				transfers,
				rclone_args,
			)
			.await?;
			None
		}
		Commands::Serve {
			server,
			address,
			root,
			user,
			password,
			read_only,
			cache_size,
			transfers,
			rclone_args,
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
					cache_size,
					transfers,
				},
				rclone_args,
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
		Commands::ViewHtmlDocs => {
			serve_markdown_docs_as_html(ui).context("Failed to serve markdown docs as HTML")?;
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
			NonRootFileType::Dir(_) | NonRootFileType::Root(_) => Ok(directory),
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
	long: bool,
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
	let directory: DirType<'_, Normal> = match directory {
		NonRootFileType::Dir(dir) => DirType::Dir(dir),
		NonRootFileType::Root(root) => DirType::Root(root),
		_ => return Err(UI::failure(&format!("Not a directory: {}", directory_str))),
	};
	list_directory_by_dir(ui, client, &directory, None, long).await
}

fn print_items_after_list(
	ui: &mut UI,
	dirs: Vec<RemoteDirectory>,
	files: Vec<RemoteFile>,
	directory_label: Option<&str>,
	long: bool,
) -> Result<()> {
	let mut directories = dirs
		.iter()
		.map(|f| {
			f.name()
				.map(str::to_string)
				.unwrap_or_else(|| f.uuid().to_string())
		})
		.collect::<Vec<String>>();
	directories.sort();
	let mut files = files
		.iter()
		.map(|f| {
			(
				f.name()
					.map(str::to_string)
					.unwrap_or_else(|| f.uuid().to_string()),
				f.size(),
			)
		})
		.collect::<Vec<(String, u64)>>();
	files.sort_by(|(a, _), (b, _)| a.cmp(b));
	let file_names = files
		.iter()
		.map(|(name, _)| name.clone())
		.collect::<Vec<String>>();
	if ui.json {
		ui.print_json(json!({
			"directories": directories,
			"files": file_names,
		}))?;
		return Ok(());
	}
	if directories.is_empty() && files.is_empty() {
		ui.print_muted(&format!(
			"{} is empty",
			directory_label.unwrap_or("Directory")
		));
		return Ok(());
	}
	if long {
		// one item per line, with a right-aligned size column (directories have no size)
		let sizes = files
			.iter()
			.map(|(_, size)| ui::format_size(*size))
			.collect::<Vec<String>>();
		let size_width = sizes.iter().map(|s| s.len()).max().unwrap_or(0).max(1);
		for name in &directories {
			// pad before styling, so the ANSI codes don't count towards the column width
			let size = format!("{:>size_width$}", "-");
			ui.print(&format!("{}  {}", style(size).dim(), style(name).blue()));
		}
		for ((name, _), size) in files.iter().zip(sizes.iter()) {
			ui.print(&format!("{:>size_width$}  {}", size, name));
		}
		return Ok(());
	}
	// print directory names in blue
	let directories = directories
		.iter()
		.map(|s| style(s).blue().to_string())
		.collect::<Vec<String>>();
	let all_items = directories
		.iter()
		.chain(file_names.iter())
		.map(|s| s.as_ref())
		.collect::<Vec<&str>>();
	ui.print_grid(&all_items);
	Ok(())
}

async fn list_directory_by_dir(
	ui: &mut UI,
	client: &Client,
	directory: &DirType<'_, Normal>,
	directory_label: Option<&str>,
	long: bool,
) -> Result<()> {
	let (dirs, files) = client
		.list_dir::<_, Normal>(directory, None::<&fn(u64, Option<u64>)>)
		.await
		.context("Failed to list directory")?;
	print_items_after_list(ui, dirs, files, directory_label, long)
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
		NonRootFileType::File(file) => file,
		_ => return Err(UI::failure(&format!("Not a file: {}", file_str))),
	};
	if file.size() < 1024
		|| ui.prompt_confirm("File is larger than 1KB, do you want to continue?", false)?
	{
		let content = client.download_file(file.as_ref()).await?;
		let content = String::from_utf8_lossy(&content);
		let content = match lines {
			PrintFileLines::Full => content.to_string(),
			PrintFileLines::Head(n) => content.lines().take(n).collect::<Vec<&str>>().join("\n"),
			PrintFileLines::Tail(n) => content
				.lines()
				.rev()
				.take(n)
				.collect::<Vec<&str>>()
				.into_iter()
				.rev()
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
		NonRootFileType::File(file) => {
			if ui.json {
				ui.print_json(json!({
					"name": file.name().map(str::to_string).unwrap_or_else(|| file.uuid().to_string()),
					"type": "file",
					"size": file.size(),
					"modified": file.last_modified(),
					"created": file.created(),
					"uuid": file.uuid(),
				}))?;
			} else {
				let file_uuid = file.uuid().to_string();
				let file_name = file
					.name()
					.map(str::to_string)
					.unwrap_or_else(|| file_uuid.clone());
				ui.print_key_value_table(&[
					("Name", &file_name),
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
					("UUID", &file_uuid),
				]);
			}
		}
		NonRootFileType::Dir(dir) => {
			let size_info = Normal::dir_size(&**client, &DirType::from(&*dir), ())
				.await
				.context("Failed to get directory size")?;
			if ui.json {
				ui.print_json(json!({
					"name": dir.name().map(str::to_string).unwrap_or_else(|| dir.uuid().to_string()),
					"type": "directory",
					"size": size_info.size,
					"files": size_info.files,
					"directories": size_info.dirs,
					"created": dir.created(),
					"uuid": dir.uuid(),
				}))?;
			} else {
				let dir_uuid = dir.uuid().to_string();
				let dir_name = dir
					.name()
					.map(str::to_string)
					.unwrap_or_else(|| dir_uuid.clone());
				ui.print_key_value_table(&[
					("Name", &dir_name),
					("Type", "Directory"),
					("Size", &ui::format_size(size_info.size)),
					("Files", &size_info.files.to_string()),
					("Directories", &size_info.dirs.to_string()),
					(
						"Created",
						&dir.created()
							.map(|d| ui::format_date(&d))
							.unwrap_or("-".to_string()),
					),
					("UUID", &dir_uuid),
				]);
			}
		}
		NonRootFileType::Root(root) => {
			let user_info = client
				.get_user_info()
				.await
				.context("Failed to get user info")?;
			let size_info = Normal::dir_size(&**client, &DirType::from(&*root), ())
				.await
				.context("Failed to get drive size")?;
			if ui.json {
				ui.print_json(json!({
					"type": "drive",
					"usedStorage": user_info.storage_used,
					"totalStorage": user_info.max_storage,
					"files": size_info.files,
					"directories": size_info.dirs,
				}))?;
			} else {
				ui.print_key_value_table(&[
					("Type", "Drive"),
					("Used", &ui::format_size(user_info.storage_used)),
					("Total", &ui::format_size(user_info.max_storage)),
					("Files", &size_info.files.to_string()),
					("Directories", &size_info.dirs.to_string()),
				]);
			}
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
		Ok(Some(NonRootFileType::Dir(parent_dir))) => Ok(DirType::Dir(parent_dir)),
		Ok(Some(NonRootFileType::Root(root))) => Ok(DirType::Root(root)),
		Ok(Some(_)) => Err(UI::failure(&format!("Not a directory: {}", parent.0))),
		Ok(None) => {
			if recursive {
				Box::pin(create_directory_(client, &parent, true))
					.await
					.map(|d| DirType::Dir(std::borrow::Cow::Owned(d)))
			} else {
				Err(UI::failure(&format!(
					"No such parent directory: {}",
					parent
				)))
			}
		}
	}?;
	client
		.create_dir(&parent, directory.basename().unwrap())
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
		NonRootFileType::File(mut file) => {
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
		NonRootFileType::Dir(mut dir) => {
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
		NonRootFileType::Root(_) => {
			return Err(UI::failure("Cannot delete root directory"));
		}
	}
	Ok(())
}

/// Moves and/or renames a file or directory, following the semantics of the Unix `mv`:
/// if the destination is an existing directory, the source is moved into it under its
/// current name; otherwise the destination names the source's new path, so the source is
/// moved to that path's parent directory and renamed to that path's base name.
async fn move_file_or_directory(
	ui: &mut UI,
	client: &mut LazyClient,
	working_path: &RemotePath,
	source_str: &str,
	destination_str: &str,
) -> Result<()> {
	let source_path = working_path.navigate(source_str);
	let destination_path = working_path.navigate(destination_str);
	let client = client.get(ui).await?;
	let Some(source) = client
		.find_item_at_path(&source_path.0)
		.await
		.context("Failed to find source file or directory")?
	else {
		return Err(UI::failure(&format!(
			"No such source file or directory: {}",
			source_path.0
		)));
	};
	let source_filename = match &source {
		NonRootFileType::File(file) => file.name(),
		NonRootFileType::Dir(dir) => dir.name(),
		NonRootFileType::Root(_) => return Err(UI::failure("Cannot move root directory")),
	}
	.context("Failed to decrypt source name")?
	.to_string();

	// resolve the destination into the directory the source ends up in, plus the name it
	// ends up under
	let destination_dir = match client
		.find_item_at_path(&destination_path.0)
		.await
		.context("Failed to find destination")?
	{
		Some(NonRootFileType::Dir(dir)) => Some(DirType::Dir(dir)),
		Some(NonRootFileType::Root(root)) => Some(DirType::Root(root)),
		Some(NonRootFileType::File(_)) => {
			return Err(UI::failure(&format!(
				"Destination already exists: {}",
				destination_path.0
			)));
		}
		None => None,
	};
	let (destination_dir, new_name, new_path) = match destination_dir {
		// the destination is an existing directory, so move the source into it as-is
		Some(destination_dir) => {
			let new_path = destination_path.navigate(&source_filename);
			if new_path == source_path {
				return Err(UI::failure(&format!(
					"{} is already in {}",
					source_path.0, destination_path.0
				)));
			}
			// check that the destination doesn't already exist
			if client
				.find_item_at_path(&new_path.0)
				.await
				.context("Failed to check destination")?
				.is_some()
			{
				return Err(UI::failure(&format!(
					"Destination already exists: {}",
					new_path.0
				)));
			}
			(destination_dir, source_filename.clone(), new_path)
		}
		// the destination doesn't exist, so it names the source's new path
		None => {
			let new_name = destination_path.basename().expect("cannot fail");
			let parent_path = destination_path.parent();
			let destination_dir = match client
				.find_item_at_path(&parent_path.0)
				.await
				.context("Failed to find destination parent directory")?
			{
				Some(NonRootFileType::Dir(dir)) => DirType::Dir(dir),
				Some(NonRootFileType::Root(root)) => DirType::Root(root),
				Some(NonRootFileType::File(_)) => {
					return Err(UI::failure(&format!("Not a directory: {}", parent_path.0)));
				}
				None => {
					return Err(UI::failure(&format!(
						"No such destination directory: {}",
						parent_path.0
					)));
				}
			};
			(
				destination_dir,
				new_name.to_string(),
				destination_path.clone(),
			)
		}
	};

	if new_path.0.starts_with(&format!("{}/", source_path.0)) {
		return Err(UI::failure(&format!(
			"Cannot move {} into itself: {}",
			source_path.0, new_path.0
		)));
	}

	let needs_rename = new_name != source_filename;
	match source {
		NonRootFileType::File(file) => {
			let mut file = file.into_owned();
			if *file.parent() != destination_dir.uuid() {
				client
					.move_file(&mut file, &destination_dir)
					.await
					.context("Failed to move file")?;
			}
			if needs_rename {
				client
					.update_file_metadata(
						&mut file,
						FileMetaChanges::default()
							.name(&new_name)
							.context("Invalid destination file name")?,
					)
					.await
					.context("Failed to rename file")?;
			}
		}
		NonRootFileType::Dir(dir) => {
			let mut dir = dir.into_owned();
			if *dir.parent() != destination_dir.uuid() {
				client
					.move_dir(&mut dir, &destination_dir)
					.await
					.context("Failed to move directory")?;
			}
			if needs_rename {
				client
					.update_dir_metadata(
						&mut dir,
						DirectoryMetaChanges::default()
							.name(&new_name)
							.context("Invalid destination directory name")?,
					)
					.await
					.context("Failed to rename directory")?;
			}
		}
		NonRootFileType::Root(_) => return Err(UI::failure("Cannot move root directory")),
	}
	ui.print_success(&format!("Moved {} to {}", source_path.0, new_path.0));
	Ok(())
}

async fn copy_file_or_directory(
	ui: &mut UI,
	client: &mut LazyClient,
	working_path: &RemotePath,
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
		NonRootFileType::Dir(dir) => DirType::Dir(dir),
		NonRootFileType::Root(root) => DirType::Root(root),
		_ => {
			return Err(UI::failure(&format!(
				"Not a directory: {}",
				destination_str.0
			)));
		}
	};
	match source_file_or_directory {
		NonRootFileType::File(file) => {
			copy_file(client, file.as_ref(), &destination_dir).await?;
		}
		NonRootFileType::Dir(dir) => {
			copy_dir_recursive(client, dir.as_ref(), &destination_dir).await?;
		}
		NonRootFileType::Root(_) => {
			return Err(UI::failure("Cannot copy root directory"));
		}
	}
	ui.print_success(&format!(
		"Copied {} into {}",
		source_str.0, destination_str.0
	));
	Ok(())
}

async fn copy_file(
	client: &Client,
	file: &RemoteFile,
	destination_dir: &DirType<'_, Normal>,
) -> Result<RemoteFile> {
	let name = file.name().context("Failed to decrypt file name")?;
	let mut builder = client
		.make_file_builder(name, destination_dir.uuid())
		.context("Failed to prepare file copy")?;
	if let Some(mime) = file.mime() {
		builder = builder.mime(mime.to_string());
	}
	if let Some(created) = file.created() {
		builder = builder.created(created);
	}
	if let Some(modified) = file.last_modified() {
		builder = builder.modified(modified);
	}
	let data = client
		.download_file(file)
		.await
		.context("Failed to download file for copying")?;
	// todo: does this consume too much memory for large files? maybe we should stream the data
	client
		.upload_file(builder, &data)
		.await
		.context("Failed to upload copied file")
}

async fn copy_dir_recursive(
	client: &Client,
	source_dir: &RemoteDirectory,
	destination_parent: &DirType<'_, Normal>,
) -> Result<RemoteDirectory> {
	let name = source_dir
		.name()
		.context("Failed to decrypt directory name")?;
	let new_dir = client
		.create_dir(destination_parent, name)
		.await
		.context("Failed to create destination directory for copying")?;
	let (subdirs, files) = client
		.list_dir::<_, Normal>(
			&DirType::Dir(std::borrow::Cow::Borrowed(source_dir)),
			None::<&fn(u64, Option<u64>)>,
		)
		.await
		.context("Failed to list source directory for copying")?;
	for file in &files {
		copy_file(
			client,
			file,
			&DirType::Dir(std::borrow::Cow::Borrowed(&new_dir)),
		)
		.await?;
	}
	for subdir in &subdirs {
		Box::pin(copy_dir_recursive(
			client,
			subdir,
			&DirType::Dir(std::borrow::Cow::Borrowed(&new_dir)),
		))
		.await?;
	}
	Ok(new_dir)
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
		NonRootFileType::File(mut file) => {
			client
				.set_file_favorite(file.to_mut(), favorite)
				.await
				.context("Failed to set file favorite status")?;
			ui.print_success(&format!(
				"{} file: {}",
				if favorite { "Favorited" } else { "Unfavorited" },
				file_or_directory_str
			));
		}
		NonRootFileType::Dir(mut dir) => {
			client
				.set_dir_favorite(dir.to_mut(), favorite)
				.await
				.context("Failed to set directory favorite status")?;
			ui.print_success(&format!(
				"{} directory: {}",
				if favorite { "Favorited" } else { "Unfavorited" },
				file_or_directory_str
			));
		}
		NonRootFileType::Root(_) => {
			return Err(UI::failure(
				"Cannot change favorite status of root directory",
			));
		}
	}
	Ok(())
}

async fn list_trash(ui: &mut UI, client: &mut LazyClient) -> Result<()> {
	let client = client.get(ui).await?;
	let (dirs, files) = client
		.list_trash(None::<&fn(u64, Option<u64>)>)
		.await
		.context("Failed to list trash")?;
	print_items_after_list(ui, dirs, files, Some("Trash"), false)
}

enum TrashAction {
	Restore,
	Delete,
}

enum TrashItem {
	Dir(RemoteDirectory),
	File(RemoteFile),
}

impl TrashItem {
	fn uuid_string(&self) -> String {
		match self {
			TrashItem::Dir(dir) => dir.uuid().to_string(),
			TrashItem::File(file) => file.uuid().to_string(),
		}
	}
}

/// Lists the trash and lets the user pick one item to restore or permanently delete.
async fn select_trash_item(
	ui: &mut UI,
	client: &mut LazyClient,
	action: TrashAction,
) -> Result<()> {
	let client = client.get(ui).await?;
	let (dirs, files) = client
		.list_trash(None::<&fn(u64, Option<u64>)>)
		.await
		.context("Failed to list trash")?;

	// directories first, each group sorted by name, like `ls` prints them
	let mut items = dirs
		.into_iter()
		.map(|dir| {
			let name = dir
				.name()
				.map(str::to_string)
				.unwrap_or_else(|| dir.uuid().to_string());
			(name, TrashItem::Dir(dir))
		})
		.collect::<Vec<(String, TrashItem)>>();
	items.sort_by(|(a, _), (b, _)| a.cmp(b));
	let mut file_items = files
		.into_iter()
		.map(|file| {
			let name = file
				.name()
				.map(str::to_string)
				.unwrap_or_else(|| file.uuid().to_string());
			(name, TrashItem::File(file))
		})
		.collect::<Vec<(String, TrashItem)>>();
	file_items.sort_by(|(a, _), (b, _)| a.cmp(b));
	items.append(&mut file_items);
	if items.is_empty() {
		ui.print_muted("Trash is empty");
		return Ok(());
	}

	// mark directories with a trailing slash, and disambiguate items that share a name by
	// their UUID, so every option maps back to exactly one item
	let options = items
		.iter()
		.map(|(name, item)| {
			let mut label = match item {
				TrashItem::Dir(_) => format!("{}/", name),
				TrashItem::File(_) => name.clone(),
			};
			if items.iter().filter(|(other, _)| other == name).count() > 1 {
				label.push_str(&format!(" ({})", item.uuid_string()));
			}
			label
		})
		.collect::<Vec<String>>();
	let Some(selection) = ui.prompt_select(
		match action {
			TrashAction::Restore => "Select an item to restore",
			TrashAction::Delete => "Select an item to permanently delete",
		},
		options.clone(),
	)?
	else {
		return Ok(());
	};
	let index = options
		.iter()
		.position(|option| *option == selection)
		.context("Failed to resolve selected item")?;
	let (name, item) = items.remove(index);

	match action {
		TrashAction::Restore => match item {
			TrashItem::Dir(mut dir) => {
				client
					.restore_dir(&mut dir)
					.await
					.context("Failed to restore directory")?;
				ui.print_success(&format!("Restored directory: {}", name));
			}
			TrashItem::File(mut file) => {
				client
					.restore_file(&mut file)
					.await
					.context("Failed to restore file")?;
				ui.print_success(&format!("Restored file: {}", name));
			}
		},
		TrashAction::Delete => {
			if !ui.prompt_confirm(&format!("Permanently delete {}?", name), false)? {
				return Ok(());
			}
			match item {
				TrashItem::Dir(dir) => {
					client
						.delete_dir_permanently(dir)
						.await
						.context("Failed to permanently delete directory")?;
					ui.print_success(&format!("Permanently deleted directory: {}", name));
				}
				TrashItem::File(file) => {
					client
						.delete_file_permanently(file)
						.await
						.context("Failed to permanently delete file")?;
					ui.print_success(&format!("Permanently deleted file: {}", name));
				}
			}
		}
	}
	Ok(())
}

async fn empty_trash(ui: &mut UI, client: &mut LazyClient) -> Result<()> {
	let client = client.get(ui).await?;
	client.empty_trash().await?;
	ui.print_success("Emptied trash");
	Ok(())
}

mod rclone {
	//! [cli-doc] managed-rclone
	//! The Filen CLI includes a managed installation of Rclone, which can be used to [access Filen](https://rclone.org/filen).
	//! It is automatically downloaded and configured (authenticated) when you run the commands like `rclone`, `mount`, etc.

	use anyhow::{Context as _, Result};
	use filen_rclone_wrapper::{
		rclone_installation::{RcloneInstallation, RcloneInstallationConfig},
		serve::BasicServerOptions,
	};
	use tokio::select;

	use crate::{CliConfig, auth::LazyClient, ui::UI};

	pub(crate) async fn mount(
		config: &CliConfig,
		ui: &mut UI,
		client: &mut LazyClient,
		mount_point: Option<String>,
		cache_size: Option<String>,
		transfers: Option<usize>,
		rclone_args: Vec<String>,
	) -> Result<()> {
		let client = client.get(ui).await?;
		let config_dir = config.config_dir.join("rclone");
		check_already_downloaded(ui, &config_dir).await;
		let mut network_drive = filen_rclone_wrapper::network_drive::NetworkDrive::mount(
			client,
			&RcloneInstallationConfig::new(&config_dir),
			mount_point.as_deref(),
			false,
			cache_size,
			transfers,
			rclone_args,
		)
		.await
		.context("Failed to mount network drive (use --verbose for more info)")?;
		RcloneInstallation::pipe_output_to_logs(&mut network_drive.process);
		network_drive
			.wait_until_active()
			.await
			.context("Failed to mount network drive (use --verbose for more info)")?;
		ui.print_success("Mounted network drive (kill the CLI to unmount and exit)");
		let mut stop_rx = crate::CTRLC_TX.subscribe();
		select! {
			_ = stop_rx.recv() => {
				ui.print_muted("Unmounting network drive...");
				network_drive.process.kill().await.context("Failed to kill mount process")?;
			}
			result = network_drive.process.wait() => {
				let status = result.context("Failed to wait for mount process")?;
				if !status.success() {
					return Err(anyhow::anyhow!(match status.code() {
						Some(c) => format!("Mount process exited with code: {}", c),
						None => "Mount process exited with unknown code".to_string(),
					}));
				}
			}
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
		rclone_args: Vec<String>,
	) -> Result<()> {
		let client = client.get(ui).await?;
		let config_dir = config.config_dir.join("rclone");
		check_already_downloaded(ui, &config_dir).await;
		let mut server = filen_rclone_wrapper::serve::start_basic_server(
			client,
			&RcloneInstallationConfig::new(&config_dir),
			server_type,
			options,
			rclone_args,
		)
		.await
		.with_context(|| format!("Failed to start {} server", display_server_type))?;
		RcloneInstallation::pipe_output_to_logs(&mut server.process);
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
		let mut stop_rx = crate::CTRLC_TX.subscribe();
		select! {
			_ = stop_rx.recv() => {
				ui.print_muted(&format!("Stopping {} server...", display_server_type));
				server.process.kill().await.with_context(|| {
					format!("Failed to kill {} server process", display_server_type)
				})?;
			}
			result = server.process.wait() => {
				let status = result.with_context(|| {
					format!("Failed to wait for {} server process", display_server_type)
				})?;
				if !status.success() {
					return Err(anyhow::anyhow!(match status.code() {
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
			}
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
			&RcloneInstallationConfig::new(&config_dir),
			Some(client.get(ui).await?),
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
			&RcloneInstallationConfig::new(config_dir),
		)
		.await
		{
			ui.print_muted("Downloading managed Rclone...");
		}
	}
}
