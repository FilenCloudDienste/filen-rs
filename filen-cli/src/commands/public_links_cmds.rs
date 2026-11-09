//! [cli-doc] public-links
//! `public-link <path>` shows the public link of a file or directory: its settings, when it
//! expires (UTC) if it does, and its URL, the address to share. `list-public-links` lists the path,
//! expiry (UTC, or "never") and URL of every linked item (with `--json`, also each link's
//! `linkUuid`, and its expiry as `expiresAt`, `null` for a link that never expires).
//!
//! In a terminal, `public-link` offers to edit or delete an existing link, and asks for a new
//! link's settings: when it expires, a password, and whether the item can be downloaded. The link
//! is created as soon as its settings are confirmed, and its URL is printed then. A new link is
//! created with the default settings (no password, never expires, downloadable) and then given its
//! own; if that fails and they would have protected the item more, with a password, an expiry or
//! downloads off, the link is deleted again rather than left open. Press Esc to
//! cancel a prompt, which creates and changes nothing. `--yes` answers "Enable download?" with yes,
//! as it answers every yes/no question, so editing a link without downloads turns them on, which it
//! says on stderr (`--no-download` keeps them off); the other settings are still asked. Without a
//! terminal to
//! prompt on (or with `--no-input`), `public-link` only prints an existing link, and fails with
//! exit code 2 instead of creating a missing one: the setting options below create it without
//! prompting.
//!
//! The options do the same without prompting. `--expires <never|1h|6h|1d|3d|7d|14d|30d>`,
//! `--password <PASSWORD>` or `--no-password`, and `--download` or `--no-download` create the link
//! if there is none, and change only the settings given. `--show` prints the link and changes
//! nothing. `--delete` deletes it after asking to confirm, which `--yes` answers.
//!
//! A password given with `--password` is part of the command line, so your shell saves it in its
//! history, and other users of the computer can see it while the command runs. The CLI never reads
//! a link password from the environment: to keep one out of the history, enter it at the prompt.

use crate::{
	CommandResult,
	auth::LazyClient,
	commands::{
		CommandContext,
		fs_cmds::{PATH_LOOKUP_CONCURRENCY, find_existing_item, full_path},
	},
	completion::{FilenCompleter, quote_for_command_line},
	docs::DocElement,
	ui::{UI, format_date},
	util::RemotePath,
};
use anyhow::{Context as _, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use clap::{Args, Subcommand, ValueEnum};
use console::style;
use filen_sdk_rs::{
	Error, ErrorKind,
	auth::Client,
	connect::{DirPublicLinkRW, FilePublicLink, PasswordState},
	fs::{
		HasName, HasUUID,
		categories::{NonRootFileType, NonRootItemType, Normal},
		file::traits::HasFileInfo as _,
	},
	io::{RemoteDirectory, RemoteFile},
};
use filen_types::api::v3::dir::link::PublicLinkExpiration;
use futures::{StreamExt as _, TryStreamExt as _, stream};
use serde_json::json;
use std::{
	borrow::Cow,
	fmt::{Display, Write as _},
};
use unicode_width::UnicodeWidthStr as _;

#[derive(Debug, Subcommand)]
pub(crate) enum PublicLinksCommands {
	/// List public links, with the path and URL of each linked file or directory
	ListPublicLinks,
	/// Create, update or delete a public link for a file or directory
	///
	/// Without options, it shows the link and offers to edit or delete it, or asks for a new
	/// link's settings. With a setting option, it creates the link if there is none and changes
	/// only the settings given.
	PublicLink {
		/// File or directory to create, update or delete a public link for
		#[arg(add = FilenCompleter::file_or_directory())]
		file_or_directory: String,
		#[command(flatten)]
		settings: LinkSettingFlags,
		/// Print the link's settings and URL, and change nothing
		#[arg(long, conflicts_with_all = [LINK_SETTING_FLAGS, "delete"])]
		show: bool,
		/// Delete the link (asks to confirm, which --yes answers)
		#[arg(long, conflicts_with = LINK_SETTING_FLAGS)]
		delete: bool,
	},
}

/// The id of the group of [`LinkSettingFlags`], which `--show` and `--delete` conflict with.
const LINK_SETTING_FLAGS: &str = "link-settings";

/// The options of `public-link` that set a link's settings without prompting.
#[derive(Debug, Args)]
#[group(id = LINK_SETTING_FLAGS, multiple = true)]
pub(crate) struct LinkSettingFlags {
	/// Protect the link with this password. It is read from the command line only, so it ends
	/// up in your shell's history
	// no `env`: a link password is never read from the environment
	#[arg(
		long,
		value_name = "PASSWORD",
		value_parser = non_empty_password,
		conflicts_with = "no_password"
	)]
	password: Option<String>,
	/// Remove the link's password
	#[arg(long)]
	no_password: bool,
	/// When the link expires
	#[arg(long, value_name = "EXPIRATION")]
	expires: Option<ExpiresArg>,
	/// Allow downloading the linked item
	#[arg(long, conflicts_with = "no_download")]
	download: bool,
	/// Do not allow downloading the linked item
	#[arg(long)]
	no_download: bool,
}

/// Parses `--password`, which rejects an empty value: a script whose password variable is unset
/// would otherwise set an empty password, where it meant to set one or to remove it.
fn non_empty_password(password: &str) -> Result<String, &'static str> {
	if password.is_empty() {
		Err("the password is empty; to remove the link's password, use --no-password")
	} else {
		Ok(password.to_owned())
	}
}

/// `--expires`, with the tokens the API (and the old CLI's prompt) name expirations by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ExpiresArg {
	Never,
	#[value(name = "1h")]
	OneHour,
	#[value(name = "6h")]
	SixHours,
	#[value(name = "1d")]
	OneDay,
	#[value(name = "3d")]
	ThreeDays,
	#[value(name = "7d")]
	OneWeek,
	#[value(name = "14d")]
	TwoWeeks,
	#[value(name = "30d")]
	ThirtyDays,
}

impl From<ExpiresArg> for PublicLinkExpiration {
	fn from(expires: ExpiresArg) -> Self {
		match expires {
			ExpiresArg::Never => PublicLinkExpiration::Never,
			ExpiresArg::OneHour => PublicLinkExpiration::OneHour,
			ExpiresArg::SixHours => PublicLinkExpiration::SixHours,
			ExpiresArg::OneDay => PublicLinkExpiration::OneDay,
			ExpiresArg::ThreeDays => PublicLinkExpiration::ThreeDays,
			ExpiresArg::OneWeek => PublicLinkExpiration::OneWeek,
			ExpiresArg::TwoWeeks => PublicLinkExpiration::TwoWeeks,
			ExpiresArg::ThirtyDays => PublicLinkExpiration::ThirtyDays,
		}
	}
}

impl LinkSettingFlags {
	/// The changes the options ask for, or `None` when none was given.
	fn into_changes(self) -> Option<LinkChanges> {
		let password = match (self.password, self.no_password) {
			(Some(password), _) => PasswordChange::Set(password),
			(None, true) => PasswordChange::Clear,
			(None, false) => PasswordChange::Keep,
		};
		let downloadable = match (self.download, self.no_download) {
			(true, _) => Some(true),
			(false, true) => Some(false),
			(false, false) => None,
		};
		let changes = LinkChanges {
			expiration: self.expires.map(PublicLinkExpiration::from),
			password,
			downloadable,
		};
		(changes != LinkChanges::NONE).then_some(changes)
	}
}

/// What `public-link` does with an item's link.
#[derive(Debug, PartialEq, Eq)]
enum PublicLinkAction {
	/// Show the link and offer to edit or delete it, or prompt for a new one's settings.
	Interactive,
	/// Print the link, without prompting.
	Show,
	/// Delete the link, once confirmed.
	Delete,
	/// Create the link if it is missing, then change these settings, without prompting.
	Change(LinkChanges),
}

impl PublicLinkAction {
	/// The action the options ask for. clap lets at most one of `--show`, `--delete` and the
	/// setting options through.
	fn from_flags(settings: LinkSettingFlags, show: bool, delete: bool) -> Self {
		if show {
			PublicLinkAction::Show
		} else if delete {
			PublicLinkAction::Delete
		} else {
			settings
				.into_changes()
				.map_or(PublicLinkAction::Interactive, PublicLinkAction::Change)
		}
	}
}

