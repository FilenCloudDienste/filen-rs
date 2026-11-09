//! [cli-doc] shares-overview
//! `share <path> <email>` shares a file or directory of your drive with one of your contacts (see
//! `contacts`): they can then browse and download it. A directory is shared with everything in
//! it, and what you later add to it is shared too. Someone who is not a contact yet needs a
//! contact request first (`contact-request <email>`).
//!
//! `shared-by-me` lists what you shared, by its path in your drive, each with whom it is shared.
//! `unshare <path> [email]` stops sharing a file or directory with that contact, or with everyone
//! it is shared with when no email is given (after asking to confirm, which `--yes` answers).
//!
//! `shared-with-me` lists what others shared with you, each with who shared it. Given a path, it
//! lists the directory at that path instead, or shows the file there: the path starts with the
//! name of a shared item, as `shared-with-me` lists it, and goes down from there (for example
//! `shared-with-me Photos/2024`). When several people shared items of the same name with you,
//! `--from <email>` picks the one shared by that account; without a path, `--from` lists only what
//! that account shared with you. `leave-share <name>` removes an item
//! someone shared with you from what is shared with you (after asking to confirm, which `--yes`
//! answers); only they can share it again.
//!
//! `download-shared <path> [destination]` downloads a file or directory (recursively) from what
//! is shared with you into a local directory, keeping its name, as `download` does for your own
//! drive. The path is one `shared-with-me` takes, and so is `--from`. An item whose name is not a
//! plain file name (one holding a `/`, or `..`) is not downloaded, and inside a directory it is
//! skipped with what is below it.
//!
//! With `--json`, the listings are printed as arrays.

use std::{borrow::Cow, sync::Arc};

use anyhow::{Context as _, Result};
use clap::Subcommand;
use clap_complete::engine::{ArgValueCompleter, PathCompleter};
use filen_sdk_rs::{
	auth::Client,
	connect::fs::{SharedDirectory, SharedRootFile, SharingRole},
	fs::{
		HasName as _, HasUUID as _,
		categories::{DirType, NonRootFileType, NonRootItemType, RootItemType, Shared},
		file::traits::File,
	},
	io::{AnonymousRemoteFile, DirDownloadOptions, HasFileInfo as _},
};
use filen_types::fs::Uuid;
use futures::{StreamExt as _, stream};
use serde_json::json;

use crate::{
	CommandResult,
	auth::LazyClient,
	commands::{
		CommandContext,
		contacts_cmds::find_entry,
		fs_cmds::{PATH_LOOKUP_CONCURRENCY, find_existing_item, resolve_item_path},
		transfer_cmds::{
			DownloadPath, TransferKind, TransferProgress, download_directory, download_file,
			finish, run_cancellable,
		},
	},
	completion::FilenCompleter,
	docs::DocElement,
	ui::{self, UI, name_or_uuid},
	util::RemotePath,
};

#[derive(Debug, Subcommand)]
pub(crate) enum SharesCommands {
	/// Share a file or directory with a contact
	Share {
		/// File or directory to share
		#[arg(add = FilenCompleter::file_or_directory())]
		path: String,
		/// The email address of the contact to share it with
		// not `email`, which is the id of the global --email option
		#[arg(id = "contact_email", value_name = "EMAIL")]
		email: String,
	},
	/// List what you shared, and with whom
	SharedByMe,
	/// Stop sharing a file or directory (without an email, with everyone, after asking to
	/// confirm, which --yes answers)
	Unshare {
		/// File or directory to stop sharing
		#[arg(add = FilenCompleter::file_or_directory())]
		path: String,
		/// The contact to stop sharing it with (default: everyone it is shared with)
		#[arg(id = "contact_email", value_name = "EMAIL")]
		email: Option<String>,
	},
	/// List what others shared with you, or a directory in it
	SharedWithMe {
		/// A path in what is shared with you, starting with a shared item's name (default: list
		/// the shared items)
		path: Option<String>,
		/// Only what this account shared: picks the shared item when several have the path's
		/// first name, and without a path lists only its items
		#[arg(long, value_name = "EMAIL")]
		from: Option<String>,
	},
	/// Remove an item someone shared with you from what is shared with you (asks to confirm,
	/// which --yes answers)
	LeaveShare {
		/// The shared item's name, as shared-with-me lists it
		name: String,
		/// Pick the item shared by this account, when several have the name
		#[arg(long, value_name = "EMAIL")]
		from: Option<String>,
	},
	/// Download a file or directory (recursively) from what others shared with you into a local
	/// directory
	DownloadShared {
		/// A path in what is shared with you, starting with a shared item's name
		source: String,
		/// Local destination directory (default: the current local directory)
		#[arg(add = ArgValueCompleter::new(PathCompleter::dir()))]
		destination: Option<String>,
		/// Pick the shared item shared by this account, when several have the path's first name
		#[arg(long, value_name = "EMAIL")]
		from: Option<String>,
	},
}

/// The share commands' docs, placed in the "Sharing" section.
pub(crate) const DOCS: &[DocElement] = &[
	DocElement::DocFragment("shares-overview"),
	DocElement::CommandHelp("share"),
	DocElement::CommandHelp("shared-by-me"),
	DocElement::CommandHelp("unshare"),
	DocElement::CommandHelp("shared-with-me"),
	DocElement::CommandHelp("leave-share"),
	DocElement::CommandHelp("download-shared"),
];

