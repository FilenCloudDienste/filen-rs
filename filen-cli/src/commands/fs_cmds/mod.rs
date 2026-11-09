//! [cli-doc] cloud-paths
//! Commands name the files and directories in your Filen drive by their paths. A path that starts
//! with `/` starts at the root of the drive; any other path starts at the working directory: the
//! one `cd` went to in the REPL, or the one `--working-path` gives (`/` by default). `.` is the
//! directory a path is at and `..` its parent, so `cd ..` goes up one directory, and the root is
//! its own parent. Doubled and trailing slashes change nothing, but on the destination of `mv`
//! and `cp`: `mv a.txt docs/` moves `a.txt` into the existing directory `docs`, and fails if there
//! is none, where `mv a.txt docs` would rename `a.txt` to `docs`.
//!
//! Quote a name that holds spaces, in your shell and in the REPL alike: `cat "my notes.txt"`.
//!
//! `uuid:<UUID>` names the item with that UUID wherever it is, as `stat` and `ls -l --json` show
//! it: `stat uuid:<UUID>`. It is the only way to name an item whose name cannot be decrypted,
//! which listings show by its UUID in place of its name, and of an item below such a directory,
//! which no path leads to. Messages show an item without a path by its UUID, and an item put in a
//! directory without a path as `<name> in uuid:<UUID>`. Only `cd` needs a path, and refuses such
//! a directory; `mv`, `cp`, `edit`, `write` and `touch` need the name of the item itself. An item
//! in the trash is named this way in `trash-restore` and `trash-delete` only. Write a name that
//! starts with `uuid:` as `./uuid:...`.
//!
//! `rm`, `mv`, `cp`, `cat`, `favorite` and `unfavorite` take several items, and a glob in the last
//! segment of a path: `*` matches any run of characters, `?` any one character and `[...]` one of
//! the characters in the brackets (`[a-z]` for a range, `[!...]` for one not in them). Names are
//! matched case-sensitively, and a name that starts with `.` only by a glob that does too. A glob
//! that matches nothing is an error, and an item whose name is exactly the glob is that item. Quote
//! globs so that your shell does not match them against your local files: `rm '/photos/*.tmp'`.
//! With several sources, the destination of `mv` and `cp` must be an existing directory. A command
//! given several items goes on past one it fails for, says why, and exits with code 1 at the end.
//!
//! With `--json`, commands print JSON whose keys are the same everywhere. Sizes are in bytes,
//! dates are RFC 3339 strings in UTC, and a date an item lacks is `null`:
//! - `stat` prints a file as `{"name", "type": "file", "size", "modified", "created", "uuid"}`, a
//!   directory as `{"name", "type": "directory", "size", "files", "directories", "created",
//!   "color", "uuid"}`, and the drive (`stat /`) as `{"type": "drive", "usedStorage",
//!   "totalStorage", "files", "directories"}`. The old CLI's `mtimeMs` is now `modified`.
//! - `ls`, `ls -R`, `tree`, `list-trash`, `list-favorites` and `list-recents` print an array with
//!   an object per item: `{"name", "type", "created", "favorited", "uuid"}`, plus `"size"` and
//!   `"modified"` for a file and `"color"` for a directory. `ls -R` and `tree` add `"path"`, the
//!   path below the listed directory, and the listings of items from many directories
//!   (`list-favorites`, `list-recents`) the absolute `"path"`, which is `null` when it cannot be
//!   found out. An empty listing is `[]`.
//! - `list-public-links` prints an array with an object per link instead: `{"path", "type",
//!   "uuid", "linkUuid", "expiresAt", "url"}`, where `"expiresAt"` is `null` for a link that never
//!   expires.
//! - `search` prints `{"results": [{"path", "type", "size", "uuid"}], "total", "complete"}`.
//! - `du` prints `{"path", "size", "items": [{"name", "type", "size"}]}`, `versions` an array of
//!   `{"uuid", "timestamp", "size", "current"}`, `cat`, `head` and `tail` `{"text"}`, `pwd`
//!   `{"path"}`, and `view` and `drive` `{"url"}`.

use std::{
	borrow::Cow,
	cmp::Ordering,
	collections::HashMap,
	env::{self, VarError},
	ffi::OsStr,
	fmt,
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
	command_line::split_command_line,
	commands::CommandContext,
	completion::FilenCompleter,
	docs::DocElement,
	error::{CliError, find_cli_error},
	interrupt,
	ui::{self, UI, name_or_uuid},
	util::RemotePath,
};
use anyhow::{Context as _, Result};
use chrono::{DateTime, Utc};
use clap::Subcommand;
use console::{Term, style};
use filen_sdk_rs::{
	Error, ErrorKind,
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
		dir::{meta::DirectoryMetaChanges, traits::HasRemoteDirInfo as _},
		file::{
			FileBuilderOptionalName, client_impl::FileReaderSharedClientExt as _,
			meta::FileMetaChanges, read::FileReader, traits::HasFileInfo as _,
		},
		name::ValidatedName,
	},
	io::{RemoteDirectory, RemoteFile, client_impl::IoSharedClientExt as _},
};
use filen_types::{
	api::v3::dir::color::DirColor,
	fs::{ParentUuid, Uuid},
	traits::CowHelpers as _,
};
use futures::{
	AsyncBufReadExt as _, AsyncRead, AsyncReadExt as _, SinkExt as _, StreamExt as _,
	TryStreamExt as _, channel::mpsc, executor, stream,
};
use serde_json::json;
use tempfile::{NamedTempFile, TempDir};
use tokio::select;

mod operand;
mod tree;
mod versions;