/// The settings options given, each `None` (or [`PasswordChange::Keep`]) keeping the link's own.
#[derive(Debug, PartialEq, Eq)]
struct LinkChanges {
	expiration: Option<PublicLinkExpiration>,
	password: PasswordChange,
	downloadable: Option<bool>,
}

impl LinkChanges {
	const NONE: LinkChanges = LinkChanges {
		expiration: None,
		password: PasswordChange::Keep,
		downloadable: None,
	};

	/// The link's settings once these changes are made to `current`.
	fn applied_to(self, current: CurrentLinkSettings) -> LinkSettings {
		LinkSettings {
			expiration: self.expiration.unwrap_or(current.expiration),
			password: self.password,
			downloadable: self.downloadable.unwrap_or(current.downloadable),
		}
	}
}

/// The commands that close the docs' "Accessing Files" section.
pub(crate) const DOCS: &[DocElement] = &[
	DocElement::DocFragment("public-links"),
	DocElement::CommandHelp("list-public-links"),
	DocElement::CommandHelp("public-link"),
];

impl PublicLinksCommands {
	pub(super) async fn execute(self, ctx: CommandContext<'_>) -> Result<CommandResult> {
		let CommandContext {
			ui,
			client,
			working_path,
			..
		} = ctx;
		match self {
			Self::ListPublicLinks => list_public_links(ui, client).await?,
			Self::PublicLink {
				file_or_directory,
				settings,
				show,
				delete,
			} => {
				let action = PublicLinkAction::from_flags(settings, show, delete);
				public_link(ui, client, working_path, &file_or_directory, action).await?;
			}
		}
		Ok(CommandResult::default())
	}
}

/// Where the web app opens public links, as filen-web's `PUBLIC_LINK_BASE_URL` builds them.
const PUBLIC_LINK_BASE_URL: &str = "https://app.filen.io/#/";

/// Shown in place of a public link's URL when the key in it could not be decrypted.
const LINK_URL_UNAVAILABLE: &str = "unavailable (the key could not be decrypted)";

/// Shown in place of a public link's URL when the link was deleted while it was being listed.
const LINK_REMOVED: &str = "none (the link was removed while listing)";

/// Shown in `list-public-links`' expiry column for a link that never expires.
const NEVER_EXPIRES: &str = "never";

/// Shown in `list-public-links`' expiry column for a link that was deleted while it was being
/// listed.
const NO_EXPIRY: &str = "-";

/// A linked item, as `list-public-links` prints it.
struct ListedPublicLink {
	/// The item's absolute path, or its uuid when the path could not be resolved.
	path: String,
	kind: PublicLinkKind,
	item_uuid: String,
	/// `None` when the link was removed between listing the linked items and fetching its status.
	link: Option<ListedLinkStatus>,
}

struct ListedLinkStatus {
	link_uuid: String,
	/// When the link expires, `None` for a link that never expires.
	expires_at: Option<DateTime<Utc>>,
	/// `None` when the key in the link could not be decrypted.
	url: Option<String>,
}

pub(crate) async fn list_public_links(ui: &mut UI, client: &mut LazyClient) -> Result<()> {
	let client = client.get(ui).await?;
	let (dirs, files) = client
		.list_linked(Some(&(|_, _| {})))
		.await
		.context("Failed to list public links")?;

	let mut links = {
		// resolving a path walks up the tree, and `get_item_path` takes the drive lock for each
		// walk; holding one here means those all share it instead of re-acquiring it per item
		let _lock = client.lock_drive().await.context("Failed to lock drive")?;
		let mut links = listed_links(client, &dirs, |dir| {
			NonRootItemType::Dir(Cow::Borrowed(dir))
		})
		.await?;
		links.extend(
			listed_links(client, &files, |file| {
				NonRootItemType::File(Cow::Borrowed(file))
			})
			.await?,
		);
		links
	};
	links.sort_by(|a, b| a.path.cmp(&b.path));

	print_public_links(ui, &links)
}

/// The links of `items`, resolved [`PATH_LOOKUP_CONCURRENCY`] at a time, in the order of `items`.
async fn listed_links<'a, I: LinkableItem + HasUUID>(
	client: &Client,
	items: &'a [I],
	item_type: fn(&'a I) -> NonRootItemType<'a, Normal>,
) -> Result<Vec<ListedPublicLink>> {
	stream::iter(
		items
			.iter()
			.map(|item| listed_link(client, item, item_type(item))),
	)
	.buffered(PATH_LOOKUP_CONCURRENCY)
	.try_collect()
	.await
}

async fn listed_link<I: LinkableItem + HasUUID>(
	client: &Client,
	item: &I,
	item_type: NonRootItemType<'_, Normal>,
) -> Result<ListedPublicLink> {
	let path = item_path(client, &item_type).await;
	let link = item
		.link_status(client)
		.await
		.with_context(|| format!("Failed to get the public link of {path}"))?;
	Ok(ListedPublicLink {
		path,
		kind: I::KIND,
		item_uuid: item.uuid().to_string(),
		link: link.map(|link| ListedLinkStatus {
			link_uuid: I::link_uuid(&link),
			expires_at: I::expires_at(&link),
			url: item.link_url(&link),
		}),
	})
}

/// [`full_path`], falling back to the uuid, as the other listings do, so that one unresolvable
/// item doesn't fail the whole listing.
async fn item_path(client: &Client, item: &NonRootItemType<'_, Normal>) -> String {
	full_path(client, item)
		.await
		.unwrap_or_else(|| item.uuid().to_string())
}

/// Prints the linked items, expected sorted by path: a path, expiry (UTC) and URL column,
/// directories in blue, or with `--json` one array of `{path, type, uuid, linkUuid, expiresAt,
/// url}` objects, where `expiresAt` is an RFC 3339 date in UTC, `null` for a link that never
/// expires, `linkUuid`, `expiresAt` and `url` are `null` for a link that is gone, and `url` is
/// `null` when its key could not be decrypted. The link's uuid, its expiry and its URL are omitted
/// while replay testing, since they differ on every run.
fn print_public_links(ui: &mut UI, links: &[ListedPublicLink]) -> Result<()> {
	if ui.json {
		let json = links
			.iter()
			.map(|link| {
				// the form `json!` gives every other date the listings print
				let expires_at = link
					.link
					.as_ref()
					.and_then(|status| status.expires_at)
					.map(|date| date.to_rfc3339_opts(SecondsFormat::AutoSi, true));
				json!({
					"path": link.path,
					"type": link.kind.noun(),
					"uuid": ui.omit_for_replay_testing(&link.item_uuid),
					"linkUuid": link
						.link
						.as_ref()
						.map(|status| ui.omit_for_replay_testing(&status.link_uuid)),
					"expiresAt": expires_at
						.as_deref()
						.map(|date| ui.omit_for_replay_testing(date)),
					"url": link
						.link
						.as_ref()
						.and_then(|status| status.url.as_deref())
						.map(|url| ui.omit_for_replay_testing(url)),
				})
			})
			.collect::<serde_json::Value>();
		return ui.print_json(json);
	}
	if links.is_empty() {
		ui.print_muted("No public links yet.");
		return Ok(());
	}
	let expiries = links
		.iter()
		.map(|link| {
			link.link
				.as_ref()
				.map_or(Cow::Borrowed(NO_EXPIRY), |status| {
					status
						.expires_at
						.as_ref()
						.map_or(Cow::Borrowed(NEVER_EXPIRES), |date| {
							let date = format_date(date);
							Cow::Owned(ui.omit_for_replay_testing(&date).to_owned())
						})
				})
		})
		.collect::<Vec<_>>();
	let path_width = links
		.iter()
		.map(|link| link.path.width())
		.max()
		.unwrap_or(0);
	let expiry_width = expiries
		.iter()
		.map(|expiry| expiry.width())
		.max()
		.unwrap_or(0);
	for (link, expiry) in links.iter().zip(&expiries) {
		// pad before styling, so the ANSI codes don't count towards the column width
		let padding = " ".repeat(path_width.saturating_sub(link.path.width()));
		let path = style(link.path.as_str());
		let path = match link.kind {
			PublicLinkKind::Directory => path.blue(),
			PublicLinkKind::File => path,
		};
		let url = link.link.as_ref().map_or(LINK_REMOVED, |status| {
			url_or_unavailable(ui, status.url.as_deref())
		});
		ui.print(&format!("{path}{padding}  {expiry:<expiry_width$}  {url}"));
	}
	Ok(())
}

