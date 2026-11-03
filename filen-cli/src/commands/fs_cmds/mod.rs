use std::{
	borrow::Cow,
	cmp::Ordering,
	env::{self, VarError},
	ffi::OsStr,
	fs::{self, File},
	io::{self, BufRead as _, BufReader, IsTerminal as _, Read as _},
	iter::Peekable,
	path::{Path, PathBuf},
	pin::pin,
	str::Chars,
	sync::{Arc, Mutex, MutexGuard, PoisonError},
	thread,
};

use crate::{
	CommandResult,
	auth::LazyClient,
	commands::CommandContext,
	completion::FilenCompleter,
	docs::DocElement,
	error::{CliError, find_cli_error},
	interrupt,
	ui::{self, UI},
	util::RemotePath,
};
use anyhow::{Context as _, Result};
use chrono::{DateTime, Utc};
use clap::Subcommand;
use console::{Term, style};
use filen_sdk_rs::{
	ErrorKind,
	auth::Client,
	consts::CHUNK_SIZE_U64,
	fs::{
		HasName as _, HasParent as _, HasRemoteInfo as _, HasUUID,
		categories::{
			DirType, NonRootFileType, NonRootItemType, Normal,
			fs::{CategoryFS, ObjectOrRemainingPath},
		},
		copy::{
			CopiedTopLevel, CopyCallback, CopyConfig, CopyFailed, CopyPhase, CopyReport,
			CopyRequest, CopyUpdate, ItemCounts, ItemSource, ItemSourceDir, JobControl, PlanTotals,
			PlannedTopLevelItem, ScanProgress, SkipReason,
		},
		dir::meta::DirectoryMetaChanges,
		file::{
			FileBuilderOptionalName, client_impl::FileReaderSharedClientExt as _,
			meta::FileMetaChanges, read::FileReader, traits::HasFileInfo as _,
		},
		name::ValidatedName,
	},
	io::{RemoteDirectory, RemoteFile, client_impl::IoSharedClientExt as _},
};
use filen_types::{fs::Uuid, traits::CowHelpers as _};
use futures::{
	AsyncBufReadExt as _, AsyncRead, AsyncReadExt as _, SinkExt as _, TryStreamExt as _,
	channel::mpsc, executor,
};
use serde_json::json;
use tempfile::{NamedTempFile, TempDir};
use tokio::select;

#[derive(Debug, Subcommand)]
pub(crate) enum FsCommands {
	/// Change the working directory (in REPL)
	Cd {
		/// Directory to navigate into (supports "..")
		#[arg(add = FilenCompleter::directory())]
		directory: String,
	},
	/// Print the working directory
	Pwd,
	/// List files in a directory
	Ls {
		/// Directory to list files in, or a file to show (default: the current working directory)
		#[arg(add = FilenCompleter::file_or_directory())]
		directory: Option<String>,
		/// Use a long listing format: sizes, modification dates (creation dates of directories)
		/// and (*) after favorited items
		#[arg(short = 'l', long)]
		long: bool,
	},
	/// Print the contents of a file as the bytes it holds (with --json: {"text": ...}, for UTF-8
	/// text only)
	Cat {
		/// File to print
		#[arg(add = FilenCompleter::file())]
		file: String,
	},
	/// Print the first lines of a file as the bytes they hold (with --json: {"text": ...})
	Head {
		/// File to print
		#[arg(add = FilenCompleter::file())]
		file: String,
		/// Number of lines to print
		#[arg(short = 'n', long, default_value_t = 10)]
		lines: usize,
	},
	/// Print the last lines of a file as the bytes they hold (with --json: {"text": ...})
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
	/// Open a file in the app your system opens its type with, from a local copy that is kept
	Open {
		/// File to open
		#[arg(add = FilenCompleter::file())]
		file: String,
	},
	/// Edit a local copy of a file in $VISUAL, $EDITOR or the app your system opens its type with,
	/// then upload your changes as a new version (kept locally instead if the file changed
	/// meanwhile)
	Edit {
		/// File to edit
		#[arg(add = FilenCompleter::file())]
		file: String,
	},
	/// Print the web drive's URL of a directory, or of a file's directory, and open it in the
	/// browser
	View {
		/// File or directory to view (default: the current working directory)
		#[arg(add = FilenCompleter::file_or_directory())]
		file_or_directory: Option<String>,
		/// Only print the URL
		#[arg(long)]
		no_open: bool,
	},
	/// Print the web drive's URL and open it in the browser
	Drive {
		/// Only print the URL
		#[arg(long)]
		no_open: bool,
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
	/// Write text to a file, creating it or replacing its content
	Write {
		/// File to write to
		#[arg(add = FilenCompleter::file())]
		file: String,
		/// Text to write, its words joined by spaces (default: what stdin reads when it is not a
		/// terminal, otherwise nothing)
		content: Vec<String>,
	},
	/// Create an empty file, or set the modification date of an existing one to now
	Touch {
		/// File to create or touch
		#[arg(add = FilenCompleter::file())]
		file: String,
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
		/// Destination: an existing directory to copy the source into,
		/// or the path of the copy (to copy it under a new name)
		#[arg(add = FilenCompleter::file_or_directory())]
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
	ListTrash {
		/// Use a long listing format: sizes, modification dates (creation dates of directories)
		/// and (*) after favorited items
		#[arg(short = 'l', long)]
		long: bool,
	},
	/// Restore trashed items
	TrashRestore {
		/// Trashed items by name, or by UUID when several share a name
		/// (default: pick one interactively)
		items: Vec<String>,
		/// Restore every trashed item
		#[arg(long, conflicts_with = "items")]
		all: bool,
	},
	/// Permanently delete trashed items, asking first unless --yes is given
	TrashDelete {
		/// Trashed items by name, or by UUID when several share a name
		/// (default: pick one interactively)
		items: Vec<String>,
		/// Permanently delete every trashed item
		#[arg(long, conflicts_with = "items")]
		all: bool,
	},
	/// Permanently delete all trashed items, asking first unless --yes is given
	EmptyTrash,
}

/// The commands that open the docs' "Accessing Files" section, before `search`.
pub(crate) const BROWSING_DOCS: &[DocElement] = &[
	DocElement::CommandHelp("cd"),
	DocElement::CommandHelp("pwd"),
	DocElement::CommandHelp("ls"),
	DocElement::CommandHelp("cat"),
	DocElement::CommandHelp("head"),
	DocElement::CommandHelp("tail"),
	DocElement::CommandHelp("stat"),
	DocElement::CommandHelp("open"),
	DocElement::CommandHelp("edit"),
	DocElement::CommandHelp("view"),
	DocElement::CommandHelp("drive"),
];

/// The commands of the docs' "Accessing Files" section after `search`.
pub(crate) const ORGANIZING_DOCS: &[DocElement] = &[
	DocElement::CommandHelp("mkdir"),
	DocElement::CommandHelp("rm"),
	DocElement::CommandHelp("write"),
	DocElement::CommandHelp("touch"),
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
			Self::Pwd => print_working_path(ui, working_path)?,
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
			Self::Open { file } => open_file(ui, client, working_path, &file).await?,
			Self::Edit { file } => edit_file(ui, client, working_path, &file).await?,
			Self::View {
				file_or_directory,
				no_open,
			} => {
				let path = working_path.navigate(file_or_directory.as_deref().unwrap_or(""));
				view_in_web_drive(ui, client, &path, !no_open).await?;
			}
			Self::Drive { no_open } => {
				view_in_web_drive(ui, client, &RemotePath::new("/"), !no_open).await?;
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
			Self::Write { file, content } => {
				write_file(ui, client, working_path, &file, &content).await?;
			}
			Self::Touch { file } => touch_file(ui, client, working_path, &file).await?,
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
			Self::ListTrash { long } => list_trash(ui, client, long).await?,
			Self::TrashRestore { items, all } => {
				restore_trash_items(ui, client, TrashSelection::of(items, all)).await?;
			}
			Self::TrashDelete { items, all } => {
				delete_trash_items(ui, client, TrashSelection::of(items, all)).await?;
			}
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

/// Prints the working directory: the REPL's, or the one `--working-path` gives.
fn print_working_path(ui: &mut UI, working_path: &RemotePath) -> Result<()> {
	if ui.json {
		ui.print_json(json!({ "path": working_path.0 }))
	} else {
		ui.print(&working_path.0);
		Ok(())
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
			"No such file or directory: {}",
			directory_str
		)));
	};
	let directory: DirType<'_, Normal> = match directory {
		NonRootFileType::Dir(dir) => DirType::Dir(dir),
		NonRootFileType::Root(root) => DirType::Root(root),
		NonRootFileType::File(file) => {
			// like `ls` on a local file: that one file, the way a listing shows it
			let item = ListedItem::of_file(&file);
			return print_listing(ui, &[item], ListedBy::Name, "", long);
		}
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
	let mut items = dirs
		.iter()
		.map(ListedItem::of_dir)
		.chain(files.iter().map(ListedItem::of_file))
		.collect::<Vec<ListedItem>>();
	sort_listing(&mut items, ListedBy::Name);
	print_listing(
		ui,
		&items,
		ListedBy::Name,
		&format!("{} is empty", directory_label.unwrap_or("Directory")),
		long,
	)
}

/// Whether an item is a directory or a file. Listings print directories first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ListedKind {
	Directory,
	File,
}

impl ListedKind {
	/// The kind's name, as `--json` prints it.
	fn name(self) -> &'static str {
		match self {
			ListedKind::Directory => "directory",
			ListedKind::File => "file",
		}
	}
}

/// What a listing shows to identify its items.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ListedBy {
	/// The items share a parent, so their names identify them.
	Name,
	/// The items don't share a parent, so only their absolute paths identify them.
	Path,
}

/// One item of a listing, with everything `ls` and the `list-*` commands print of it.
#[derive(Debug, Clone, PartialEq)]
struct ListedItem {
	kind: ListedKind,
	/// The item's name, or its UUID when the name cannot be decrypted.
	name: String,
	/// The item's absolute path, in a listing by path, or `None` when it could not be resolved
	/// (or the listing is by name).
	path: Option<String>,
	/// The file's size in bytes; directories have none.
	size: Option<u64>,
	/// When the file was last modified; directories have no such date.
	modified: Option<DateTime<Utc>>,
	created: Option<DateTime<Utc>>,
	favorited: bool,
	uuid: Uuid,
}

impl ListedItem {
	fn of_dir(dir: &RemoteDirectory) -> Self {
		Self {
			kind: ListedKind::Directory,
			name: name_or_uuid(dir.name(), dir.uuid()),
			path: None,
			size: None,
			modified: None,
			created: dir.created(),
			favorited: dir.favorited(),
			uuid: dir.uuid(),
		}
	}

	fn of_file(file: &RemoteFile) -> Self {
		Self {
			kind: ListedKind::File,
			name: name_or_uuid(file.name(), file.uuid()),
			path: None,
			size: Some(file.size()),
			modified: file.last_modified(),
			created: file.created(),
			favorited: file.favorited(),
			uuid: file.uuid(),
		}
	}

	/// What the listing prints for the item: its name, or in a listing by path its path, falling
	/// back to its UUID like an undecryptable name does, so that one unresolvable item doesn't drop
	/// the rest of the listing.
	fn label(&self, by: ListedBy) -> Cow<'_, str> {
		match (by, &self.path) {
			(ListedBy::Name, _) => Cow::Borrowed(&self.name),
			(ListedBy::Path, Some(path)) => Cow::Borrowed(path),
			(ListedBy::Path, None) => Cow::Owned(self.uuid.to_string()),
		}
	}

	/// The item as `--json` prints it, with the keys `stat` uses, plus its path in a listing by
	/// path (`null` when it could not be resolved).
	fn to_json(&self, by: ListedBy) -> serde_json::Value {
		let mut object = serde_json::Map::new();
		object.insert("name".to_owned(), json!(self.name));
		if by == ListedBy::Path {
			object.insert("path".to_owned(), json!(self.path));
		}
		object.insert("type".to_owned(), json!(self.kind.name()));
		if self.kind == ListedKind::File {
			object.insert("size".to_owned(), json!(self.size));
			object.insert("modified".to_owned(), json!(self.modified));
		}
		object.insert("created".to_owned(), json!(self.created));
		object.insert("favorited".to_owned(), json!(self.favorited));
		object.insert("uuid".to_owned(), json!(self.uuid));
		serde_json::Value::Object(object)
	}

	/// The date `ls -l` shows: when a file was last modified, when a directory was created.
	fn date(&self) -> Option<DateTime<Utc>> {
		match self.kind {
			ListedKind::Directory => self.created,
			ListedKind::File => self.modified,
		}
	}
}

fn name_or_uuid(name: Option<&str>, uuid: Uuid) -> String {
	name.map(str::to_string).unwrap_or_else(|| uuid.to_string())
}

/// Sorts a listing the way it is printed (see [`listing_order`]).
fn sort_listing(items: &mut [ListedItem], by: ListedBy) {
	items.sort_by(|a, b| listing_order(a, b, by));
}

/// The order listings are printed in: directories first, each group by label in
/// [`natural_cmp`] order.
fn listing_order(a: &ListedItem, b: &ListedItem, by: ListedBy) -> Ordering {
	a.kind
		.cmp(&b.kind)
		.then_with(|| natural_cmp(&a.label(by), &b.label(by)))
}

/// Orders names the way a person reads them: ignoring case, and with each run of digits compared
/// as a number, so that `file2` comes before `file10`. Names equal in that order (`readme` and
/// `README`, `file02` and `file2`) are ordered by their characters, so that the order is total
/// and the same every time.
fn natural_cmp(a: &str, b: &str) -> Ordering {
	natural_cmp_folded(a, b).then_with(|| a.cmp(b))
}

fn natural_cmp_folded(a: &str, b: &str) -> Ordering {
	let mut a = a.chars().peekable();
	let mut b = b.chars().peekable();
	loop {
		let (Some(&a_char), Some(&b_char)) = (a.peek(), b.peek()) else {
			return a.peek().is_some().cmp(&b.peek().is_some());
		};
		let order = if a_char.is_ascii_digit() && b_char.is_ascii_digit() {
			cmp_numbers(&take_digits(&mut a), &take_digits(&mut b))
		} else {
			a.next();
			b.next();
			a_char.to_lowercase().cmp(b_char.to_lowercase())
		};
		if order != Ordering::Equal {
			return order;
		}
	}
}

/// Takes the run of ASCII digits `chars` starts with.
fn take_digits(chars: &mut Peekable<Chars<'_>>) -> String {
	let mut digits = String::new();
	while let Some(digit) = chars.next_if(char::is_ascii_digit) {
		digits.push(digit);
	}
	digits
}