impl SharesCommands {
	pub(super) async fn execute(self, ctx: CommandContext<'_>) -> Result<CommandResult> {
		let CommandContext {
			ui,
			client,
			working_path,
			..
		} = ctx;
		match self {
			Self::Share { path, email } => {
				let path = working_path.navigate(&path);
				share(ui, client, &path, &email).await?;
			}
			Self::SharedByMe => print_shared_by_me(ui, client).await?,
			Self::Unshare { path, email } => {
				let path = working_path.navigate(&path);
				unshare(ui, client, &path, email.as_deref()).await?;
			}
			Self::SharedWithMe { path, from } => {
				print_shared_with_me(ui, client, path.as_deref(), from.as_deref()).await?;
			}
			Self::LeaveShare { name, from } => {
				leave_share(ui, client, &name, from.as_deref()).await?;
			}
			Self::DownloadShared {
				source,
				destination,
				from,
			} => {
				download_shared(ui, client, &source, destination.as_deref(), from.as_deref())
					.await?;
			}
		}
		Ok(CommandResult::default())
	}
}

fn no_progress() -> Option<&'static fn(u64, Option<u64>)> {
	None
}

// share and unshare

async fn share(ui: &mut UI, client: &mut LazyClient, path: &RemotePath, email: &str) -> Result<()> {
	let client = client.get(ui).await?;
	let contacts = client
		.get_contacts()
		.await
		.context("Failed to list contacts")?;
	let contact = find_entry(&contacts, email).ok_or_else(|| {
		UI::failure(&format!(
			"{} is not a contact: send a contact request first (contact-request {})",
			email.trim(),
			email.trim()
		))
	})?;
	let item = find_existing_item(client, path).await?;
	match item {
		NonRootFileType::Dir(dir) => client
			.share_dir(&dir, contact, no_progress())
			.await
			.with_context(|| format!("Failed to share {path}"))?,
		NonRootFileType::File(file) => client
			.share_file(&file, contact)
			.await
			.with_context(|| format!("Failed to share {path}"))?,
		NonRootFileType::Root(_) => return Err(UI::failure("Cannot share the root")),
	}
	ui.print_success(&format!("Shared {path} with {}", contact.email));
	Ok(())
}

/// What this account shared: each item, with whom.
struct OutShare {
	item: RootItemType<'static, Shared>,
	receiver: String,
}

async fn list_out_shares(client: &Client) -> Result<Vec<OutShare>> {
	let (dirs, files) = client
		.list_out_shared(None, no_progress())
		.await
		.context("Failed to list what you shared")?;
	let dirs = dirs.into_iter().map(|dir| OutShare {
		receiver: dir.sharing_role().email().to_owned(),
		item: RootItemType::Dir(Cow::Owned(dir)),
	});
	let files = files.into_iter().map(|file| OutShare {
		receiver: file.sharing_role().email().to_owned(),
		item: RootItemType::File(Cow::Owned(file)),
	});
	Ok(dirs.chain(files).collect())
}

fn root_name<'a>(item: &'a RootItemType<'_, Shared>) -> Option<&'a str> {
	match item {
		RootItemType::Dir(dir) => dir.name(),
		RootItemType::File(file) => file.name(),
	}
}

/// One item that this account shared, with everyone it is shared with.
#[derive(Debug, PartialEq, Eq)]
struct SharedItem<'a> {
	uuid: Uuid,
	name: Option<&'a str>,
	is_dir: bool,
	receivers: Vec<&'a str>,
}

/// The out-shares grouped by item, in the order the server listed them.
fn group_by_item(shares: &[OutShare]) -> Vec<SharedItem<'_>> {
	let mut items: Vec<SharedItem<'_>> = Vec::new();
	for share in shares {
		let uuid = share.item.uuid();
		match items.iter_mut().find(|item| item.uuid == uuid) {
			Some(item) => item.receivers.push(&share.receiver),
			None => items.push(SharedItem {
				uuid,
				name: root_name(&share.item),
				is_dir: matches!(share.item, RootItemType::Dir(_)),
				receivers: vec![&share.receiver],
			}),
		}
	}
	items
}

/// A shared item with its path in this account's drive.
struct ListedSharedItem<'a> {
	item: SharedItem<'a>,
	/// The absolute path, or `None` when it could not be resolved.
	path: Option<String>,
}

impl ListedSharedItem<'_> {
	/// How the plain listing names the item: its path, with a slash after a directory's, or its
	/// name (its uuid when the name could not be decrypted) when the path could not be resolved.
	fn shown_name(&self) -> String {
		match &self.path {
			Some(path) if self.item.is_dir => format!("{path}/"),
			Some(path) => path.clone(),
			None => name_or_uuid(self.item.name, self.item.uuid),
		}
	}
}

/// The absolute drive path of the item this account shared, or `None` when the item or its path
/// could not be fetched, so that one unresolvable item does not fail the whole listing. The shared
/// item carries no parent, so the drive's own copy is fetched first.
async fn drive_path(client: &Client, item: &SharedItem<'_>) -> Option<String> {
	let fetched = if item.is_dir {
		client
			.get_dir(item.uuid)
			.await
			.map(|dir| NonRootItemType::Dir(Cow::Owned(dir)))
	} else {
		client
			.get_file(item.uuid)
			.await
			.map(|file| NonRootItemType::File(Cow::Owned(file)))
	};
	let path = match fetched {
		Ok(fetched) => resolve_item_path(client, &fetched).await,
		Err(e) => Err(e),
	};
	path.inspect_err(|e| log::warn!("Failed to resolve path of shared item {}: {}", item.uuid, e))
		.ok()
}