/// Confirmation asked before `public-link --delete` deletes a link.
fn delete_confirmation(path: &str) -> String {
	format!("Delete the public link of {path}?")
}

async fn public_link(
	ui: &mut UI,
	client: &mut LazyClient,
	working_path: &RemotePath,
	file_or_directory_str: &str,
	action: PublicLinkAction,
) -> Result<()> {
	let path = working_path.navigate(file_or_directory_str);
	if action == PublicLinkAction::Delete {
		// before logging in, so that a script learns it needs --yes before anything is looked up
		ui.ensure_can_confirm(&delete_confirmation(&path.0))?;
	}
	let client = client.get(ui).await?;
	match find_existing_item(client, &path).await? {
		NonRootFileType::File(file) => run_link_action(ui, client, &*file, &path.0, action).await,
		NonRootFileType::Dir(dir) => run_link_action(ui, client, &*dir, &path.0, action).await,
		NonRootFileType::Root(_) => Err(UI::failure("Cannot public link the root")),
	}
}

/// Runs `action` on the public link of `item`, found at `path`.
async fn run_link_action<I: LinkableItem>(
	ui: &mut UI,
	client: &Client,
	item: &I,
	path: &str,
	action: PublicLinkAction,
) -> Result<()> {
	let existing_link = link_status(client, item).await?;
	match (action, existing_link) {
		(PublicLinkAction::Interactive, Some(link)) => {
			edit_or_delete_link(ui, client, item, link).await
		}
		(PublicLinkAction::Interactive, None) => create_link(ui, client, item, path).await,
		(PublicLinkAction::Show, Some(link)) => {
			print_link_table(ui, item, &link);
			Ok(())
		}
		(PublicLinkAction::Show, None) => {
			ui.print_muted(&format!("No public link for {path}."));
			Ok(())
		}
		(PublicLinkAction::Delete, Some(link)) => {
			if !ui.prompt_confirm(&delete_confirmation(path), false)? {
				return Ok(());
			}
			delete_link(ui, client, item, link).await
		}
		(PublicLinkAction::Delete, None) => Err(UI::failure(&format!("No public link for {path}"))),
		(PublicLinkAction::Change(changes), Some(mut link)) => {
			let settings = changes.applied_to(I::current_settings(&link));
			I::apply_settings(&mut link, settings);
			save_link(ui, client, item, &link).await
		}
		(PublicLinkAction::Change(changes), None) => {
			let link = create_new_link(ui, client, item).await?;
			save_new_link(
				ui,
				client,
				item,
				path,
				link,
				changes.applied_to(NEW_LINK_SETTINGS),
			)
			.await
		}
	}
}

/// The item's public link, or `None` when it has none.
async fn link_status<I: LinkableItem>(client: &Client, item: &I) -> Result<Option<I::Link>> {
	match item.link_status(client).await {
		Ok(link) => Ok(link),
		Err(e)
			if e.kind() == ErrorKind::Server
				&& e.server_code()
					.is_some_and(|code| code == "subscription_needed") =>
		{
			Err(UI::failure(
				"Public links are not available for your account. Please upgrade your subscription to access this feature.",
			))
		}
		Err(e) => Err(e).context("Failed to get public link"),
	}
}

async fn edit_or_delete_link<I: LinkableItem>(
	ui: &mut UI,
	client: &Client,
	item: &I,
	mut link: I::Link,
) -> Result<()> {
	print_link_table(ui, item, &link);
	match prompt_link_operation(ui)? {
		None | Some(PublicLinkOperation::Cancel) => Ok(()),
		Some(PublicLinkOperation::Delete) => delete_link(ui, client, item, link).await,
		Some(PublicLinkOperation::Edit) => {
			let Some(settings) = prompt_link_settings(ui, I::current_settings(&link))? else {
				return Ok(());
			};
			I::apply_settings(&mut link, settings);
			save_link(ui, client, item, &link).await
		}
	}
}

/// What to do with an existing link, or `None` when there is no terminal to ask on, in which case
/// the link is only shown.
fn prompt_link_operation(ui: &mut UI) -> Result<Option<PublicLinkOperation>> {
	if ui.ensure_can_prompt(OPERATION_PROMPT).is_err() {
		return Ok(None);
	}
	ui.print("");
	ui.prompt_choice(
		OPERATION_PROMPT,
		&OPERATION_CHOICES,
		PublicLinkOperation::Cancel,
	)
}

/// Asks for the new link's settings before creating it, so that cancelling a prompt creates
/// nothing.
async fn create_link<I: LinkableItem>(
	ui: &mut UI,
	client: &Client,
	item: &I,
	path: &str,
) -> Result<()> {
	let Some(settings) = new_link_settings(ui, path)? else {
		return Ok(());
	};
	let link = create_new_link(ui, client, item).await?;
	save_new_link(ui, client, item, path, link, settings).await
}

/// Saves `settings` to `link`, which was just created for `item`, found at `path`, with the default
/// settings. When that fails and `settings` protect the item more than the defaults do, the link
/// is deleted, so that a failed save does not leave the item public without the protection asked
/// for.
async fn save_new_link<I: LinkableItem>(
	ui: &mut UI,
	client: &Client,
	item: &I,
	path: &str,
	mut link: I::Link,
	settings: LinkSettings,
) -> Result<()> {
	let protections = protections_beyond_defaults(&settings);
	I::apply_settings(&mut link, settings);
	let Err(save_error) = save_link(ui, client, item, &link).await else {
		return Ok(());
	};
	let unsaved = UnsavedNewLink {
		noun: I::KIND.noun(),
		path,
		url: item.link_url(&link),
		protections,
	};
	Err(unsaved_new_link_error(save_error, &unsaved, delete_link(ui, client, item, link)).await)
}

/// What a new link's settings give it that its default settings do not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Protection {
	Password,
	Expiry,
	NoDownloads,
}

impl Protection {
	/// The protection as the messages about a new link name it.
	fn label(self) -> &'static str {
		match self {
			Protection::Password => "the password",
			Protection::Expiry => "the expiry",
			Protection::NoDownloads => "the download restriction",
		}
	}
}

/// The protections `settings` give a new link beyond [`NEW_LINK_SETTINGS`], empty when the link is
/// no more open with the default settings than with `settings`.
fn protections_beyond_defaults(settings: &LinkSettings) -> Vec<Protection> {
	let password = match settings.password {
		PasswordChange::Set(_) => Some(Protection::Password),
		// a new link has no password to keep or remove
		PasswordChange::Keep | PasswordChange::Clear => None,
	};
	let expiry =
		(settings.expiration != NEW_LINK_SETTINGS.expiration).then_some(Protection::Expiry);
	let no_downloads = (NEW_LINK_SETTINGS.downloadable && !settings.downloadable)
		.then_some(Protection::NoDownloads);
	[password, expiry, no_downloads]
		.into_iter()
		.flatten()
		.collect()
}

/// A link created for an item whose settings could not be saved.
struct UnsavedNewLink<'a> {
	/// What the item is, as [`PublicLinkKind::noun`] names it.
	noun: &'static str,
	path: &'a str,
	/// `None` when the key in the link could not be decrypted.
	url: Option<String>,
	/// What the settings that could not be saved would have protected the item with.
	protections: Vec<Protection>,
}

/// The error for a new link whose settings failed to save with `save_error`. The link is left as
/// it is when its default settings are no more open than those asked for; otherwise `delete`
/// deletes it, and only if that fails too is the item left public.
async fn unsaved_new_link_error(
	save_error: anyhow::Error,
	unsaved: &UnsavedNewLink<'_>,
	delete: impl Future<Output = Result<()>>,
) -> anyhow::Error {
	if unsaved.protections.is_empty() {
		return save_error.context(new_link_not_saved(unsaved));
	}
	match delete.await {
		Ok(()) => save_error.context(new_link_deleted(unsaved)),
		Err(delete_error) => save_error.context(new_link_left_open(unsaved, &delete_error)),
	}
}

/// The link of `unsaved`, with its URL when known.
fn unsaved_link_label(unsaved: &UnsavedNewLink<'_>) -> String {
	let UnsavedNewLink {
		noun, path, url, ..
	} = unsaved;
	match url {
		Some(url) => format!("the public link of the {noun} {path} ({url})"),
		None => format!("the public link of the {noun} {path}"),
	}
}