/// Compares two runs of ASCII digits by the numbers they write, however long they are.
fn cmp_numbers(a: &str, b: &str) -> Ordering {
	let a = a.trim_start_matches('0');
	let b = b.trim_start_matches('0');
	a.len().cmp(&b.len()).then_with(|| a.cmp(b))
}

/// Prints one item per line: a right-aligned size column (directories have none), the date
/// column, the label, and `(*)` after a favorited item.
fn print_long_listing(ui: &mut UI, items: &[ListedItem], by: ListedBy) {
	let rows = items
		.iter()
		.map(|item| {
			(
				item.size.map_or_else(|| "-".to_string(), ui::format_size),
				item.date()
					.map_or_else(|| "-".to_string(), |date| ui::format_date(&date)),
			)
		})
		.collect::<Vec<(String, String)>>();
	let size_width = rows.iter().map(|(size, _)| size.len()).max().unwrap_or(0);
	let date_width = rows.iter().map(|(_, date)| date.len()).max().unwrap_or(0);
	for (item, (size, date)) in items.iter().zip(rows) {
		// pad before styling, so the ANSI codes don't count towards the column width
		let size = format!("{:>size_width$}", size);
		let date = format!("{:<date_width$}", date);
		let label = match item.kind {
			ListedKind::Directory => style(item.label(by)).blue().to_string(),
			ListedKind::File => item.label(by).into_owned(),
		};
		let size = match item.kind {
			ListedKind::Directory => style(size).dim().to_string(),
			ListedKind::File => size,
		};
		let favorite = if item.favorited { " (*)" } else { "" };
		ui.print(&format!(
			"{}  {}  {}{}",
			size,
			style(date).dim(),
			label,
			favorite
		));
	}
}

/// Prints a listing, in the order of `items`.
fn print_listing(
	ui: &mut UI,
	items: &[ListedItem],
	by: ListedBy,
	empty_message: &str,
	long: bool,
) -> Result<()> {
	if ui.json {
		ui.print_json(serde_json::Value::Array(
			items.iter().map(|item| item.to_json(by)).collect(),
		))?;
		return Ok(());
	}
	if items.is_empty() {
		ui.print_muted(empty_message);
		return Ok(());
	}
	if long {
		print_long_listing(ui, items, by);
		return Ok(());
	}
	// print directory names in blue
	let labels = items
		.iter()
		.map(|item| match item.kind {
			ListedKind::Directory => style(item.label(by)).blue().to_string(),
			ListedKind::File => item.label(by).into_owned(),
		})
		.collect::<Vec<String>>();
	ui.print_grid(&labels.iter().map(String::as_str).collect::<Vec<&str>>());
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

impl PrintFileLines {
	fn command(&self) -> &'static str {
		match self {
			Self::Full => "cat",
			Self::Head(_) => "head",
			Self::Tail(_) => "tail",
		}
	}
}

/// Prints a file's content, or its first or last lines, as the bytes it holds, with no newline
/// added: streamed to stdout as they download, but for the last lines, which are printed once it
/// is found where they start. With `--json`, the printed part is collected
/// and printed as `{"text": ...}`, which only UTF-8 text can be.
pub(crate) async fn print_file(
	ui: &mut UI,
	client: &mut LazyClient,
	working_path: &RemotePath,
	file_str: &str,
	lines: PrintFileLines,
) -> Result<()> {
	let file_str = working_path.navigate(file_str);
	let client = client.get(ui).await?;
	let file = find_file(client, &file_str).await?;
	if !confirm_printing_file(ui, file.size())? {
		return Ok(());
	}
	let mut out = if ui.json {
		PrintedContent::Json(Vec::new())
	} else {
		PrintedContent::Stdout
	};
	match lines {
		PrintFileLines::Full => {
			print_from_reader(ui, &mut out, client.get_file_reader(&*file), None).await?;
		}
		// nothing to download
		PrintFileLines::Head(0) | PrintFileLines::Tail(0) => {}
		PrintFileLines::Head(n) => {
			let head = Some(HeadLines::new(n));
			print_from_reader(ui, &mut out, client.get_file_reader(&*file), head).await?;
		}
		PrintFileLines::Tail(n) => {
			let remote_file: &RemoteFile = &file;
			let read_block = move |start, end| async move {
				let mut block = Vec::new();
				client
					.get_file_reader_for_range(remote_file, start, end)
					.read_to_end(&mut block)
					.await
					.context("Failed to download file")?;
				Ok(block)
			};
			match last_lines(file.size(), n, CHUNK_SIZE_U64, TAIL_HELD_MAX, read_block).await? {
				LastLines::Read(blocks) => {
					for block in blocks {
						out.write(ui, &block)?;
					}
				}
				LastLines::From(start) => {
					let reader = client.get_file_reader_for_range(&*file, start, file.size());
					print_from_reader(ui, &mut out, reader, None).await?;
				}
			}
		}
	}
	if let PrintedContent::Json(content) = out {
		let text = String::from_utf8(content).map_err(|_| {
			UI::failure(&format!(
				"Not UTF-8 text: {file_str}. Use {} without --json for binary content",
				lines.command()
			))
		})?;
		ui.print_json(json!({ "text": text }))?;
	}
	Ok(())
}

/// Where [`print_file`] puts the content it prints.
enum PrintedContent {
	/// Straight to stdout.
	Stdout,
	/// Collected, to be printed as JSON.
	Json(Vec<u8>),
}

impl PrintedContent {
	fn write(&mut self, ui: &mut UI, bytes: &[u8]) -> Result<()> {
		match self {
			Self::Stdout => ui.write_stdout_bytes(bytes),
			Self::Json(content) => {
				content.extend_from_slice(bytes);
				Ok(())
			}
		}
	}
}

/// Prints what `reader` reads, up to the end of the lines `head` takes if it is given. The reader
/// is dropped once those are printed, which stops the rest of the file from downloading.
async fn print_from_reader(
	ui: &mut UI,
	out: &mut PrintedContent,
	mut reader: FileReader<'_>,
	mut head: Option<HeadLines>,
) -> Result<()> {
	loop {
		let chunk = reader.fill_buf().await.context("Failed to download file")?;
		if chunk.is_empty() {
			return Ok(());
		}
		let read = chunk.len();
		let (printed, done) = match &mut head {
			Some(head) => head.cut(chunk),
			None => (chunk, false),
		};
		out.write(ui, printed)?;
		if done {
			return Ok(());
		}
		reader.consume_unpin(read);
	}
}

/// Cuts the first lines out of a file read front to back, a chunk at a time. A line ends after
/// its `\n` (so a `\r\n` line keeps its `\r`); a last line without one is printed as it is.
#[derive(Debug)]
struct HeadLines {
	/// How many lines are still to be printed.
	remaining: usize,
}

impl HeadLines {
	fn new(lines: usize) -> Self {
		Self { remaining: lines }
	}

	/// The part of `chunk`, the file's next bytes, to print, and whether it ends the lines asked
	/// for, so that no more of the file is needed.
	fn cut<'a>(&mut self, chunk: &'a [u8]) -> (&'a [u8], bool) {
		if self.remaining == 0 {
			return (&[], true);
		}
		let mut end = 0;
		for line in chunk.split_inclusive(|&byte| byte == b'\n') {
			end += line.len();
			if line.ends_with(b"\n") {
				self.remaining -= 1;
				if self.remaining == 0 {
					return (chunk.split_at(end).0, true);
				}
			}
		}
		(chunk, false)
	}
}

/// Finds where the last lines of a file start, from blocks of it read back to front. The `\n`
/// that ends the file ends its last line rather than starting an empty one, as in `tail`.
#[derive(Debug)]
struct TailLines {
	/// How many line starts are still to be found.
	remaining: usize,
	file_size: u64,
}

impl TailLines {
	fn new(lines: usize, file_size: u64) -> Self {
		Self {
			remaining: lines,
			file_size,
		}
	}

	/// Scans `block`, the file's bytes from offset `block_start` up to the block scanned before,
	/// and returns the offset the last lines start at if it is in this block. If no block has it,
	/// they start at the file's start.
	fn scan(&mut self, block_start: u64, block: &[u8]) -> Option<u64> {
		if self.remaining == 0 {
			return Some(self.file_size);
		}
		let block_len =
			u64::try_from(block.len()).expect("a usize fits in a u64 (should be impossible)");
		// the offset after the byte looked at
		let mut line_start = block_start + block_len;
		for &byte in block.iter().rev() {
			if byte == b'\n' && line_start != self.file_size {
				self.remaining -= 1;
				if self.remaining == 0 {
					return Some(line_start);
				}
			}
			line_start -= 1;
		}
		None
	}
}

/// How many bytes of a file's last lines `tail` keeps from finding where they start, to print
/// them without downloading them again.
const TAIL_HELD_MAX: u64 = 16 * CHUNK_SIZE_U64;

/// The last lines of a file, as [`last_lines`] finds them.
#[derive(Debug, PartialEq, Eq)]
enum LastLines {
	/// The bytes they hold, in blocks from front to back, as read while finding where they start.
	Read(Vec<Vec<u8>>),
	/// The offset they start at, to download them from: they hold more bytes than were kept.
	From(u64),
}

/// The last `lines` lines of a file of `file_size` bytes. Reads the file back to front in blocks
/// of `block_size` bytes aligned to its start (`read_block(start, end)` reads one), so that only
/// the blocks holding those lines are read, and each of them once: the blocks are kept to print,
/// up to `held_max` bytes of the lines.
async fn last_lines<F, Fut>(
	file_size: u64,
	lines: usize,
	block_size: u64,
	held_max: u64,
	mut read_block: F,
) -> Result<LastLines>
where
	F: FnMut(u64, u64) -> Fut,
	Fut: Future<Output = Result<Vec<u8>>>,
{
	let mut tail = TailLines::new(lines, file_size);
	// the blocks read, back to front, until they hold more than `held_max` bytes
	let mut held = Some(Vec::new());
	let mut held_len = 0_u64;
	let mut end = file_size;
	while end > 0 {
		let start = (end - 1) / block_size * block_size;
		let mut block = read_block(start, end).await?;
		let line_start = tail.scan(start, &block);
		if let Some(line_start) = line_start {
			// the lines start inside this block, after the `\n` that ends the line before them
			let before = usize::try_from(line_start.saturating_sub(start)).expect(
				"an offset into a block held in memory fits in a usize (should be impossible)",
			);
			block.drain(..before.min(block.len()));
		}
		held_len = held_len.saturating_add(
			u64::try_from(block.len()).expect("a usize fits in a u64 (should be impossible)"),
		);
		if held_len > held_max {
			held = None;
		} else if let Some(held) = &mut held
			&& !block.is_empty()
		{
			held.push(block);
		}
		if let Some(line_start) = line_start {
			return Ok(last_lines_from(held, line_start));
		}
		end = start;
	}
	Ok(last_lines_from(held, 0))
}

/// The last lines starting at `start`, from the blocks `held` back to front if they were kept.
fn last_lines_from(held: Option<Vec<Vec<u8>>>, start: u64) -> LastLines {
	match held {
		Some(mut blocks) => {
			blocks.reverse();
			LastLines::Read(blocks)
		}
		None => LastLines::From(start),
	}
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

/// Finds the file at `path`.
async fn find_file<'a>(client: &'a Client, path: &RemotePath) -> Result<Cow<'a, RemoteFile>> {
	match client
		.find_item_at_path(&path.0)
		.await
		.context("Failed to find file")?
	{
		Some(NonRootFileType::File(file)) => Ok(file),
		Some(_) => Err(UI::failure(&format!("Not a file: {}", path))),
		None => Err(UI::failure(&format!("No such file: {}", path))),
	}
}

/// A file downloaded into a fresh temporary directory under the remote file's name, so that the
/// app opening it goes by the right extension. The directory is deleted when this is dropped,
/// unless it is kept.
struct LocalCopy {
	dir: TempDir,
	/// The file's name in the directory.
	name: String,
}

impl LocalCopy {
	async fn download(client: &Client, file: &RemoteFile) -> Result<Self> {
		let dir = tempfile::Builder::new()
			.prefix("filen-cli-")
			.tempdir()
			.context("Failed to create a temporary directory")?;
		let copy = Self {
			dir,
			name: local_file_name(file.name(), file.uuid()),
		};
		client
			.download_file_to_path(file, &copy.path(), None)
			.await
			.context("Failed to download file")?;
		Ok(copy)
	}

	fn path(&self) -> PathBuf {
		self.dir.path().join(&self.name)
	}

	/// Keeps the directory rather than deleting it, and returns the file's path in it.
	fn keep(self) -> PathBuf {
		self.dir.keep().join(self.name)
	}
}

/// The name a local copy of a file gets: the file's, unless it is not one name of a file on this
/// system (`..`, or a name holding a path separator) or cannot be decrypted, in which case the
/// file's UUID. The name comes from the drive, so it must not lead out of the directory.
fn local_file_name(name: Option<&str>, uuid: Uuid) -> String {
	name.filter(|name| Path::new(name).file_name() == Some(OsStr::new(name)))
		.map_or_else(|| uuid.to_string(), str::to_owned)
}

/// Opens a file in the app the system opens its type with, from a local copy. The copy is kept,
/// as the app reads it after the command has ended.
pub(crate) async fn open_file(
	ui: &mut UI,
	client: &mut LazyClient,
	working_path: &RemotePath,
	file_str: &str,
) -> Result<()> {
	let path = working_path.navigate(file_str);
	let client = client.get(ui).await?;
	let file = find_file(client, &path).await?;
	let local = LocalCopy::download(client, &file).await?.keep();
	open::that_detached(&local)
		.map_err(|e| UI::failure(&format!("Failed to open {}: {e}", local.display())))?;
	ui.print_success(&format!(
		"Opened {} (a local copy at {})",
		path,
		local.display()
	));
	Ok(())
}

/// What `edit` asks when it opened the file in the app for its type, which may not wait until
/// the file is edited.
const DONE_EDITING_PROMPT: &str = "Press enter when done editing";

/// The editor `edit` runs: the program, the arguments before the file's path.
#[derive(Debug, PartialEq, Eq)]
struct EditorCommand {
	program: String,
	args: Vec<String>,
}

