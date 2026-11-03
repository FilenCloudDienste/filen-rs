use std::borrow::Cow;

use crate::{
	CommandResult,
	auth::LazyClient,
	commands::CommandContext,
	completion::FilenCompleter,
	docs::DocElement,
	ui::{self, UI},
	util::RemotePath,
};
use anyhow::{Context as _, Result};
use clap::Subcommand;
use console::style;
use filen_sdk_rs::{
	auth::Client,
	fs::{
		HasName as _, HasParent as _, HasUUID,
		categories::{DirType, NonRootFileType, NonRootItemType, Normal, fs::CategoryFS},
		dir::meta::DirectoryMetaChanges,
		file::{meta::FileMetaChanges, traits::HasFileInfo as _},
	},
	io::{RemoteDirectory, RemoteFile, client_impl::IoSharedClientExt},
};
use serde_json::json;

#[derive(Debug, Subcommand)]
pub(crate) enum FsCommands {
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
		/// Permanently delete the file or directory, asking first unless --yes is given (default:
		/// move to trash)
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
	/// List favorited items
	ListFavorites,
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
	/// List recently accessed items
	ListRecents,
	/// List trashed items
	ListTrash,
	/// Restore a trashed item interactively
	TrashRestore,
	/// Permanently delete a trashed item interactively (--yes skips the last confirmation)
	TrashDelete,
	/// Permanently delete all trashed items
	EmptyTrash,
}

/// The commands that open the docs' "Accessing Files" section, before `search`.
pub(crate) const BROWSING_DOCS: &[DocElement] = &[
	DocElement::CommandHelp("cd"),
	DocElement::CommandHelp("ls"),
	DocElement::CommandHelp("cat"),
	DocElement::CommandHelp("head"),
	DocElement::CommandHelp("tail"),
	DocElement::CommandHelp("stat"),
];

/// The commands of the docs' "Accessing Files" section after `search`.
pub(crate) const ORGANIZING_DOCS: &[DocElement] = &[
	DocElement::CommandHelp("mkdir"),
	DocElement::CommandHelp("rm"),
	DocElement::CommandHelp("mv"),
	DocElement::CommandHelp("cp"),
	DocElement::CommandHelp("list-favorites"),
	DocElement::CommandHelp("favorite"),
	DocElement::CommandHelp("unfavorite"),
	DocElement::CommandHelp("list-recents"),
	DocElement::CommandHelp("list-trash"),
	DocElement::CommandHelp("trash-restore"),
	DocElement::CommandHelp("trash-delete"),
	DocElement::CommandHelp("empty-trash"),
];

impl FsCommands {
	pub(super) async fn execute(self, ctx: CommandContext<'_>) -> Result<CommandResult> {
		let CommandContext {
			ui,
			client,
			working_path,
			..
		} = ctx;
		match self {
			Self::Cd { directory } => {
				let working_path = cd(ui, client, working_path, &directory).await?;
				return Ok(CommandResult {
					working_path: Some(working_path),
					..Default::default()
				});
			}
			Self::Ls { directory, long } => {
				list_directory(ui, client, working_path, directory, long).await?;
			}
			Self::Cat { file } => {
				print_file(ui, client, working_path, &file, PrintFileLines::Full).await?;
			}
			Self::Head { file, lines } => {
				print_file(ui, client, working_path, &file, PrintFileLines::Head(lines)).await?;
			}
			Self::Tail { file, lines } => {
				print_file(ui, client, working_path, &file, PrintFileLines::Tail(lines)).await?;
			}
			Self::Stat { file_or_directory } => {
				print_file_or_directory_info(ui, client, working_path, &file_or_directory).await?;
			}
			Self::Mkdir {
				directory,
				recursive,
			} => {
				create_directory(ui, client, working_path, &directory, recursive).await?;
			}
			Self::Rm {
				file_or_directory,
				permanent,
			} => {
				delete_file_or_directory(ui, client, working_path, &file_or_directory, permanent)
					.await?;
			}
			Self::Mv {
				source,
				destination,
			} => {
				move_file_or_directory(ui, client, working_path, &source, &destination).await?;
			}
			Self::Cp {
				source,
				destination,
			} => {
				copy_file_or_directory(ui, client, working_path, &source, &destination).await?;
			}
			Self::ListFavorites => list_favorites(ui, client).await?,
			Self::Favorite { file_or_directory } => {
				set_file_or_directory_favorite(ui, client, working_path, &file_or_directory, true)
					.await?;
			}
			Self::Unfavorite { file_or_directory } => {
				set_file_or_directory_favorite(ui, client, working_path, &file_or_directory, false)
					.await?;
			}
			Self::ListRecents => list_recents(ui, client).await?,
			Self::ListTrash => list_trash(ui, client).await?,
			Self::TrashRestore => select_trash_item(ui, client, TrashAction::Restore).await?,
			Self::TrashDelete => select_trash_item(ui, client, TrashAction::Delete).await?,
			Self::EmptyTrash => empty_trash(ui, client).await?,
		}
		Ok(CommandResult::default())
	}
}

