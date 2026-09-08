use anyhow::{Context, Result};
use clap::Subcommand;
use clap_complete::engine::{ArgValueCompleter, PathCompleter};
use filen_rclone_wrapper::serve::BasicServerOptions;
use serde_json::json;

use crate::{
	CliConfig, CommandResult,
	auth::{self, LazyClient, export_auth_config},
	completion::FilenCompleter,
	docs::{print_in_app_docs, serve_markdown_docs_as_html},
	ui::UI,
	util::RemotePath,
};

mod fs_cmds;
mod rclone_cmds;
mod search_cmd;
mod transfer_cmds;

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
			let working_path = fs_cmds::cd(ui, client, working_path, &directory).await?;
			Some(CommandResult {
				working_path: Some(working_path),
				..Default::default()
			})
		}
		Commands::Ls { directory, long } => {
			fs_cmds::list_directory(ui, client, working_path, directory, long).await?;
			None
		}
		Commands::Cat { file } => {
			fs_cmds::print_file(
				ui,
				client,
				working_path,
				&file,
				fs_cmds::PrintFileLines::Full,
			)
			.await?;
			None
		}
		Commands::Head { file, lines } => {
			fs_cmds::print_file(
				ui,
				client,
				working_path,
				&file,
				fs_cmds::PrintFileLines::Head(lines),
			)
			.await?;
			None
		}
		Commands::Tail { file, lines } => {
			fs_cmds::print_file(
				ui,
				client,
				working_path,
				&file,
				fs_cmds::PrintFileLines::Tail(lines),
			)
			.await?;
			None
		}
		Commands::Stat { file_or_directory } => {
			fs_cmds::print_file_or_directory_info(ui, client, working_path, &file_or_directory)
				.await?;
			None
		}
		Commands::Mkdir {
			directory,
			recursive,
		} => {
			fs_cmds::create_directory(ui, client, working_path, &directory, recursive).await?;
			None
		}
		Commands::Rm {
			file_or_directory,
			permanent,
		} => {
			fs_cmds::delete_file_or_directory(
				ui,
				client,
				working_path,
				&file_or_directory,
				permanent,
			)
			.await?;
			None
		}
		Commands::Mv {
			source,
			destination,
		} => {
			fs_cmds::move_file_or_directory(ui, client, working_path, &source, &destination)
				.await?;
			None
		}
		Commands::Cp {
			source,
			destination,
		} => {
			fs_cmds::copy_file_or_directory(ui, client, working_path, &source, &destination)
				.await?;
			None
		}
		Commands::Upload {
			source,
			destination,
		} => {
			transfer_cmds::upload(ui, client, working_path, &source, destination.as_deref())
				.await?;
			None
		}
		Commands::Download {
			source,
			destination,
		} => {
			transfer_cmds::download(ui, client, working_path, &source, destination.as_deref())
				.await?;
			None
		}
		Commands::Search => search_cmd::search_cmd(ui, client, working_path).await?,
		Commands::Favorite { file_or_directory } => {
			fs_cmds::set_file_or_directory_favorite(
				ui,
				client,
				working_path,
				&file_or_directory,
				true,
			)
			.await?;
			None
		}
		Commands::Unfavorite { file_or_directory } => {
			fs_cmds::set_file_or_directory_favorite(
				ui,
				client,
				working_path,
				&file_or_directory,
				false,
			)
			.await?;
			None
		}
		Commands::ListTrash => {
			fs_cmds::list_trash(ui, client).await?;
			None
		}
		Commands::TrashRestore => {
			fs_cmds::select_trash_item(ui, client, fs_cmds::TrashAction::Restore).await?;
			None
		}
		Commands::TrashDelete => {
			fs_cmds::select_trash_item(ui, client, fs_cmds::TrashAction::Delete).await?;
			None
		}
		Commands::EmptyTrash => {
			fs_cmds::empty_trash(ui, client).await?;
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
			rclone_cmds::execute_rclone(config, ui, client, cmd).await?;
			None
		}
		Commands::Mount {
			mount_point,
			cache_size,
			transfers,
			rclone_args,
		} => {
			rclone_cmds::mount(
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
			rclone_cmds::start_server(
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