/// The commands that change or delete the link of the item at `path`, quoted for the command line.
fn change_or_delete_commands(path: &str) -> String {
	let path = quote_for_command_line(path);
	format!(
		"change them with `filen public-link {path}`, or delete it with `filen public-link --delete {path}`"
	)
}

/// Context for a new link that was left with the default settings, which protect the item no less
/// than those that could not be saved.
fn new_link_not_saved(unsaved: &UnsavedNewLink<'_>) -> String {
	format!(
		"Created {} with the default settings, but saving the settings failed: {}",
		unsaved_link_label(unsaved),
		change_or_delete_commands(unsaved.path)
	)
}

/// Context for a new link that was deleted after its settings could not be saved. It names no
/// command to create the link again: the one that ran may have given the settings as options.
fn new_link_deleted(unsaved: &UnsavedNewLink<'_>) -> String {
	let UnsavedNewLink { noun, path, .. } = unsaved;
	format!(
		"Saving the settings of the new public link of the {noun} {path} failed, so the link was \
		 deleted and the {noun} is not public; run the command again to create it"
	)
}

/// Context for a new link that could neither be given its settings nor deleted, failing with
/// `delete_error`, and so leaves its item public without the protection asked for.
fn new_link_left_open(unsaved: &UnsavedNewLink<'_>, delete_error: &anyhow::Error) -> String {
	let UnsavedNewLink {
		noun,
		path,
		protections,
		..
	} = unsaved;
	format!(
		"Created {}, but saving its settings failed, and deleting the link failed too \
		 ({delete_error:#}): the {noun} {path} is public without {} asked for; {}",
		unsaved_link_label(unsaved),
		protection_list(protections),
		change_or_delete_commands(path)
	)
}

/// `protections` named in a sentence: "the password, the expiry and the download restriction".
fn protection_list(protections: &[Protection]) -> String {
	match protections.split_last() {
		None => String::new(),
		Some((last, [])) => last.label().to_owned(),
		Some((last, rest)) => {
			let rest = rest
				.iter()
				.map(|protection| protection.label())
				.collect::<Vec<_>>();
			format!("{} and {}", rest.join(", "), last.label())
		}
	}
}

/// The settings for a new link for the item at `path`, or `None` when a prompt was cancelled.
/// Without a terminal to ask on it fails with a usage error, as a command that looks like it only
/// shows the link must not make the item public unasked.
fn new_link_settings(ui: &mut UI, path: &str) -> Result<Option<LinkSettings>> {
	ui.ensure_can_prompt(EXPIRATION_PROMPT).with_context(|| {
		format!(
			"No public link for {path}, and creating one asks for its settings; give a setting \
			 option, such as --expires never, to create it without prompting"
		)
	})?;
	prompt_link_settings(ui, NEW_LINK_SETTINGS)
}

async fn create_new_link<I: LinkableItem>(
	ui: &mut UI,
	client: &Client,
	item: &I,
) -> Result<I::Link> {
	ui.print(&format!(
		"Creating a new public link for the {}...",
		I::KIND.noun()
	));
	item.create_link(client)
		.await
		.context("Failed to create public link")
}

async fn delete_link<I: LinkableItem>(
	ui: &mut UI,
	client: &Client,
	item: &I,
	link: I::Link,
) -> Result<()> {
	item.remove_link(client, link)
		.await
		.context("Failed to delete public link")?;
	ui.print_success("Public link deleted.");
	Ok(())
}

async fn save_link<I: LinkableItem>(
	ui: &mut UI,
	client: &Client,
	item: &I,
	link: &I::Link,
) -> Result<()> {
	item.update_link(client, link)
		.await
		.context("Failed to update public link")?;
	ui.print_success("Public link saved.");
	print_public_link_url(ui, item.link_url(link).as_deref());
	Ok(())
}

/// A file or directory, as `public-link` shows, creates, edits and deletes its public link.
trait LinkableItem: HasName {
	type Link;
	const KIND: PublicLinkKind;

	async fn link_status(&self, client: &Client) -> Result<Option<Self::Link>, Error>;
	async fn create_link(&self, client: &Client) -> Result<Self::Link, Error>;
	async fn update_link(&self, client: &Client, link: &Self::Link) -> Result<(), Error>;
	async fn remove_link(&self, client: &Client, link: Self::Link) -> Result<(), Error>;
	fn link_url(&self, link: &Self::Link) -> Option<String>;
	fn link_uuid(link: &Self::Link) -> String;
	fn expires_at(link: &Self::Link) -> Option<DateTime<Utc>>;
	fn current_settings(link: &Self::Link) -> CurrentLinkSettings;
	fn apply_settings(link: &mut Self::Link, settings: LinkSettings);
}

impl LinkableItem for RemoteFile {
	type Link = FilePublicLink;
	const KIND: PublicLinkKind = PublicLinkKind::File;

	async fn link_status(&self, client: &Client) -> Result<Option<FilePublicLink>, Error> {
		client.get_file_link_status(self).await
	}

	async fn create_link(&self, client: &Client) -> Result<FilePublicLink, Error> {
		client.public_link_file(self).await
	}

	async fn update_link(&self, client: &Client, link: &FilePublicLink) -> Result<(), Error> {
		client.update_file_link(self, link).await
	}

	async fn remove_link(&self, client: &Client, link: FilePublicLink) -> Result<(), Error> {
		client.remove_file_link(self, link).await
	}

	fn link_url(&self, link: &FilePublicLink) -> Option<String> {
		file_public_link_url(self, link)
	}

	fn link_uuid(link: &FilePublicLink) -> String {
		link.uuid().to_string()
	}

	fn expires_at(link: &FilePublicLink) -> Option<DateTime<Utc>> {
		link.expires_at()
	}

	fn current_settings(link: &FilePublicLink) -> CurrentLinkSettings {
		CurrentLinkSettings {
			expiration: link.expiration(),
			has_password: has_password(link.password()),
			downloadable: link.downloadable(),
		}
	}

	fn apply_settings(link: &mut FilePublicLink, settings: LinkSettings) {
		link.set_expiration(settings.expiration);
		match settings.password {
			// a password the server reports is resent as its hash, so keeping it needs no call
			PasswordChange::Keep => {}
			PasswordChange::Clear => link.clear_password(),
			PasswordChange::Set(password) => link.set_password(password),
		}
		link.set_downloadable(settings.downloadable);
	}
}

impl LinkableItem for RemoteDirectory {
	type Link = DirPublicLinkRW;
	const KIND: PublicLinkKind = PublicLinkKind::Directory;

	async fn link_status(&self, client: &Client) -> Result<Option<DirPublicLinkRW>, Error> {
		client.get_dir_link_rw(self).await
	}

	async fn create_link(&self, client: &Client) -> Result<DirPublicLinkRW, Error> {
		client.public_link_dir(self, Some(&(|_, _| {}))).await
	}

	async fn update_link(&self, client: &Client, link: &DirPublicLinkRW) -> Result<(), Error> {
		client.update_dir_link(self, link).await
	}

	async fn remove_link(&self, client: &Client, _link: DirPublicLinkRW) -> Result<(), Error> {
		// the directory's uuid, not the link's, identifies a directory link to remove
		client.remove_dir_link(self).await
	}

	fn link_url(&self, link: &DirPublicLinkRW) -> Option<String> {
		dir_public_link_url(link)
	}

	fn link_uuid(link: &DirPublicLinkRW) -> String {
		link.uuid().to_string()
	}

	fn expires_at(link: &DirPublicLinkRW) -> Option<DateTime<Utc>> {
		link.expires_at()
	}

	fn current_settings(link: &DirPublicLinkRW) -> CurrentLinkSettings {
		CurrentLinkSettings {
			expiration: link.expiration(),
			has_password: has_password(link.password()),
			downloadable: link.download_enabled(),
		}
	}