fn shared_item_line(listed: &ListedSharedItem<'_>) -> String {
	format!(
		"{}  shared with {}",
		listed.shown_name(),
		listed.item.receivers.join(", ")
	)
}

fn shared_item_json(listed: &ListedSharedItem<'_>) -> serde_json::Value {
	let item = &listed.item;
	json!({
		"uuid": item.uuid.to_string(),
		"name": item.name,
		"path": listed.path,
		"type": if item.is_dir { "dir" } else { "file" },
		"receivers": item.receivers,
	})
}

async fn print_shared_by_me(ui: &mut UI, client: &mut LazyClient) -> Result<()> {
	let client = client.get(ui).await?;
	let shares = list_out_shares(client).await?;
	let items = group_by_item(&shares);
	let mut listed = if items.is_empty() {
		Vec::new()
	} else {
		// resolving a path walks up the tree under the drive lock, which `get_item_path` takes for
		// each walk, so the listing waits for a lock another client holds either way; holding one
		// here keeps it between walks instead of releasing and re-acquiring it
		let _lock = client.lock_drive().await.context("Failed to lock drive")?;
		stream::iter(items.into_iter().map(|item| async move {
			let path = drive_path(client, &item).await;
			ListedSharedItem { item, path }
		}))
		.buffered(PATH_LOOKUP_CONCURRENCY)
		.collect::<Vec<_>>()
		.await
	};
	listed.sort_by_cached_key(ListedSharedItem::shown_name);
	if ui.json {
		return ui.print_json(serde_json::Value::Array(
			listed.iter().map(shared_item_json).collect(),
		));
	}
	if listed.is_empty() {
		ui.print_muted("You have not shared anything");
	}
	for item in &listed {
		ui.print(&shared_item_line(item));
	}
	Ok(())
}

/// The confirmation asked before `unshare` without an email stops sharing `path` with everyone.
fn unshare_all_question(path: &RemotePath) -> String {
	format!("Stop sharing {path} with everyone it is shared with?")
}

/// Fails unless unsharing `path` can be confirmed, by `--yes` or at a terminal, when there is a
/// confirmation to ask: without an email, which unshares it with everyone. Runs before logging
/// in, so that a script learns it needs `--yes` before anything is looked up.
fn check_unshare(ui: &UI, path: &RemotePath, email: Option<&str>) -> Result<()> {
	match email {
		Some(_) => Ok(()),
		None => ui.ensure_can_confirm(&unshare_all_question(path)),
	}
}

async fn unshare(
	ui: &mut UI,
	client: &mut LazyClient,
	path: &RemotePath,
	email: Option<&str>,
) -> Result<()> {
	check_unshare(ui, path, email)?;
	let client = client.get(ui).await?;
	let uuid = match find_existing_item(client, path).await? {
		NonRootFileType::Dir(dir) => dir.uuid(),
		NonRootFileType::File(file) => file.uuid(),
		NonRootFileType::Root(_) => return Err(UI::failure("The root is never shared")),
	};
	let shares = list_out_shares(client).await?;
	let to_remove = shares
		.iter()
		.filter(|share| share.item.uuid() == uuid)
		.filter(|share| email.is_none_or(|email| share.receiver.eq_ignore_ascii_case(email.trim())))
		.collect::<Vec<_>>();
	if to_remove.is_empty() {
		return Err(UI::failure(&match email {
			Some(email) => format!("{path} is not shared with {}", email.trim()),
			None => format!("{path} is not shared"),
		}));
	}
	if email.is_none() && !ui.prompt_confirm(&unshare_all_question(path), false)? {
		return Ok(());
	}
	for share in to_remove {
		client
			.remove_shared_item(&share.item)
			.await
			.with_context(|| format!("Failed to stop sharing {path} with {}", share.receiver))?;
		ui.print_success(&format!("Stopped sharing {path} with {}", share.receiver));
	}
	Ok(())
}

// shared with me

/// Which shared item a name, and maybe a sharer, picks out of the items shared with this account.
#[derive(Debug, PartialEq, Eq)]
enum RootMatch {
	/// The item at this index.
	One(usize),
	NotFound,
	/// Several items have the name: these accounts shared them.
	Several(Vec<String>),
}

/// Whether `sharer` is the account `--from` names: emails compare case-insensitively, and the
/// spaces around `from` are ignored.
fn is_shared_by(sharer: &str, from: &str) -> bool {
	sharer.eq_ignore_ascii_case(from.trim())
}

/// The item, of `(name, sharer email)` pairs, named `name` and shared by `from` when it is given.
fn match_root<'a>(
	roots: impl Iterator<Item = (Option<&'a str>, &'a str)>,
	name: &str,
	from: Option<&str>,
) -> RootMatch {
	let mut matching = roots.enumerate().filter(|(_, (root_name, sharer))| {
		*root_name == Some(name) && from.is_none_or(|from| is_shared_by(sharer, from))
	});
	match (matching.next(), matching.next()) {
		(None, _) => RootMatch::NotFound,
		(Some((index, _)), None) => RootMatch::One(index),
		(Some(first), Some(second)) => RootMatch::Several(
			[first, second]
				.into_iter()
				.chain(matching)
				.map(|(_, (_, sharer))| sharer.to_owned())
				.collect(),
		),
	}
}

