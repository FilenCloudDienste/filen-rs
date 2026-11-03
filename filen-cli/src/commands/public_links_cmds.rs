use crate::{
	CommandResult, auth::LazyClient, commands::CommandContext, completion::FilenCompleter,
	docs::DocElement, ui::UI, util::RemotePath,
};
use anyhow::{Context as _, Result};
use clap::Subcommand;
use console::style;
use filen_sdk_rs::{
	Error, ErrorKind,
	auth::Client,
	connect::{DirPublicLinkRW, FilePublicLink, PasswordState},
	fs::{
		HasName, HasUUID as _,
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
	PublicLink {
		/// File or directory to create, update or delete a public link for
		#[arg(add = FilenCompleter::file_or_directory())]
		file_or_directory: String,
	},
}

/// The commands that close the docs' "Accessing Files" section.
pub(crate) const DOCS: &[DocElement] = &[
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
			Self::PublicLink { file_or_directory } => {
				view_or_create_or_edit_public_link(ui, client, working_path, &file_or_directory)
					.await?;
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

/// How many items `list-public-links` resolves (path and link status) at once.
const LINK_STATUS_CONCURRENCY: usize = 8;

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
		let mut links = stream::iter(dirs.iter().map(|dir| listed_dir_link(client, dir)))
			.buffered(LINK_STATUS_CONCURRENCY)
			.try_collect::<Vec<_>>()
			.await?;
		links.extend(
			stream::iter(files.iter().map(|file| listed_file_link(client, file)))
				.buffered(LINK_STATUS_CONCURRENCY)
				.try_collect::<Vec<_>>()
				.await?,
		);
		links
	};
	links.sort_by(|a, b| a.path.cmp(&b.path));

	print_public_links(ui, &links)
}

async fn listed_dir_link(client: &Client, dir: &RemoteDirectory) -> Result<ListedPublicLink> {
	let path = item_path(client, &NonRootItemType::Dir(Cow::Borrowed(dir))).await;
	let link = client
		.get_dir_link_rw(dir)
		.await
		.with_context(|| format!("Failed to get the public link of {path}"))?;
	Ok(ListedPublicLink {
		path,
		kind: PublicLinkKind::Directory,
		item_uuid: dir.uuid().to_string(),
		link: link.map(|link| ListedLinkStatus {
			link_uuid: link.uuid().to_string(),
			url: dir_public_link_url(&link),
		}),
	})
}

async fn listed_file_link(client: &Client, file: &RemoteFile) -> Result<ListedPublicLink> {
	let path = item_path(client, &NonRootItemType::File(Cow::Borrowed(file))).await;
	let link = client
		.get_file_link_status(file)
		.await
		.with_context(|| format!("Failed to get the public link of {path}"))?;
	Ok(ListedPublicLink {
		path,
		kind: PublicLinkKind::File,
		item_uuid: file.uuid().to_string(),
		link: link.map(|link| ListedLinkStatus {
			link_uuid: link.uuid().to_string(),
			url: file_public_link_url(file, &link),
		}),
	})
}

/// The item's absolute path, without the trailing slash `get_item_path` puts on directories.
/// Falls back to the uuid, as the other listings do, so that one unresolvable item doesn't fail
/// the whole listing.
async fn item_path(client: &Client, item: &NonRootItemType<'_, Normal>) -> String {
	match client.get_item_path(item).await {
		Ok((path, _)) => format!("/{}", path.trim_end_matches('/')),
		Err(e) => {
			log::warn!("Failed to resolve path of item {}: {}", item.uuid(), e);
			item.uuid().to_string()
		}
	}
}

/// Prints the linked items, expected sorted by path: a path and URL column, directories in blue,
/// or with `--json` one array of `{path, type, uuid, linkUuid, url}` objects, where `linkUuid` and
/// `url` are `null` for a link that is gone and `url` is `null` when its key could not be
/// decrypted.
fn print_public_links(ui: &mut UI, links: &[ListedPublicLink]) -> Result<()> {
	if ui.json {
		let json = links
			.iter()
			.map(|link| {
				json!({
					"path": link.path,
					"type": link.kind.noun(),
					"uuid": ui.omit_for_replay_testing(&link.item_uuid),
					"linkUuid": link
						.link
						.as_ref()
						.map(|status| ui.omit_for_replay_testing(&status.link_uuid)),
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
	let path_width = links
		.iter()
		.map(|link| link.path.width())
		.max()
		.unwrap_or(0);
	for link in links {
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
		ui.print(&format!("{path}{padding}  {url}"));
	}
	Ok(())
}

pub(crate) async fn view_or_create_or_edit_public_link(
	ui: &mut UI,
	client: &mut LazyClient,
	working_path: &RemotePath,
	file_or_directory_str: &str,
) -> Result<()> {
	let path = working_path.navigate(file_or_directory_str).0;
	let client = client.get(ui).await?;
	let Some(file_or_directory) = client
		.find_item_at_path(&path)
		.await
		.context("Failed to find file or directory")?
	else {
		return Err(UI::failure(&format!("No such file or directory: {}", path)));
	};
	match file_or_directory {
		NonRootFileType::File(file) => view_or_create_or_edit_link(ui, client, &*file, &path).await,
		NonRootFileType::Dir(dir) => view_or_create_or_edit_link(ui, client, &*dir, &path).await,
		NonRootFileType::Root(_) => Err(UI::failure("Cannot public link the root")),
	}
}

/// Shows the public link of `item`, found at `path`, and offers to edit or delete it, or creates
/// one. Without a terminal to prompt on, it only shows an existing link, and creates none.
async fn view_or_create_or_edit_link<I: LinkableItem>(
	ui: &mut UI,
	client: &Client,
	item: &I,
	path: &str,
) -> Result<()> {
	let existing_link = match item.link_status(client).await {
		Ok(link) => link,
		Err(e) => {
			if e.kind() == ErrorKind::Server
				&& let Some(code) = e.server_code()
				&& code == "subscription_needed"
			{
				return Err(UI::failure(
					"Public links are not available for your account. Please upgrade your subscription to access this feature.",
				));
			} else {
				return Err(e).context("Failed to get public link");
			}
		}
	};
	match existing_link {
		Some(link) => edit_or_delete_link(ui, client, item, link).await,
		None => create_link(ui, client, item, path).await,
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
		Some(PublicLinkOperation::Delete) => {
			item.remove_link(client, link)
				.await
				.context("Failed to delete public link")?;
			ui.print_success("Public link deleted.");
			Ok(())
		}
		Some(PublicLinkOperation::Edit) => {
			let Some(settings) = prompt_link_settings(ui)? else {
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
	ui.print(&format!(
		"Creating a new public link for the {}...",
		I::KIND.noun()
	));
	let mut link = item
		.create_link(client)
		.await
		.context("Failed to create public link")?;
	I::apply_settings(&mut link, settings);
	save_link(ui, client, item, &link).await
}

/// The settings for a new link for the item at `path`, or `None` when a prompt was cancelled.
/// Without a terminal to ask on it fails with a usage error, as a command that looks like it only
/// shows the link must not make the item public unasked.
fn new_link_settings(ui: &mut UI, path: &str) -> Result<Option<LinkSettings>> {
	ui.ensure_can_prompt(EXPIRATION_PROMPT).with_context(|| {
		format!("No public link for {path}, and creating one asks for its settings")
	})?;
	prompt_link_settings(ui)
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

/// A link's settings, as the link table shows them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CurrentLinkSettings {
	expiration: PublicLinkExpiration,
	has_password: bool,
	downloadable: bool,
}

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
	Clear,
	Set(String),
}

impl PasswordChange {
	fn from_answer(answer: String) -> Self {
		if answer.is_empty() {
			PasswordChange::Clear
		} else {
			PasswordChange::Set(answer)
		}
	}
}

/// Asks for a link's settings, or `None` when a prompt was cancelled.
fn prompt_link_settings(ui: &mut UI) -> Result<Option<LinkSettings>> {
	let expiration_choices =
		EXPIRATIONS.map(|expiration| (expiration_label(expiration), expiration));
	let Some(expiration) = ui.prompt_choice(
		EXPIRATION_PROMPT,
		&expiration_choices,
		PublicLinkExpiration::Never,
	)?
	else {
		return Ok(None);
	};
	let password =
		PasswordChange::from_answer(ui.prompt("Password (leave empty for no password):")?);
	// a setting, which --yes does not choose
	let downloadable = ui.prompt_yes_no("Enable download?", false)?;
	Ok(Some(LinkSettings {
		expiration,
		password,
		downloadable,
	}))
}

/// Prints the item's name, then its link's settings and URL.
fn print_link_table<I: LinkableItem>(ui: &mut UI, item: &I, link: &I::Link) {
	let settings = I::current_settings(link);
	let url = item.link_url(link);
	let url = url_or_unavailable(ui, url.as_deref());
	ui.print_key_value_table(&[
		(I::KIND.label(), item.name().unwrap_or_default()),
		("Expiration", expiration_label(settings.expiration)),
		("Password", yes_no(settings.has_password)),
		("Download Enabled", yes_no(settings.downloadable)),
		("URL", url),
	]);
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

	use filen_types::crypto::LinkHashedPassword;

	use super::*;
	use crate::{error::exit_code, ui::test_support::Printed};

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
			url: url.map(str::to_owned),
		})
	}

	fn printed_links(json: bool, links: &[ListedPublicLink]) -> String {
		let printed = Printed::default();
		let mut ui = printed.ui();
		ui.json = json;
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
	fn listed_links_print_their_path_and_url_in_aligned_columns() {
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
			"/docs             https://dir\n\
			 /docs/report.pdf  https://file\n\
			 /gone.txt         none (the link was removed while listing)\n\
			 /locked           unavailable (the key could not be decrypted)"
		);
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
					"url": "https://dir",
				},
				{
					"path": "/gone.txt",
					"type": "file",
					"uuid": "00000000-0000-0000-0000-000000000001",
					"linkUuid": null,
					"url": null,
				},
				{
					"path": "/locked",
					"type": "directory",
					"uuid": "00000000-0000-0000-0000-000000000001",
					"linkUuid": LINK_UUID,
					"url": null,
				},
			])
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
			"No public link for /a.txt, and creating one asks for its settings: Cannot ask \
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
}