	fn apply_settings(link: &mut DirPublicLinkRW, settings: LinkSettings) {
		link.set_expiration(settings.expiration);
		match settings.password {
			// a password the server reports is resent as its hash, so keeping it needs no call
			PasswordChange::Keep => {}
			PasswordChange::Clear => link.clear_password(),
			PasswordChange::Set(password) => link.set_password(password),
		}
		link.set_enable_download(settings.downloadable);
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PublicLinkOperation {
	Cancel,
	Edit,
	Delete,
}

const OPERATION_PROMPT: &str = "This is an existing public link.";

const OPERATION_CHOICES: [(&str, PublicLinkOperation); 3] = [
	("Cancel", PublicLinkOperation::Cancel),
	("Edit", PublicLinkOperation::Edit),
	("Delete", PublicLinkOperation::Delete),
];

const EXPIRATION_PROMPT: &str = "Expires after:";

/// The expirations a link can have, in the order the prompt offers them.
const EXPIRATIONS: [PublicLinkExpiration; 8] = [
	PublicLinkExpiration::Never,
	PublicLinkExpiration::OneHour,
	PublicLinkExpiration::SixHours,
	PublicLinkExpiration::OneDay,
	PublicLinkExpiration::ThreeDays,
	PublicLinkExpiration::OneWeek,
	PublicLinkExpiration::TwoWeeks,
	PublicLinkExpiration::ThirtyDays,
];

/// How the link tables and the expiration prompt name an expiration.
fn expiration_label(expiration: PublicLinkExpiration) -> &'static str {
	match expiration {
		PublicLinkExpiration::Never => "Never",
		PublicLinkExpiration::OneHour => "1 hour",
		PublicLinkExpiration::SixHours => "6 hours",
		PublicLinkExpiration::OneDay => "1 day",
		PublicLinkExpiration::ThreeDays => "3 days",
		PublicLinkExpiration::OneWeek => "1 week",
		PublicLinkExpiration::TwoWeeks => "2 weeks",
		PublicLinkExpiration::ThirtyDays => "30 days",
	}
}

/// A link's settings, as the link table shows them and the prompts start from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CurrentLinkSettings {
	expiration: PublicLinkExpiration,
	has_password: bool,
	downloadable: bool,
}

/// The settings a new link is created with, which the SDK gives file and directory links alike.
const NEW_LINK_SETTINGS: CurrentLinkSettings = CurrentLinkSettings {
	expiration: PublicLinkExpiration::Never,
	has_password: false,
	downloadable: true,
};

/// The settings chosen in the prompts, to apply to a link.
#[derive(Debug, PartialEq, Eq)]
struct LinkSettings {
	expiration: PublicLinkExpiration,
	password: PasswordChange,
	downloadable: bool,
}

/// What the password prompt's answer does to the link's password.
#[derive(Debug, PartialEq, Eq)]
enum PasswordChange {
	Keep,
	Clear,
	Set(String),
}

/// The password prompt for a link with a password, which an empty answer keeps.
const CHANGE_PASSWORD_PROMPT: &str = "Password (empty keeps the current one, '-' removes it):";

/// The password prompt for a link without a password, which an empty answer leaves without one.
const NEW_PASSWORD_PROMPT: &str = "Password (leave empty for no password):";

impl PasswordChange {
	/// The change the answer to the password prompt asks for. An empty answer keeps the link's
	/// password, or its lack of one, and `-` removes a password.
	fn from_answer(answer: String, has_password: bool) -> Self {
		match answer.as_str() {
			"" => PasswordChange::Keep,
			"-" if has_password => PasswordChange::Clear,
			_ => PasswordChange::Set(answer),
		}
	}
}

/// Asks for a link's settings, starting from `current`, or `None` when a prompt was cancelled.
fn prompt_link_settings(ui: &mut UI, current: CurrentLinkSettings) -> Result<Option<LinkSettings>> {
	let expiration_choices =
		EXPIRATIONS.map(|expiration| (expiration_label(expiration), expiration));
	let Some(expiration) =
		ui.prompt_choice(EXPIRATION_PROMPT, &expiration_choices, current.expiration)?
	else {
		return Ok(None);
	};
	let password_prompt = if current.has_password {
		CHANGE_PASSWORD_PROMPT
	} else {
		NEW_PASSWORD_PROMPT
	};
	let password = PasswordChange::from_answer(ui.prompt(password_prompt)?, current.has_password);
	let downloadable = prompt_downloadable(ui, current.downloadable)?;
	Ok(Some(LinkSettings {
		expiration,
		password,
		downloadable,
	}))
}

/// The question whether the link's item can be downloaded.
const DOWNLOAD_PROMPT: &str = "Enable download?";

/// Asks whether the link's item can be downloaded, for a link that is downloadable or not as
/// `downloadable` says. `--yes` answers yes, as it answers every yes/no question, and says so on
/// stderr when that turns downloads on for a link that had them off: nothing else the command
/// prints shows that the link now gives its item away.
fn prompt_downloadable(ui: &mut UI, downloadable: bool) -> Result<bool> {
	let answer = ui.prompt_confirm(DOWNLOAD_PROMPT, downloadable)?;
	if answer && !downloadable && ui.assumes_yes() {
		ui.print_muted_to_stderr(&format!("{DOWNLOAD_PROMPT} Yes (--yes)"));
	}
	Ok(answer)
}

/// Prints the item's name, then its link's settings and URL.
fn print_link_table<I: LinkableItem>(ui: &mut UI, item: &I, link: &I::Link) {
	print_link_rows(
		ui,
		&LinkTable {
			kind: I::KIND,
			name: item.name().unwrap_or_default(),
			settings: I::current_settings(link),
			expires_at: I::expires_at(link),
			link_uuid: &I::link_uuid(link),
			url: item.link_url(link).as_deref(),
		},
	);
}

/// What the link table shows about a link.
struct LinkTable<'a> {
	kind: PublicLinkKind,
	name: &'a str,
	settings: CurrentLinkSettings,
	/// When the link expires, `None` for a link that never expires.
	expires_at: Option<DateTime<Utc>>,
	link_uuid: &'a str,
	/// `None` when the key in the link could not be decrypted.
	url: Option<&'a str>,
}

/// Prints the link table, with when the link expires (UTC) for one that does. The expiry, the
/// link's uuid and its URL are omitted while replay testing, since they differ on every run.
fn print_link_rows(ui: &mut UI, table: &LinkTable<'_>) {
	let url = url_or_unavailable(ui, table.url);
	let expires_at = table.expires_at.as_ref().map(format_date);
	let mut rows = vec![
		(table.kind.label(), table.name),
		("Expiration", expiration_label(table.settings.expiration)),
	];
	if let Some(expires_at) = &expires_at {
		rows.push(("Expires At", ui.omit_for_replay_testing(expires_at)));
	}
	rows.extend([
		("Password", yes_no(table.settings.has_password)),
		("Download Enabled", yes_no(table.settings.downloadable)),
		("Link UUID", ui.omit_for_replay_testing(table.link_uuid)),
		("URL", url),
	]);
	ui.print_key_value_table(&rows);
}

/// Whether a link has a password, from the server's hash or one set in this session.
fn has_password(password: &PasswordState) -> bool {
	match password {
		PasswordState::Known(_) | PasswordState::Hashed(_) => true,
		PasswordState::None => false,
	}
}

/// A yes/no setting, as the link tables show it.
fn yes_no(value: bool) -> &'static str {
	if value { "Yes" } else { "No" }
}

/// What a public link opens in the web app.
enum PublicLinkKind {
	File,
	Directory,
}

impl PublicLinkKind {
	/// The web app's route for this kind: `d` (download) for a file, `f` (folder) for a directory.
	fn route(&self) -> &'static str {
		match self {
			PublicLinkKind::File => "d",
			PublicLinkKind::Directory => "f",
		}
	}

	/// The item type as the link table labels the item's name.
	fn label(&self) -> &'static str {
		match self {
			PublicLinkKind::File => "File",
			PublicLinkKind::Directory => "Directory",
		}
	}

	/// The item type as messages and the `--json` listings name it.
	fn noun(&self) -> &'static str {
		match self {
			PublicLinkKind::File => "file",
			PublicLinkKind::Directory => "directory",
		}
	}
}

/// The URL of a public link, as the web app copies it: its route, the link's uuid, then an
/// escaped `#` (`%23`) and the key that decrypts the item, hex-encoded from its UTF-8 bytes. The
/// web app decodes a hex key back and takes any other key as it is, so the raw form of a key that
/// is itself hex (a v3 key) would be misread; the hex form never is.
fn public_link_url(kind: PublicLinkKind, link_uuid: impl Display, key: &str) -> String {
	let mut url = format!("{PUBLIC_LINK_BASE_URL}{}/{link_uuid}%23", kind.route());
	for byte in key.bytes() {
		write!(url, "{byte:02x}").expect("writing to a String cannot fail (should be impossible)");
	}
	url
}