/// The editor that `$VISUAL`, or else `$EDITOR`, names, split into words as a shell would split
/// them. Empty variables count as unset.
fn editor_command(visual: Option<&str>, editor: Option<&str>) -> Result<Option<EditorCommand>> {
	let Some((variable, command)) = [("VISUAL", visual), ("EDITOR", editor)]
		.into_iter()
		.find_map(|(variable, value)| {
			value
				.filter(|value| !value.trim().is_empty())
				.map(|value| (variable, value))
		})
	else {
		return Ok(None);
	};
	let mut words = shlex::split(command)
		.ok_or_else(|| UI::failure(&format!("Cannot split ${variable} into words: {command}")))?
		.into_iter();
	let program = words
		.next()
		.ok_or_else(|| UI::failure(&format!("${variable} names no program: {command}")))?;
	Ok(Some(EditorCommand {
		program,
		args: words.collect(),
	}))
}

/// The value of the environment variable `name`, if it is set.
fn env_var(name: &str) -> Result<Option<String>> {
	match env::var(name) {
		Ok(value) => Ok(Some(value)),
		Err(VarError::NotPresent) => Ok(None),
		Err(VarError::NotUnicode(_)) => Err(UI::failure(&format!("${name} is not valid UTF-8"))),
	}
}

/// The file as it was downloaded to be edited, to tell whether it changed in the drive meanwhile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RemoteVersion {
	uuid: Uuid,
	size: u64,
	modified: Option<DateTime<Utc>>,
}

impl RemoteVersion {
	fn of(file: &RemoteFile) -> Self {
		Self {
			uuid: file.uuid(),
			size: file.size(),
			modified: file.last_modified(),
		}
	}
}

/// Whether an edit may be uploaded as a new version of the file it was made to.
#[derive(Debug, PartialEq, Eq)]
enum EditUpload {
	/// The file is in the drive as it was downloaded: upload into this parent directory.
	Upload { parent: Uuid },
	/// No file is at the path any more.
	Gone,
	/// Another file is at the path, such as a new version of it, or the file itself changed.
	Changed,
}

/// Whether an edit of the file downloaded as `downloaded` may be uploaded, given the file at its
/// path now and that file's parent directory: only over the very same file, unchanged, so that
/// uploading replaces nothing the user has not seen.
fn edit_upload(downloaded: RemoteVersion, current: Option<(Uuid, RemoteVersion)>) -> EditUpload {
	match current {
		None => EditUpload::Gone,
		Some((parent, current)) if current == downloaded => EditUpload::Upload { parent },
		Some(_) => EditUpload::Changed,
	}
}

/// Edits a file locally, from a copy downloaded into a temporary directory, and uploads the
/// changes as a new version of it, under its name in its directory. The copy is opened in
/// `$VISUAL` or `$EDITOR`, waiting for it to exit, or else in the app the system opens its type
/// with, waiting for the user to press enter. Nothing is uploaded when the content did not
/// change, nor when the file changed in the drive meanwhile: then, as after any failure once the
/// content changed, the copy is kept and its path printed.
pub(crate) async fn edit_file(
	ui: &mut UI,
	client: &mut LazyClient,
	working_path: &RemotePath,
	file_str: &str,
) -> Result<()> {
	let path = working_path.navigate(file_str);
	let editor = editor_command(env_var("VISUAL")?.as_deref(), env_var("EDITOR")?.as_deref())?;
	// before logging in and downloading, so that a script fails at once
	if editor.is_none() {
		ui.ensure_can_prompt(DONE_EDITING_PROMPT)?;
	}
	let client = client.get(ui).await?;
	let file = find_file(client, &path).await?;
	let downloaded = RemoteVersion::of(&file);
	let created = file.created();
	let copy = LocalCopy::download(client, &file).await?;
	let local = copy.path();
	let original = copy_of(&local).await?;
	// Ctrl-C does not cancel the command from here on, which would delete the edited copy. A
	// terminal editor takes Ctrl-C as a key.
	let _editing = interrupt::own();
	let edited = async {
		edit_locally(ui, editor.as_ref(), &local).await?;
		if same_file_content(original.path(), &local).await? {
			return Ok(false);
		}
		// only a Ctrl-C from now on stops the upload, not one pressed in the editor
		let mut ctrl_c = interrupt::own();
		select! {
			uploaded = upload_edit(client, &path, downloaded, created, &local) => uploaded?,
			() = ctrl_c.pressed() => return Err(CliError::Interrupted.into()),
		}
		Ok(true)
	}
	.await;
	match edited {
		Ok(true) => ui.print_success(&format!("Uploaded your changes to {}", path)),
		Ok(false) => ui.print_success(&format!("No changes to {}", path)),
		Err(e) => {
			// whatever failed, the changes are not lost
			if matches!(same_file_content(original.path(), &local).await, Ok(true)) {
				return Err(e);
			}
			return Err(kept_changes(e, &copy.keep()));
		}
	}
	Ok(())
}

/// `e`, saying that the changes were not uploaded and are kept at `kept`. An error the CLI does
/// not report as it is becomes a failure, so that its cause is printed too.
fn kept_changes(e: anyhow::Error, kept: &Path) -> anyhow::Error {
	let message = format!(
		"Your changes were not uploaded and are kept at {}",
		kept.display()
	);
	if find_cli_error(&e).is_some() {
		e.context(message)
	} else {
		e.context(CliError::Failure(message))
	}
}

/// Lets the user edit the file at `local`: in `editor`, until it exits, or else in the app the
/// system opens its type with, until the user says they are done.
async fn edit_locally(ui: &mut UI, editor: Option<&EditorCommand>, local: &Path) -> Result<()> {
	match editor {
		Some(editor) => {
			let status = tokio::process::Command::new(&editor.program)
				.args(&editor.args)
				.arg(local)
				.status()
				.await
				.map_err(|e| UI::failure(&format!("Failed to run {}: {e}", editor.program)))?;
			if !status.success() {
				return Err(UI::failure(&format!(
					"{} exited with {}",
					editor.program, status
				)));
			}
		}
		None => {
			let opened = local.to_path_buf();
			tokio::task::spawn_blocking(move || open::that(opened))
				.await
				.context("Failed to open file")?
				.map_err(|e| UI::failure(&format!("Failed to open {}: {e}", local.display())))?;
			ui.prompt(DONE_EDITING_PROMPT)?;
		}
	}
	Ok(())
}

/// A temporary copy of the file at `path`, deleted when it is dropped.
async fn copy_of(path: &Path) -> Result<NamedTempFile> {
	let path = path.to_path_buf();
	tokio::task::spawn_blocking(move || {
		let copy = NamedTempFile::new()?;
		fs::copy(&path, copy.path())?;
		Ok::<_, io::Error>(copy)
	})
	.await
	.context("Failed to copy the downloaded file")?
	.context("Failed to copy the downloaded file")
}

/// Whether the files at `a` and `b` hold the same bytes.
async fn same_file_content(a: &Path, b: &Path) -> Result<bool> {
	let (a, b) = (a.to_path_buf(), b.to_path_buf());
	tokio::task::spawn_blocking(move || same_content(File::open(a)?, File::open(b)?))
		.await
		.context("Failed to compare the edited file with the downloaded one")?
		.context("Failed to compare the edited file with the downloaded one")
}

/// Whether `a` and `b` read the same bytes.
fn same_content(a: impl io::Read, b: impl io::Read) -> io::Result<bool> {
	let (mut a, mut b) = (BufReader::new(a), BufReader::new(b));
	loop {
		let a_buf = a.fill_buf()?;
		let b_buf = b.fill_buf()?;
		let len = a_buf.len().min(b_buf.len());
		if len == 0 {
			return Ok(a_buf.is_empty() && b_buf.is_empty());
		}
		if a_buf.get(..len) != b_buf.get(..len) {
			return Ok(false);
		}
		a.consume(len);
		b.consume(len);
	}
}

/// Uploads the edited file at `local` as a new version of the file at `path`, if that is still the
/// file downloaded as `downloaded`. The check and the upload hold the drive lock, so that nothing
/// changes the file in between.
async fn upload_edit(
	client: &Client,
	path: &RemotePath,
	downloaded: RemoteVersion,
	created: Option<DateTime<Utc>>,
	local: &Path,
) -> Result<()> {
	let Some(name) = path.basename() else {
		return Err(UI::failure(&format!("Not a file: {}", path)));
	};
	let _lock = client.lock_drive().await.context("Failed to lock drive")?;
	let current = match client.get_items_in_path(&path.0).await {
		Ok((dirs, ObjectOrRemainingPath::Object(NonRootFileType::File(file)))) => dirs
			.last()
			.map(|parent| (parent.uuid(), RemoteVersion::of(&file))),
		Ok(_) => None,
		// a file is where a directory on the path was
		Err((e, _, _)) if e.kind() == ErrorKind::InvalidType => None,
		Err((e, _, _)) => return Err(e).context("Failed to find file"),
	};
	let parent = match edit_upload(downloaded, current) {
		EditUpload::Upload { parent } => parent,
		EditUpload::Gone => {
			return Err(UI::failure(&format!(
				"{} was moved or deleted while you edited it",
				path
			)));
		}
		EditUpload::Changed => {
			return Err(UI::failure(&format!(
				"{} changed in the drive while you edited it",
				path
			)));
		}
	};
	let mut builder = FileBuilderOptionalName::new(parent);
	builder
		.name(name)
		.map_err(|e| UI::failure(&format!("Invalid file name: {}", e)))?;
	if let Some(created) = created {
		builder.created(created);
	}
	client
		.upload_file_from_path_with_builder(builder, local.to_path_buf(), None)
		.await
		.context("Failed to upload your changes")?;
	Ok(())
}

/// The web drive's URL of the root directory's parent: a directory's URL adds the directories
/// from the root directory down to it.
const WEB_DRIVE_URL: &str = "https://app.filen.io/#/drive";

/// The web drive's URL of the directory the UUIDs `chain` lead to, from the root directory
/// down: filen-web routes `#/drive/` by that chain.
fn web_drive_url(chain: impl IntoIterator<Item = Uuid>) -> String {
	chain
		.into_iter()
		.fold(WEB_DRIVE_URL.to_owned(), |mut url, uuid| {
			url.push('/');
			url.push_str(&uuid.to_string());
			url
		})
}

/// Prints the web drive's URL of a directory, or of a file's directory, and opens it in the
/// browser if `open_browser`. A browser that fails to open is only warned about: the URL is
/// printed.
pub(crate) async fn view_in_web_drive(
	ui: &mut UI,
	client: &mut LazyClient,
	path: &RemotePath,
	open_browser: bool,
) -> Result<()> {
	let client = client.get(ui).await?;
	let no_such_item = || UI::failure(&format!("No such file or directory: {}", path));
	let (dirs, item) = match client.get_items_in_path(&path.0).await {
		Ok(found) => found,
		// a file is where a directory on the path was
		Err((e, _, _)) if e.kind() == ErrorKind::InvalidType => return Err(no_such_item()),
		Err((e, _, _)) => return Err(e).context("Failed to find file or directory"),
	};
	let mut chain = dirs.iter().map(|dir| dir.uuid()).collect::<Vec<_>>();
	match item {
		ObjectOrRemainingPath::RemainingPath(_) => return Err(no_such_item()),
		// a file is shown in its directory, the last of `dirs`
		ObjectOrRemainingPath::Object(NonRootFileType::File(_)) => {}
		ObjectOrRemainingPath::Object(NonRootFileType::Dir(dir)) => chain.push(dir.uuid()),
		ObjectOrRemainingPath::Object(NonRootFileType::Root(root)) => chain.push(root.uuid()),
	}
	let url = web_drive_url(chain);
	if ui.json {
		ui.print_json(json!({ "url": url }))?;
	} else {
		ui.print(&url);
	}
	if open_browser && let Err(e) = open::that_detached(&url) {
		ui.print_warning(&format!("Failed to open the browser: {}", e));
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
	let client = client.get(ui).await?;
	// like `mkdir -p`, an existing directory is no failure
	match client.find_item_at_path(&directory_str.0).await {
		Ok(Some(NonRootFileType::Dir(_) | NonRootFileType::Root(_))) => {
			print_directory_exists(ui, &directory_str);
			return Ok(());
		}
		Ok(Some(NonRootFileType::File(_))) => {
			return Err(UI::failure(&format!("Not a directory: {}", directory_str)));
		}
		Ok(None) => {}
		// a file on the way, which creating the directory reports
		Err(e) if e.kind() == ErrorKind::InvalidType => {}
		Err(e) => return Err(e).context("Failed to find directory"),
	}
	let created = create_directory_(client, &directory_str, recursive).await?;
	match created
		.name()
		.and_then(|name| existing_directory_name(directory_str.basename(), name))
	{
		Some(existing) => print_directory_exists(ui, &directory_str.parent().navigate(existing)),
		None => ui.print_success(&format!("Directory created: {}", directory_str)),
	}
	Ok(())
}

fn print_directory_exists(ui: &mut UI, directory: &RemotePath) {
	if !ui.is_quiet() {
		ui.print_muted(&format!("Directory already exists: {}", directory));
	}
}

/// The name of the directory that creating one named `requested` returned, if that is an existing
/// directory: the server takes a name that differs only in case from one in the same directory as
/// that directory's, and returns that directory rather than creating one. Looking the path up
/// first finds only a directory of exactly the requested name.
fn existing_directory_name<'a>(requested: Option<&str>, returned: &'a str) -> Option<&'a str> {
	(requested != Some(returned)).then_some(returned)
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

/// Where a file written to a path goes.
struct FileToWrite<'a> {
	parent: DirType<'a, Normal>,
	/// The name of the file there already, as it differs from the path's only in case if at all,
	/// or else the path's name.
	name: String,
	/// The path, with [`FileToWrite::name`].
	path: RemotePath,
	/// The file there already, which writing replaces.
	existing: Option<RemoteFile>,
}