pub(crate) use operand::find_existing_item;
use operand::{
	Given, GivenItem, ItemOperand, directory_chain, expand_all, optional, parse_operands, path_of,
	resolve_item, shown_as,
};

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
		/// Use a long listing format: sizes, modification dates (creation dates of directories),
		/// [color] after colored directories and (*) after favorited items
		#[arg(short = 'l', long)]
		long: bool,
		/// List everything below the directory, one path relative to it per line, each directory
		/// followed by what it holds
		#[arg(short = 'R', long)]
		recursive: bool,
	},
	/// Draw everything below a directory as a tree (with --json: the listing of ls -R --json)
	Tree {
		/// Directory to draw (default: the current working directory)
		#[arg(add = FilenCompleter::directory())]
		directory: Option<String>,
	},
	/// Show the size of each item in a directory, a directory's being that of every file below
	/// it, and the total
	Du {
		/// Directory or file to show the size of (default: the current working directory)
		#[arg(add = FilenCompleter::file_or_directory())]
		file_or_directory: Option<String>,
		/// Show only the total
		#[arg(short = 's', long)]
		summarize: bool,
	},
	/// Print the contents of files as the bytes they hold, one after the other (with --json:
	/// {"text": ...}, for UTF-8 text only)
	Cat {
		/// Files to print, or globs such as '*.txt' (quoted)
		#[arg(add = FilenCompleter::file(), required = true)]
		files: Vec<String>,
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
	/// Remove files or directories
	Rm {
		/// Files or directories to remove, or globs such as '*.txt' (quoted)
		#[arg(add = FilenCompleter::file_or_directory(), required = true)]
		files_or_directories: Vec<String>,
		/// Permanently delete the files or directories, asking first unless --yes is given
		/// (default: move to trash)
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
	/// Move and/or rename files or directories
	Mv {
		/// Files or directories to move, or globs such as '*.txt' (quoted)
		#[arg(add = FilenCompleter::file_or_directory(), required = true)]
		sources: Vec<String>,
		/// Destination: an existing directory to move the sources into,
		/// or, for one source, its new path (to move and/or rename it)
		#[arg(add = FilenCompleter::file_or_directory())]
		destination: String,
	},
	/// Copy files or directories
	Cp {
		/// Files or directories to copy, or globs such as '*.txt' (quoted)
		#[arg(add = FilenCompleter::file_or_directory(), required = true)]
		sources: Vec<String>,
		/// Destination: an existing directory to copy the sources into,
		/// or, for one source, the path of the copy (to copy it under a new name)
		#[arg(add = FilenCompleter::file_or_directory())]
		destination: String,
	},
	/// List the versions of a file, newest first, or restore or delete one of them
	Versions {
		/// File whose versions to list
		#[arg(add = FilenCompleter::file())]
		file: String,
		/// Restore the version with this UUID, making it the file's current content
		#[arg(long, value_name = "VERSION", conflicts_with = "delete")]
		restore: Option<Uuid>,
		/// Permanently delete the version with this UUID, asking first unless --yes is given
		#[arg(long, value_name = "VERSION")]
		delete: Option<Uuid>,
	},
	/// List favorited items
	ListFavorites,
	/// Favorite files or directories
	Favorite {
		/// Files or directories to favorite, or globs such as '*.txt' (quoted)
		#[arg(add = FilenCompleter::file_or_directory(), required = true)]
		files_or_directories: Vec<String>,
	},
	/// Unfavorite files or directories
	Unfavorite {
		/// Files or directories to unfavorite, or globs such as '*.txt' (quoted)
		#[arg(add = FilenCompleter::file_or_directory(), required = true)]
		files_or_directories: Vec<String>,
	},
	/// Set the color of a directory
	Color {
		/// Directory to color
		#[arg(add = FilenCompleter::directory())]
		directory: String,
		/// The color: default, blue, green, purple, red, gray, or a hex color like #85bcff
		#[arg(value_parser = parse_dir_color)]
		color: DirColor<'static>,
	},
	/// List recently accessed items
	ListRecents,
	/// List trashed items
	ListTrash {
		/// Use a long listing format: sizes, modification dates (creation dates of directories),
		/// [color] after colored directories and (*) after favorited items
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

/// How paths name items, and the commands that open the docs' "Accessing Files" section, before
/// `search`.
pub(crate) const BROWSING_DOCS: &[DocElement] = &[
	DocElement::DocFragment("cloud-paths"),
	DocElement::CommandHelp("cd"),
	DocElement::CommandHelp("pwd"),
	DocElement::CommandHelp("ls"),
	DocElement::CommandHelp("tree"),
	DocElement::CommandHelp("du"),
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
	DocElement::CommandHelp("versions"),
	DocElement::CommandHelp("list-favorites"),
	DocElement::CommandHelp("favorite"),
	DocElement::CommandHelp("unfavorite"),
	DocElement::CommandHelp("color"),
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
			Self::Ls {
				directory,
				long,
				recursive: false,
			} => {
				list_directory(ui, client, working_path, directory, long).await?;
			}
			Self::Ls {
				directory,
				long,
				recursive: true,
			} => {
				let path = ItemOperand::parse(working_path, directory.as_deref().unwrap_or(""))?;
				tree::list_directory_recursively(ui, client, &path, long).await?;
			}
			Self::Tree { directory } => {
				let path = ItemOperand::parse(working_path, directory.as_deref().unwrap_or(""))?;
				tree::print_tree(ui, client, &path).await?;
			}
			Self::Du {
				file_or_directory,
				summarize,
			} => {
				let path =
					ItemOperand::parse(working_path, file_or_directory.as_deref().unwrap_or(""))?;
				tree::print_disk_usage(ui, client, &path, summarize).await?;
			}
			Self::Cat { files } => print_files(ui, client, working_path, &files).await?,
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
				let path =
					ItemOperand::parse(working_path, file_or_directory.as_deref().unwrap_or(""))?;
				view_in_web_drive(ui, client, &path, !no_open).await?;
			}
			Self::Drive { no_open } => {
				let root = ItemOperand::Path(RemotePath::new("/"));
				view_in_web_drive(ui, client, &root, !no_open).await?;
			}
			Self::Mkdir {
				directory,
				recursive,
			} => {
				create_directory(ui, client, working_path, &directory, recursive).await?;
			}
			Self::Rm {
				files_or_directories,
				permanent,
			} => {
				delete_files_or_directories(
					ui,
					client,
					working_path,
					&files_or_directories,
					permanent,
				)
				.await?;
			}
			Self::Write { file, content } => {
				write_file(ui, client, working_path, &file, &content).await?;
			}
			Self::Touch { file } => touch_file(ui, client, working_path, &file).await?,
			Self::Mv {
				sources,
				destination,
			} => {
				move_files_or_directories(ui, client, working_path, &sources, &destination).await?;
			}
			Self::Cp {
				sources,
				destination,
			} => {
				copy_files_or_directories(ui, client, working_path, &sources, &destination).await?;
			}
			Self::Versions {
				file,
				restore,
				delete,
			} => {
				let path = ItemOperand::parse(working_path, &file)?;
				let action = versions::VersionsAction::of(restore, delete);
				versions::file_versions(ui, client, &path, action).await?;
			}
			Self::ListFavorites => list_favorites(ui, client).await?,
			Self::Favorite {
				files_or_directories,
			} => {
				set_favorites(ui, client, working_path, &files_or_directories, true).await?;
			}
			Self::Unfavorite {
				files_or_directories,
			} => {
				set_favorites(ui, client, working_path, &files_or_directories, false).await?;
			}
			Self::Color { directory, color } => {
				let path = ItemOperand::parse(working_path, &directory)?;
				set_directory_color(ui, client, &path, color).await?;
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
	let operand = ItemOperand::parse(working_path, directory)?;
	let client = client.get(ui).await?;
	match resolve_item(client, &operand, "directory").await? {
		dir @ (NonRootFileType::Dir(_) | NonRootFileType::Root(_)) => {
			path_of(client, &operand, &dir).await
		}
		NonRootFileType::File(_) => Err(UI::failure(&format!("Not a directory: {}", operand))),
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
	let operand = ItemOperand::parse(working_path, directory.as_deref().unwrap_or(""))?;
	let client = client.get(ui).await?;
	let directory = resolve_item(client, &operand, "file or directory").await?;
	let directory: DirType<'_, Normal> = match directory {
		NonRootFileType::Dir(dir) => DirType::Dir(dir),
		NonRootFileType::Root(root) => DirType::Root(root),
		NonRootFileType::File(file) => {
			// like `ls` on a local file: that one file, the way a listing shows it
			let item = ListedItem::of_file(&file);
			return print_listing(ui, &[item], ListedBy::Name, "", long, ListingLayout::Grid);
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
		ListingLayout::Grid,
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
	/// The directory's color; files have none.
	color: Option<DirColor<'static>>,
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
			color: Some(dir.color().into_owned_cow()),
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
			color: None,
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
		if let Some(color) = &self.color {
			object.insert("color".to_owned(), json!(color.as_ref()));
		}
		object.insert("uuid".to_owned(), json!(self.uuid));
		serde_json::Value::Object(object)
	}

	/// `label` as listings print it for this item: in blue for a directory.
	fn styled(&self, label: &str) -> String {
		match self.kind {
			ListedKind::Directory => style(label).blue().to_string(),
			ListedKind::File => label.to_owned(),
		}
	}

	/// The date `ls -l` shows: when a file was last modified, when a directory was created.
	fn date(&self) -> Option<DateTime<Utc>> {
		match self.kind {
			ListedKind::Directory => self.created,
			ListedKind::File => self.modified,
		}
	}
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
/// column (omitted while replay testing, since it differs on every run), the label, the color of
/// a directory that has one in brackets, and `(*)` after a favorited item.
fn print_long_listing(ui: &mut UI, items: &[ListedItem], by: ListedBy) {
	let rows = items
		.iter()
		.map(|item| {
			(
				item.size.map_or_else(|| "-".to_string(), ui::format_size),
				item.date().map_or_else(
					|| "-".to_string(),
					|date| {
						ui.omit_for_replay_testing(&ui::format_date(&date))
							.to_owned()
					},
				),
			)
		})
		.collect::<Vec<(String, String)>>();
	let size_width = rows.iter().map(|(size, _)| size.len()).max().unwrap_or(0);
	let date_width = rows.iter().map(|(_, date)| date.len()).max().unwrap_or(0);
	for (item, (size, date)) in items.iter().zip(rows) {
		// pad before styling, so the ANSI codes don't count towards the column width
		let size = format!("{:>size_width$}", size);
		let date = format!("{:<date_width$}", date);
		let label = item.styled(&item.label(by));
		let size = match item.kind {
			ListedKind::Directory => style(size).dim().to_string(),
			ListedKind::File => size,
		};
		let color = match &item.color {
			None | Some(DirColor::Default) => String::new(),
			Some(color) => format!(" [{}]", color.as_ref()),
		};
		let favorite = if item.favorited { " (*)" } else { "" };
		ui.print(&format!(
			"{}  {}  {}{}{}",
			size,
			style(date).dim(),
			label,
			color,
			favorite
		));
	}
}

/// How a listing that is not long lays out its labels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ListingLayout {
	/// In columns across the terminal, like `ls`.
	Grid,
	/// One per line, like `ls -R`.
	Lines,
}

/// Prints a listing, in the order of `items`.
fn print_listing(
	ui: &mut UI,
	items: &[ListedItem],
	by: ListedBy,
	empty_message: &str,
	long: bool,
	layout: ListingLayout,
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
	match layout {
		ListingLayout::Grid => {
			let labels = items
				.iter()
				.map(|item| item.styled(&item.label(by)))
				.collect::<Vec<String>>();
			ui.print_grid(&labels.iter().map(String::as_str).collect::<Vec<&str>>());
		}
		ListingLayout::Lines => {
			for item in items {
				ui.print(&item.styled(&item.label(by)));
			}
		}
	}
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
	Head(usize),
	Tail(usize),
}

/// The largest file, in bytes, that `cat` prints to a terminal without asking first. `head` and
/// `tail` never ask, as they print only some of a file's lines.
const PRINT_WITHOUT_ASKING_MAX_SIZE: u64 = 8 * 1024;

/// Whether `cat` prints files of `size` bytes in all: more than
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
			Self::Head(_) => "head",
			Self::Tail(_) => "tail",
		}
	}
}

/// Prints the contents of the files `operands` name one after the other, as `cat` does: each as
/// the bytes it holds, streamed to stdout as it downloads, with nothing added. With `--json`, it
/// is collected and printed as `{"text": ...}`, which only UTF-8 text can be. Several files larger
/// together than a terminal prints without asking are asked about once.
pub(crate) async fn print_files(
	ui: &mut UI,
	client: &mut LazyClient,
	working_path: &RemotePath,
	operands: &[String],
) -> Result<()> {
	let operands = parse_operands(working_path, operands)?;
	let client = client.get(ui).await?;
	let Given {
		items,
		mut failures,
	} = expand_all(ui, client, operands, "file").await?;
	let mut files = Vec::with_capacity(items.len());
	for GivenItem { operand, item } in items {
		match item {
			NonRootFileType::File(file) => files.push((operand, file)),
			NonRootFileType::Dir(_) | NonRootFileType::Root(_) => {
				failures.record(ui, Err(UI::failure(&format!("Not a file: {}", operand))))?
			}
		}
	}
	if files.is_empty() {
		return failures.finish("printed");
	}
	let size = files
		.iter()
		.fold(0u64, |size, (_, file)| size.saturating_add(file.size()));
	if !confirm_printing_file(ui, size)? {
		return failures.finish("printed");
	}
	let mut out = PrintedContent::of(ui);
	for (_, file) in &files {
		print_from_reader(ui, &mut out, client.get_file_reader(&**file), None).await?;
	}
	let printed = match files.as_slice() {
		[(operand, _)] => operand.to_string(),
		_ => "the files given".to_owned(),
	};
	out.finish(ui, &printed, "cat")?;
	failures.finish("printed")
}

/// Prints a file's first or last lines as the bytes they hold, with no newline added: the first
/// streamed to stdout as they download, the last once it is found where they start. With
/// `--json`, the printed part is collected and printed as `{"text": ...}`, which only UTF-8 text
/// can be.
pub(crate) async fn print_file(
	ui: &mut UI,
	client: &mut LazyClient,
	working_path: &RemotePath,
	file_str: &str,
	lines: PrintFileLines,
) -> Result<()> {
	let operand = ItemOperand::parse(working_path, file_str)?;
	let client = client.get(ui).await?;
	let file = find_file(client, &operand).await?;
	let mut out = PrintedContent::of(ui);
	match lines {
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
	out.finish(ui, &operand.to_string(), lines.command())
}

/// Where [`print_files`] and [`print_file`] put the content they print.
enum PrintedContent {
	/// Straight to stdout, where a reader that goes cuts it short.
	Stdout(interrupt::DataStream),
	/// Collected, to be printed as JSON.
	Json(Vec<u8>),
}

impl PrintedContent {
	/// Straight to stdout, or with `--json` collected.
	fn of(ui: &UI) -> Self {
		if ui.json {
			Self::Json(Vec::new())
		} else {
			Self::Stdout(interrupt::stream_data())
		}
	}

	/// Prints what was collected as `{"text": ...}`: `printed` (what `command` printed), which
	/// only UTF-8 text can be.
	fn finish(self, ui: &mut UI, printed: &str, command: &str) -> Result<()> {
		let Self::Json(content) = self else {
			return Ok(());
		};
		let text = String::from_utf8(content).map_err(|_| {
			UI::failure(&format!(
				"Not UTF-8 text: {printed}. Use {command} without --json for binary content"
			))
		})?;
		ui.print_json(json!({ "text": text }))
	}

	fn write(&mut self, ui: &mut UI, bytes: &[u8]) -> Result<()> {
		match self {
			Self::Stdout(_) => ui.write_stdout_bytes(bytes),
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
	let operand = ItemOperand::parse(working_path, file_or_directory_str)?;
	let client = client.get(ui).await?;
	match resolve_item(client, &operand, "file or directory").await? {
		NonRootFileType::File(file) => {
			let name = name_or_uuid(file.name(), file.uuid());
			if ui.json {
				ui.print_json(json!({
					"name": name,
					"type": "file",
					"size": file.size(),
					"modified": file.last_modified(),
					"created": file.created(),
					"uuid": file.uuid(),
				}))?;
			} else {
				ui.print_key_value_table(&[
					("Name", &name),
					("Type", "File"),
					("Size", &ui::format_size(file.size())),
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
					("UUID", &file.uuid().to_string()),
				]);
			}
		}
		NonRootFileType::Dir(dir) => {
			let size_info = Normal::dir_size(&**client, &DirType::from(&*dir), ())
				.await
				.context("Failed to get directory size")?;
			let name = name_or_uuid(dir.name(), dir.uuid());
			if ui.json {
				ui.print_json(json!({
					"name": name,
					"type": "directory",
					"size": size_info.size,
					"files": size_info.files,
					"directories": size_info.dirs,
					"created": dir.created(),
					"color": dir.color().as_ref(),
					"uuid": dir.uuid(),
				}))?;
			} else {
				ui.print_key_value_table(&[
					("Name", &name),
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
					("Color", dir.color().as_ref()),
					("UUID", &dir.uuid().to_string()),
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

/// Finds the file `operand` names.
async fn find_file<'a>(client: &'a Client, operand: &ItemOperand) -> Result<Cow<'a, RemoteFile>> {
	match resolve_item(client, operand, "file").await? {
		NonRootFileType::File(file) => Ok(file),
		_ => Err(UI::failure(&format!("Not a file: {}", operand))),
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
	let path = ItemOperand::parse(working_path, file_str)?;
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

/// The editor that `$VISUAL`, or else `$EDITOR`, names, split into words by
/// [`split_command_line`]: as a shell would split them on Unix, at double quotes only on Windows.
/// Empty variables count as unset.
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
	let mut words = split_command_line(command)
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
	let operand = ItemOperand::parse(working_path, file_str)?;
	let editor = editor_command(env_var("VISUAL")?.as_deref(), env_var("EDITOR")?.as_deref())?;
	// before logging in and downloading, so that a script fails at once
	if editor.is_none() {
		ui.ensure_can_prompt(DONE_EDITING_PROMPT)?;
	}
	let client = client.get(ui).await?;
	let file = find_file(client, &operand).await?;
	let target = EditedFile::of(client, &operand, &file).await?;
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
			uploaded = upload_edit(client, &target, downloaded, created, &local) => uploaded?,
			() = ctrl_c.pressed() => return Err(CliError::Interrupted.into()),
		}
		Ok(true)
	}
	.await;
	match edited {
		Ok(true) => ui.print_success(&format!("Uploaded your changes to {}", target)),
		Ok(false) => ui.print_success(&format!("No changes to {}", target)),
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

/// Where the file being edited is looked up again before the changes are uploaded, as a new
/// version of whatever file is there.
enum EditedFile {
	/// At its path.
	AtPath(RemotePath),
	/// Under its name in its directory, for a file named by its UUID that has no path.
	InDirectory {
		parent: Uuid,
		name: String,
		operand: ItemOperand,
	},
}

impl EditedFile {
	/// Where `file`, which `operand` names, is looked up again: at its path, or, if it has none,
	/// under its own name in its directory, which needs a name that can be decrypted.
	async fn of(client: &Client, operand: &ItemOperand, file: &RemoteFile) -> Result<Self> {
		let item = NonRootFileType::File(Cow::Borrowed(file));
		let operand = match shown_as(client, operand, &item).await? {
			ItemOperand::Path(path) => return Ok(Self::AtPath(path)),
			operand @ ItemOperand::Uuid(_) => operand,
		};
		let Some(name) = file.name().map(str::to_owned) else {
			return Err(UI::failure(&format!(
				"Cannot edit {}, as its name cannot be decrypted",
				operand
			)));
		};
		// a file that is neither trashed nor an earlier version is in a directory
		let ParentUuid::Uuid(parent) = *file.parent() else {
			return Err(UI::failure(&format!("No directory holds {}", operand)));
		};
		Ok(Self::InDirectory {
			parent,
			name,
			operand,
		})
	}

	/// The name the file is uploaded under.
	fn name(&self) -> Result<&str> {
		match self {
			Self::AtPath(path) => path
				.basename()
				.ok_or_else(|| UI::failure(&format!("Not a file: {}", path))),
			Self::InDirectory { name, .. } => Ok(name),
		}
	}

	/// The file there now and its parent directory's UUID, if there is one.
	async fn current(&self, client: &Client) -> Result<Option<(Uuid, RemoteVersion)>> {
		match self {
			Self::AtPath(path) => match client.get_items_in_path(&path.0).await {
				Ok((dirs, ObjectOrRemainingPath::Object(NonRootFileType::File(file)))) => Ok(dirs
					.last()
					.map(|parent| (parent.uuid(), RemoteVersion::of(&file)))),
				Ok(_) => Ok(None),
				// a file is where a directory on the path was
				Err((e, _, _)) if e.kind() == ErrorKind::InvalidType => Ok(None),
				Err((e, _, _)) => Err(e).context("Failed to find file"),
			},
			Self::InDirectory { parent, name, .. } => {
				let Some(dir) = optional(client.get_dir(*parent).await)
					.context("Failed to find the file's directory")?
				else {
					return Ok(None);
				};
				// a directory moved to the trash no longer holds the file, and the server reports
				// every directory in a trashed one as trashed too
				if !matches!(dir.parent(), ParentUuid::Uuid(_)) {
					return Ok(None);
				}
				let (_, files) = client
					.list_dir::<_, Normal>(
						&DirType::Dir(Cow::Owned(dir)),
						None::<&fn(u64, Option<u64>)>,
					)
					.await
					.context("Failed to list the file's directory")?;
				Ok(files
					.iter()
					.find(|file| file.name() == Some(name.as_str()))
					.map(|file| (*parent, RemoteVersion::of(file))))
			}
		}
	}
}

impl fmt::Display for EditedFile {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::AtPath(path) => write!(f, "{}", path),
			Self::InDirectory { operand, .. } => write!(f, "{}", operand),
		}
	}
}

/// Uploads the edited file at `local` as a new version of the file `target` finds, if that is
/// still the file downloaded as `downloaded`. The check and the upload hold the drive lock, so
/// that nothing changes the file in between.
async fn upload_edit(
	client: &Client,
	target: &EditedFile,
	downloaded: RemoteVersion,
	created: Option<DateTime<Utc>>,
	local: &Path,
) -> Result<()> {
	let name = target.name()?;
	let _lock = client.lock_drive().await.context("Failed to lock drive")?;
	let current = target.current(client).await?;
	let parent = match edit_upload(downloaded, current) {
		EditUpload::Upload { parent } => parent,
		EditUpload::Gone => {
			return Err(UI::failure(&format!(
				"{} was moved or deleted while you edited it",
				target
			)));
		}
		EditUpload::Changed => {
			return Err(UI::failure(&format!(
				"{} changed in the drive while you edited it",
				target
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

/// The UUIDs of the directories from the root directory down to the one at `path`, or to the
/// directory of the file there, as the lookup of the path finds them.
async fn directory_chain_at(client: &Client, path: &RemotePath) -> Result<Vec<Uuid>> {
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
	Ok(chain)
}

/// Prints the web drive's URL of a directory, or of a file's directory, and opens it in the
/// browser if `open_browser`. A browser that fails to open is only warned about: the URL is
/// printed.
pub(crate) async fn view_in_web_drive(
	ui: &mut UI,
	client: &mut LazyClient,
	operand: &ItemOperand,
	open_browser: bool,
) -> Result<()> {
	let client = client.get(ui).await?;
	let chain = match operand {
		ItemOperand::Path(path) => directory_chain_at(client, path).await?,
		// Walked up by UUID, as an item named by it may have no path: one request per directory
		// above it, as many as finding its path would take.
		ItemOperand::Uuid(_) => {
			let item = resolve_item(client, operand, "file or directory").await?;
			directory_chain(client, operand, &item).await?
		}
	};
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
	let operand = ItemOperand::parse(working_path, directory_str)?;
	let client = client.get(ui).await?;
	let directory_str = match operand {
		ItemOperand::Path(path) => path,
		// an item named by its UUID exists, so all there is to do is say what it is
		ItemOperand::Uuid(_) => {
			let item = resolve_item(client, &operand, "directory").await?;
			let shown = shown_as(client, &operand, &item).await?;
			return match item {
				NonRootFileType::Dir(_) | NonRootFileType::Root(_) => {
					print_directory_exists(ui, &shown);
					Ok(())
				}
				NonRootFileType::File(_) => {
					Err(UI::failure(&format!("Not a directory: {}", shown)))
				}
			};
		}
	};
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

fn print_directory_exists(ui: &mut UI, directory: &impl fmt::Display) {
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
				// check the name before creating any parent, so that a name the SDK rejects
				// leaves nothing behind; each level checks its own, so every missing component
				// is checked before the first is created, and existing ones never are
				if let Some(name) = directory.basename() {
					ValidatedName::try_from(name)
						.map_err(Error::from)
						.context("Failed to create directory")?;
				}
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

/// Trashes, or permanently deletes, the items `operands` name. Permanently deleting asks once,
/// about all of them.
pub(crate) async fn delete_files_or_directories(
	ui: &mut UI,
	client: &mut LazyClient,
	working_path: &RemotePath,
	operands: &[String],
	permanent: bool,
) -> Result<()> {
	let operands = parse_operands(working_path, operands)?;
	// before logging in and looking the items up, so that a script missing --yes fails at once
	if permanent {
		ui.ensure_can_confirm(&deletion_confirmation(&operands))?;
	}
	let client = client.get(ui).await?;
	let Given {
		items,
		mut failures,
	} = expand_all(ui, client, operands, "file or directory").await?;
	let done = if permanent {
		"permanently deleted"
	} else {
		"trashed"
	};
	if permanent && !items.is_empty() {
		let named = items
			.iter()
			.map(|given| &given.operand)
			.collect::<Vec<&ItemOperand>>();
		if !ui.prompt_confirm(&deletion_confirmation(&named), false)? {
			return failures.finish(done);
		}
	}
	for given in items {
		let deleted = delete_item(ui, client, given, permanent).await;
		failures.record(ui, deleted)?;
	}
	failures.finish(done)
}

/// Trashes, or permanently deletes, one item.
async fn delete_item(
	ui: &mut UI,
	client: &Client,
	GivenItem { operand, item }: GivenItem<'_>,
	permanent: bool,
) -> Result<()> {
	match item {
		NonRootFileType::File(mut file) => {
			if permanent {
				client
					.delete_file_permanently(file.into_owned())
					.await
					.context("Failed to permanently delete file")?;
				ui.print_success(&format!("Permanently deleted file: {}", operand));
			} else {
				client
					.trash_file(file.to_mut())
					.await
					.context("Failed to trash file")?;
				ui.print_success(&format!("Trashed file: {}", operand));
			}
		}
		NonRootFileType::Dir(mut dir) => {
			if permanent {
				client
					.delete_dir_permanently(dir.into_owned())
					.await
					.context("Failed to permanently delete directory")?;
				ui.print_success(&format!("Permanently deleted directory: {}", operand));
			} else {
				client
					.trash_dir(dir.to_mut())
					.await
					.context("Failed to trash directory")?;
				ui.print_success(&format!("Trashed directory: {}", operand));
			}
		}
		NonRootFileType::Root(_) => {
			return Err(UI::failure("Cannot delete root directory"));
		}
	}
	Ok(())
}

/// Where a file written to an operand goes.
struct FileToWrite {
	/// The UUID of the directory it goes into.
	parent: Uuid,
	/// The name of the file there already, as it differs from the path's only in case if at all,
	/// or else the path's name.
	name: String,
	/// How messages show the file: by its path, with [`FileToWrite::name`], or by its UUID if it
	/// has no path.
	shown: ItemOperand,
	/// The file there already, which writing replaces.
	existing: Option<RemoteFile>,
}

/// Finds where a file written to `operand` goes: see [`find_file_to_write_at`] for a path. A UUID
/// names an existing file, which is replaced under its own name in its own directory, so it is
/// found by its UUID even where no path leads to it.
async fn find_file_to_write(client: &Client, operand: &ItemOperand) -> Result<FileToWrite> {
	if let ItemOperand::Path(path) = operand {
		return find_file_to_write_at(client, path).await;
	}
	let file = match resolve_item(client, operand, "file").await? {
		NonRootFileType::File(file) => file.into_owned(),
		NonRootFileType::Dir(_) | NonRootFileType::Root(_) => {
			return Err(UI::failure(&format!("Not a file: {}", operand)));
		}
	};
	let Some(name) = file.name().map(str::to_owned) else {
		return Err(UI::failure(&format!(
			"Cannot write to {}, as its name cannot be decrypted",
			operand
		)));
	};
	// a file that is neither trashed nor an earlier version is in a directory
	let ParentUuid::Uuid(parent) = *file.parent() else {
		return Err(UI::failure(&format!("No directory holds {}", operand)));
	};
	let shown = shown_as(
		client,
		operand,
		&NonRootFileType::File(Cow::Borrowed(&file)),
	)
	.await?;
	Ok(FileToWrite {
		parent,
		name,
		shown,
		existing: Some(file),
	})
}

/// Finds where a file written to `path` goes: its parent directory must exist, and nothing there
/// may be a directory whose name is the file's but for case. Names are compared ignoring case, as
/// the server does: uploading a file whose name a file there has but for case replaces that file.
async fn find_file_to_write_at(client: &Client, path: &RemotePath) -> Result<FileToWrite> {
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
		parent: parent.uuid(),
		shown: ItemOperand::Path(parent_path.navigate(&name)),
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
	let operand = ItemOperand::parse(working_path, file_str)?;
	let client = client.get(ui).await?;
	let target = find_file_to_write(client, &operand).await?;
	let builder = client
		.make_file_builder(&target.name, target.parent)
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
	ui.print_success(&format!("Wrote to {}", target.shown));
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
	let operand = ItemOperand::parse(working_path, file_str)?;
	let client = client.get(ui).await?;
	let target = find_file_to_write(client, &operand).await?;
	match target.existing {
		Some(mut file) => {
			client
				.update_file_metadata(
					&mut file,
					FileMetaChanges::default().last_modified(Utc::now()),
				)
				.await
				.context("Failed to update the modification date")?;
			ui.print_success(&format!(
				"Updated the modification date of {}",
				target.shown
			));
		}
		None => {
			let builder = client
				.make_file_builder(&target.name, target.parent)
				.map_err(|e| UI::failure(&format!("Invalid file name: {}", e)))?;
			client
				.upload_file(builder, &[])
				.await
				.context("Failed to create file")?;
			ui.print_success(&format!("Created empty file: {}", target.shown));
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

/// Moves and/or renames files or directories, following the semantics of the Unix `mv`:
/// if the destination is an existing directory, each source is moved into it under its
/// current name; otherwise the destination names the source's new path, so the source is
/// moved to that path's parent directory and renamed to that path's base name. A destination
/// ending in '/', or given for several sources, must be an existing directory.
pub(crate) async fn move_files_or_directories(
	ui: &mut UI,
	client: &mut LazyClient,
	working_path: &RemotePath,
	sources: &[String],
	destination_str: &str,
) -> Result<()> {
	let sources = parse_operands(working_path, sources)?;
	let client = client.get(ui).await?;
	let Given {
		items,
		mut failures,
	} = expand_all(ui, client, sources, "source file or directory").await?;
	if items.is_empty() {
		return failures.finish("moved");
	}
	let mut destination =
		destination_path(client, working_path, destination_str, failures.given()).await?;
	for source in items {
		let moved = move_item(ui, client, source, &mut destination).await;
		failures.record(ui, moved)?;
	}
	failures.finish("moved")
}

async fn move_item(
	ui: &mut UI,
	client: &Client,
	GivenItem {
		operand: source_operand,
		item: source,
	}: GivenItem<'_>,
	destination: &mut GivenDestination<'_>,
) -> Result<()> {
	let (source_filename, source_parent) = match &source {
		NonRootFileType::File(file) => (file.name(), *file.parent()),
		NonRootFileType::Dir(dir) => (dir.name(), *dir.parent()),
		NonRootFileType::Root(_) => return Err(UI::failure("Cannot move root directory")),
	};
	let source_filename = source_filename.context("Failed to decrypt source name")?;
	let source_uuid = source.uuid();
	let source_shown = shown_as(client, &source_operand, &source).await?;
	let Destination {
		dir: destination_dir,
		name: new_name,
		dir_shown,
	} = resolve_destination(
		client,
		Placement::Move,
		&Source {
			shown: &source_shown,
			name: source_filename,
			uuid: source_uuid,
			parent: source_parent,
		},
		destination,
	)
	.await?;

	let needs_rename = new_name != source_filename;
	match source {
		NonRootFileType::File(file) => {
			// built before the move so an invalid name fails without moving anything
			let rename = needs_rename
				.then(|| FileMetaChanges::default().name(&new_name))
				.transpose()
				.context("Invalid destination file name")?;
			let mut file = file.into_owned();
			if *file.parent() != destination_dir.uuid() {
				client
					.move_file(&mut file, &destination_dir)
					.await
					.context("Failed to move file")?;
			}
			if let Some(changes) = rename {
				client
					.update_file_metadata(&mut file, changes)
					.await
					.context("Failed to rename file")?;
			}
		}
		NonRootFileType::Dir(dir) => {
			// built before the move so an invalid name fails without moving anything
			let rename = needs_rename
				.then(|| DirectoryMetaChanges::default().name(&new_name))
				.transpose()
				.context("Invalid destination directory name")?;
			let mut dir = dir.into_owned();
			if *dir.parent() != destination_dir.uuid() {
				client
					.move_dir(&mut dir, &destination_dir)
					.await
					.context("Failed to move directory")?;
			}
			if let Some(changes) = rename {
				client
					.update_dir_metadata(&mut dir, changes)
					.await
					.context("Failed to rename directory")?;
			}
		}
		NonRootFileType::Root(_) => return Err(UI::failure("Cannot move root directory")),
	}
	ui.print_success(&format!(
		"Moved {} to {}",
		source_shown,
		dir_shown.child(&new_name)
	));
	destination.record_placed(source_uuid, &new_name);
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
	/// How messages show it: by its path, or, for an item named by its UUID that has none (as a
	/// name above it cannot be decrypted), by its UUID.
	shown: &'a ItemOperand,
	name: &'a str,
	uuid: Uuid,
	parent: ParentUuid,
}

impl Source<'_> {
	fn path(&self) -> Option<&RemotePath> {
		self.shown.path()
	}
}

/// Where a `mv` or `cp` puts its source.
struct Destination<'a> {
	/// The directory the source ends up in.
	dir: DirType<'a, Normal>,
	/// The name the source ends up under.
	name: String,
	/// How messages show that directory.
	dir_shown: ShownDirectory,
}

/// How messages show the directory a `mv` or `cp` puts its source in.
enum ShownDirectory {
	/// By its path.
	Path(RemotePath),
	/// By its UUID, for a directory that has no path.
	Uuid(ItemOperand),
}

impl ShownDirectory {
	/// How messages show the item named `name` in the directory.
	fn child(&self, name: &str) -> String {
		match self {
			Self::Path(path) => path.navigate(name).to_string(),
			Self::Uuid(dir) => format!("{} in {}", name, dir),
		}
	}
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
	// A source without a path lies below a directory whose name cannot be decrypted, so no path
	// leads to it or into it: it is neither at nor above the destination, which has a path.
	let Some(source_path) = source.path() else {
		return Ok(target);
	};
	if placement == Placement::Move && target.path() == source_path {
		return Err(UI::failure(&format!(
			"{} is already in {}",
			source_path,
			target.dir()
		)));
	}
	if is_inside(target.path(), source_path) {
		return Err(UI::failure(&format!(
			"Cannot {} {} into itself: {}",
			placement.verb(),
			source_path,
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

/// The destination a `mv` or `cp` was given.
enum GivenDestination<'a> {
	/// A path, or a directory named by its UUID that has a path, given for one source.
	Path(DestinationPath),
	/// An existing directory at a path, given for several sources.
	Directory(DestinationDirectory<'a>),
	/// A directory named by its UUID that has no path, as a name above it cannot be decrypted.
	Pathless {
		dir: RemoteDirectory,
		/// How messages show it.
		operand: ItemOperand,
		/// The UUIDs of the directories from the root directory down to it, itself included.
		chain: Vec<Uuid>,
	},
}

/// The destination path a `mv` or `cp` was given.
struct DestinationPath {
	path: RemotePath,
	/// The destination must be an existing directory to put the sources into, as one written with
	/// a trailing '/' or named by its UUID must.
	must_be_a_directory: bool,
}

/// The existing directory a `mv` or `cp` of several sources puts all of them into, looked up once
/// for the whole command.
struct DestinationDirectory<'a> {
	path: RemotePath,
	dir: DirType<'a, Normal>,
	/// The names in it, listed when the first source gets as far as the clash check, then kept up
	/// to date with the items the command puts there. A snapshot: what anything else changes in
	/// the directory meanwhile is not seen.
	names: Option<ExistingNames>,
}

impl GivenDestination<'_> {
	/// Notes that the item with the UUID `uuid` is now in the destination under `name`.
	fn record_placed(&mut self, uuid: Uuid, name: &str) {
		if let Self::Directory(DestinationDirectory {
			names: Some(names), ..
		}) = self
		{
			names.record(uuid, name);
		}
	}
}

/// The destination a `mv` or `cp` of `sources` items was given as `destination_str`. For several
/// sources, it is checked to be an existing directory before any is moved or copied.
async fn destination_path<'a>(
	client: &'a Client,
	working_path: &RemotePath,
	destination_str: &str,
	sources: usize,
) -> Result<GivenDestination<'a>> {
	let destination = match ItemOperand::parse(working_path, destination_str)? {
		// before navigating, which drops the trailing '/'
		ItemOperand::Path(path) => DestinationPath {
			path,
			must_be_a_directory: names_a_directory(destination_str),
		},
		operand => {
			let item = resolve_item(client, &operand, "destination directory").await?;
			match (shown_as(client, &operand, &item).await?, item) {
				(ItemOperand::Path(path), _) => DestinationPath {
					path,
					must_be_a_directory: true,
				},
				(operand, NonRootFileType::Dir(dir)) => {
					let item = NonRootFileType::Dir(Cow::Borrowed(dir.as_ref()));
					let chain = directory_chain(client, &operand, &item).await?;
					return Ok(GivenDestination::Pathless {
						dir: dir.into_owned(),
						operand,
						chain,
					});
				}
				(operand, NonRootFileType::File(_) | NonRootFileType::Root(_)) => {
					// the root directory always has a path
					return Err(UI::failure(&format!("Not a directory: {}", operand)));
				}
			}
		}
	};
	if sources > 1 {
		let dir = match client
			.find_item_at_path(&destination.path.0)
			.await
			.context("Failed to find destination")?
		{
			Some(NonRootFileType::Dir(dir)) => DirType::Dir(dir),
			Some(NonRootFileType::Root(root)) => DirType::Root(root),
			Some(NonRootFileType::File(_)) => {
				return Err(UI::failure(&format!(
					"Not a directory: {}",
					destination.path
				)));
			}
			None => {
				return Err(UI::failure(&format!(
					"No such destination directory: {}",
					destination.path
				)));
			}
		};
		return Ok(GivenDestination::Directory(DestinationDirectory {
			path: destination.path,
			dir,
			names: None,
		}));
	}
	Ok(GivenDestination::Path(destination))
}

/// Resolves the `destination_path` of a `mv` or `cp` into the directory the source ends up
/// in and the name it ends up under (see [`target_of`]), and refuses a name the server would take
/// as one already there: it compares names case-insensitively. Moving an item to a name that
/// differs from its own only in case renames it.
///
/// The comparison lists the whole destination directory: once per source, except for a directory
/// given for several sources, which is listed once per command and then kept up to date with what
/// the command puts there (see [`DestinationDirectory`]). The server's name-hash lookups
/// (`dir_exists`, `file_exists`) would take two requests instead, but match only the hash the
/// uploading client sent, so they miss an item whose name hash a non-conforming client computed
/// differently, which the listing still finds by its decrypted name.
async fn resolve_destination<'a>(
	client: &'a Client,
	placement: Placement,
	source: &Source<'_>,
	destination: &'a mut GivenDestination<'_>,
) -> Result<Destination<'a>> {
	// a moved item does not clash with itself
	let ignored = (placement == Placement::Move).then_some(source.uuid);
	match destination {
		GivenDestination::Path(destination) => {
			let (dir, dir_shown, name) =
				resolve_destination_path(client, placement, source, destination).await?;
			let names = list_existing_names(client, &dir).await?;
			check_name_free(&names, &name, ignored, &dir_shown)?;
			Ok(Destination {
				dir,
				name,
				dir_shown,
			})
		}
		GivenDestination::Directory(DestinationDirectory { path, dir, names }) => {
			target_of(placement, source, path, Found::Directory, true)?;
			let dir: &'a DirType<'_, Normal> = dir;
			let dir_shown = ShownDirectory::Path(path.clone());
			let names = match names {
				Some(names) => names,
				None => names.insert(list_existing_names(client, dir).await?),
			};
			check_name_free(names, source.name, ignored, &dir_shown)?;
			Ok(Destination {
				dir: dir.as_borrowed_cow(),
				name: source.name.to_owned(),
				dir_shown,
			})
		}
		GivenDestination::Pathless {
			dir,
			operand,
			chain,
		} => {
			check_pathless_destination(placement, source, dir.uuid(), operand, chain)?;
			let dir: &'a RemoteDirectory = dir;
			let dir = DirType::Dir(Cow::Borrowed(dir));
			let dir_shown = ShownDirectory::Uuid(operand.clone());
			let names = list_existing_names(client, &dir).await?;
			check_name_free(&names, source.name, ignored, &dir_shown)?;
			Ok(Destination {
				dir,
				name: source.name.to_owned(),
				dir_shown,
			})
		}
	}
}

/// Lists the names in the destination directory `dir` of a `mv` or `cp`.
async fn list_existing_names(client: &Client, dir: &DirType<'_, Normal>) -> Result<ExistingNames> {
	let (dirs, files) = client
		.list_dir::<_, Normal>(dir, None::<&fn(u64, Option<u64>)>)
		.await
		.context("Failed to list destination directory")?;
	Ok(ExistingNames::new(
		dirs.iter()
			.map(|dir| (dir.uuid(), dir.name()))
			.chain(files.iter().map(|file| (file.uuid(), file.name())))
			.filter_map(|(uuid, name)| Some((uuid, name?))),
	))
}

/// Refuses `name` for a source put into the directory shown as `dir_shown` when the server would
/// take it as one of `names` already there, other than the item with the UUID `ignored`. A
/// same-name upload would replace the existing file (as a new version) and creating a same-name
/// directory returns the existing one, so a copy would merge into it.
fn check_name_free(
	names: &ExistingNames,
	name: &str,
	ignored: Option<Uuid>,
	dir_shown: &ShownDirectory,
) -> Result<()> {
	match names.clashing(name, ignored) {
		Some(existing_name) => Err(UI::failure(&format!(
			"Destination already exists: {}",
			dir_shown.child(existing_name)
		))),
		None => Ok(()),
	}
}

/// Refuses to put `source` into the directory with the UUID `dir`, shown as `shown`, which has no
/// path, where the source would end up inside itself, or, for a move, where it already is. `chain`
/// holds the UUIDs of the directories from the root directory down to `dir`.
fn check_pathless_destination(
	placement: Placement,
	source: &Source<'_>,
	dir: Uuid,
	shown: &ItemOperand,
	chain: &[Uuid],
) -> Result<()> {
	if chain.contains(&source.uuid) {
		return Err(UI::failure(&format!(
			"Cannot {} {} into itself: {}",
			placement.verb(),
			source.shown,
			shown
		)));
	}
	if placement == Placement::Move && source.parent == ParentUuid::Uuid(dir) {
		return Err(UI::failure(&format!(
			"{} is already in {}",
			source.shown, shown
		)));
	}
	Ok(())
}

/// Finds the directory a destination path puts the source in, how messages show it, and the
/// name the source ends up under there.
async fn resolve_destination_path<'a>(
	client: &'a Client,
	placement: Placement,
	source: &Source<'_>,
	DestinationPath {
		path: destination_path,
		must_be_a_directory,
	}: &DestinationPath,
) -> Result<(DirType<'a, Normal>, ShownDirectory, String)> {
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
		destination_path,
		found,
		*must_be_a_directory,
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
	let (dir_path, name) = match target {
		Target::Into { dir, .. } => (dir, source.name.to_owned()),
		Target::At { parent, name, .. } => (parent, name),
	};
	Ok((dir, ShownDirectory::Path(dir_path), name))
}

/// Copies files or directories, following the semantics of the Unix `cp`: if the destination is
/// an existing directory, each copy is put into it under its source's name; otherwise the
/// destination names the copy's path, so the copy is put into that path's parent directory under
/// that path's base name. A destination ending in '/', or given for several sources, must be an
/// existing directory. Each source is copied by a copy job of its own, one after the other.
pub(crate) async fn copy_files_or_directories(
	ui: &mut UI,
	client: &mut LazyClient,
	working_path: &RemotePath,
	sources: &[String],
	destination_str: &str,
) -> Result<()> {
	let sources = parse_operands(working_path, sources)?;
	let client = client.get(ui).await?;
	let Given {
		items,
		mut failures,
	} = expand_all(ui, client, sources, "source file or directory").await?;
	if items.is_empty() {
		return failures.finish("copied");
	}
	let mut destination =
		destination_path(client, working_path, destination_str, failures.given()).await?;
	for source in items {
		let copied = copy_item(ui, client, source, &mut destination).await;
		failures.record(ui, copied)?;
	}
	failures.finish("copied")
}

async fn copy_item(
	ui: &mut UI,
	client: &Arc<Client>,
	GivenItem {
		operand: source_operand,
		item: source,
	}: GivenItem<'_>,
	given: &mut GivenDestination<'_>,
) -> Result<()> {
	let (source_name, source_parent) = match &source {
		NonRootFileType::File(file) => (file.name(), *file.parent()),
		NonRootFileType::Dir(dir) => (dir.name(), *dir.parent()),
		NonRootFileType::Root(_) => return Err(UI::failure("Cannot copy root directory")),
	};
	let source_name = source_name.context("Failed to decrypt source name")?;
	let source_shown = shown_as(client, &source_operand, &source).await?;
	let Destination {
		dir: destination_dir,
		name: new_name,
		dir_shown,
	} = resolve_destination(
		client,
		Placement::Copy,
		&Source {
			shown: &source_shown,
			name: source_name,
			uuid: source.uuid(),
			parent: source_parent,
		},
		given,
	)
	.await?;
	// the source's own name is left to the copy, which also copies a name today's rules reject
	let name = if new_name == source_name {
		None
	} else {
		Some(
			ValidatedName::try_from(new_name.as_str())
				.map_err(|e| UI::failure(&format!("Invalid destination name: {}", e)))?,
		)
	};
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
		destination: destination_dir.into_owned_cow(),
		name,
	};
	let result = run_copy(ui, client, request, config).await;
	// a copy that failed partway may still have put its top-level item there
	let report = match &result {
		Ok(report) => report,
		Err(failed) => &failed.report,
	};
	if let Some(copied) = report.top_level.first() {
		given.record_placed(copied.item.uuid(), copied.item.name().unwrap_or(&new_name));
	}
	finish_copy(ui, &source_shown, &dir_shown, result)
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
	let progress = Arc::new(StderrProgress::new(ui));
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
	source: &ItemOperand,
	destination_dir: &ShownDirectory,
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
		.map(|name| destination_dir.child(name));
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
					source,
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
			ui.print_success(&format!("Copied {} to {}", source, copy_path));
			Ok(())
		}
		// a copy that failed at its top-level item reports that as a failure
		None => Err(UI::failure(&format!("Failed to copy {}", source))),
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

/// Draws a long-running command's progress on one line of stderr, redrawn in place, such as a
/// copy's listing of the source and the destination and then the bytes it copied. The work calls
/// it from its own tasks or callbacks, so it writes to the terminal itself instead of through
/// [`UI`], which the command holds.
struct StderrProgress {
	term: Term,
	/// Only for a person at a terminal: not with `--quiet` or `--json`, not when stdout or stderr
	/// is redirected, where the redraws would end up in the captured output, and not while replay
	/// testing.
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

impl StderrProgress {
	fn new(ui: &UI) -> Self {
		let term = Term::stderr();
		let enabled = ui.output_is_read_by_a_person() && ui.may_draw_progress() && term.is_term();
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

	/// Draws `text` over the line drawn before, if the progress is shown at all.
	fn draw(&self, text: &str) {
		if !self.enabled {
			return;
		}
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

impl CopyCallback for StderrProgress {
	fn on_top_level_planned(&self, _items: Vec<PlannedTopLevelItem>) {}

	fn on_top_level_created(&self, _item: CopiedTopLevel) {}

	fn on_update(&self, update: CopyUpdate) {
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
	let name = folded_name(name);
	existing
		.into_iter()
		.find(|existing| folded_name(existing) == name)
}

/// `name` as the server compares names: case-insensitively.
fn folded_name(name: &str) -> String {
	name.to_lowercase()
}

/// The names of the items in a directory, looked up as the server compares them (see
/// [`folded_name`]).
#[derive(Debug, Default, PartialEq, Eq)]
struct ExistingNames {
	/// The items under each folded name, with their names as written.
	by_name: HashMap<String, Vec<(Uuid, String)>>,
	/// The folded name each item is under in `by_name`, so that recording an item again finds its
	/// entry without scanning them all.
	folded_of: HashMap<Uuid, String>,
}

impl ExistingNames {
	fn new<'a>(items: impl IntoIterator<Item = (Uuid, &'a str)>) -> Self {
		let mut names = Self::default();
		for (uuid, name) in items {
			names.insert(uuid, name);
		}
		names
	}

	fn insert(&mut self, uuid: Uuid, name: &str) {
		let folded = folded_name(name);
		self.by_name
			// cloned, as both maps key by it
			.entry(folded.clone())
			.or_default()
			.push((uuid, name.to_owned()));
		self.folded_of.insert(uuid, folded);
	}

	/// Notes that the item with the UUID `uuid` is now in the directory under `name`, replacing the
	/// entry it had there, as a moved item may have one under its old name.
	fn record(&mut self, uuid: Uuid, name: &str) {
		if let Some(previous) = self.folded_of.remove(&uuid)
			&& let Some(items) = self.by_name.get_mut(&previous)
		{
			items.retain(|(item, _)| *item != uuid);
			if items.is_empty() {
				self.by_name.remove(&previous);
			}
		}
		self.insert(uuid, name);
	}

	/// The name of an item other than the one with the UUID `ignored` that the server treats as the
	/// same as `name`.
	fn clashing(&self, name: &str, ignored: Option<Uuid>) -> Option<&str> {
		self.by_name
			.get(&folded_name(name))?
			.iter()
			.find(|(uuid, _)| Some(*uuid) != ignored)
			.map(|(_, name)| name.as_str())
	}
}

/// The named directory colors, as the apps offer them.
const NAMED_DIR_COLORS: [DirColor<'static>; 6] = [
	DirColor::Default,
	DirColor::Blue,
	DirColor::Green,
	DirColor::Purple,
	DirColor::Red,
	DirColor::Gray,
];

/// Parses a directory color as the apps set one: a named color, or a custom one as `#` and six
/// hex digits, which is all the web app's color picker writes and shows.
fn parse_dir_color(color: &str) -> Result<DirColor<'static>, String> {
	if let Some(named) = NAMED_DIR_COLORS
		.iter()
		.find(|named| named.as_ref() == color)
	{
		return Ok(named.clone());
	}
	let is_hex = color
		.strip_prefix('#')
		.is_some_and(|hex| hex.len() == 6 && hex.bytes().all(|b| b.is_ascii_hexdigit()));
	if is_hex {
		return Ok(DirColor::Custom(Cow::Owned(color.to_owned())));
	}
	Err(format!(
		"use one of {}, or a hex color like #85bcff",
		NAMED_DIR_COLORS
			.iter()
			.map(AsRef::as_ref)
			.collect::<Vec<&str>>()
			.join(", ")
	))
}

/// Sets the color of the directory at `path`.
async fn set_directory_color(
	ui: &mut UI,
	client: &mut LazyClient,
	path: &ItemOperand,
	color: DirColor<'static>,
) -> Result<()> {
	let client = client.get(ui).await?;
	let mut dir = match resolve_item(client, path, "directory").await? {
		NonRootFileType::Dir(dir) => dir.into_owned(),
		NonRootFileType::Root(_) => {
			return Err(UI::failure("Cannot set the color of the root directory"));
		}
		NonRootFileType::File(_) => {
			return Err(UI::failure(&format!("Not a directory: {}", path)));
		}
	};
	client
		.set_dir_color(&mut dir, color)
		.await
		.context("Failed to set directory color")?;
	ui.print_success(&format!(
		"Set the color of {} to {}",
		path,
		dir.color().as_ref()
	));
	Ok(())
}

/// Favorites, or unfavorites, the items `operands` name.
pub(crate) async fn set_favorites(
	ui: &mut UI,
	client: &mut LazyClient,
	working_path: &RemotePath,
	operands: &[String],
	favorite: bool,
) -> Result<()> {
	let operands = parse_operands(working_path, operands)?;
	let client = client.get(ui).await?;
	let Given {
		items,
		mut failures,
	} = expand_all(ui, client, operands, "file or directory").await?;
	for given in items {
		let set = set_favorite(ui, client, given, favorite).await;
		failures.record(ui, set)?;
	}
	failures.finish(if favorite { "favorited" } else { "unfavorited" })
}

async fn set_favorite(
	ui: &mut UI,
	client: &Client,
	GivenItem { operand, item }: GivenItem<'_>,
	favorite: bool,
) -> Result<()> {
	match item {
		NonRootFileType::File(mut file) => {
			client
				.set_file_favorite(file.to_mut(), favorite)
				.await
				.context("Failed to set file favorite status")?;
			ui.print_success(&format!(
				"{} file: {}",
				if favorite { "Favorited" } else { "Unfavorited" },
				operand
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
				operand
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
	print_listing(
		ui,
		&items,
		ListedBy::Path,
		"No recent items",
		false,
		ListingLayout::Grid,
	)
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
	print_listing(
		ui,
		&items,
		ListedBy::Path,
		empty_message,
		false,
		ListingLayout::Grid,
	)
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
	let dir_items = dirs.iter().map(|dir| {
		(
			NonRootItemType::Dir(Cow::Borrowed(dir)),
			ListedItem::of_dir(dir),
		)
	});
	let file_items = files.iter().map(|file| {
		(
			NonRootItemType::File(Cow::Borrowed(file)),
			ListedItem::of_file(file),
		)
	});
	Ok(stream::iter(
		dir_items
			.chain(file_items)
			.map(|(item, listed)| async move {
				ListedItem {
					path: full_path(client, &item).await,
					..listed
				}
			}),
	)
	.buffered(PATH_LOOKUP_CONCURRENCY)
	.collect()
	.await)
}

/// The item's absolute path, without the trailing slash `get_item_path` puts on directories
/// (listings already distinguish directories by color).
pub(crate) async fn resolve_item_path(
	client: &Client,
	item: &NonRootItemType<'_, Normal>,
) -> Result<String, Error> {
	let (path, _) = client.get_item_path(item).await?;
	Ok(format!("/{}", path.trim_end_matches('/')))
}

/// How many item paths a listing resolves at once. Each resolution walks up the tree with its own
/// requests, so a listing of many items waits on the server far less when several run together.
pub(crate) const PATH_LOOKUP_CONCURRENCY: usize = 8;

/// [`resolve_item_path`], or `None` if it cannot be resolved, which is logged.
pub(crate) async fn full_path(
	client: &Client,
	item: &NonRootItemType<'_, Normal>,
) -> Option<String> {
	resolve_item_path(client, item)
		.await
		.inspect_err(|e| log::warn!("Failed to resolve path of item {}: {}", item.uuid(), e))
		.ok()
}

pub(crate) async fn list_trash(ui: &mut UI, client: &mut LazyClient, long: bool) -> Result<()> {
	let client = client.get(ui).await?;
	let items = list_trash_items(client)
		.await?
		.into_iter()
		.map(|entry| entry.listed)
		.collect::<Vec<ListedItem>>();
	print_listing(
		ui,
		&items,
		ListedBy::Name,
		"Trash is empty",
		long,
		ListingLayout::Grid,
	)
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

	let mut name_counts: HashMap<&str, usize> = HashMap::new();
	for entry in &entries {
		*name_counts.entry(entry.listed.name.as_str()).or_default() += 1;
	}
	let options = entries
		.iter()
		.map(|entry| {
			let shares_name = name_counts
				.get(entry.listed.name.as_str())
				.is_some_and(|&count| count > 1);
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
		TrashSelection::Named(operands) => deletion_confirmation(operands),
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
	if !ui.prompt_confirm(&deletion_confirmation(&names), false)? {
		return Ok(());
	}
	act_on_trash_entries(ui, client, entries, TrashAction::Delete).await
}

/// What permanently deleting the items `names` asks first.
fn deletion_confirmation(names: &[impl fmt::Display]) -> String {
	match names {
		[name] => format!("Permanently delete {}?", name),
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

	#[test]
	fn changes_kept_after_an_unexpected_error_are_a_failure() {
		let err = kept_changes(
			anyhow::anyhow!("connection reset"),
			Path::new("/tmp/edit/a.txt"),
		);

		assert!(matches!(find_cli_error(&err), Some(CliError::Failure(_))));
		assert_eq!(crate::error::exit_code(&err), 1);
		assert_eq!(
			format!("{err:#}"),
			"Your changes were not uploaded and are kept at /tmp/edit/a.txt: connection reset"
		);
	}

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

	fn source(shown: &ItemOperand) -> Source<'_> {
		Source {
			shown,
			name: shown
				.path()
				.and_then(RemotePath::basename)
				.unwrap_or("x.txt"),
			uuid: Uuid::from_u128(1),
			parent: ParentUuid::Uuid(Uuid::from_u128(2)),
		}
	}

	#[test]
	fn a_source_without_a_path_goes_where_the_destination_says() {
		let shown = ItemOperand::Uuid(Uuid::from_u128(1));
		let source = source(&shown);

		assert_eq!(
			target_of(
				Placement::Move,
				&source,
				&RemotePath::new("/b"),
				Found::Directory,
				false
			)
			.unwrap(),
			Target::Into {
				dir: RemotePath::new("/b"),
				path: RemotePath::new("/b/x.txt"),
			}
		);
		assert_eq!(
			target_of(
				Placement::Copy,
				&source,
				&RemotePath::new("/b/y.txt"),
				Found::Nothing,
				false
			)
			.unwrap(),
			Target::At {
				parent: RemotePath::new("/b"),
				name: "y.txt".to_owned(),
				path: RemotePath::new("/b/y.txt"),
			}
		);
	}

	fn target(
		placement: Placement,
		source_path: &str,
		destination: &str,
		found: Found,
	) -> Result<Target> {
		let source_path = ItemOperand::Path(RemotePath::new(source_path));
		target_of(
			placement,
			&source(&source_path),
			&RemotePath::new(destination),
			found,
			names_a_directory(destination),
		)
	}

	/// Checks `source` (UUID 1, in directory 2) against the pathless directory 3, below the
	/// directories `above`.
	fn check_pathless(placement: Placement, source: &Source<'_>, above: &[u128]) -> Result<()> {
		let chain = above
			.iter()
			.chain([&3])
			.map(|&uuid| Uuid::from_u128(uuid))
			.collect::<Vec<_>>();
		check_pathless_destination(
			placement,
			source,
			Uuid::from_u128(3),
			&ItemOperand::Uuid(Uuid::from_u128(3)),
			&chain,
		)
	}

	#[test]
	fn a_directory_without_a_path_takes_a_source_from_elsewhere() {
		let shown = ItemOperand::Path(RemotePath::new("/a/x.txt"));

		check_pathless(Placement::Move, &source(&shown), &[0, 4]).unwrap();
		check_pathless(Placement::Copy, &source(&shown), &[0, 4]).unwrap();
	}

	#[test]
	fn a_directory_without_a_path_below_the_source_is_refused() {
		let shown = ItemOperand::Path(RemotePath::new("/a"));

		assert_eq!(
			check_pathless(Placement::Copy, &source(&shown), &[0, 1, 4])
				.unwrap_err()
				.to_string(),
			format!("Cannot copy /a into itself: uuid:{}", Uuid::from_u128(3))
		);
	}

	#[test]
	fn a_source_cannot_be_moved_into_itself_without_a_path() {
		let shown = ItemOperand::Uuid(Uuid::from_u128(3));
		let source = Source {
			uuid: Uuid::from_u128(3),
			..source(&shown)
		};

		assert_eq!(
			check_pathless(Placement::Move, &source, &[0])
				.unwrap_err()
				.to_string(),
			format!(
				"Cannot move uuid:{0} into itself: uuid:{0}",
				Uuid::from_u128(3)
			)
		);
	}

	#[test]
	fn a_source_already_in_a_directory_without_a_path_is_not_moved_there() {
		let shown = ItemOperand::Uuid(Uuid::from_u128(1));
		let source = Source {
			parent: ParentUuid::Uuid(Uuid::from_u128(3)),
			..source(&shown)
		};

		assert_eq!(
			check_pathless(Placement::Move, &source, &[0])
				.unwrap_err()
				.to_string(),
			format!(
				"uuid:{} is already in uuid:{}",
				Uuid::from_u128(1),
				Uuid::from_u128(3)
			)
		);
		// a copy next to its source is refused by the name it clashes with
		check_pathless(Placement::Copy, &source, &[0]).unwrap();
	}

	#[test]
	fn an_item_in_a_directory_without_a_path_is_shown_with_its_uuid() {
		let dir = ShownDirectory::Uuid(ItemOperand::Uuid(Uuid::from_u128(3)));

		assert_eq!(
			dir.child("a.txt"),
			format!("a.txt in uuid:{}", Uuid::from_u128(3))
		);
		assert_eq!(
			ShownDirectory::Path(RemotePath::new("/b")).child("a.txt"),
			"/b/a.txt"
		);
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
			console::strip_ansi_codes(&printed.lines()),
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
		let printed = console::strip_ansi_codes(&printed.lines()).into_owned();
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
		assert!(!StderrProgress::new(&Printed::default().ui()).enabled);
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
			color: None,
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
			deletion_confirmation(&["a.txt"]),
			"Permanently delete a.txt?"
		);
	}

	#[test]
	fn deleting_several_trashed_items_asks_about_them_by_count() {
		assert_eq!(
			deletion_confirmation(&["a.txt", "b.txt"]),
			"Permanently delete 2 items?"
		);
	}

	#[test]
	fn permanently_deleting_items_named_by_uuid_asks_about_them_as_given() {
		let uuid = ItemOperand::Uuid(Uuid::from_u128(1));
		assert_eq!(
			deletion_confirmation(&[&uuid]),
			"Permanently delete uuid:00000000-0000-0000-0000-000000000001?"
		);
		assert_eq!(
			deletion_confirmation(&[&uuid, &ItemOperand::Path(RemotePath::new("/a"))]),
			"Permanently delete 2 items?"
		);
	}

	#[derive(Debug, clap::Parser)]
	struct Args {
		#[command(subcommand)]
		command: FsCommands,
	}

	fn parsed(args: &[&str]) -> Result<FsCommands, clap::error::ErrorKind> {
		<Args as clap::Parser>::try_parse_from(["filen"].iter().chain(args))
			.map(|args| args.command)
			.map_err(|e| e.kind())
	}

	#[test]
	fn mv_and_cp_take_several_sources_then_the_destination() {
		let Ok(FsCommands::Mv {
			sources,
			destination,
		}) = parsed(&["mv", "a", "*.txt", "dir"])
		else {
			panic!("mv did not parse");
		};
		assert_eq!(sources, ["a", "*.txt"]);
		assert_eq!(destination, "dir");
		let Ok(FsCommands::Cp {
			sources,
			destination,
		}) = parsed(&["cp", "a", "b"])
		else {
			panic!("cp did not parse");
		};
		assert_eq!(sources, ["a"]);
		assert_eq!(destination, "b");
		assert_eq!(
			parsed(&["mv", "a"]).unwrap_err(),
			clap::error::ErrorKind::MissingRequiredArgument
		);
	}

	#[test]
	fn rm_cat_and_favorite_take_one_or_more_items() {
		let Ok(FsCommands::Rm {
			files_or_directories,
			permanent: true,
		}) = parsed(&["rm", "-p", "a", "b"])
		else {
			panic!("rm did not parse");
		};
		assert_eq!(files_or_directories, ["a", "b"]);
		for command in ["rm", "cat", "favorite", "unfavorite"] {
			assert_eq!(
				parsed(&[command]).unwrap_err(),
				clap::error::ErrorKind::MissingRequiredArgument,
				"{command}"
			);
		}
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
	fn clash_on_the_second_of_two_same_named_sources_in_one_command() {
		let mut names = ExistingNames::new([(Uuid::from_u128(1), "other.txt")]);
		assert_eq!(names.clashing("a.txt", None), None);
		names.record(Uuid::from_u128(2), "a.txt");
		assert_eq!(names.clashing("A.txt", None), Some("a.txt"));
		assert_eq!(
			check_name_free(
				&names,
				"A.txt",
				None,
				&ShownDirectory::Path(RemotePath::new("/dest"))
			)
			.unwrap_err()
			.to_string(),
			"Destination already exists: /dest/a.txt"
		);
	}

	#[test]
	fn clash_ignores_a_moved_item_already_in_the_destination() {
		let names = ExistingNames::new([(Uuid::from_u128(1), "a.txt")]);
		assert_eq!(names.clashing("A.TXT", Some(Uuid::from_u128(1))), None);
		assert_eq!(
			names.clashing("A.TXT", Some(Uuid::from_u128(2))),
			Some("a.txt")
		);
	}

	#[test]
	fn clash_ignores_only_the_moved_item_among_same_named_ones() {
		let names =
			ExistingNames::new([(Uuid::from_u128(1), "a.txt"), (Uuid::from_u128(2), "A.txt")]);
		assert_eq!(
			names.clashing("a.txt", Some(Uuid::from_u128(1))),
			Some("A.txt")
		);
	}

	#[test]
	fn clash_check_of_a_refused_item_leaves_the_names_unchanged() {
		let names = ExistingNames::new([(Uuid::from_u128(1), "a.txt")]);
		assert!(
			check_name_free(
				&names,
				"a.txt",
				None,
				&ShownDirectory::Path(RemotePath::new("/dest"))
			)
			.is_err()
		);
		assert_eq!(names, ExistingNames::new([(Uuid::from_u128(1), "a.txt")]));
		assert_eq!(names.clashing("b.txt", None), None);
	}

	#[test]
	fn clash_after_a_move_within_the_directory_sees_only_the_new_name() {
		let mut names = ExistingNames::new([(Uuid::from_u128(1), "a.txt")]);
		names.record(Uuid::from_u128(1), "b.txt");
		assert_eq!(names.clashing("a.txt", None), None);
		assert_eq!(names.clashing("B.txt", None), Some("b.txt"));
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
			color: Some(DirColor::Default),
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
			color: None,
			uuid: Uuid::from_u128(uuid),
		}
	}

	/// What `print_listing` prints for `items`, sorted first, without colors.
	fn printed_listing(mut items: Vec<ListedItem>, by: ListedBy, long: bool, json: bool) -> String {
		let printed = Printed::default();
		let mut ui = printed.ui();
		ui.json = json;
		sort_listing(&mut items, by);
		print_listing(
			&mut ui,
			&items,
			by,
			"Directory is empty",
			long,
			ListingLayout::Grid,
		)
		.unwrap();
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
	fn a_long_listing_omits_the_dates_while_replay_testing() {
		let items = vec![
			ListedItem {
				modified: Some(date(2024, 3, 9)),
				..listed_file("a.txt", 5, 1)
			},
			listed_file("b.txt", 5, 2),
		];
		let printed = Printed::default();
		let mut ui = printed.ui();
		ui.initialize(false, false, None, true, true);
		print_listing(
			&mut ui,
			&items,
			ListedBy::Name,
			"Directory is empty",
			true,
			ListingLayout::Grid,
		)
		.unwrap();

		assert_eq!(
			console::strip_ansi_codes(&printed.lines()),
			"5 B  [omit]  a.txt\n5 B  -       b.txt"
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
	fn a_long_listing_shows_the_color_of_a_colored_directory() {
		let items = vec![
			ListedItem {
				color: Some(DirColor::Red),
				favorited: true,
				..listed_dir("docs", 1)
			},
			ListedItem {
				color: Some(DirColor::Custom(Cow::Borrowed("#85bcff"))),
				..listed_dir("more", 2)
			},
			listed_dir("plain", 3),
		];

		assert_eq!(
			printed_listing(items, ListedBy::Name, true, false),
			"-  -  docs [red] (*)\n-  -  more [#85bcff]\n-  -  plain"
		);
	}

	#[test]
	fn a_json_listing_gives_a_directorys_color() {
		let items = vec![ListedItem {
			color: Some(DirColor::Custom(Cow::Borrowed("#85bcff"))),
			..listed_dir("docs", 1)
		}];

		let printed: serde_json::Value =
			serde_json::from_str(&printed_listing(items, ListedBy::Name, false, true)).unwrap();

		assert_eq!(printed[0]["color"], "#85bcff");
	}

	#[test]
	fn a_directory_color_is_a_named_color() {
		for (name, color) in [
			("default", DirColor::Default),
			("blue", DirColor::Blue),
			("green", DirColor::Green),
			("purple", DirColor::Purple),
			("red", DirColor::Red),
			("gray", DirColor::Gray),
		] {
			assert_eq!(parse_dir_color(name), Ok(color));
		}
	}

	#[test]
	fn a_directory_color_is_a_hex_color_of_six_digits() {
		assert_eq!(
			parse_dir_color("#85bcFF"),
			Ok(DirColor::Custom(Cow::Borrowed("#85bcFF")))
		);
	}

	#[test]
	fn other_directory_colors_are_refused_with_the_valid_ones() {
		for color in ["Red", "pink", "#85bcf", "#85bcffa", "85bcff", "#85bcfg", ""] {
			assert_eq!(
				parse_dir_color(color),
				Err(
					"use one of default, blue, green, purple, red, gray, or a hex color like \
				     #85bcff"
						.to_owned()
				),
				"{color}"
			);
		}
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
					"color": "default",
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
					"color": "default",
					"uuid": "00000000-0000-0000-0000-000000000001",
				},
				{
					"name": "lost",
					"path": null,
					"type": "directory",
					"created": null,
					"favorited": false,
					"color": "default",
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

	#[cfg(not(windows))]
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

	#[cfg(not(windows))]
	#[test]
	fn an_editor_command_with_an_unclosed_quote_fails() {
		assert_eq!(
			editor_command(None, Some("vi 'a")).unwrap_err().to_string(),
			"Cannot split $EDITOR into words: vi 'a"
		);
	}

	#[cfg(windows)]
	#[test]
	fn an_editor_command_keeps_the_backslashes_of_a_windows_path() {
		assert_eq!(
			editor_command(None, Some(r"C:\Tools\vim\vim.exe -f")).unwrap(),
			Some(EditorCommand {
				program: r"C:\Tools\vim\vim.exe".to_owned(),
				args: vec!["-f".to_owned()],
			})
		);
		assert_eq!(
			editor_command(None, Some(r#"vi "a"#))
				.unwrap_err()
				.to_string(),
			r#"Cannot split $EDITOR into words: vi "a"#
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