/// The URL of a file's public link, whose key is the file's own, or `None` when the file's
/// metadata, which holds that key, could not be decrypted.
fn file_public_link_url(file: &RemoteFile, link: &FilePublicLink) -> Option<String> {
	file.key()
		.map(|key| public_link_url(PublicLinkKind::File, link.uuid(), key.to_str().as_ref()))
}

/// The URL of a directory's public link, or `None` when its link key could not be decrypted.
fn dir_public_link_url(link: &DirPublicLinkRW) -> Option<String> {
	link.key_string()
		.map(|key| public_link_url(PublicLinkKind::Directory, link.uuid(), &key))
}

/// The URL to print for a public link: `url`, the placeholder while replay testing (the link's
/// uuid and key differ on every run), or a note when it could not be built.
fn url_or_unavailable<'a>(ui: &UI, url: Option<&'a str>) -> &'a str {
	url.map_or(LINK_URL_UNAVAILABLE, |url| ui.omit_for_replay_testing(url))
}

fn print_public_link_url(ui: &mut UI, url: Option<&str>) {
	let url = url_or_unavailable(ui, url);
	ui.print_key_value_table(&[("URL", url)]);
}

#[cfg(test)]
mod tests {
	use std::borrow::Cow;

	use chrono::TimeZone as _;
	use filen_types::crypto::LinkHashedPassword;

	use super::*;
	use crate::{commands::Commands, error::exit_code, parse_args, ui::test_support::Printed};

	/// The action `filen public-link /item <args>` asks for.
	fn parsed_action(args: &[&str]) -> Result<PublicLinkAction, clap::Error> {
		let command = ["filen", "public-link", "/item"]
			.into_iter()
			.chain(args.iter().copied());
		let cli_args = parse_args(command)?.args;
		let Some(Commands::PublicLinks(PublicLinksCommands::PublicLink {
			settings,
			show,
			delete,
			..
		})) = cli_args.command
		else {
			panic!("parsed {:?}", cli_args.command);
		};
		Ok(PublicLinkAction::from_flags(settings, show, delete))
	}

	fn parse_error(args: &[&str]) -> clap::error::ErrorKind {
		match parsed_action(args) {
			Ok(action) => panic!("parsed {action:?}"),
			Err(e) => e.kind(),
		}
	}

	#[test]
	fn public_link_without_options_is_interactive() {
		assert_eq!(parsed_action(&[]).unwrap(), PublicLinkAction::Interactive);
	}

	#[test]
	fn public_link_show_only_shows() {
		assert_eq!(parsed_action(&["--show"]).unwrap(), PublicLinkAction::Show);
	}

	#[test]
	fn public_link_delete_takes_the_global_yes_after_it() {
		let cli_args = parse_args(["filen", "public-link", "/item", "--delete", "--yes"])
			.unwrap()
			.args;
		assert!(cli_args.yes);
		assert_eq!(
			parsed_action(&["--delete"]).unwrap(),
			PublicLinkAction::Delete
		);
	}

	#[test]
	fn public_link_setting_options_change_only_what_they_name() {
		assert_eq!(
			parsed_action(&["--expires", "1d", "--no-download", "--password", "x"]).unwrap(),
			PublicLinkAction::Change(LinkChanges {
				expiration: Some(PublicLinkExpiration::OneDay),
				password: PasswordChange::Set("x".to_owned()),
				downloadable: Some(false),
			})
		);
		assert_eq!(
			parsed_action(&["--no-password"]).unwrap(),
			PublicLinkAction::Change(LinkChanges {
				expiration: None,
				password: PasswordChange::Clear,
				downloadable: None,
			})
		);
		assert_eq!(
			parsed_action(&["--download"]).unwrap(),
			PublicLinkAction::Change(LinkChanges {
				expiration: None,
				password: PasswordChange::Keep,
				downloadable: Some(true),
			})
		);
	}

	#[test]
	fn public_link_rejects_an_empty_password() {
		let err = match parsed_action(&["--password", ""]) {
			Ok(action) => panic!("parsed {action:?}"),
			Err(e) => e,
		};
		assert_eq!(err.kind(), clap::error::ErrorKind::ValueValidation);
		assert!(err.to_string().contains("use --no-password"), "{err}");
	}

	#[test]
	fn public_link_expires_takes_the_api_tokens() {
		let tokens = ["never", "1h", "6h", "1d", "3d", "7d", "14d", "30d"];
		let parsed = tokens.map(
			|token| match parsed_action(&["--expires", token]).unwrap() {
				PublicLinkAction::Change(changes) => changes.expiration,
				action => panic!("parsed {action:?}"),
			},
		);
		assert_eq!(parsed, EXPIRATIONS.map(Some));
	}

	#[test]
	fn public_link_expires_rejects_another_duration() {
		assert_eq!(
			parse_error(&["--expires", "2d"]),
			clap::error::ErrorKind::InvalidValue
		);
	}

	#[test]
	fn public_link_options_that_contradict_each_other_are_rejected() {
		for args in [
			&["--password", "x", "--no-password"][..],
			&["--download", "--no-download"],
			&["--show", "--delete"],
			&["--show", "--expires", "1d"],
			&["--delete", "--no-password"],
			&["--delete", "--download"],
		] {
			assert_eq!(
				parse_error(args),
				clap::error::ErrorKind::ArgumentConflict,
				"{args:?}"
			);
		}
	}

	#[test]
	fn link_changes_keep_the_settings_they_do_not_name() {
		let current = CurrentLinkSettings {
			expiration: PublicLinkExpiration::OneWeek,
			has_password: true,
			downloadable: false,
		};
		let changes = LinkChanges {
			expiration: Some(PublicLinkExpiration::OneHour),
			password: PasswordChange::Keep,
			downloadable: None,
		};
		assert_eq!(
			changes.applied_to(current),
			LinkSettings {
				expiration: PublicLinkExpiration::OneHour,
				password: PasswordChange::Keep,
				downloadable: false,
			}
		);
	}

	fn listed(
		path: &str,
		kind: PublicLinkKind,
		link: Option<ListedLinkStatus>,
	) -> ListedPublicLink {
		ListedPublicLink {
			path: path.to_owned(),
			kind,
			item_uuid: "00000000-0000-0000-0000-000000000001".to_owned(),
			link,
		}
	}

	fn status(url: Option<&str>) -> Option<ListedLinkStatus> {
		Some(ListedLinkStatus {
			link_uuid: LINK_UUID.to_owned(),
			expires_at: None,
			url: url.map(str::to_owned),
		})
	}

	/// The status of a link that expires at 2026-03-09 10:20:30 UTC.
	fn expiring_status(url: &str) -> Option<ListedLinkStatus> {
		Some(ListedLinkStatus {
			expires_at: Some(Utc.with_ymd_and_hms(2026, 3, 9, 10, 20, 30).unwrap()),
			..status(Some(url)).unwrap()
		})
	}

	fn printed_links(json: bool, links: &[ListedPublicLink]) -> String {
		let printed = Printed::default();
		let mut ui = printed.ui();
		ui.json = json;
		print_public_links(&mut ui, links).unwrap();
		console::strip_ansi_codes(&printed.lines()).into_owned()
	}

	fn printed_links_while_replay_testing(json: bool, links: &[ListedPublicLink]) -> String {
		let printed = Printed::default();
		let mut ui = printed.ui();
		ui.initialize(false, json, None, true, true);
		print_public_links(&mut ui, links).unwrap();
		console::strip_ansi_codes(&printed.lines()).into_owned()
	}

	#[test]
	fn an_empty_link_listing_says_there_are_no_links_yet() {
		assert_eq!(printed_links(false, &[]), "No public links yet.");
	}

	#[test]
	fn an_empty_link_listing_is_an_empty_json_array() {
		assert_eq!(printed_links(true, &[]), "[]");
	}

	#[test]
	fn listed_links_print_their_path_expiry_and_url_in_aligned_columns() {
		let links = [
			listed(
				"/docs",
				PublicLinkKind::Directory,
				status(Some("https://dir")),
			),
			listed(
				"/docs/report.pdf",
				PublicLinkKind::File,
				status(Some("https://file")),
			),
			listed("/gone.txt", PublicLinkKind::File, None),
			listed("/locked", PublicLinkKind::Directory, status(None)),
		];
		assert_eq!(
			printed_links(false, &links),
			"/docs             never  https://dir\n\
			 /docs/report.pdf  never  https://file\n\
			 /gone.txt         -      none (the link was removed while listing)\n\
			 /locked           never  unavailable (the key could not be decrypted)"
		);
	}