/// Finds where a file written to `path` goes: its parent directory must exist, and nothing there
/// may be a directory whose name is the file's but for case. Names are compared ignoring case, as
/// the server does: uploading a file whose name a file there has but for case replaces that file.
async fn find_file_to_write<'a>(client: &'a Client, path: &RemotePath) -> Result<FileToWrite<'a>> {
	let Some(name) = path.basename() else {
		return Err(UI::failure(&format!("Not a file: {}", path)));
	};
	let parent_path = path.parent();
	let parent = match client
		.find_item_at_path(&parent_path.0)
		.await
		.context("Failed to find parent directory")?
	{
		Some(NonRootFileType::Dir(dir)) => DirType::Dir(dir),
		Some(NonRootFileType::Root(root)) => DirType::Root(root),
		Some(NonRootFileType::File(_)) => {
			return Err(UI::failure(&format!("Not a directory: {}", parent_path)));
		}
		None => {
			return Err(UI::failure(&format!("No such directory: {}", parent_path)));
		}
	};
	let (dirs, mut files) = client
		.list_dir::<_, Normal>(&parent, None::<&fn(u64, Option<u64>)>)
		.await
		.context("Failed to list parent directory")?;
	if let Some(dir_name) = clashing_name(dirs.iter().filter_map(|dir| dir.name()), name) {
		return Err(UI::failure(&format!(
			"Not a file: {}",
			parent_path.navigate(dir_name)
		)));
	}
	// the file of that very name, should several differ only in case
	let lowercase_name = name.to_lowercase();
	let existing = files
		.iter()
		.position(|file| file.name() == Some(name))
		.or_else(|| {
			files.iter().position(|file| {
				file.name()
					.is_some_and(|n| n.to_lowercase() == lowercase_name)
			})
		})
		.map(|index| files.swap_remove(index));
	let name = existing
		.as_ref()
		.and_then(|file| file.name())
		.unwrap_or(name)
		.to_owned();
	Ok(FileToWrite {
		parent,
		path: parent_path.navigate(&name),
		name,
		existing,
	})
}

/// Writes `content`, its words joined by spaces, to a file, creating it or replacing its content.
/// Without words, writes what stdin reads when it is not a terminal (streamed, not held in
/// memory), and otherwise nothing. Replacing a file's content keeps the old content as a previous
/// version if the account keeps versions.
pub(crate) async fn write_file(
	ui: &mut UI,
	client: &mut LazyClient,
	working_path: &RemotePath,
	file_str: &str,
	content: &[String],
) -> Result<()> {
	let path = working_path.navigate(file_str);
	let client = client.get(ui).await?;
	let target = find_file_to_write(client, &path).await?;
	let builder = client
		.make_file_builder(&target.name, target.parent.uuid())
		.map_err(|e| UI::failure(&format!("Invalid file name: {}", e)))?;
	if content.is_empty() && !io::stdin().is_terminal() {
		client
			.upload_file_from_reader(builder, &mut stdin_reader(), None, None)
			.await
	} else {
		client
			.upload_file(builder, content.join(" ").as_bytes())
			.await
	}
	.context("Failed to write file")?;
	ui.print_success(&format!("Wrote to {}", target.path));
	Ok(())
}

/// Creates an empty file, or sets the modification date of an existing one to now and leaves its
/// content as it is.
pub(crate) async fn touch_file(
	ui: &mut UI,
	client: &mut LazyClient,
	working_path: &RemotePath,
	file_str: &str,
) -> Result<()> {
	let path = working_path.navigate(file_str);
	let client = client.get(ui).await?;
	let target = find_file_to_write(client, &path).await?;
	match target.existing {
		Some(mut file) => {
			client
				.update_file_metadata(
					&mut file,
					FileMetaChanges::default().last_modified(Utc::now()),
				)
				.await
				.context("Failed to update the modification date")?;
			ui.print_success(&format!("Updated the modification date of {}", target.path));
		}
		None => {
			let builder = client
				.make_file_builder(&target.name, target.parent.uuid())
				.map_err(|e| UI::failure(&format!("Invalid file name: {}", e)))?;
			client
				.upload_file(builder, &[])
				.await
				.context("Failed to create file")?;
			ui.print_success(&format!("Created empty file: {}", target.path));
		}
	}
	Ok(())
}

/// Bytes read from stdin. A thread reads them, as std reads stdin blocking, and passes them on
/// through a channel short enough that it reads no faster than they are taken.
fn stdin_reader() -> impl AsyncRead + Unpin {
	const READ_SIZE: usize = 64 * 1024;
	let (mut sender, receiver) = mpsc::channel::<io::Result<Vec<u8>>>(4);
	thread::spawn(move || {
		let mut stdin = io::stdin().lock();
		loop {
			let mut buf = vec![0; READ_SIZE];
			let read = match stdin.read(&mut buf) {
				Ok(0) => return,
				Ok(read) => Ok(read),
				Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
				Err(e) => Err(e),
			};
			let done = read.is_err();
			let sent = executor::block_on(sender.send(read.map(|read| {
				buf.truncate(read);
				buf
			})));
			// a closed channel: the upload has ended
			if done || sent.is_err() {
				return;
			}
		}
	});
	receiver.into_async_read()
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
	.context("Failed to decrypt source name")?;
	let Destination {
		dir: destination_dir,
		name: new_name,
		path: new_path,
	} = resolve_destination(
		client,
		Placement::Move,
		&Source {
			path: &source_path,
			name: source_filename,
			uuid: source.uuid(),
		},
		working_path,
		destination_str,
	)
	.await?;

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

/// Which command puts a source at a destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Placement {
	Move,
	Copy,
}

impl Placement {
	fn verb(self) -> &'static str {
		match self {
			Self::Move => "move",
			Self::Copy => "copy",
		}
	}
}

/// The item a `mv` or `cp` puts somewhere.
struct Source<'a> {
	path: &'a RemotePath,
	name: &'a str,
	uuid: Uuid,
}

/// Where a `mv` or `cp` puts its source.
struct Destination<'a> {
	/// The directory the source ends up in.
	dir: DirType<'a, Normal>,
	/// The name the source ends up under.
	name: String,
	/// The path the source ends up at.
	path: RemotePath,
}

/// What a destination path names, as looked up on the drive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Found {
	Directory,
	File,
	Nothing,
}

/// Where a destination puts the source, before the directory it ends up in was looked up.
#[derive(Debug, PartialEq, Eq)]
enum Target {
	/// Into an existing directory at `dir`, under the source's own name.
	Into { dir: RemotePath, path: RemotePath },
	/// At a new path: into the directory at `parent`, under `name`.
	At {
		parent: RemotePath,
		name: String,
		path: RemotePath,
	},
}

impl Target {
	/// The path the source ends up at.
	fn path(&self) -> &RemotePath {
		match self {
			Self::Into { path, .. } | Self::At { path, .. } => path,
		}
	}

	/// The directory the source ends up in.
	fn dir(&self) -> &RemotePath {
		match self {
			Self::Into { dir, .. } => dir,
			Self::At { parent, .. } => parent,
		}
	}
}

/// Decides, as Unix `mv` and `cp` do, what `destination` means for `source`: an existing
/// directory takes the source under its own name, and anything else is the source's new path. A
/// destination written with a trailing '/' (`must_be_a_directory`) has to be an existing
/// directory. Refuses to put the source inside itself, and to move it to where it already is.
fn target_of(
	placement: Placement,
	source: &Source<'_>,
	destination: &RemotePath,
	found: Found,
	must_be_a_directory: bool,
) -> Result<Target> {
	let target = match found {
		Found::Directory => Target::Into {
			dir: destination.clone(),
			path: destination.navigate(source.name),
		},
		Found::File if must_be_a_directory => {
			return Err(UI::failure(&format!("Not a directory: {}", destination)));
		}
		Found::File => {
			return Err(UI::failure(&format!(
				"Destination already exists: {}",
				destination
			)));
		}
		Found::Nothing if must_be_a_directory => {
			return Err(UI::failure(&format!(
				"No such destination directory: {}",
				destination
			)));
		}
		Found::Nothing => {
			// only the root has no base name, and the root always exists
			let Some(name) = destination.basename() else {
				return Err(UI::failure(&format!(
					"Cannot {} onto the root directory",
					placement.verb()
				)));
			};
			Target::At {
				parent: destination.parent(),
				name: name.to_owned(),
				path: destination.clone(),
			}
		}
	};
	if placement == Placement::Move && target.path() == source.path {
		return Err(UI::failure(&format!(
			"{} is already in {}",
			source.path,
			target.dir()
		)));
	}
	if is_inside(target.path(), source.path) {
		return Err(UI::failure(&format!(
			"Cannot {} {} into itself: {}",
			placement.verb(),
			source.path,
			target.path()
		)));
	}
	Ok(target)
}

/// Whether `path` lies below `ancestor`.
fn is_inside(path: &RemotePath, ancestor: &RemotePath) -> bool {
	path.0
		.strip_prefix(&ancestor.0)
		.is_some_and(|rest| rest.starts_with('/'))
}

/// Resolves the raw `destination_str` of a `mv` or `cp` into the directory the source ends up
/// in and the name it ends up under (see [`target_of`]), and refuses a name the server would take
/// as one already there: it compares names case-insensitively. Moving an item to a name that
/// differs from its own only in case renames it.
///
/// The comparison lists the whole destination directory, once per source. The server's name-hash
/// lookups (`dir_exists`, `file_exists`) would take two requests instead, but match only the
/// hash the uploading client sent, so they miss an item whose name hash a non-conforming client
/// computed differently, which the listing still finds by its decrypted name.
async fn resolve_destination<'a>(
	client: &'a Client,
	placement: Placement,
	source: &Source<'_>,
	working_path: &RemotePath,
	destination_str: &str,
) -> Result<Destination<'a>> {
	// before navigating, which drops the trailing '/'
	let must_be_a_directory = names_a_directory(destination_str);
	let destination_path = working_path.navigate(destination_str);
	let found_item = client
		.find_item_at_path(&destination_path.0)
		.await
		.context("Failed to find destination")?;
	let found = match &found_item {
		Some(NonRootFileType::Dir(_) | NonRootFileType::Root(_)) => Found::Directory,
		Some(NonRootFileType::File(_)) => Found::File,
		None => Found::Nothing,
	};
	let target = target_of(
		placement,
		source,
		&destination_path,
		found,
		must_be_a_directory,
	)?;
	let dir = match found_item {
		// an existing directory at the destination takes the source (`Target::Into`), so it is
		// not looked up again
		Some(NonRootFileType::Dir(dir)) => DirType::Dir(dir),
		Some(NonRootFileType::Root(root)) => DirType::Root(root),
		// otherwise the destination is the source's new path, in its parent directory
		Some(NonRootFileType::File(_)) | None => match client
			.find_item_at_path(&target.dir().0)
			.await
			.context("Failed to find destination directory")?
		{
			Some(NonRootFileType::Dir(dir)) => DirType::Dir(dir),
			Some(NonRootFileType::Root(root)) => DirType::Root(root),
			Some(NonRootFileType::File(_)) => {
				return Err(UI::failure(&format!("Not a directory: {}", target.dir())));
			}
			None => {
				return Err(UI::failure(&format!(
					"No such destination directory: {}",
					target.dir()
				)));
			}
		},
	};
	let (dir_path, name, path) = match target {
		Target::Into { dir, path } => (dir, source.name.to_owned(), path),
		Target::At { parent, name, path } => (parent, name, path),
	};

	// A same-name upload would replace the existing file (as a new version) and creating a
	// same-name directory returns the existing one, so a copy would merge into it.
	let (existing_dirs, existing_files) = client
		.list_dir::<_, Normal>(&dir, None::<&fn(u64, Option<u64>)>)
		.await
		.context("Failed to list destination directory")?;
	let existing_names = existing_dirs
		.iter()
		.map(|dir| (dir.uuid(), dir.name()))
		.chain(existing_files.iter().map(|file| (file.uuid(), file.name())))
		// a moved item does not clash with itself
		.filter(|(uuid, _)| placement == Placement::Copy || *uuid != source.uuid)
		.filter_map(|(_, name)| name);
	if let Some(existing_name) = clashing_name(existing_names, &name) {
		return Err(UI::failure(&format!(
			"Destination already exists: {}",
			dir_path.navigate(existing_name)
		)));
	}
	Ok(Destination { dir, name, path })
}

/// Copies a file or directory, following the semantics of the Unix `cp`: if the destination is
/// an existing directory, the copy is put into it under the source's name; otherwise the
/// destination names the copy's path, so the copy is put into that path's parent directory under
/// that path's base name. A destination ending in '/' must be an existing directory.
pub(crate) async fn copy_file_or_directory(
	ui: &mut UI,
	client: &mut LazyClient,
	working_path: &RemotePath,
	source_str: &str,
	destination_str: &str,
) -> Result<()> {
	let source_path = working_path.navigate(source_str);
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
	let source_name = match &source {
		NonRootFileType::File(file) => file.name(),
		NonRootFileType::Dir(dir) => dir.name(),
		NonRootFileType::Root(_) => return Err(UI::failure("Cannot copy root directory")),
	}
	.context("Failed to decrypt source name")?;
	let destination = resolve_destination(
		client,
		Placement::Copy,
		&Source {
			path: &source_path,
			name: source_name,
			uuid: source.uuid(),
		},
		working_path,
		destination_str,
	)
	.await?;
	// the source's own name is left to the copy, which also copies a name today's rules reject
	let name = if destination.name == source_name {
		None
	} else {
		Some(
			ValidatedName::try_from(destination.name.as_str())
				.map_err(|e| UI::failure(&format!("Invalid destination name: {}", e)))?,
		)
	};
	let destination_dir_path = destination.path.parent();
	let source = match source {
		NonRootFileType::File(file) => ItemSource::File(file.into_owned().into()),
		NonRootFileType::Dir(dir) => ItemSource::Dir(ItemSourceDir::Normal(dir.into_owned())),
		NonRootFileType::Root(_) => return Err(UI::failure("Cannot copy root directory")),
	};
	let config = CopyConfig {
		max_bytes: free_storage(client).await,
	};
	let request = CopyRequest {
		source,
		destination: destination.dir.into_owned_cow(),
		name,
	};
	let result = run_copy(ui, client, request, config).await;
	finish_copy(ui, &source_path, &destination_dir_path, result)
}

/// Runs a copy, drawing its progress, until it ends or Ctrl-C cancels it. A cancelled copy stops
/// what it is doing and reports what it copied, rather than being dropped with half a tree
/// written and nothing said about it; a second Ctrl-C quits without waiting for that.
async fn run_copy(
	ui: &mut UI,
	client: &Arc<Client>,
	request: CopyRequest,
	config: CopyConfig,
) -> Result<CopyReport, CopyFailed> {
	let progress = Arc::new(CopyProgress::new(ui));
	let (control, controller) = JobControl::new();
	let mut ctrl_c = interrupt::own();
	let copy =
		Arc::clone(client).copy_items_to(vec![request], config, Arc::clone(&progress), control);
	let mut copy = pin!(copy);
	let ended = select! {
		result = &mut copy => Some(result),
		() = ctrl_c.pressed() => None,
	};
	let result = match ended {
		Some(result) => result,
		None => {
			controller.cancel();
			// before saying anything, so that no progress line is drawn over it
			progress.end();
			let _stopping = ctrl_c.into_stopping(ui);
			copy.await
		}
	};
	progress.end();
	result
}