/// The names in a path through what is shared with this account. Empty segments (a leading,
/// trailing or doubled `/`) are skipped.
fn shared_path_segments(path: &str) -> Vec<&str> {
	path.split('/')
		.filter(|segment| !segment.is_empty())
		.collect()
}

/// The items shared with this account, directories first.
struct InShares {
	items: Vec<RootItemType<'static, Shared>>,
}

impl InShares {
	async fn list(client: &Client) -> Result<Self> {
		let (dirs, files) = client
			.list_in_shared_root(no_progress())
			.await
			.context("Failed to list what is shared with you")?;
		let dirs = dirs
			.into_iter()
			.map(|dir| RootItemType::Dir(Cow::Owned(dir)));
		let files = files
			.into_iter()
			.map(|file| RootItemType::File(Cow::Owned(file)));
		Ok(Self {
			items: dirs.chain(files).collect(),
		})
	}

	/// The shared item named `name`, shared by `from` when it is given.
	fn find(self, name: &str, from: Option<&str>) -> Result<RootItemType<'static, Shared>> {
		let roots = self
			.items
			.iter()
			.map(|item| (root_name(item), sharer_of(item).email()));
		match match_root(roots, name, from) {
			RootMatch::One(index) => Ok(self.items.into_iter().nth(index).expect(
				"match_root returns an index into the items it was given (should be impossible)",
			)),
			RootMatch::NotFound => Err(UI::failure(&match from {
				Some(from) => format!("Nothing named {name} is shared with you by {}", from.trim()),
				None => format!("Nothing named {name} is shared with you"),
			})),
			RootMatch::Several(sharers) if from.is_none() => Err(UI::failure(&format!(
				"Several items named {name} are shared with you, by {}: pick one with --from \
				 <email>",
				sharers.join(", ")
			))),
			RootMatch::Several(_) => Err(UI::failure(&format!(
				"Several items named {name} are shared with you by the same account"
			))),
		}
	}
}

/// A shared item's size, or `None` for a directory.
fn root_size(item: &RootItemType<'_, Shared>) -> Option<u64> {
	match item {
		RootItemType::Dir(_) => None,
		RootItemType::File(file) => Some(file.size()),
	}
}

fn sharer_of<'a>(item: &'a RootItemType<'_, Shared>) -> &'a SharingRole {
	match item {
		RootItemType::Dir(dir) => dir.sharing_role(),
		RootItemType::File(file) => file.sharing_role(),
	}
}

/// A line of a shared-with-me listing: a directory's name with a `/`, a file's with its size.
fn entry_line(name: &str, size: Option<u64>) -> String {
	match size {
		None => format!("{name}/"),
		Some(size) => format!("{name}  {}", ui::format_size(size)),
	}
}

fn entry_json(
	uuid: Uuid,
	name: Option<&str>,
	size: Option<u64>,
	sharer: &str,
) -> serde_json::Value {
	match size {
		None => json!({ "uuid": uuid.to_string(), "name": name, "type": "dir", "sharer": sharer }),
		Some(size) => json!({
			"uuid": uuid.to_string(),
			"name": name,
			"type": "file",
			"size": size,
			"sharer": sharer,
		}),
	}
}