	#[test]
	fn a_listed_link_that_expires_prints_when_in_utc() {
		let links = [
			listed(
				"/docs",
				PublicLinkKind::Directory,
				status(Some("https://dir")),
			),
			listed("/gone.txt", PublicLinkKind::File, None),
			listed(
				"/report.pdf",
				PublicLinkKind::File,
				expiring_status("https://file"),
			),
		];
		assert_eq!(
			printed_links(false, &links),
			"/docs        never                      https://dir\n\
			 /gone.txt    -                          none (the link was removed while listing)\n\
			 /report.pdf  2026-03-09 10:20:30 (UTC)  https://file"
		);
	}

	#[test]
	fn listed_links_omit_the_expiry_while_replay_testing_but_not_never() {
		let links = [
			listed(
				"/docs",
				PublicLinkKind::Directory,
				status(Some("https://dir")),
			),
			listed(
				"/report.pdf",
				PublicLinkKind::File,
				expiring_status("https://file"),
			),
		];
		assert_eq!(
			printed_links_while_replay_testing(false, &links),
			"/docs        never   [omit]\n\
			 /report.pdf  [omit]  [omit]"
		);
	}

	#[test]
	fn a_listed_link_that_expires_has_an_rfc_3339_expiry_in_json() {
		let links = [listed(
			"/report.pdf",
			PublicLinkKind::File,
			expiring_status("https://file"),
		)];
		let printed: serde_json::Value =
			serde_json::from_str(&printed_links(true, &links)).unwrap();
		assert_eq!(printed[0]["expiresAt"], "2026-03-09T10:20:30Z");
	}

	#[test]
	fn listed_links_as_json_omit_the_expiry_while_replay_testing() {
		let links = [listed(
			"/report.pdf",
			PublicLinkKind::File,
			expiring_status("https://file"),
		)];
		let printed: serde_json::Value =
			serde_json::from_str(&printed_links_while_replay_testing(true, &links)).unwrap();
		assert_eq!(printed[0]["expiresAt"], "[omit]");
	}

	#[test]
	fn listed_links_as_json_have_null_link_fields_for_a_removed_link() {
		let links = [
			listed(
				"/docs",
				PublicLinkKind::Directory,
				status(Some("https://dir")),
			),
			listed("/gone.txt", PublicLinkKind::File, None),
			listed("/locked", PublicLinkKind::Directory, status(None)),
		];
		let printed: serde_json::Value =
			serde_json::from_str(&printed_links(true, &links)).unwrap();
		assert_eq!(
			printed,
			json!([
				{
					"path": "/docs",
					"type": "directory",
					"uuid": "00000000-0000-0000-0000-000000000001",
					"linkUuid": LINK_UUID,
					"expiresAt": null,
					"url": "https://dir",
				},
				{
					"path": "/gone.txt",
					"type": "file",
					"uuid": "00000000-0000-0000-0000-000000000001",
					"linkUuid": null,
					"expiresAt": null,
					"url": null,
				},
				{
					"path": "/locked",
					"type": "directory",
					"uuid": "00000000-0000-0000-0000-000000000001",
					"linkUuid": LINK_UUID,
					"expiresAt": null,
					"url": null,
				},
			])
		);
	}

	#[test]
	fn an_empty_password_answer_keeps_an_existing_password() {
		assert_eq!(
			PasswordChange::from_answer(String::new(), true),
			PasswordChange::Keep
		);
	}

	#[test]
	fn an_empty_password_answer_leaves_a_link_without_a_password() {
		assert_eq!(
			PasswordChange::from_answer(String::new(), false),
			PasswordChange::Keep
		);
	}

	#[test]
	fn a_dash_removes_an_existing_password() {
		assert_eq!(
			PasswordChange::from_answer("-".to_owned(), true),
			PasswordChange::Clear
		);
	}

	#[test]
	fn a_dash_is_the_password_of_a_link_without_one() {
		assert_eq!(
			PasswordChange::from_answer("-".to_owned(), false),
			PasswordChange::Set("-".to_owned())
		);
	}

	#[test]
	fn any_other_password_answer_sets_that_password() {
		assert_eq!(
			PasswordChange::from_answer("hunter2".to_owned(), true),
			PasswordChange::Set("hunter2".to_owned())
		);
	}

	#[test]
	fn the_link_table_shows_the_settings_uuid_and_url_of_a_link() {
		let printed = Printed::default();
		let mut ui = printed.ui();
		print_link_rows(
			&mut ui,
			&LinkTable {
				kind: PublicLinkKind::File,
				name: "report.pdf",
				settings: CurrentLinkSettings {
					expiration: PublicLinkExpiration::OneWeek,
					has_password: true,
					downloadable: false,
				},
				expires_at: None,
				link_uuid: LINK_UUID,
				url: Some("https://file"),
			},
		);
		assert_eq!(
			console::strip_ansi_codes(&printed.lines()),
			[
				"            File report.pdf",
				"      Expiration 1 week",
				"        Password Yes",
				"Download Enabled No",
				"       Link UUID 0f8fad5b-d9cb-469f-a165-70867728950e",
				"             URL https://file",
			]
			.join("\n")
		);
	}

	#[test]
	fn the_link_table_shows_when_a_link_expires_in_utc() {
		let printed = Printed::default();
		let mut ui = printed.ui();
		print_link_rows(
			&mut ui,
			&LinkTable {
				kind: PublicLinkKind::Directory,
				name: "docs",
				settings: CurrentLinkSettings {
					expiration: PublicLinkExpiration::OneDay,
					has_password: false,
					downloadable: true,
				},
				expires_at: DateTime::from_timestamp(1_700_000_000, 0),
				link_uuid: LINK_UUID,
				url: Some("https://dir"),
			},
		);
		assert_eq!(
			console::strip_ansi_codes(&printed.lines()),
			[
				"       Directory docs",
				"      Expiration 1 day",
				"      Expires At 2023-11-14 22:13:20 (UTC)",
				"        Password No",
				"Download Enabled Yes",
				"       Link UUID 0f8fad5b-d9cb-469f-a165-70867728950e",
				"             URL https://dir",
			]
			.join("\n")
		);
	}

	#[test]
	fn a_link_has_a_password_the_server_reports() {
		let hash = LinkHashedPassword(Cow::Borrowed("0123456789abcdef"));
		assert!(has_password(&PasswordState::Hashed(hash)));
	}

	#[test]
	fn a_link_has_a_password_set_in_this_session() {
		assert!(has_password(&PasswordState::Known("hunter2".to_owned())));
	}

	#[test]
	fn a_link_without_a_password_has_none() {
		assert!(!has_password(&PasswordState::None));
	}

	#[test]
	fn a_new_link_without_a_terminal_is_a_usage_error_that_creates_nothing() {
		let printed = Printed::default();
		let mut ui = printed.ui();
		ui.disable_prompts();
		let err = new_link_settings(&mut ui, "/a.txt").unwrap_err();
		assert_eq!(exit_code(&err), 2);
		assert_eq!(
			format!("{err:#}"),
			"No public link for /a.txt, and creating one asks for its settings; give a setting \
			 option, such as --expires never, to create it without prompting: Cannot ask \
			 \"Expires after:\": --no-input was given."
		);
		assert_eq!(printed.lines_or_empty(), "");
	}

	#[test]
	fn an_existing_link_without_a_terminal_is_only_shown() {
		let printed = Printed::default();
		let mut ui = printed.ui();
		ui.disable_prompts();
		assert_eq!(prompt_link_operation(&mut ui).unwrap(), None);
		assert_eq!(printed.lines_or_empty(), "");
	}

	#[test]
	fn yes_turning_downloads_on_says_so() {
		let printed = Printed::default();
		let mut ui = printed.ui();
		ui.disable_prompts();
		ui.set_assume_yes(true);
		assert!(prompt_downloadable(&mut ui, false).unwrap());
		assert_eq!(
			console::strip_ansi_codes(&printed.lines()),
			"Enable download? Yes (--yes)"
		);
	}

	#[test]
	fn yes_keeping_downloads_on_prints_nothing() {
		let printed = Printed::default();
		let mut ui = printed.ui();
		ui.disable_prompts();
		ui.set_assume_yes(true);
		assert!(prompt_downloadable(&mut ui, true).unwrap());
		assert_eq!(printed.lines_or_empty(), "");
	}