/// Storage left on the account, for the copy to check that it fits before it writes anything.
/// Without it the copy still runs, and fails once the server refuses an upload.
async fn free_storage(client: &Client) -> Option<u64> {
	match client.get_user_info().await {
		Ok(info) => Some(info.max_storage.saturating_sub(info.storage_used)),
		Err(e) => {
			log::warn!(
				"Failed to get the storage left, copying without checking it: {}",
				e
			);
			None
		}
	}
}

/// At most this many failed or skipped items are listed after a copy; a broken directory tree can
/// produce one per file.
const MAX_PRINTED_COPY_PROBLEMS: usize = 10;

/// Prints what went wrong in a copy, then the success message, or fails if anything was not
/// copied. A copy whose items failed still completes, so without this the command would claim
/// success while items were missing.
fn finish_copy(
	ui: &mut UI,
	source_path: &RemotePath,
	destination_dir_path: &RemotePath,
	result: Result<CopyReport, CopyFailed>,
) -> Result<()> {
	let report = match &result {
		Ok(report) => report,
		Err(failed) => &failed.report,
	};
	print_copy_problems(ui, report);
	let copy_path = report
		.top_level
		.first()
		.and_then(|copied| copied.item.name())
		.map(|name| destination_dir_path.navigate(name));
	let report = match result {
		Ok(report) => report,
		Err(failed) => {
			if let Some(copy_path) = &copy_path {
				ui.print_failure(&format!(
					"Copied only {} to {}",
					copied_amount(&failed.report),
					copy_path
				));
			}
			if failed.error.kind() == ErrorKind::Cancelled {
				return Err(CliError::Interrupted.into());
			}
			if failed.error.kind() == ErrorKind::MaxStorageReached {
				return Err(UI::failure(&format!(
					"Not enough storage left: copying {} needs {}",
					source_path,
					ui::format_size(failed.report.totals.bytes)
				)));
			}
			return Err(filen_sdk_rs::Error::from(failed)).context("Failed to copy");
		}
	};
	let failed = report.failures.len();
	if failed > 0 {
		return Err(UI::failure(&format!("{} item(s) failed to copy", failed)));
	}
	match copy_path {
		Some(copy_path) => {
			ui.print_success(&format!("Copied {} to {}", source_path, copy_path));
			Ok(())
		}
		// a copy that failed at its top-level item reports that as a failure
		None => Err(UI::failure(&format!("Failed to copy {}", source_path))),
	}
}

/// Lists the items a copy could not copy and the ones it left out.
fn print_copy_problems(ui: &mut UI, report: &CopyReport) {
	let failures = report
		.failures
		.iter()
		.map(|failure| format!("{}: {}", failure.info.source_path, failure.info.error));
	let skipped = report.skipped.iter().map(|skipped| {
		format!(
			"Skipped {}: {}",
			skipped.source_path,
			match skipped.reason {
				SkipReason::UndecryptableFile { .. } => "cannot be decrypted".to_owned(),
				SkipReason::Unreachable { count } => format!("{} unreachable items", count),
			}
		)
	});
	let problems = failures.chain(skipped).collect::<Vec<String>>();
	for problem in problems.iter().take(MAX_PRINTED_COPY_PROBLEMS) {
		ui.print_failure(problem);
	}
	if problems.len() > MAX_PRINTED_COPY_PROBLEMS {
		ui.print_failure(&format!(
			"... and {} more",
			problems.len() - MAX_PRINTED_COPY_PROBLEMS
		));
	}
}

/// How much of what it planned a copy copied, as "3 of 10 files (1 MiB of 5 MiB)".
fn copied_amount(report: &CopyReport) -> String {
	format!(
		"{} of {} files ({} of {})",
		report.counts.files_done,
		report.totals.files,
		ui::format_size(report.counts.bytes_done),
		ui::format_size(report.totals.bytes)
	)
}

/// Draws a copy's progress on one line of stderr, redrawn in place: first the listing of the
/// source and the destination, then the bytes copied. The copy calls it from its own tasks, so it
/// writes to the terminal itself instead of through [`UI`], which the command holds.
struct CopyProgress {
	term: Term,
	/// Only for a person at a terminal: not with `--quiet` or `--json`, and not when stdout or
	/// stderr is redirected, where the redraws would end up in the captured output.
	enabled: bool,
	line: Mutex<ProgressLine>,
}

/// Where the progress line stands.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ProgressLine {
	NotShown,
	Shown,
	/// The command prints its own output from now on, so the line is never drawn again.
	Ended,
}

impl CopyProgress {
	fn new(ui: &UI) -> Self {
		let term = Term::stderr();
		let enabled = ui.output_is_read_by_a_person() && !ui.is_quiet() && term.is_term();
		Self {
			term,
			enabled,
			line: Mutex::new(ProgressLine::NotShown),
		}
	}

	fn lock_line(&self) -> MutexGuard<'_, ProgressLine> {
		// the line state is a plain value, which a panicking holder cannot leave half-written
		self.line.lock().unwrap_or_else(PoisonError::into_inner)
	}

	/// Erases the progress line, and draws no other one, so that a REPL prompt or the command's
	/// own output starts on a clean row.
	fn end(&self) {
		let mut line = self.lock_line();
		if *line == ProgressLine::Shown {
			let _ = self.term.clear_line();
			let _ = self.term.flush();
		}
		*line = ProgressLine::Ended;
	}

	fn draw(&self, text: &str) {
		let mut line = self.lock_line();
		if *line == ProgressLine::Ended {
			return;
		}
		let text = ui::truncate_to_width(text, usize::from(self.term.size().1));
		let _ = self.term.clear_line();
		let _ = self.term.write_str(&style(text).dim().to_string());
		let _ = self.term.flush();
		*line = ProgressLine::Shown;
	}
}

impl CopyCallback for CopyProgress {
	fn on_top_level_planned(&self, _items: Vec<PlannedTopLevelItem>) {}

	fn on_top_level_created(&self, _item: CopiedTopLevel) {}

	fn on_update(&self, update: CopyUpdate) {
		if !self.enabled {
			return;
		}
		if let Some(text) =
			progress_text(update.phase, &update.scan, &update.totals, &update.counts)
		{
			self.draw(&text);
		}
	}
}

/// The progress line for a copy in `phase`, if one is shown in it.
fn progress_text(
	phase: CopyPhase,
	scan: &ScanProgress,
	totals: &PlanTotals,
	counts: &ItemCounts,
) -> Option<String> {
	match phase {
		CopyPhase::Scanning => Some(format!(
			"Scanning... {}/{} directories listed",
			scan.sources_done, scan.sources_total
		)),
		CopyPhase::CreatingDirectories | CopyPhase::CopyingFiles | CopyPhase::Finishing => {
			Some(if totals.bytes == 0 {
				format!("Copying {}/{} files", counts.files_done, totals.files)
			} else {
				let percent =
					(u128::from(counts.bytes_done) * 100 / u128::from(totals.bytes)).min(100);
				format!(
					"Copying {} / {} ({}%) - {}/{} files",
					ui::format_size(counts.bytes_done),
					ui::format_size(totals.bytes),
					percent,
					counts.files_done,
					totals.files
				)
			})
		}
		CopyPhase::Done | CopyPhase::Cancelled | CopyPhase::Failed => None,
	}
}

/// The name among `existing` that the server treats as the same as `name`: it compares names
/// case-insensitively.
fn clashing_name<'a>(existing: impl IntoIterator<Item = &'a str>, name: &str) -> Option<&'a str> {
	let name = name.to_lowercase();
	existing
		.into_iter()
		.find(|existing| existing.to_lowercase() == name)
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
	// in the order the server lists them, which sorting by path would lose
	let items = list_with_full_paths(client, &dirs, &files).await?;
	print_listing(ui, &items, ListedBy::Path, "No recent items", false)
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
	let mut items = list_with_full_paths(client, &dirs, &files).await?;
	sort_listing(&mut items, ListedBy::Path);
	print_listing(ui, &items, ListedBy::Path, empty_message, false)
}

/// The listing of `dirs` and `files`, in that order, each with its absolute path.
async fn list_with_full_paths(
	client: &Client,
	dirs: &[RemoteDirectory],
	files: &[RemoteFile],
) -> Result<Vec<ListedItem>> {
	// resolving a path walks up the tree, and `get_item_path` takes the drive lock for each
	// walk — holding one here means those all share it instead of re-acquiring it per item
	let _lock = client.lock_drive().await.context("Failed to lock drive")?;
	let mut items = Vec::with_capacity(dirs.len() + files.len());
	for dir in dirs {
		items.push(ListedItem {
			path: full_path(client, &NonRootItemType::Dir(Cow::Borrowed(dir))).await,
			..ListedItem::of_dir(dir)
		});
	}
	for file in files {
		items.push(ListedItem {
			path: full_path(client, &NonRootItemType::File(Cow::Borrowed(file))).await,
			..ListedItem::of_file(file)
		});
	}
	Ok(items)
}

/// The item's absolute path, with the trailing slash `get_item_path` puts on directories
/// stripped (directories are already distinguished by color), or `None` if it cannot be resolved.
async fn full_path(client: &Client, item: &NonRootItemType<'_, Normal>) -> Option<String> {
	match client.get_item_path(item).await {
		Ok((path, _)) => Some(format!("/{}", path.trim_end_matches('/'))),
		Err(e) => {
			log::warn!("Failed to resolve path of item {}: {}", item.uuid(), e);
			None
		}
	}
}

pub(crate) async fn list_trash(ui: &mut UI, client: &mut LazyClient, long: bool) -> Result<()> {
	let client = client.get(ui).await?;
	let items = list_trash_items(client)
		.await?
		.into_iter()
		.map(|entry| entry.listed)
		.collect::<Vec<ListedItem>>();
	print_listing(ui, &items, ListedBy::Name, "Trash is empty", long)
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum TrashAction {
	Restore,
	Delete,
}

impl TrashAction {
	/// What was done to an item, as in "could not be restored".
	fn done(self) -> &'static str {
		match self {
			TrashAction::Restore => "restored",
			TrashAction::Delete => "permanently deleted",
		}
	}
}

/// Which trashed items a command line chose.
enum TrashSelection {
	/// None given: the user picks one interactively.
	Pick,
	/// `--all`.
	All,
	/// By name or UUID (see [`resolve_trash_operand`]).
	Named(Vec<String>),
}

impl TrashSelection {
	fn of(items: Vec<String>, all: bool) -> Self {
		match (all, items.is_empty()) {
			(true, _) => TrashSelection::All,
			(false, true) => TrashSelection::Pick,
			(false, false) => TrashSelection::Named(items),
		}
	}
}

pub(crate) enum TrashItem {
	Dir(RemoteDirectory),
	File(RemoteFile),
}

/// A trashed item, with what a listing shows of it.
struct TrashEntry {
	listed: ListedItem,
	item: TrashItem,
}

/// The items in the trash, in the order `list-trash` prints them.
async fn list_trash_items(client: &Client) -> Result<Vec<TrashEntry>> {
	let (dirs, files) = client
		.list_trash(None::<&fn(u64, Option<u64>)>)
		.await
		.context("Failed to list trash")?;
	let mut entries = dirs
		.into_iter()
		.map(|dir| TrashEntry {
			listed: ListedItem::of_dir(&dir),
			item: TrashItem::Dir(dir),
		})
		.chain(files.into_iter().map(|file| TrashEntry {
			listed: ListedItem::of_file(&file),
			item: TrashItem::File(file),
		}))
		.collect::<Vec<TrashEntry>>();
	entries.sort_by(|a, b| listing_order(&a.listed, &b.listed, ListedBy::Name));
	Ok(entries)
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
	let mut entries = list_trash_items(client).await?;
	if entries.is_empty() {
		ui.print_muted("Trash is empty");
		return Ok(());
	}

	let options = entries
		.iter()
		.map(|entry| {
			let shares_name = entries
				.iter()
				.filter(|other| other.listed.name == entry.listed.name)
				.count() > 1;
			trash_picker_label(&entry.listed, shares_name)
		})
		.collect::<Vec<String>>();
	let Some(selection) = ui.prompt_select(select_message, options.clone())? else {
		return Ok(());
	};
	let index = options
		.iter()
		.position(|option| *option == selection)
		.context("Failed to resolve selected item")?;
	let entry = entries.remove(index);

	match action {
		TrashAction::Restore => restore_trash_item(ui, client, entry).await,
		TrashAction::Delete => {
			if !ui.prompt_confirm(&format!("Permanently delete {}?", entry.listed.name), false)? {
				return Ok(());
			}
			delete_trash_item_permanently(ui, client, entry).await
		}
	}
}

/// How the trash pickers show `item`: its name, with a trailing slash on a directory and its UUID
/// when it `shares_name` with another trashed item, so that every option maps back to exactly one
/// item, then its size and the date `ls -l` shows.
fn trash_picker_label(item: &ListedItem, shares_name: bool) -> String {
	let mut label = item.name.clone();
	if item.kind == ListedKind::Directory {
		label.push('/');
	}
	if shares_name {
		label.push_str(&format!(" ({})", item.uuid));
	}
	if let Some(size) = item.size {
		label.push_str(&format!("  {}", ui::format_size(size)));
	}
	if let Some(date) = item.date() {
		label.push_str(&format!("  {}", ui::format_date(&date)));
	}
	label
}

/// Restores the trashed items `selection` chooses.
async fn restore_trash_items(
	ui: &mut UI,
	client: &mut LazyClient,
	selection: TrashSelection,
) -> Result<()> {
	if let TrashSelection::Pick = selection {
		return select_trash_item(ui, client, TrashAction::Restore).await;
	}
	let client = client.get(ui).await?;
	let entries = selected_trash_entries(ui, client, selection).await?;
	if entries.is_empty() {
		ui.print_muted("Trash is empty");
		return Ok(());
	}
	act_on_trash_entries(ui, client, entries, TrashAction::Restore).await
}

/// Permanently deletes the trashed items `selection` chooses, asking once for all of them unless
/// --yes is given.
async fn delete_trash_items(
	ui: &mut UI,
	client: &mut LazyClient,
	selection: TrashSelection,
) -> Result<()> {
	let confirmation = match &selection {
		TrashSelection::Pick => {
			return select_trash_item(ui, client, TrashAction::Delete).await;
		}
		TrashSelection::All => "Permanently delete all trashed items?".to_owned(),
		TrashSelection::Named(operands) => trash_deletion_confirmation(operands),
	};
	// before logging in and listing the trash, so that a script missing --yes fails at once
	ui.ensure_can_confirm(&confirmation)?;
	let client = client.get(ui).await?;
	let entries = selected_trash_entries(ui, client, selection).await?;
	if entries.is_empty() {
		ui.print_muted("Trash is empty");
		return Ok(());
	}
	let names = entries
		.iter()
		.map(|entry| entry.listed.name.as_str())
		.collect::<Vec<&str>>();
	if !ui.prompt_confirm(&trash_deletion_confirmation(&names), false)? {
		return Ok(());
	}
	act_on_trash_entries(ui, client, entries, TrashAction::Delete).await
}