/// What a shared-with-me path leads to.
enum SharedTarget {
	Dir(DirType<'static, Shared>),
	/// A file that is itself a shared item.
	RootFile(SharedRootFile),
	/// A file inside a shared directory.
	File(AnonymousRemoteFile),
}

/// Goes down from the shared directory `dir` through the names `rest`, listing each directory on
/// the way.
async fn descend(
	client: &Client,
	mut dir: DirType<'static, Shared>,
	role: &SharingRole,
	rest: &[&str],
) -> Result<SharedTarget> {
	let mut walked = Vec::new();
	let mut rest = rest.iter();
	while let Some(name) = rest.next() {
		walked.push(*name);
		let (dirs, files) = client
			.list_shared_dir(&dir, role, no_progress())
			.await
			.context("Failed to list a shared directory")?;
		if let Some(child) = dirs.into_iter().find(|child| child.name() == Some(name)) {
			dir = DirType::Dir(Cow::Owned(child));
			continue;
		}
		let Some(file) = files.into_iter().find(|file| file.name() == Some(name)) else {
			return Err(UI::failure(&format!(
				"No such file or directory in what is shared with you: {}",
				walked.join("/")
			)));
		};
		if !rest.as_slice().is_empty() {
			return Err(UI::failure(&format!(
				"{} is a file, not a directory",
				walked.join("/")
			)));
		}
		return Ok(SharedTarget::File(file));
	}
	Ok(SharedTarget::Dir(dir))
}

/// Finds what the path `first/rest..` leads to in what is shared with this account, and the role
/// it is shared under.
async fn resolve_shared(
	client: &Client,
	shares: InShares,
	first: &str,
	rest: &[&str],
	from: Option<&str>,
) -> Result<(SharedTarget, SharingRole)> {
	let root = shares.find(first, from)?;
	let role = sharer_of(&root).clone();
	let target = match root {
		RootItemType::File(file) if rest.is_empty() => SharedTarget::RootFile(file.into_owned()),
		RootItemType::File(_) => {
			return Err(UI::failure(&format!("{first} is a file, not a directory")));
		}
		RootItemType::Dir(dir) => {
			descend(
				client,
				DirType::Root(Cow::Owned(dir.into_owned())),
				&role,
				rest,
			)
			.await?
		}
	};
	Ok((target, role))
}

async fn print_shared_with_me(
	ui: &mut UI,
	client: &mut LazyClient,
	path: Option<&str>,
	from: Option<&str>,
) -> Result<()> {
	let segments = path.map(shared_path_segments).unwrap_or_default();
	let client = client.get(ui).await?;
	let shares = InShares::list(client).await?;
	let Some((first, rest)) = segments.split_first() else {
		return print_in_share_roots(ui, &shares, from);
	};
	let (target, role) = resolve_shared(client, shares, first, rest, from).await?;
	match target {
		SharedTarget::RootFile(file) => print_shared_root_file(ui, &file),
		SharedTarget::File(file) => print_shared_file(ui, &file, role.email()),
		SharedTarget::Dir(dir) => {
			let (dirs, files) = client
				.list_shared_dir(&dir, &role, no_progress())
				.await
				.context("Failed to list a shared directory")?;
			print_shared_dir(ui, &dirs, &files, role.email())
		}
	}
}

/// The items, of `(item, sharer email)` pairs, shared by `from`, or all of them without it.
fn shared_by<'a, T>(
	items: impl Iterator<Item = (T, &'a str)>,
	from: Option<&str>,
) -> impl Iterator<Item = T> {
	items
		.filter(move |(_, sharer)| from.is_none_or(|from| is_shared_by(sharer, from)))
		.map(|(item, _)| item)
}

/// What `shared-with-me` prints when no item is listed.
fn nothing_shared_message(from: Option<&str>) -> String {
	match from {
		Some(from) => format!("Nothing is shared with you by {}", from.trim()),
		None => "Nothing is shared with you".to_owned(),
	}
}

/// Lists the items shared with this account, only those shared by `from` when it is given.
fn print_in_share_roots(ui: &mut UI, shares: &InShares, from: Option<&str>) -> Result<()> {
	let items: Vec<_> = shared_by(
		shares
			.items
			.iter()
			.map(|item| (item, sharer_of(item).email())),
		from,
	)
	.collect();
	if ui.json {
		return ui.print_json(serde_json::Value::Array(
			items
				.iter()
				.map(|item| {
					entry_json(
						item.uuid(),
						root_name(item),
						root_size(item),
						sharer_of(item).email(),
					)
				})
				.collect(),
		));
	}
	if items.is_empty() {
		ui.print_muted(&nothing_shared_message(from));
	}
	for item in items {
		let name = name_or_uuid(root_name(item), item.uuid());
		ui.print(&format!(
			"{}  from {}",
			entry_line(&name, root_size(item)),
			sharer_of(item).email()
		));
	}
	Ok(())
}

fn print_shared_dir(
	ui: &mut UI,
	dirs: &[SharedDirectory],
	files: &[AnonymousRemoteFile],
	sharer: &str,
) -> Result<()> {
	if ui.json {
		let dirs = dirs
			.iter()
			.map(|dir| entry_json(dir.uuid(), dir.name(), None, sharer));
		let files = files
			.iter()
			.map(|file| entry_json(file.uuid(), file.name(), Some(file.size()), sharer));
		return ui.print_json(serde_json::Value::Array(dirs.chain(files).collect()));
	}
	if dirs.is_empty() && files.is_empty() {
		ui.print_muted("The directory is empty");
	}
	for dir in dirs {
		ui.print(&entry_line(&name_or_uuid(dir.name(), dir.uuid()), None));
	}
	for file in files {
		ui.print(&entry_line(
			&name_or_uuid(file.name(), file.uuid()),
			Some(file.size()),
		));
	}
	Ok(())
}

fn print_file_table(
	ui: &mut UI,
	uuid: Uuid,
	name: Option<&str>,
	size: u64,
	sharer: &str,
) -> Result<()> {
	if ui.json {
		return ui.print_json(entry_json(uuid, name, Some(size), sharer));
	}
	let name = name_or_uuid(name, uuid);
	let size = ui::format_size(size);
	let uuid = uuid.to_string();
	ui.print_key_value_table(&[
		("Name", &name),
		("Size", &size),
		("Shared by", sharer),
		("UUID", &uuid),
	]);
	Ok(())
}

fn print_shared_root_file(ui: &mut UI, file: &SharedRootFile) -> Result<()> {
	print_file_table(
		ui,
		file.uuid(),
		file.name(),
		file.size(),
		file.sharing_role().email(),
	)
}

fn print_shared_file(ui: &mut UI, file: &AnonymousRemoteFile, sharer: &str) -> Result<()> {
	print_file_table(ui, file.uuid(), file.name(), file.size(), sharer)
}

// download from a share

/// The local path a shared item named `name` is downloaded to inside `destination`. A name that is
/// not one plain path component would land outside it, so it is refused, not cleaned up.
fn download_target(
	destination: &DownloadPath,
	name: Option<&str>,
	source: &str,
) -> Result<DownloadPath> {
	match name {
		Some(name) => destination.child(name).ok_or_else(|| {
			UI::failure(&format!(
				"Cannot download {source}: its name {name:?} is not a plain file name"
			))
		}),
		None => Err(UI::failure(&format!(
			"Cannot download {source}: its name could not be decrypted"
		))),
	}
}

fn shared_dir_name<'a>(dir: &'a DirType<'_, Shared>) -> Option<&'a str> {
	match dir {
		DirType::Root(root) => root.name(),
		DirType::Dir(dir) => dir.name(),
	}
}

/// The path `download-shared` downloads, split into the shared item's name and the names below it.
fn split_download_source(source: &str) -> Result<(&str, Vec<&str>)> {
	let mut segments = shared_path_segments(source).into_iter();
	let first = segments.next().ok_or_else(|| {
		UI::usage_failure(
			"download-shared takes a path in what is shared with you, starting with a shared \
			 item's name, as shared-with-me lists it",
		)
	})?;
	Ok((first, segments.collect()))
}

async fn download_shared(
	ui: &mut UI,
	client: &mut LazyClient,
	source: &str,
	destination: Option<&str>,
	from: Option<&str>,
) -> Result<()> {
	let (first, rest) = split_download_source(source)?;
	let destination = destination.unwrap_or(".");
	let destination = DownloadPath::canonicalize(destination)
		.map_err(|_| UI::failure(&format!("No such local directory: {destination}")))?;
	if !destination.io().is_dir() {
		return Err(UI::failure(&format!(
			"Not a directory: {}",
			destination.shown()
		)));
	}
	// the path as it is shown, without the empty segments a leading or doubled `/` leaves
	let source = [first]
		.iter()
		.chain(&rest)
		.copied()
		.collect::<Vec<_>>()
		.join("/");
	let client = client.get(ui).await?;
	let shares = InShares::list(client).await?;
	let (target, role) = resolve_shared(client, shares, first, &rest, from).await?;
	let progress = TransferProgress::new(TransferKind::Download, ui);
	let options = DirDownloadOptions::default();
	let target = match target {
		SharedTarget::RootFile(file) => {
			download_shared_file(ui, client, &progress, &file, &destination, &source).await?
		}
		SharedTarget::File(file) => {
			download_shared_file(ui, client, &progress, &file, &destination, &source).await?
		}
		SharedTarget::Dir(dir) => {
			let path = download_target(&destination, shared_dir_name(&dir), &source)?;
			run_cancellable(
				ui,
				&progress,
				download_directory::<Shared>(
					Arc::clone(client),
					&progress,
					dir,
					&path,
					&role,
					options,
				),
			)
			.await??;
			path
		}
	};
	let target = target.shown();
	finish(ui, &progress, &format!("{source} to {target}"), &target)
}

/// Downloads the shared `file` into `destination` under its own name, returning its local path.
async fn download_shared_file(
	ui: &mut UI,
	client: &Client,
	progress: &TransferProgress,
	file: &dyn File,
	destination: &DownloadPath,
	source: &str,
) -> Result<DownloadPath> {
	let path = download_target(destination, file.name(), source)?;
	run_cancellable(
		ui,
		progress,
		download_file(
			client,
			progress,
			file,
			&path,
			&DirDownloadOptions::default(),
		),
	)
	.await??;
	Ok(path)
}

// leave a share

fn leave_question(name: &str) -> String {
	format!("Leave the share {name}? Only its owner can share it with you again.")
}

/// Fails unless leaving the share named `name` can be confirmed, by `--yes` or at a terminal, and
/// unless `name` is one name, not a path into a share. Runs before logging in, so that a script
/// learns what is wrong before anything is looked up.
fn check_leave_share(ui: &UI, name: &str) -> Result<()> {
	if shared_path_segments(name).len() != 1 {
		return Err(UI::usage_failure(
			"leave-share takes the name of an item shared with you, as shared-with-me lists it, \
			 not a path inside it",
		));
	}
	ui.ensure_can_confirm(&leave_question(name.trim_matches('/')))
}

async fn leave_share(
	ui: &mut UI,
	client: &mut LazyClient,
	name: &str,
	from: Option<&str>,
) -> Result<()> {
	check_leave_share(ui, name)?;
	let name = name.trim_matches('/');
	let client = client.get(ui).await?;
	let item = InShares::list(client).await?.find(name, from)?;
	let sharer = sharer_of(&item).email().to_owned();
	if !ui.prompt_confirm(&leave_question(&format!("{name} from {sharer}")), false)? {
		return Ok(());
	}
	client
		.remove_shared_item(&item)
		.await
		.with_context(|| format!("Failed to leave the share {name}"))?;
	ui.print_success(&format!("Left the share {name} from {sharer}"));
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::{commands::Commands, error::exit_code, parse_args, ui::test_support::Printed};

	const ALICE: &str = "alice@example.com";
	const BOB: &str = "bob@example.com";

	fn match_in(roots: &[(Option<&str>, &str)], name: &str, from: Option<&str>) -> RootMatch {
		match_root(roots.iter().copied(), name, from)
	}

	#[test]
	fn a_shared_item_is_found_by_its_exact_name() {
		let roots = [(Some("Photos"), ALICE), (None, BOB), (Some("Docs"), BOB)];
		assert_eq!(match_in(&roots, "Docs", None), RootMatch::One(2));
		assert_eq!(match_in(&roots, "docs", None), RootMatch::NotFound);
		assert_eq!(match_in(&roots, "Pho", None), RootMatch::NotFound);
	}

	#[test]
	fn from_picks_one_of_several_items_of_the_same_name() {
		let roots = [(Some("Photos"), ALICE), (Some("Photos"), BOB)];
		assert_eq!(
			match_in(&roots, "Photos", None),
			RootMatch::Several(vec![ALICE.to_owned(), BOB.to_owned()])
		);
		assert_eq!(
			match_in(&roots, "Photos", Some("BOB@example.com")),
			RootMatch::One(1)
		);
		assert_eq!(
			match_in(&roots, "Photos", Some("carol@example.com")),
			RootMatch::NotFound
		);
	}

	#[test]
	fn is_shared_by_ignores_case_and_spaces() {
		assert!(is_shared_by(ALICE, ALICE));
		assert!(is_shared_by(ALICE, "  Alice@Example.COM \t"));
		assert!(!is_shared_by(ALICE, BOB));
		assert!(!is_shared_by(ALICE, "alice@example"));
	}

	#[test]
	fn from_keeps_only_the_items_its_account_shared() {
		let items = [(1, ALICE), (2, BOB), (3, ALICE)];
		assert_eq!(
			shared_by(items.into_iter(), Some(" Alice@example.com")).collect::<Vec<_>>(),
			[1, 3]
		);
		assert_eq!(
			shared_by(items.into_iter(), Some("carol@example.com")).collect::<Vec<_>>(),
			Vec::<i32>::new()
		);
		assert_eq!(
			shared_by(items.into_iter(), None).collect::<Vec<_>>(),
			[1, 2, 3]
		);
	}

	#[test]
	fn the_empty_listing_names_the_account_from_names() {
		assert_eq!(
			nothing_shared_message(Some(" carol@example.com ")),
			"Nothing is shared with you by carol@example.com"
		);
		assert_eq!(nothing_shared_message(None), "Nothing is shared with you");
	}

	#[test]
	fn empty_path_segments_are_skipped() {
		assert_eq!(shared_path_segments("Photos/2024"), ["Photos", "2024"]);
		assert_eq!(shared_path_segments("/Photos//2024/"), ["Photos", "2024"]);
		assert!(shared_path_segments("/").is_empty());
		assert!(shared_path_segments("").is_empty());
	}

	#[test]
	fn listing_lines_mark_directories_and_size_files() {
		assert_eq!(entry_line("Photos", None), "Photos/");
		assert_eq!(entry_line("a.txt", Some(1024)), "a.txt  1 KiB");
	}

	#[test]
	fn entry_json_has_a_size_only_for_files() {
		let uuid = Uuid::from_u128(1);
		assert_eq!(
			entry_json(uuid, Some("Photos"), None, ALICE),
			json!({ "uuid": uuid.to_string(), "name": "Photos", "type": "dir", "sharer": ALICE })
		);
		assert_eq!(
			entry_json(uuid, None, Some(3), ALICE),
			json!({
				"uuid": uuid.to_string(),
				"name": null,
				"type": "file",
				"size": 3,
				"sharer": ALICE,
			})
		);
	}

	fn listed_photos() -> ListedSharedItem<'static> {
		ListedSharedItem {
			item: SharedItem {
				uuid: Uuid::from_u128(1),
				name: Some("Photos"),
				is_dir: true,
				receivers: vec![ALICE, BOB],
			},
			path: Some("/media/Photos".to_owned()),
		}
	}