	const LINK_UUID: &str = "0f8fad5b-d9cb-469f-a165-70867728950e";

	#[test]
	fn file_link_url_uses_the_download_route_and_a_hex_encoded_key() {
		assert_eq!(
			public_link_url(PublicLinkKind::File, LINK_UUID, "abcXYZ09"),
			"https://app.filen.io/#/d/0f8fad5b-d9cb-469f-a165-70867728950e%2361626358595a3039"
		);
	}

	#[test]
	fn directory_link_url_uses_the_folder_route() {
		assert_eq!(
			public_link_url(PublicLinkKind::Directory, LINK_UUID, "key"),
			"https://app.filen.io/#/f/0f8fad5b-d9cb-469f-a165-70867728950e%236b6579"
		);
	}

	#[test]
	fn link_url_hex_encodes_a_key_that_is_already_hex() {
		let key = "00ff".repeat(16);
		let url = public_link_url(PublicLinkKind::File, LINK_UUID, &key);
		assert_eq!(
			url.split_once("%23").map(|(_, key)| key),
			Some("30306666".repeat(16).as_str())
		);
	}

	fn settings_of(
		expiration: PublicLinkExpiration,
		password: PasswordChange,
		downloadable: bool,
	) -> LinkSettings {
		LinkSettings {
			expiration,
			password,
			downloadable,
		}
	}

	#[test]
	fn the_default_settings_protect_a_new_link_no_less() {
		for password in [PasswordChange::Keep, PasswordChange::Clear] {
			assert_eq!(
				protections_beyond_defaults(&settings_of(
					PublicLinkExpiration::Never,
					password,
					true
				)),
				[]
			);
		}
	}

	#[test]
	fn a_password_an_expiry_and_downloads_off_each_protect_a_new_link() {
		assert_eq!(
			protections_beyond_defaults(&settings_of(
				PublicLinkExpiration::Never,
				PasswordChange::Set("secret".to_owned()),
				true
			)),
			[Protection::Password]
		);
		assert_eq!(
			protections_beyond_defaults(&settings_of(
				PublicLinkExpiration::OneHour,
				PasswordChange::Keep,
				true
			)),
			[Protection::Expiry]
		);
		assert_eq!(
			protections_beyond_defaults(&settings_of(
				PublicLinkExpiration::Never,
				PasswordChange::Keep,
				false
			)),
			[Protection::NoDownloads]
		);
		assert_eq!(
			protections_beyond_defaults(&settings_of(
				PublicLinkExpiration::ThirtyDays,
				PasswordChange::Set("secret".to_owned()),
				false
			)),
			[
				Protection::Password,
				Protection::Expiry,
				Protection::NoDownloads
			]
		);
	}

	fn unsaved_link(
		noun: &'static str,
		path: &'static str,
		url: Option<&str>,
		protections: Vec<Protection>,
	) -> UnsavedNewLink<'static> {
		UnsavedNewLink {
			noun,
			path,
			url: url.map(str::to_owned),
			protections,
		}
	}

	/// The error [`unsaved_new_link_error`] gives for `unsaved`, and whether it tried to delete the
	/// link, which `delete_result` answers.
	async fn unsaved_error(
		unsaved: &UnsavedNewLink<'_>,
		delete_result: Result<()>,
	) -> (String, bool) {
		let mut deleted = false;
		let error = unsaved_new_link_error(anyhow::anyhow!("server refused"), unsaved, async {
			deleted = true;
			delete_result
		})
		.await;
		(format!("{error:#}"), deleted)
	}

	#[tokio::test]
	async fn a_new_link_left_with_settings_no_more_open_is_kept() {
		let unsaved = unsaved_link(
			"file",
			"/docs/a.txt",
			Some("https://app.filen.io/#/d/x"),
			vec![],
		);
		assert_eq!(
			unsaved_error(&unsaved, Ok(())).await,
			(
				"Created the public link of the file /docs/a.txt (https://app.filen.io/#/d/x) with \
				 the default settings, but saving the settings failed: change them with `filen \
				 public-link /docs/a.txt`, or delete it with `filen public-link --delete \
				 /docs/a.txt`: server refused"
					.to_owned(),
				false
			)
		);
	}

	#[tokio::test]
	async fn a_new_directory_link_left_with_its_defaults_quotes_the_path_in_the_commands() {
		let unsaved = unsaved_link("directory", "/my docs", None, vec![]);
		let quoted = if cfg!(unix) {
			"'/my docs'"
		} else {
			"\"/my docs\""
		};
		assert_eq!(
			unsaved_error(&unsaved, Ok(())).await,
			(
				format!(
					"Created the public link of the directory /my docs with the default settings, \
					 but saving the settings failed: change them with `filen public-link {quoted}`, \
					 or delete it with `filen public-link --delete {quoted}`: server refused"
				),
				false
			)
		);
	}

	#[tokio::test]
	async fn a_path_the_shell_would_expand_is_quoted_against_it_in_the_commands() {
		let unsaved = unsaved_link("file", "/a $b", None, vec![]);
		let quoted = if cfg!(unix) { "'/a $b'" } else { "\"/a $b\"" };
		assert_eq!(
			unsaved_error(&unsaved, Ok(())).await,
			(
				format!(
					"Created the public link of the file /a $b with the default settings, but saving \
					 the settings failed: change them with `filen public-link {quoted}`, or delete \
					 it with `filen public-link --delete {quoted}`: server refused"
				),
				false
			)
		);
	}

	#[tokio::test]
	async fn a_new_file_link_whose_protection_failed_to_save_is_deleted() {
		let unsaved = unsaved_link(
			"file",
			"/docs/a b.txt",
			Some("https://app.filen.io/#/d/x"),
			vec![Protection::Password],
		);
		assert_eq!(
			unsaved_error(&unsaved, Ok(())).await,
			(
				"Saving the settings of the new public link of the file /docs/a b.txt failed, so \
				 the link was deleted and the file is not public; run the command again to create \
				 it: server refused"
					.to_owned(),
				true
			)
		);
	}

	#[tokio::test]
	async fn a_new_directory_link_whose_protection_failed_to_save_is_deleted() {
		let unsaved = unsaved_link("directory", "/docs", None, vec![Protection::Expiry]);
		assert_eq!(
			unsaved_error(&unsaved, Ok(())).await,
			(
				"Saving the settings of the new public link of the directory /docs failed, so the \
				 link was deleted and the directory is not public; run the command again to create \
				 it: server refused"
					.to_owned(),
				true
			)
		);
	}

	#[tokio::test]
	async fn a_new_link_that_cannot_be_deleted_either_says_the_item_is_public_without_its_protection()
	 {
		let unsaved = unsaved_link(
			"file",
			"/docs/a b.txt",
			Some("https://app.filen.io/#/d/x"),
			vec![
				Protection::Password,
				Protection::Expiry,
				Protection::NoDownloads,
			],
		);
		let delete_error = Err(anyhow::anyhow!("offline").context("Failed to delete public link"));
		let quoted = if cfg!(unix) {
			"'/docs/a b.txt'"
		} else {
			"\"/docs/a b.txt\""
		};
		assert_eq!(
			unsaved_error(&unsaved, delete_error).await,
			(
				format!(
					"Created the public link of the file /docs/a b.txt (https://app.filen.io/#/d/x), \
					 but saving its settings failed, and deleting the link failed too (Failed to \
					 delete public link: offline): the file /docs/a b.txt is public without the \
					 password, the expiry and the download restriction asked for; change them with \
					 `filen public-link {quoted}`, or delete it with `filen public-link --delete \
					 {quoted}`: server refused"
				),
				true
			)
		);
	}

	#[tokio::test]
	async fn a_new_directory_link_that_cannot_be_deleted_names_its_one_missing_protection() {
		let unsaved = unsaved_link("directory", "/docs", None, vec![Protection::NoDownloads]);
		let delete_error = Err(anyhow::anyhow!("offline"));
		assert_eq!(
			unsaved_error(&unsaved, delete_error).await,
			(
				"Created the public link of the directory /docs, but saving its settings failed, \
				 and deleting the link failed too (offline): the directory /docs is public without \
				 the download restriction asked for; change them with `filen public-link /docs`, or delete it \
				 with `filen public-link --delete /docs`: server refused"
					.to_owned(),
				true
			)
		);
	}
}