/// What deleting the trashed items `names` asks first.
fn trash_deletion_confirmation(names: &[impl AsRef<str>]) -> String {
	match names {
		[name] => format!("Permanently delete {}?", name.as_ref()),
		_ => format!("Permanently delete {}?", count_of_items(names.len())),
	}
}

/// The trashed items `selection` chooses: all of them, or those its operands name, each once, in
/// the order they were named. Every operand is resolved before anything is done, so that one
/// that names no single item fails the command before it changes anything.
async fn selected_trash_entries(
	ui: &mut UI,
	client: &Client,
	selection: TrashSelection,
) -> Result<Vec<TrashEntry>> {
	let entries = list_trash_items(client).await?;
	let operands = match selection {
		TrashSelection::Pick | TrashSelection::All => return Ok(entries),
		TrashSelection::Named(operands) => operands,
	};
	let indices = match resolve_trash_operands(entries.iter().map(|entry| &entry.listed), &operands)
	{
		Ok(indices) => indices,
		Err(problems) => return Err(unresolved_trash_operands(ui, problems, operands.len())),
	};
	let mut entries = entries
		.into_iter()
		.map(Some)
		.collect::<Vec<Option<TrashEntry>>>();
	Ok(indices
		.into_iter()
		.filter_map(|index| entries.get_mut(index).and_then(Option::take))
		.collect())
}

/// The failure of a command whose operands named no single trashed item: the one problem, or,
/// after printing each of them, how many operands had one.
fn unresolved_trash_operands(ui: &mut UI, problems: Vec<String>, operands: usize) -> anyhow::Error {
	if let [problem] = problems.as_slice() {
		return UI::failure(problem);
	}
	for problem in &problems {
		ui.print_failure(problem);
	}
	UI::failure(&format!(
		"Changed nothing, as {} of the {} items given are not in the trash or share their name",
		problems.len(),
		operands
	))
}

/// The indices among the trash's `items` of the items `operands` name (see
/// [`resolve_trash_operand`]), each once, in the order they were first named; or what is wrong
/// with each operand that names no single item.
fn resolve_trash_operands<'a>(
	items: impl Iterator<Item = &'a ListedItem> + Clone,
	operands: &[String],
) -> Result<Vec<usize>, Vec<String>> {
	let mut indices = Vec::with_capacity(operands.len());
	let mut problems = Vec::new();
	for operand in operands {
		match resolve_trash_operand(items.clone(), operand) {
			Ok(index) if !indices.contains(&index) => indices.push(index),
			Ok(_) => {}
			Err(problem) => problems.push(problem),
		}
	}
	if problems.is_empty() {
		Ok(indices)
	} else {
		Err(problems)
	}
}

/// The index of the one item among the trash's `items` that `operand` names: `uuid:<UUID>` names
/// the item with that UUID; anything else names the item of that name, or else, if it is a UUID,
/// the item with that UUID. Trashed items can share a name, as they come from different
/// directories; those are only told apart by their UUIDs, which the failure lists with what tells
/// the items apart.
fn resolve_trash_operand<'a>(
	mut items: impl Iterator<Item = &'a ListedItem> + Clone,
	operand: &str,
) -> Result<usize, String> {
	if let Some(uuid) = operand.strip_prefix("uuid:") {
		let uuid = Uuid::parse_str(uuid).map_err(|_| format!("Not a UUID: {}", uuid))?;
		return items
			.position(|item| item.uuid == uuid)
			.ok_or_else(|| format!("No item with UUID {} in the trash", uuid));
	}
	let named = items
		.clone()
		.enumerate()
		.filter(|(_, item)| item.name == operand)
		.collect::<Vec<(usize, &ListedItem)>>();
	match named.as_slice() {
		[(index, _)] => Ok(*index),
		[] => Uuid::parse_str(operand)
			.ok()
			.and_then(|uuid| items.position(|item| item.uuid == uuid))
			.ok_or_else(|| format!("No such item in the trash: {}", operand)),
		several => Err(format!(
			"{} trashed items are named {}, so name one by its UUID:\n{}",
			several.len(),
			operand,
			several
				.iter()
				.map(|(_, item)| trash_candidate_line(item))
				.collect::<Vec<String>>()
				.join("\n")
		)),
	}
}

/// One of the trashed items sharing a name, as the failure lists it: its UUID, kind, size and
/// the date `ls -l` shows.
fn trash_candidate_line(item: &ListedItem) -> String {
	format!(
		"  {}  {}  {}  {}",
		item.uuid,
		item.kind.name(),
		item.size.map_or_else(|| "-".to_owned(), ui::format_size),
		item.date()
			.map_or_else(|| "-".to_owned(), |date| ui::format_date(&date))
	)
}

/// Restores or permanently deletes each of `entries`, reporting each one that fails and going on
/// with the rest.
async fn act_on_trash_entries(
	ui: &mut UI,
	client: &Client,
	entries: Vec<TrashEntry>,
	action: TrashAction,
) -> Result<()> {
	let total = entries.len();
	let mut failed = 0usize;
	for entry in entries {
		let result = match action {
			TrashAction::Restore => restore_trash_item(ui, client, entry).await,
			TrashAction::Delete => delete_trash_item_permanently(ui, client, entry).await,
		};
		if let Err(e) = result {
			if total == 1 {
				return Err(e);
			}
			ui.print_failure_or_error(&e);
			failed = failed.saturating_add(1);
		}
	}
	if failed > 0 {
		return Err(UI::failure(&format!(
			"{} of {} items could not be {}",
			failed,
			total,
			action.done()
		)));
	}
	Ok(())
}

async fn restore_trash_item(ui: &mut UI, client: &Client, entry: TrashEntry) -> Result<()> {
	let name = &entry.listed.name;
	match entry.item {
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
	}
	Ok(())
}