pub(crate) async fn cd(
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

pub(crate) async fn list_directory(
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

pub(crate) fn print_items_after_list(
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
	print_item_names(
		ui,
		directories,
		files,
		&format!("{} is empty", directory_label.unwrap_or("Directory")),
		long,
	)
}

/// Prints an already resolved listing: directory names (or paths) first, then file names (or
/// paths) with their sizes. Both are expected to be sorted the way they should be printed.
fn print_item_names(
	ui: &mut UI,
	directories: Vec<String>,
	files: Vec<(String, u64)>,
	empty_message: &str,
	long: bool,
) -> Result<()> {
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
		ui.print_muted(empty_message);
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

pub(crate) async fn list_directory_by_dir(
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

pub(crate) enum PrintFileLines {
	Full,
	Head(usize),
	Tail(usize),
}

/// The largest file, in bytes, that is printed to a terminal without asking first.
const PRINT_WITHOUT_ASKING_MAX_SIZE: u64 = 8 * 1024;

/// Whether to print a file of `size` bytes: a larger one than
/// [`PRINT_WITHOUT_ASKING_MAX_SIZE`] only after asking, and only when a person reads the output.
/// Output piped into another program or printed as JSON was asked for as it is.
fn confirm_printing_file(ui: &mut UI, size: u64) -> Result<bool> {
	if size <= PRINT_WITHOUT_ASKING_MAX_SIZE || !ui.output_is_read_by_a_person() {
		return Ok(true);
	}
	ui.prompt_confirm(
		&format!(
			"File is {} (more than {}), do you want to continue?",
			ui::format_size(size),
			ui::format_size(PRINT_WITHOUT_ASKING_MAX_SIZE)
		),
		false,
	)
}

pub(crate) async fn print_file(
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
	if confirm_printing_file(ui, file.size())? {
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

pub(crate) async fn print_file_or_directory_info(
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

pub(crate) async fn create_directory(
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

pub(crate) async fn create_directory_(
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
	let Some(name) = directory.basename() else {
		return Err(UI::failure("Cannot create root directory"));
	};
	client
		.create_dir(&parent, name)
		.await
		.context("Failed to create directory")
}

pub(crate) async fn delete_file_or_directory(
	ui: &mut UI,
	client: &mut LazyClient,
	working_path: &RemotePath,
	file_or_directory_str: &str,
	permanent: bool,
) -> Result<()> {
	let file_or_directory_str = working_path.navigate(file_or_directory_str).0;
	let confirmation = format!("Permanently delete {}?", file_or_directory_str);
	// before logging in and looking the item up, so that a script missing --yes fails at once
	if permanent {
		ui.ensure_can_confirm(&confirmation)?;
	}
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
	if permanent && !ui.prompt_confirm(&confirmation, false)? {
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
/// moved to that path's parent directory and renamed to that path's base name. A destination
/// ending in '/' must be an existing directory.
pub(crate) async fn move_file_or_directory(
	ui: &mut UI,
	client: &mut LazyClient,
	working_path: &RemotePath,
	source_str: &str,
	destination_str: &str,
) -> Result<()> {
	let source_path = working_path.navigate(source_str);
	// before navigating, which drops the trailing '/'
	let destination_must_be_a_directory = names_a_directory(destination_str);
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
		Some(NonRootFileType::File(_)) if destination_must_be_a_directory => {
			return Err(UI::failure(&format!(
				"Not a directory: {}",
				destination_path.0
			)));
		}
		Some(NonRootFileType::File(_)) => {
			return Err(UI::failure(&format!(
				"Destination already exists: {}",
				destination_path.0
			)));
		}
		None if destination_must_be_a_directory => {
			return Err(UI::failure(&format!(
				"No such destination directory: {}",
				destination_path.0
			)));
		}
		None => None,
	};
	let (new_name, new_path, destination_dir_path) = match &destination_dir {
		// the destination is an existing directory, so move the source into it as-is
		Some(_) => {
			let new_path = destination_path.navigate(&source_filename);
			if new_path == source_path {
				return Err(UI::failure(&format!(
					"{} is already in {}",
					source_path.0, destination_path.0
				)));
			}
			(source_filename.clone(), new_path, destination_path.clone())
		}
		// the destination doesn't exist, so it names the source's new path
		None => {
			// only the root has no base name, and the root always exists
			let Some(new_name) = destination_path.basename() else {
				return Err(UI::failure("Cannot move onto the root directory"));
			};
			(
				new_name.to_string(),
				destination_path.clone(),
				destination_path.parent(),
			)
		}
	};

	if new_path.0.starts_with(&format!("{}/", source_path.0)) {
		return Err(UI::failure(&format!(
			"Cannot move {} into itself: {}",
			source_path.0, new_path.0
		)));
	}

	let destination_dir = match destination_dir {
		Some(destination_dir) => destination_dir,
		None => match client
			.find_item_at_path(&destination_dir_path.0)
			.await
			.context("Failed to find destination directory")?
		{
			Some(NonRootFileType::Dir(dir)) => DirType::Dir(dir),
			Some(NonRootFileType::Root(root)) => DirType::Root(root),
			Some(NonRootFileType::File(_)) => {
				return Err(UI::failure(&format!(
					"Not a directory: {}",
					destination_dir_path.0
				)));
			}
			None => {
				return Err(UI::failure(&format!(
					"No such destination directory: {}",
					destination_dir_path.0
				)));
			}
		},
	};

	// The server treats names that differ only in case as the same, so the destination
	// directory is listed to compare them case-insensitively. The moved item does not clash with
	// itself, so renaming it to a name that differs from its own only in case works.
	let (existing_dirs, existing_files) = client
		.list_dir::<_, Normal>(&destination_dir, None::<&fn(u64, Option<u64>)>)
		.await
		.context("Failed to list destination directory")?;
	let source_uuid = source.uuid();
	let existing_names = existing_dirs
		.iter()
		.map(|dir| (dir.uuid(), dir.name()))
		.chain(existing_files.iter().map(|file| (file.uuid(), file.name())))
		.filter(|(uuid, _)| *uuid != source_uuid)
		.filter_map(|(_, name)| name);
	if let Some(existing_name) = clashing_name(existing_names, &new_name) {
		return Err(UI::failure(&format!(
			"Destination already exists: {}",
			destination_dir_path.navigate(existing_name)
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

/// Whether a raw `mv` destination names a directory to move into rather than a new path: it ends
/// in '/', as in Unix `mv a dir/`.
fn names_a_directory(destination: &str) -> bool {
	destination.ends_with('/')
}

pub(crate) async fn copy_file_or_directory(
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
	// A same-name upload would replace the existing file (as a new version) and creating a
	// same-name directory returns the existing one, so the copy would merge into it.
	let source_name = match &source_file_or_directory {
		NonRootFileType::File(file) => file.name(),
		NonRootFileType::Dir(dir) => dir.name(),
		NonRootFileType::Root(_) => return Err(UI::failure("Cannot copy root directory")),
	}
	.context("Failed to decrypt source name")?;
	let (existing_dirs, existing_files) = client
		.list_dir::<_, Normal>(&destination_dir, None::<&fn(u64, Option<u64>)>)
		.await
		.context("Failed to list destination directory")?;
	let existing_names = existing_dirs
		.iter()
		.filter_map(|dir| dir.name())
		.chain(existing_files.iter().filter_map(|file| file.name()));
	if let Some(existing_name) = clashing_name(existing_names, source_name) {
		return Err(UI::failure(&format!(
			"Destination already exists: {}",
			destination_str.navigate(existing_name)
		)));
	}
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

/// The name among `existing` that the server treats as the same as `name`: it compares names
/// case-insensitively.
fn clashing_name<'a>(existing: impl IntoIterator<Item = &'a str>, name: &str) -> Option<&'a str> {
	let name = name.to_lowercase();
	existing
		.into_iter()
		.find(|existing| existing.to_lowercase() == name)
}

pub(crate) async fn copy_file(
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

pub(crate) async fn copy_dir_recursive(
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

pub(crate) async fn set_file_or_directory_favorite(
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

pub(crate) async fn list_favorites(ui: &mut UI, client: &mut LazyClient) -> Result<()> {
	let client = client.get(ui).await?;
	let (dirs, files) = client
		.list_favorites(None::<&fn(u64, Option<u64>)>)
		.await
		.context("Failed to list favorites")?;
	print_items_with_full_paths(ui, client, dirs, files, "No favorited items").await
}

pub(crate) async fn list_recents(ui: &mut UI, client: &mut LazyClient) -> Result<()> {
	let client = client.get(ui).await?;
	let (dirs, files) = client
		.list_recents(None::<&fn(u64, Option<u64>)>)
		.await
		.context("Failed to list recents")?;
	print_items_with_full_paths(ui, client, dirs, files, "No recent items").await
}

/// Prints a listing whose items don't share a parent, so a bare name wouldn't identify them:
/// every item is resolved to its absolute path first.
pub(crate) async fn print_items_with_full_paths(
	ui: &mut UI,
	client: &Client,
	dirs: Vec<RemoteDirectory>,
	files: Vec<RemoteFile>,
	empty_message: &str,
) -> Result<()> {
	// resolving a path walks up the tree, and `get_item_path` takes the drive lock for each
	// walk — holding one here means those all share it instead of re-acquiring it per item
	let _lock = client.lock_drive().await.context("Failed to lock drive")?;
	let mut directories = Vec::with_capacity(dirs.len());
	for dir in &dirs {
		directories.push(full_path(client, &NonRootItemType::Dir(Cow::Borrowed(dir))).await);
	}
	directories.sort();
	let mut files = {
		let mut resolved = Vec::with_capacity(files.len());
		for file in &files {
			resolved.push((
				full_path(client, &NonRootItemType::File(Cow::Borrowed(file))).await,
				file.size(),
			));
		}
		resolved
	};
	files.sort_by(|(a, _), (b, _)| a.cmp(b));
	print_item_names(ui, directories, files, empty_message, false)
}

/// The item's absolute path, with the trailing slash `get_item_path` puts on directories
/// stripped (directories are already distinguished by color). Falls back to the UUID, like the
/// name-only listings do, so one unresolvable item doesn't drop the rest of the listing.
async fn full_path(client: &Client, item: &NonRootItemType<'_, Normal>) -> String {
	match client.get_item_path(item).await {
		Ok((path, _)) => format!("/{}", path.trim_end_matches('/')),
		Err(e) => {
			log::warn!("Failed to resolve path of item {}: {}", item.uuid(), e);
			item.uuid().to_string()
		}
	}
}

pub(crate) async fn list_trash(ui: &mut UI, client: &mut LazyClient) -> Result<()> {
	let client = client.get(ui).await?;
	let (dirs, files) = client
		.list_trash(None::<&fn(u64, Option<u64>)>)
		.await
		.context("Failed to list trash")?;
	print_items_after_list(ui, dirs, files, Some("Trash"), false)
}

pub(crate) enum TrashAction {
	Restore,
	Delete,
}

pub(crate) enum TrashItem {
	Dir(RemoteDirectory),
	File(RemoteFile),
}

impl TrashItem {
	pub(crate) fn uuid_string(&self) -> String {
		match self {
			TrashItem::Dir(dir) => dir.uuid().to_string(),
			TrashItem::File(file) => file.uuid().to_string(),
		}
	}
}

/// Lists the trash and lets the user pick one item to restore or permanently delete.
pub(crate) async fn select_trash_item(
	ui: &mut UI,
	client: &mut LazyClient,
	action: TrashAction,
) -> Result<()> {
	let select_message = match action {
		TrashAction::Restore => "Select an item to restore",
		TrashAction::Delete => "Select an item to permanently delete",
	};
	// before logging in and listing the trash, so that a script fails at once: nothing answers
	// the selection, not even --yes
	ui.ensure_can_prompt(select_message)?;
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
	let Some(selection) = ui.prompt_select(select_message, options.clone())? else {
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

pub(crate) async fn empty_trash(ui: &mut UI, client: &mut LazyClient) -> Result<()> {
	let client = client.get(ui).await?;
	client.empty_trash().await?;
	ui.print_success("Emptied trash");
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::ui::test_support::Printed;

	#[test]
	fn a_destination_ending_in_a_slash_names_a_directory() {
		assert!(names_a_directory("dir/"));
		assert!(names_a_directory("/a/b/"));
		assert!(names_a_directory("/"));
	}

	#[test]
	fn a_destination_without_a_trailing_slash_may_name_a_new_path() {
		assert!(!names_a_directory("dir"));
		assert!(!names_a_directory("/a/b"));
		assert!(!names_a_directory("dir/."));
		assert!(!names_a_directory(""));
	}

	#[test]
	fn a_name_clashes_with_the_same_name() {
		assert_eq!(
			clashing_name(["other.txt", "a.txt"], "a.txt"),
			Some("a.txt")
		);
	}

	#[test]
	fn a_name_clashes_with_a_name_differing_only_in_case() {
		assert_eq!(clashing_name(["Notes", "b"], "notes"), Some("Notes"));
		assert_eq!(clashing_name(["ÄRGER.md"], "ärger.md"), Some("ÄRGER.md"));
	}

	#[test]
	fn a_name_does_not_clash_with_different_names() {
		assert_eq!(clashing_name(["a.txt.bak", "b.txt"], "a.txt"), None);
		assert_eq!(clashing_name([], "a.txt"), None);
	}

	#[test]
	fn a_file_up_to_the_limit_is_printed_to_a_terminal_without_asking() {
		let mut ui = Printed::default().terminal_ui();

		assert!(confirm_printing_file(&mut ui, PRINT_WITHOUT_ASKING_MAX_SIZE).unwrap());
	}

	#[test]
	fn a_large_file_is_printed_without_asking_when_stdout_is_not_a_terminal() {
		let mut ui = Printed::default().ui();

		assert!(confirm_printing_file(&mut ui, 1024 * 1024).unwrap());
	}

	#[test]
	fn a_large_file_is_printed_to_a_terminal_when_yes_confirms_it() {
		let mut ui = Printed::default().terminal_ui();
		ui.set_assume_yes(true);

		assert!(confirm_printing_file(&mut ui, PRINT_WITHOUT_ASKING_MAX_SIZE + 1).unwrap());
	}

	#[test]
	fn a_large_file_printed_to_a_terminal_without_input_asks_for_yes() {
		let mut ui = Printed::default().terminal_ui();

		let err = confirm_printing_file(&mut ui, 12 * 1024).unwrap_err();
		assert_eq!(
			err.to_string(),
			"Cannot ask \"File is 12 KiB (more than 8 KiB), do you want to continue?\": stdin is \
			 not a terminal. Pass --yes to answer yes."
		);
	}

	#[test]
	fn a_large_file_is_printed_without_asking_as_json() {
		let mut ui = Printed::default().terminal_ui();
		ui.json = true;

		assert!(confirm_printing_file(&mut ui, 1024 * 1024).unwrap());
	}
}