	fn unresolved(name: Option<&'static str>, is_dir: bool) -> ListedSharedItem<'static> {
		ListedSharedItem {
			item: SharedItem {
				uuid: Uuid::from_u128(2),
				name,
				is_dir,
				receivers: vec![ALICE],
			},
			path: None,
		}
	}

	#[test]
	fn a_shared_by_me_line_shows_the_drive_path_and_every_receiver() {
		assert_eq!(
			shared_item_line(&listed_photos()),
			"/media/Photos/  shared with alice@example.com, bob@example.com"
		);
	}

	#[test]
	fn shared_by_me_json_has_the_name_and_the_drive_path() {
		assert_eq!(
			shared_item_json(&listed_photos()),
			json!({
				"uuid": Uuid::from_u128(1).to_string(),
				"name": "Photos",
				"path": "/media/Photos",
				"type": "dir",
				"receivers": [ALICE, BOB],
			})
		);
	}

	#[test]
	fn a_shared_by_me_line_without_a_path_shows_the_name() {
		assert_eq!(
			shared_item_line(&unresolved(Some("Photos"), true)),
			"Photos  shared with alice@example.com"
		);
	}

	#[test]
	fn a_shared_by_me_line_without_a_path_or_name_shows_the_uuid() {
		assert_eq!(
			shared_item_line(&unresolved(None, true)),
			format!("{}  shared with alice@example.com", Uuid::from_u128(2))
		);
	}

	#[test]
	fn shared_by_me_json_has_a_null_path_when_it_could_not_be_resolved() {
		assert_eq!(
			shared_item_json(&unresolved(Some("notes.txt"), false)),
			json!({
				"uuid": Uuid::from_u128(2).to_string(),
				"name": "notes.txt",
				"path": null,
				"type": "file",
				"receivers": [ALICE],
			})
		);
	}

	#[test]
	fn unshare_without_an_email_needs_a_confirmation() {
		let mut ui = Printed::default().ui();
		let path = RemotePath::new("/docs");
		let err = check_unshare(&ui, &path, None).unwrap_err();
		assert_eq!(exit_code(&err), 2);
		assert_eq!(
			err.to_string(),
			"Cannot ask \"Stop sharing /docs with everyone it is shared with?\": stdin is not a \
			 terminal. Pass --yes to answer yes."
		);
		ui.set_assume_yes(true);
		check_unshare(&ui, &path, None).unwrap();
	}

	#[test]
	fn unshare_with_an_email_needs_no_confirmation() {
		let ui = Printed::default().ui();
		check_unshare(&ui, &RemotePath::new("/docs"), Some(ALICE)).unwrap();
	}

	#[test]
	fn leave_share_takes_one_name_and_needs_a_confirmation() {
		let mut ui = Printed::default().ui();
		let err = check_leave_share(&ui, "Photos/2024").unwrap_err();
		assert_eq!(exit_code(&err), 2);
		let err = check_leave_share(&ui, "Photos").unwrap_err();
		assert_eq!(exit_code(&err), 2);
		assert_eq!(
			err.to_string(),
			"Cannot ask \"Leave the share Photos? Only its owner can share it with you again.\": \
			 stdin is not a terminal. Pass --yes to answer yes."
		);
		ui.set_assume_yes(true);
		check_leave_share(&ui, "Photos/").unwrap();
		check_leave_share(&ui, "/").unwrap_err();
	}

	#[test]
	fn a_download_lands_inside_the_destination_under_the_item_name() {
		let dir = tempfile::tempdir().unwrap();
		let canonical = std::fs::canonicalize(dir.path()).unwrap();
		let destination = &DownloadPath::canonicalize(dir.path()).unwrap();
		assert_eq!(
			download_target(destination, Some("a.txt"), "Photos/a.txt")
				.unwrap()
				.io(),
			canonical.join("a.txt")
		);
		assert_eq!(
			download_target(destination, Some("..x"), "Photos/..x")
				.unwrap()
				.io(),
			canonical.join("..x")
		);
	}

	#[test]
	fn a_download_refuses_a_name_that_is_not_one_plain_component() {
		let dir = tempfile::tempdir().unwrap();
		let destination = &DownloadPath::canonicalize(dir.path()).unwrap();
		for name in ["..", ".", "", "a/b", "/etc/passwd", "../a.txt"] {
			let err = download_target(destination, Some(name), "Photos").unwrap_err();
			assert_eq!(exit_code(&err), 1, "{name:?}");
			assert_eq!(
				err.to_string(),
				format!("Cannot download Photos: its name {name:?} is not a plain file name")
			);
		}
		let err = download_target(destination, None, "Photos").unwrap_err();
		assert_eq!(
			err.to_string(),
			"Cannot download Photos: its name could not be decrypted"
		);
	}

	#[test]
	fn download_shared_needs_a_shared_item_name() {
		assert_eq!(
			split_download_source("/Photos//2024/a.txt").unwrap(),
			("Photos", vec!["2024", "a.txt"])
		);
		assert_eq!(split_download_source("Photos").unwrap(), ("Photos", vec![]));
		for source in ["", "/", "//"] {
			let err = split_download_source(source).unwrap_err();
			assert_eq!(exit_code(&err), 2, "{source:?}");
		}
	}

	fn parsed(args: &[&str]) -> Result<SharesCommands, clap::error::ErrorKind> {
		let command = std::iter::once("filen").chain(args.iter().copied());
		match parse_args(command).map_err(|e| e.kind())?.args.command {
			Some(Commands::Shares(command)) => Ok(command),
			other => panic!("parsed {other:?}"),
		}
	}

	#[test]
	fn the_contact_of_share_and_unshare_is_not_the_login_email() {
		for args in [["share", "/a.txt", ALICE], ["unshare", "/a.txt", ALICE]] {
			let command = std::iter::once("filen").chain(args);
			assert_eq!(parse_args(command).unwrap().args.email, None, "{args:?}");
		}
	}

	#[test]
	fn the_share_commands_parse() {
		assert!(matches!(
			parsed(&["share", "/a.txt", ALICE]),
			Ok(SharesCommands::Share { path, email }) if path == "/a.txt" && email == ALICE
		));
		assert_eq!(
			parsed(&["share", "/a.txt"]).err(),
			Some(clap::error::ErrorKind::MissingRequiredArgument)
		);
		assert!(matches!(
			parsed(&["unshare", "/a.txt", ALICE]),
			Ok(SharesCommands::Unshare { email: Some(email), .. }) if email == ALICE
		));
		assert!(matches!(
			parsed(&["unshare", "/a.txt"]),
			Ok(SharesCommands::Unshare { email: None, .. })
		));
		assert!(matches!(
			parsed(&["shared-with-me"]),
			Ok(SharesCommands::SharedWithMe {
				path: None,
				from: None
			})
		));
		assert!(matches!(
			parsed(&["shared-with-me", "Photos/2024", "--from", BOB]),
			Ok(SharesCommands::SharedWithMe { path: Some(path), from: Some(from) })
				if path == "Photos/2024" && from == BOB
		));
		assert!(matches!(
			parsed(&["download-shared", "Photos/a.txt"]),
			Ok(SharesCommands::DownloadShared { source, destination: None, from: None })
				if source == "Photos/a.txt"
		));
		assert!(matches!(
			parsed(&["download-shared", "Photos", "out", "--from", BOB]),
			Ok(SharesCommands::DownloadShared {
				source,
				destination: Some(destination),
				from: Some(from),
			}) if source == "Photos" && destination == "out" && from == BOB
		));
		assert!(matches!(
			parsed(&["leave-share", "Photos", "--yes"]),
			Ok(SharesCommands::LeaveShare { name, from: None }) if name == "Photos"
		));
	}
}