async fn delete_trash_item_permanently(
	ui: &mut UI,
	client: &Client,
	entry: TrashEntry,
) -> Result<()> {
	let name = &entry.listed.name;
	match entry.item {
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
	Ok(())
}

/// Permanently deletes everything in the trash, asking first unless --yes is given.
pub(crate) async fn empty_trash(ui: &mut UI, client: &mut LazyClient) -> Result<()> {
	// before logging in and listing the trash, so that a script missing --yes fails at once
	ui.ensure_can_confirm("Permanently delete all trashed items?")?;
	let client = client.get(ui).await?;
	let (dirs, files) = client
		.list_trash(None::<&fn(u64, Option<u64>)>)
		.await
		.context("Failed to list trash")?;
	let count = dirs.len().saturating_add(files.len());
	if count == 0 {
		ui.print_muted("Trash is empty");
		return Ok(());
	}
	if !ui.prompt_confirm(&empty_trash_confirmation(count), false)? {
		return Ok(());
	}
	// the server empties the trash as it is then, so an item trashed since the listing goes
	// too, uncounted
	client
		.empty_trash()
		.await
		.context("Failed to empty trash")?;
	ui.print_success(&format!("Emptied trash ({})", count_of_items(count)));
	Ok(())
}

/// What `empty-trash` asks before deleting the `count` trashed items.
fn empty_trash_confirmation(count: usize) -> String {
	match count {
		1 => "Permanently delete the 1 trashed item?".to_owned(),
		_ => format!("Permanently delete all {} trashed items?", count),
	}
}

/// `1 item`, or `N items`.
fn count_of_items(count: usize) -> String {
	match count {
		1 => "1 item".to_owned(),
		_ => format!("{} items", count),
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::ui::test_support::Printed;
	use chrono::TimeZone as _;
	use filen_sdk_rs::fs::copy::SkippedEntry;

	/// What `head -n lines` prints of a file read in `chunks`, and how many chunks it read.
	fn head_of(chunks: &[&[u8]], lines: usize) -> (Vec<u8>, usize) {
		let mut head = HeadLines::new(lines);
		let mut printed = Vec::new();
		for (read, chunk) in chunks.iter().enumerate() {
			let (part, done) = head.cut(chunk);
			printed.extend_from_slice(part);
			if done {
				return (printed, read + 1);
			}
		}
		(printed, chunks.len())
	}

	/// What [`last_lines`] finds of `file` with `held_max`, read back to front in blocks of
	/// `block_size` bytes, and which blocks (by their start) it read.
	fn last_lines_of(
		file: &[u8],
		block_size: u64,
		held_max: u64,
		lines: usize,
	) -> (LastLines, Vec<u64>) {
		let reads = std::cell::RefCell::new(Vec::new());
		let found = futures::executor::block_on(last_lines(
			file.len() as u64,
			lines,
			block_size,
			held_max,
			|start, end| {
				reads.borrow_mut().push(start);
				let block =
					file[usize::try_from(start).unwrap()..usize::try_from(end).unwrap()].to_vec();
				async move { Ok(block) }
			},
		))
		.unwrap();
		(found, reads.into_inner())
	}

	/// What `tail -n lines` prints of `file`, read back to front in blocks of `block_size` bytes
	/// aligned to its start, as the chunks of a file are, and how many blocks it read.
	fn tail_of(file: &[u8], block_size: u64, lines: usize) -> (Vec<u8>, usize) {
		let (found, reads) = last_lines_of(file, block_size, u64::MAX, lines);
		let LastLines::Read(blocks) = found else {
			panic!("kept no blocks: {found:?}");
		};
		(blocks.concat(), reads.len())
	}

	#[test]
	fn head_prints_the_first_lines_with_their_newlines() {
		assert_eq!(head_of(&[b"a\nb\nc\n"], 2), (b"a\nb\n".to_vec(), 1));
	}

	#[test]
	fn head_prints_a_last_line_without_a_newline_as_it_is() {
		assert_eq!(head_of(&[b"a\nb"], 2), (b"a\nb".to_vec(), 1));
	}

	#[test]
	fn head_keeps_the_carriage_returns_of_crlf_lines() {
		assert_eq!(
			head_of(&[b"a\r\nb\r\nc\r\n"], 2),
			(b"a\r\nb\r\n".to_vec(), 1)
		);
	}

	#[test]
	fn head_of_more_lines_than_the_file_has_prints_the_whole_file() {
		assert_eq!(head_of(&[b"a\n", b"b\n"], 10), (b"a\nb\n".to_vec(), 2));
	}

	#[test]
	fn head_of_an_empty_file_prints_nothing() {
		assert_eq!(head_of(&[], 3), (Vec::new(), 0));
	}

	#[test]
	fn head_of_zero_lines_prints_nothing() {
		assert_eq!(head_of(&[b"a\n"], 0), (Vec::new(), 1));
	}

	#[test]
	fn head_joins_a_line_split_across_chunks_and_reads_no_further_chunk() {
		assert_eq!(
			head_of(&[b"ab", b"c\nde", b"f\ng", b"h\n"], 2),
			(b"abc\ndef\n".to_vec(), 3)
		);
	}

	#[test]
	fn tail_prints_the_last_lines_with_their_newlines() {
		assert_eq!(tail_of(b"a\nb\nc\n", 64, 2), (b"b\nc\n".to_vec(), 1));
	}

	#[test]
	fn tail_prints_a_last_line_without_a_newline_as_it_is() {
		assert_eq!(tail_of(b"a\nb\nc", 64, 2), (b"b\nc".to_vec(), 1));
	}

	#[test]
	fn tail_keeps_the_carriage_returns_of_crlf_lines() {
		assert_eq!(tail_of(b"a\r\nb\r\nc\r\n", 64, 1), (b"c\r\n".to_vec(), 1));
	}

	#[test]
	fn tail_of_more_lines_than_the_file_has_prints_the_whole_file() {
		assert_eq!(tail_of(b"a\nb\n", 64, 10), (b"a\nb\n".to_vec(), 1));
	}

	#[test]
	fn tail_of_an_empty_file_prints_nothing() {
		assert_eq!(tail_of(b"", 64, 3), (Vec::new(), 0));
	}

	#[test]
	fn tail_of_zero_lines_prints_nothing() {
		assert_eq!(tail_of(b"a\nb\n", 64, 0), (Vec::new(), 1));
	}

	#[test]
	fn tail_counts_empty_lines() {
		assert_eq!(tail_of(b"a\n\n\n", 64, 2), (b"\n\n".to_vec(), 1));
	}

	#[test]
	fn tail_finds_a_line_split_across_blocks_and_scans_no_block_before_it() {
		// blocks: "zzzz" "aaa\n" "bbbb" "b\ncc" "c\n"
		assert_eq!(
			tail_of(b"zzzzaaa\nbbbbb\nccc\n", 4, 2),
			(b"bbbbb\nccc\n".to_vec(), 4)
		);
	}

	#[test]
	fn tail_of_a_last_line_longer_than_a_block_scans_back_to_its_start() {
		assert_eq!(tail_of(b"a\nbbbbbbbbb", 4, 1), (b"bbbbbbbbb".to_vec(), 3));
	}

	#[test]
	fn tail_reads_each_block_once() {
		let (_, reads) = last_lines_of(b"zzzzaaa\nbbbbb\nccc\n", 4, u64::MAX, 2);
		assert_eq!(reads, vec![16, 12, 8, 4]);
	}

	#[test]
	fn tail_keeps_lines_that_fill_exactly_the_bytes_it_may_hold() {
		assert_eq!(
			last_lines_of(b"zzzzaaa\nbbbbb\nccc\n", 4, 10, 2).0,
			LastLines::Read(vec![b"bbbb".to_vec(), b"b\ncc".to_vec(), b"c\n".to_vec()]),
		);
	}

	#[test]
	fn tail_of_lines_holding_more_bytes_than_it_may_hold_gives_where_they_start() {
		assert_eq!(
			last_lines_of(b"zzzzaaa\nbbbbb\nccc\n", 4, 9, 2).0,
			LastLines::From(8),
		);
	}

	#[test]
	fn file_content_is_written_to_stdout_as_it_is() {
		let printed = Printed::default();
		let mut ui = printed.ui();

		ui.write_stdout_bytes(b"\x00\xff no newline").unwrap();

		assert_eq!(printed.bytes(), b"\x00\xff no newline");
	}

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

	fn source(path: &RemotePath) -> Source<'_> {
		Source {
			path,
			name: path.basename().unwrap(),
			uuid: Uuid::from_u128(1),
		}
	}

	fn target(
		placement: Placement,
		source_path: &str,
		destination: &str,
		found: Found,
	) -> Result<Target> {
		let source_path = RemotePath::new(source_path);
		target_of(
			placement,
			&source(&source_path),
			&RemotePath::new(destination),
			found,
			names_a_directory(destination),
		)
	}

	#[test]
	fn an_existing_directory_takes_the_source_under_its_own_name() {
		assert_eq!(
			target(Placement::Copy, "/a/x.txt", "/b", Found::Directory).unwrap(),
			Target::Into {
				dir: RemotePath::new("/b"),
				path: RemotePath::new("/b/x.txt"),
			}
		);
	}

	#[test]
	fn a_missing_destination_is_the_new_path_of_the_source() {
		assert_eq!(
			target(Placement::Copy, "/a/x.txt", "/b/y.txt", Found::Nothing).unwrap(),
			Target::At {
				parent: RemotePath::new("/b"),
				name: "y.txt".to_owned(),
				path: RemotePath::new("/b/y.txt"),
			}
		);
	}

	#[test]
	fn a_missing_destination_with_a_trailing_slash_is_refused() {
		assert_eq!(
			target(Placement::Move, "/a/x.txt", "/b/", Found::Nothing)
				.unwrap_err()
				.to_string(),
			"No such destination directory: /b"
		);
	}

	#[test]
	fn a_file_destination_with_a_trailing_slash_is_not_a_directory() {
		assert_eq!(
			target(Placement::Copy, "/a/x.txt", "/b/", Found::File)
				.unwrap_err()
				.to_string(),
			"Not a directory: /b"
		);
	}

	#[test]
	fn an_existing_file_destination_is_refused() {
		assert_eq!(
			target(Placement::Copy, "/a/x.txt", "/b.txt", Found::File)
				.unwrap_err()
				.to_string(),
			"Destination already exists: /b.txt"
		);
	}

	#[test]
	fn a_move_into_the_directory_the_source_is_in_is_refused() {
		assert_eq!(
			target(Placement::Move, "/a/x.txt", "/a", Found::Directory)
				.unwrap_err()
				.to_string(),
			"/a/x.txt is already in /a"
		);
	}

	#[test]
	fn a_copy_into_the_directory_the_source_is_in_is_left_to_the_name_check() {
		assert_eq!(
			target(Placement::Copy, "/a/x.txt", "/a", Found::Directory)
				.unwrap()
				.path(),
			&RemotePath::new("/a/x.txt")
		);
	}

	#[test]
	fn a_directory_is_not_copied_into_itself() {
		assert_eq!(
			target(Placement::Copy, "/a/d", "/a/d", Found::Directory)
				.unwrap_err()
				.to_string(),
			"Cannot copy /a/d into itself: /a/d/d"
		);
		assert_eq!(
			target(Placement::Copy, "/a/d", "/a/d/e/f", Found::Nothing)
				.unwrap_err()
				.to_string(),
			"Cannot copy /a/d into itself: /a/d/e/f"
		);
	}

	#[test]
	fn a_directory_is_not_moved_into_itself() {
		assert_eq!(
			target(Placement::Move, "/a/d", "/a/d/e", Found::Nothing)
				.unwrap_err()
				.to_string(),
			"Cannot move /a/d into itself: /a/d/e"
		);
	}

	#[test]
	fn a_sibling_sharing_a_name_prefix_is_not_inside() {
		assert!(!is_inside(
			&RemotePath::new("/a/dd"),
			&RemotePath::new("/a/d")
		));
		assert!(is_inside(
			&RemotePath::new("/a/d/e"),
			&RemotePath::new("/a/d")
		));
		assert!(!is_inside(
			&RemotePath::new("/a/d"),
			&RemotePath::new("/a/d")
		));
	}

	#[test]
	fn the_copied_amount_counts_files_and_bytes_against_the_plan() {
		let report = CopyReport {
			totals: PlanTotals {
				dirs: 1,
				files: 10,
				bytes: 5 * 1024 * 1024,
			},
			counts: ItemCounts {
				files_done: 3,
				bytes_done: 1024 * 1024,
				..ItemCounts::default()
			},
			..CopyReport::default()
		};

		assert_eq!(copied_amount(&report), "3 of 10 files (1 MiB of 5 MiB)");
	}

	fn skipped(source_path: &str) -> SkippedEntry {
		SkippedEntry {
			source_path: source_path.to_owned(),
			bytes: 0,
			reason: SkipReason::UndecryptableFile {
				uuid: Uuid::from_u128(1),
			},
		}
	}

	#[test]
	fn the_problems_of_a_copy_are_listed() {
		let printed = Printed::default();
		let mut ui = printed.ui();
		let report = CopyReport {
			skipped: vec![skipped("d/a"), skipped("d/b")],
			..CopyReport::default()
		};

		print_copy_problems(&mut ui, &report);
		assert_eq!(
			printed.lines(),
			"✘ Skipped d/a: cannot be decrypted\n✘ Skipped d/b: cannot be decrypted"
		);
	}

	#[test]
	fn the_problems_of_a_copy_past_the_limit_are_counted() {
		let printed = Printed::default();
		let mut ui = printed.ui();
		let report = CopyReport {
			skipped: (0..MAX_PRINTED_COPY_PROBLEMS + 2)
				.map(|i| skipped(&format!("f{i}")))
				.collect(),
			..CopyReport::default()
		};

		print_copy_problems(&mut ui, &report);
		let printed = printed.lines();
		let lines = printed.lines().collect::<Vec<&str>>();
		assert_eq!(lines.len(), MAX_PRINTED_COPY_PROBLEMS + 1);
		assert_eq!(
			lines[MAX_PRINTED_COPY_PROBLEMS - 1],
			"✘ Skipped f9: cannot be decrypted"
		);
		assert_eq!(lines[MAX_PRINTED_COPY_PROBLEMS], "✘ ... and 2 more");
	}

	#[test]
	fn the_progress_of_a_scan_counts_the_directories_listed() {
		let scan = ScanProgress {
			sources_done: 2,
			sources_total: 5,
			..ScanProgress::default()
		};

		assert_eq!(
			progress_text(
				CopyPhase::Scanning,
				&scan,
				&PlanTotals::default(),
				&ItemCounts::default()
			)
			.unwrap(),
			"Scanning... 2/5 directories listed"
		);
	}

	#[test]
	fn the_progress_of_a_copy_counts_bytes_and_files() {
		let totals = PlanTotals {
			dirs: 0,
			files: 4,
			bytes: 4 * 1024,
		};
		let counts = ItemCounts {
			files_done: 1,
			bytes_done: 1024,
			..ItemCounts::default()
		};

		assert_eq!(
			progress_text(
				CopyPhase::CopyingFiles,
				&ScanProgress::default(),
				&totals,
				&counts
			)
			.unwrap(),
			"Copying 1 KiB / 4 KiB (25%) - 1/4 files"
		);
	}

	#[test]
	fn the_progress_of_a_copy_of_empty_files_counts_files() {
		let totals = PlanTotals {
			dirs: 1,
			files: 3,
			bytes: 0,
		};
		let counts = ItemCounts {
			files_done: 2,
			..ItemCounts::default()
		};

		assert_eq!(
			progress_text(
				CopyPhase::CopyingFiles,
				&ScanProgress::default(),
				&totals,
				&counts
			)
			.unwrap(),
			"Copying 2/3 files"
		);
	}

	#[test]
	fn the_progress_percentage_does_not_overflow_and_stops_at_100() {
		let totals = PlanTotals {
			dirs: 0,
			files: 1,
			bytes: u64::MAX - 1,
		};
		let counts = ItemCounts {
			files_done: 1,
			bytes_done: u64::MAX,
			..ItemCounts::default()
		};

		let text = progress_text(
			CopyPhase::Finishing,
			&ScanProgress::default(),
			&totals,
			&counts,
		)
		.unwrap();
		assert!(text.contains("(100%)"), "{text}");
	}

	#[test]
	fn an_ended_copy_shows_no_progress() {
		for phase in [CopyPhase::Done, CopyPhase::Cancelled, CopyPhase::Failed] {
			assert_eq!(
				progress_text(
					phase,
					&ScanProgress::default(),
					&PlanTotals::default(),
					&ItemCounts::default()
				),
				None
			);
		}
	}

	#[test]
	fn no_progress_is_drawn_when_stdout_is_not_a_terminal() {
		assert!(!CopyProgress::new(&Printed::default().ui()).enabled);
	}

	/// A trashed file named `name` with the UUID `uuid`.
	fn trashed(name: &str, uuid: u128) -> ListedItem {
		ListedItem {
			kind: ListedKind::File,
			name: name.to_owned(),
			path: None,
			size: Some(7),
			modified: None,
			created: None,
			favorited: false,
			uuid: Uuid::from_u128(uuid),
		}
	}

	fn trash() -> Vec<ListedItem> {
		vec![
			trashed("a.txt", 1),
			trashed("shared", 2),
			trashed("shared", 3),
		]
	}

	fn resolve(operands: &[&str]) -> Result<Vec<usize>, Vec<String>> {
		let operands = operands
			.iter()
			.map(|operand| (*operand).to_owned())
			.collect::<Vec<String>>();
		resolve_trash_operands(trash().iter(), &operands)
	}

	#[test]
	fn a_trashed_item_is_named_by_its_name() {
		assert_eq!(resolve(&["a.txt"]), Ok(vec![0]));
	}

	#[test]
	fn a_trashed_item_sharing_its_name_is_named_by_its_bare_uuid() {
		assert_eq!(resolve(&[&Uuid::from_u128(3).to_string()]), Ok(vec![2]));
	}

	#[test]
	fn a_trashed_item_is_named_by_its_prefixed_uuid() {
		assert_eq!(
			resolve(&[&format!("uuid:{}", Uuid::from_u128(2))]),
			Ok(vec![1])
		);
	}

	#[test]
	fn a_prefixed_uuid_names_only_the_item_with_that_uuid() {
		let uuid = Uuid::from_u128(1);
		let items = [trashed(&uuid.to_string(), 9), trashed("a.txt", 1)];
		assert_eq!(
			resolve_trash_operands(items.iter(), &[format!("uuid:{}", uuid)]),
			Ok(vec![1])
		);
	}

	#[test]
	fn a_prefix_without_a_uuid_is_refused() {
		assert_eq!(
			resolve(&["uuid:a.txt"]),
			Err(vec!["Not a UUID: a.txt".to_owned()])
		);
	}

	#[test]
	fn a_uuid_no_trashed_item_has_is_refused() {
		assert_eq!(
			resolve(&[&format!("uuid:{}", Uuid::from_u128(4))]),
			Err(vec![format!(
				"No item with UUID {} in the trash",
				Uuid::from_u128(4)
			)])
		);
	}

	#[test]
	fn several_trashed_items_are_named_in_the_order_given_each_once() {
		assert_eq!(
			resolve(&[
				&Uuid::from_u128(3).to_string(),
				"a.txt",
				&format!("uuid:{}", Uuid::from_u128(3)),
			]),
			Ok(vec![2, 0])
		);
	}

	#[test]
	fn a_name_several_trashed_items_share_is_refused_listing_them() {
		let mut items = trash();
		items[2] = ListedItem {
			kind: ListedKind::Directory,
			size: None,
			created: Some(Utc.with_ymd_and_hms(2023, 1, 2, 3, 4, 5).unwrap()),
			..trashed("shared", 3)
		};
		assert_eq!(
			resolve_trash_operands(items.iter(), &["shared".to_owned()]),
			Err(vec![format!(
				"2 trashed items are named shared, so name one by its UUID:\n  {}  file  7 B  -\n  \
				 {}  directory  -  2023-01-02 03:04:05 (UTC)",
				Uuid::from_u128(2),
				Uuid::from_u128(3)
			)])
		);
	}

	#[test]
	fn a_name_no_trashed_item_has_is_refused() {
		assert_eq!(
			resolve(&["A.TXT"]),
			Err(vec!["No such item in the trash: A.TXT".to_owned()])
		);
	}

	#[test]
	fn every_operand_naming_no_single_item_is_reported() {
		assert_eq!(
			resolve(&["missing", "a.txt", "shared"])
				.unwrap_err()
				.iter()
				.map(|problem| problem.lines().next().unwrap())
				.collect::<Vec<&str>>(),
			[
				"No such item in the trash: missing",
				"2 trashed items are named shared, so name one by its UUID:"
			]
		);
	}

	#[test]
	fn deleting_one_trashed_item_asks_about_it_by_name() {
		assert_eq!(
			trash_deletion_confirmation(&["a.txt"]),
			"Permanently delete a.txt?"
		);
	}

	#[test]
	fn deleting_several_trashed_items_asks_about_them_by_count() {
		assert_eq!(
			trash_deletion_confirmation(&["a.txt", "b.txt"]),
			"Permanently delete 2 items?"
		);
	}

	#[test]
	fn no_operands_pick_a_trashed_item_and_all_wins_over_none() {
		assert!(matches!(
			TrashSelection::of(Vec::new(), false),
			TrashSelection::Pick
		));
		assert!(matches!(
			TrashSelection::of(Vec::new(), true),
			TrashSelection::All
		));
		assert!(matches!(
			TrashSelection::of(vec!["a".to_owned()], false),
			TrashSelection::Named(operands) if operands == ["a"]
		));
	}

	#[test]
	fn a_trashed_file_is_offered_with_its_size_and_modification_date() {
		let file = ListedItem {
			modified: Some(Utc.with_ymd_and_hms(2024, 3, 9, 10, 20, 30).unwrap()),
			..trashed("a.txt", 1)
		};
		assert_eq!(
			trash_picker_label(&file, false),
			"a.txt  7 B  2024-03-09 10:20:30 (UTC)"
		);
	}

	#[test]
	fn a_trashed_directory_is_offered_with_a_slash_and_its_creation_date() {
		let dir = ListedItem {
			kind: ListedKind::Directory,
			size: None,
			created: Some(Utc.with_ymd_and_hms(2023, 1, 2, 3, 4, 5).unwrap()),
			..trashed("docs", 1)
		};
		assert_eq!(
			trash_picker_label(&dir, false),
			"docs/  2023-01-02 03:04:05 (UTC)"
		);
	}

	#[test]
	fn a_trashed_item_sharing_its_name_is_offered_with_its_uuid() {
		assert_eq!(
			trash_picker_label(&trashed("shared", 2), true),
			format!("shared ({})  7 B", Uuid::from_u128(2))
		);
	}

	#[test]
	fn emptying_a_trash_of_one_item_asks_about_that_item() {
		assert_eq!(
			empty_trash_confirmation(1),
			"Permanently delete the 1 trashed item?"
		);
	}

	#[test]
	fn emptying_a_trash_of_several_items_asks_about_all_of_them() {
		assert_eq!(
			empty_trash_confirmation(3),
			"Permanently delete all 3 trashed items?"
		);
	}

	#[test]
	fn an_item_count_is_singular_only_for_one() {
		assert_eq!(count_of_items(0), "0 items");
		assert_eq!(count_of_items(1), "1 item");
		assert_eq!(count_of_items(2), "2 items");
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

	fn date(year: i32, month: u32, day: u32) -> DateTime<Utc> {
		Utc.with_ymd_and_hms(year, month, day, 10, 20, 30).unwrap()
	}

	fn listed_dir(name: &str, uuid: u128) -> ListedItem {
		ListedItem {
			kind: ListedKind::Directory,
			name: name.to_owned(),
			path: None,
			size: None,
			modified: None,
			created: None,
			favorited: false,
			uuid: Uuid::from_u128(uuid),
		}
	}

	fn listed_file(name: &str, size: u64, uuid: u128) -> ListedItem {
		ListedItem {
			kind: ListedKind::File,
			name: name.to_owned(),
			path: None,
			size: Some(size),
			modified: None,
			created: None,
			favorited: false,
			uuid: Uuid::from_u128(uuid),
		}
	}

	/// What `print_listing` prints for `items`, sorted first, without colors.
	fn printed_listing(mut items: Vec<ListedItem>, by: ListedBy, long: bool, json: bool) -> String {
		let printed = Printed::default();
		let mut ui = printed.ui();
		ui.json = json;
		sort_listing(&mut items, by);
		print_listing(&mut ui, &items, by, "Directory is empty", long).unwrap();
		console::strip_ansi_codes(&printed.lines()).into_owned()
	}

	#[test]
	fn a_long_listing_prints_directories_first_with_a_size_column() {
		let items = vec![
			listed_file("b.txt", 2048, 1),
			listed_dir("docs", 2),
			listed_file("a.txt", 5, 3),
		];

		assert_eq!(
			printed_listing(items, ListedBy::Name, true, false),
			"    -  -  docs\n  5 B  -  a.txt\n2 KiB  -  b.txt"
		);
	}

	#[test]
	fn a_long_listing_shows_when_a_file_was_modified_and_a_directory_created() {
		let items = vec![
			ListedItem {
				modified: Some(date(2024, 3, 9)),
				created: Some(date(2020, 1, 1)),
				..listed_file("a.txt", 5, 1)
			},
			ListedItem {
				created: Some(date(2023, 12, 31)),
				..listed_dir("docs", 2)
			},
			listed_file("b.txt", 5, 3),
		];

		assert_eq!(
			printed_listing(items, ListedBy::Name, true, false),
			"  -  2023-12-31 10:20:30 (UTC)  docs\n\
			 5 B  2024-03-09 10:20:30 (UTC)  a.txt\n\
			 5 B  -                          b.txt"
		);
	}

	#[test]
	fn a_long_listing_marks_favorited_items() {
		let items = vec![
			ListedItem {
				favorited: true,
				..listed_dir("docs", 1)
			},
			ListedItem {
				favorited: true,
				..listed_file("a.txt", 5, 2)
			},
			listed_file("b.txt", 5, 3),
		];

		assert_eq!(
			printed_listing(items, ListedBy::Name, true, false),
			"  -  -  docs (*)\n5 B  -  a.txt (*)\n5 B  -  b.txt"
		);
	}

	#[test]
	fn a_json_listing_prints_an_object_per_item() {
		let items = vec![
			ListedItem {
				modified: Some(date(2024, 3, 9)),
				created: Some(date(2020, 1, 1)),
				favorited: true,
				..listed_file("a.txt", 5, 1)
			},
			listed_dir("docs", 2),
		];

		let printed: serde_json::Value =
			serde_json::from_str(&printed_listing(items, ListedBy::Name, false, true)).unwrap();

		assert_eq!(
			printed,
			json!([
				{
					"name": "docs",
					"type": "directory",
					"created": null,
					"favorited": false,
					"uuid": "00000000-0000-0000-0000-000000000002",
				},
				{
					"name": "a.txt",
					"type": "file",
					"size": 5,
					"modified": "2024-03-09T10:20:30Z",
					"created": "2020-01-01T10:20:30Z",
					"favorited": true,
					"uuid": "00000000-0000-0000-0000-000000000001",
				},
			])
		);
	}

	#[test]
	fn a_json_listing_by_path_gives_each_items_path() {
		let items = vec![
			ListedItem {
				path: Some("/a/docs".to_owned()),
				..listed_dir("docs", 1)
			},
			listed_dir("lost", 2),
		];

		let printed: serde_json::Value =
			serde_json::from_str(&printed_listing(items, ListedBy::Path, false, true)).unwrap();

		assert_eq!(
			printed,
			json!([
				{
					"name": "docs",
					"path": "/a/docs",
					"type": "directory",
					"created": null,
					"favorited": false,
					"uuid": "00000000-0000-0000-0000-000000000001",
				},
				{
					"name": "lost",
					"path": null,
					"type": "directory",
					"created": null,
					"favorited": false,
					"uuid": "00000000-0000-0000-0000-000000000002",
				},
			])
		);
	}

	#[test]
	fn an_empty_json_listing_is_an_empty_array() {
		assert_eq!(
			printed_listing(Vec::new(), ListedBy::Name, false, true),
			"[]"
		);
	}

	#[test]
	fn numbers_in_names_are_ordered_by_value() {
		assert_eq!(natural_cmp("file2", "file10"), Ordering::Less);
		assert_eq!(natural_cmp("file10", "file2"), Ordering::Greater);
		assert_eq!(natural_cmp("2024-9.txt", "2024-10.txt"), Ordering::Less);
		assert_eq!(
			natural_cmp("v99999999999999999999999", "v100000000000000000000000"),
			Ordering::Less
		);
	}

	#[test]
	fn names_are_ordered_ignoring_case() {
		assert_eq!(natural_cmp("a", "B"), Ordering::Less);
		assert_eq!(natural_cmp("B", "a"), Ordering::Greater);
		assert_eq!(natural_cmp("Ä", "b"), Ordering::Greater);
	}

	#[test]
	fn names_equal_ignoring_case_and_zeros_are_ordered_by_their_characters() {
		assert_eq!(natural_cmp("README", "readme"), Ordering::Less);
		assert_eq!(natural_cmp("readme", "README"), Ordering::Greater);
		assert_eq!(natural_cmp("file02", "file2"), Ordering::Less);
		assert_eq!(natural_cmp("same", "same"), Ordering::Equal);
	}

	#[test]
	fn a_name_comes_before_the_longer_names_it_starts() {
		assert_eq!(natural_cmp("file", "file1"), Ordering::Less);
		assert_eq!(natural_cmp("file1", "file1a"), Ordering::Less);
		assert_eq!(natural_cmp("", "a"), Ordering::Less);
	}

	#[test]
	fn a_listing_is_sorted_naturally_with_directories_first() {
		let items = vec![
			listed_file("file10.txt", 1, 1),
			listed_file("File2.txt", 1, 2),
			listed_dir("b", 3),
			listed_file("a.txt", 1, 4),
			listed_dir("A", 5),
		];

		assert_eq!(
			printed_listing(items, ListedBy::Name, false, false),
			"A\nb\na.txt\nFile2.txt\nfile10.txt"
		);
	}

	#[test]
	fn an_empty_listing_says_so() {
		assert_eq!(
			printed_listing(Vec::new(), ListedBy::Name, true, false),
			"Directory is empty"
		);
	}

	#[test]
	fn a_listing_by_path_falls_back_to_the_uuid_of_an_unresolved_item() {
		let items = vec![
			ListedItem {
				path: Some("/a/b.txt".to_owned()),
				..listed_file("b.txt", 1, 1)
			},
			listed_file("c.txt", 1, 2),
		];

		assert_eq!(
			printed_listing(items, ListedBy::Path, false, false),
			"/a/b.txt\n00000000-0000-0000-0000-000000000002"
		);
	}

	#[test]
	fn a_directory_returned_under_the_requested_name_was_created() {
		assert_eq!(existing_directory_name(Some("docs"), "docs"), None);
	}

	#[test]
	fn a_directory_returned_under_a_name_differing_in_case_existed() {
		assert_eq!(existing_directory_name(Some("docs"), "Docs"), Some("Docs"));
	}

	#[test]
	fn pwd_prints_the_working_path() {
		let printed = Printed::default();
		print_working_path(&mut printed.ui(), &RemotePath::new("/a/b")).unwrap();

		assert_eq!(printed.lines(), "/a/b");
	}

	#[test]
	fn pwd_json_prints_the_working_path_as_an_object() {
		let printed = Printed::default();
		let mut ui = printed.ui();
		ui.set_quiet_and_json(false, true);
		print_working_path(&mut ui, &RemotePath::new("/a/b")).unwrap();

		assert_eq!(printed.lines(), "{\n  \"path\": \"/a/b\"\n}");
	}

	#[test]
	fn the_web_drive_url_of_the_root_directory_has_its_uuid() {
		assert_eq!(
			web_drive_url([Uuid::from_u128(1)]),
			"https://app.filen.io/#/drive/00000000-0000-0000-0000-000000000001"
		);
	}

	#[test]
	fn the_web_drive_url_of_a_directory_has_the_uuids_from_the_root_down() {
		assert_eq!(
			web_drive_url([Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3)]),
			"https://app.filen.io/#/drive/00000000-0000-0000-0000-000000000001/\
			 00000000-0000-0000-0000-000000000002/00000000-0000-0000-0000-000000000003"
		);
	}

	fn version(uuid: u128, size: u64, modified: i64) -> RemoteVersion {
		RemoteVersion {
			uuid: Uuid::from_u128(uuid),
			size,
			modified: Utc.timestamp_opt(modified, 0).single(),
		}
	}

	#[test]
	fn an_edit_of_the_unchanged_file_is_uploaded_into_its_directory() {
		assert_eq!(
			edit_upload(
				version(1, 7, 100),
				Some((Uuid::from_u128(9), version(1, 7, 100)))
			),
			EditUpload::Upload {
				parent: Uuid::from_u128(9)
			}
		);
	}

	#[test]
	fn an_edit_of_a_file_replaced_by_another_is_not_uploaded() {
		assert_eq!(
			edit_upload(
				version(1, 7, 100),
				Some((Uuid::from_u128(9), version(2, 7, 100)))
			),
			EditUpload::Changed
		);
	}

	#[test]
	fn an_edit_of_a_file_whose_size_changed_is_not_uploaded() {
		assert_eq!(
			edit_upload(
				version(1, 7, 100),
				Some((Uuid::from_u128(9), version(1, 8, 100)))
			),
			EditUpload::Changed
		);
	}

	#[test]
	fn an_edit_of_a_file_modified_meanwhile_is_not_uploaded() {
		assert_eq!(
			edit_upload(
				version(1, 7, 100),
				Some((Uuid::from_u128(9), version(1, 7, 101)))
			),
			EditUpload::Changed
		);
	}

	#[test]
	fn an_edit_of_a_file_no_longer_at_its_path_is_not_uploaded() {
		assert_eq!(edit_upload(version(1, 7, 100), None), EditUpload::Gone);
	}

	#[test]
	fn the_same_bytes_are_the_same_content() {
		assert!(same_content(&b"abc"[..], &b"abc"[..]).unwrap());
		assert!(same_content(&b""[..], &b""[..]).unwrap());
	}

	#[test]
	fn bytes_differing_in_one_byte_are_not_the_same_content() {
		assert!(!same_content(&b"abc"[..], &b"abd"[..]).unwrap());
	}

	#[test]
	fn bytes_one_longer_than_the_other_are_not_the_same_content() {
		assert!(!same_content(&b"abc"[..], &b"abcd"[..]).unwrap());
		assert!(!same_content(&b"abcd"[..], &b"abc"[..]).unwrap());
	}

	#[test]
	fn content_longer_than_one_buffer_is_compared_to_its_end() {
		let a = vec![7u8; 100_000];
		let mut b = a.clone();

		assert!(same_content(&a[..], &b[..]).unwrap());
		*b.last_mut().unwrap() = 8;
		assert!(!same_content(&a[..], &b[..]).unwrap());
	}

	#[test]
	fn visual_names_the_editor_before_editor() {
		assert_eq!(
			editor_command(Some("code --wait"), Some("vi")).unwrap(),
			Some(EditorCommand {
				program: "code".to_owned(),
				args: vec!["--wait".to_owned()],
			})
		);
	}

	#[test]
	fn editor_names_the_editor_without_visual() {
		assert_eq!(
			editor_command(Some(" "), Some("vi")).unwrap(),
			Some(EditorCommand {
				program: "vi".to_owned(),
				args: Vec::new(),
			})
		);
	}

	#[test]
	fn an_editor_command_is_split_into_words_as_a_shell_splits_it() {
		assert_eq!(
			editor_command(None, Some("'/opt/my editor/bin' -n \"a b\"")).unwrap(),
			Some(EditorCommand {
				program: "/opt/my editor/bin".to_owned(),
				args: vec!["-n".to_owned(), "a b".to_owned()],
			})
		);
	}

	#[test]
	fn no_editor_is_named_without_visual_and_editor() {
		assert_eq!(editor_command(None, Some("")).unwrap(), None);
	}

	#[test]
	fn an_editor_command_with_an_unclosed_quote_fails() {
		assert_eq!(
			editor_command(None, Some("vi 'a")).unwrap_err().to_string(),
			"Cannot split $EDITOR into words: vi 'a"
		);
	}

	#[test]
	fn a_local_copy_keeps_the_files_name() {
		assert_eq!(
			local_file_name(Some("notes.md"), Uuid::from_u128(1)),
			"notes.md"
		);
	}

	#[test]
	fn a_local_copy_of_a_file_whose_name_leads_elsewhere_is_named_by_its_uuid() {
		for name in ["..", ".", "a/b", ""] {
			assert_eq!(
				local_file_name(Some(name), Uuid::from_u128(1)),
				"00000000-0000-0000-0000-000000000001",
				"{name}"
			);
		}
	}

	#[test]
	fn a_local_copy_of_a_file_whose_name_is_not_decrypted_is_named_by_its_uuid() {
		assert_eq!(
			local_file_name(None, Uuid::from_u128(1)),
			"00000000-0000-0000-0000-000000000001"
		);
	}
}
