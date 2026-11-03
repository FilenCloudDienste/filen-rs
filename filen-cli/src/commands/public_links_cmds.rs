use crate::{
	CommandResult, auth::LazyClient, commands::CommandContext, completion::FilenCompleter,
	docs::DocElement, ui::UI, util::RemotePath,
};
use anyhow::{Context as _, Result};
use clap::Subcommand;
use console::style;
use filen_sdk_rs::{
	ErrorKind,
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
					"type": link.kind.json_type(),
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
		NonRootFileType::File(file) => {
			let existing_link = match client.get_file_link_status(&file).await {
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
			if let Some(existing_link) = existing_link {
				print_file_public_link(ui, &file, &existing_link);
				ui.print("");

				match prompt_for_operation(ui)? {
					Some(PublicLinkOperation::Delete) => {
						client
							.remove_file_link(&file, existing_link)
							.await
							.context("Failed to delete public link")?;
						ui.print_success("Public link deleted.");
						return Ok(());
					}
					Some(PublicLinkOperation::Edit) => {
						prompt_public_link_file_settings(ui, client, &file, existing_link).await?;
					}
					None => {
						return Ok(());
					}
				}
			} else {
				ui.print("Creating a new public link for the file...");
				let link = client
					.public_link_file(&file)
					.await
					.context("Failed to modify public link")?;
				prompt_public_link_file_settings(ui, client, &file, link).await?;
			}
		}
		NonRootFileType::Dir(dir) => {
			let existing_link = match client.get_dir_link_rw(&dir).await {
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
			if let Some(existing_link) = existing_link {
				print_dir_public_link(ui, &dir, &existing_link);
				ui.print("");

				match prompt_for_operation(ui)? {
					Some(PublicLinkOperation::Delete) => {
						client
							.remove_dir_link(&dir)
							.await
							.context("Failed to delete public link")?;
						ui.print_success("Public link deleted.");
						return Ok(());
					}
					Some(PublicLinkOperation::Edit) => {
						prompt_public_link_dir_settings(ui, client, &dir, existing_link).await?;
					}
					None => {
						return Ok(());
					}
				}
			} else {
				ui.print("Creating a new public link for the directory...");
				let link = client
					.public_link_dir(&dir, Some(&(|_, _| {})))
					.await
					.context("Failed to modify public link")?;
				prompt_public_link_dir_settings(ui, client, &dir, link).await?;
			}
		}
		NonRootFileType::Root(_) => {
			return Err(UI::failure("Cannot public link the root"));
		}
	}

	Ok(())
}

enum PublicLinkOperation {
	Edit,
	Delete,
}
fn prompt_for_operation(ui: &mut UI) -> Result<Option<PublicLinkOperation>> {
	let operation = ui.prompt_select(
		"This is an existing public link.",
		vec![
			"Cancel".to_string(),
			"Edit".to_string(),
			"Delete".to_string(),
		],
	)?;
	let Some(operation) = operation else {
		return Err(UI::failure("No operation selected"));
	};
	Ok(match operation.as_str() {
		"Edit" => Some(PublicLinkOperation::Edit),
		"Delete" => Some(PublicLinkOperation::Delete),
		"Cancel" => None,
		_ => unreachable!(),
	})
}

fn print_file_public_link(ui: &mut UI, file: &RemoteFile, link: &FilePublicLink) {
	let url = file_public_link_url(file, link);
	let url = url_or_unavailable(ui, url.as_deref());
	ui.print_key_value_table(&[
		("File", file.name().unwrap_or_default()),
		(
			"Expiration",
			match link.expiration() {
				PublicLinkExpiration::Never => "Never",
				PublicLinkExpiration::OneHour => "1 hour",
				PublicLinkExpiration::SixHours => "6 hours",
				PublicLinkExpiration::OneDay => "1 day",
				PublicLinkExpiration::ThreeDays => "3 days",
				PublicLinkExpiration::OneWeek => "1 week",
				PublicLinkExpiration::TwoWeeks => "2 weeks",
				PublicLinkExpiration::ThirtyDays => "30 days",
			},
		),
		("Password", password_label(link.password())),
		("URL", url),
	]);
}

/// Whether a link has a password, as the link tables show it. The password itself is never shown.
fn password_label(password: &PasswordState) -> &'static str {
	match password {
		PasswordState::Known(_) | PasswordState::Hashed(_) => "Yes",
		PasswordState::None => "No",
	}
}

async fn prompt_public_link_file_settings(
	ui: &mut UI,
	client: &Client,
	file: &RemoteFile,
	mut link: FilePublicLink,
) -> Result<()> {
	let expiration = prompt_expiration(ui)?;
	link.set_expiration(expiration);
	let password = ui.prompt("Password (leave empty for no password):")?;
	if password.is_empty() {
		link.clear_password();
	} else {
		link.set_password(password);
	}

	client
		.update_file_link(file, &link)
		.await
		.context("Failed to update public link")?;
	ui.print_success("Public link saved.");
	print_public_link_url(ui, file_public_link_url(file, &link).as_deref());

	Ok(())
}

fn print_dir_public_link(ui: &mut UI, dir: &RemoteDirectory, link: &DirPublicLinkRW) {
	let url = dir_public_link_url(link);
	let url = url_or_unavailable(ui, url.as_deref());
	ui.print_key_value_table(&[
		("Directory", dir.name().unwrap_or_default()),
		(
			"Expiration",
			match link.expiration() {
				PublicLinkExpiration::Never => "Never",
				PublicLinkExpiration::OneHour => "1 hour",
				PublicLinkExpiration::SixHours => "6 hours",
				PublicLinkExpiration::OneDay => "1 day",
				PublicLinkExpiration::ThreeDays => "3 days",
				PublicLinkExpiration::OneWeek => "1 week",
				PublicLinkExpiration::TwoWeeks => "2 weeks",
				PublicLinkExpiration::ThirtyDays => "30 days",
			},
		),
		("Password", password_label(link.password())),
		(
			"Download Enabled",
			if link.download_enabled() { "Yes" } else { "No" },
		),
		("URL", url),
	]);
}

async fn prompt_public_link_dir_settings(
	ui: &mut UI,
	client: &Client,
	dir: &RemoteDirectory,
	mut link: DirPublicLinkRW,
) -> Result<()> {
	let expiration = prompt_expiration(ui)?;
	link.set_expiration(expiration);
	let password = ui.prompt("Password (leave empty for no password):")?;
	if password.is_empty() {
		link.clear_password();
	} else {
		link.set_password(password);
	}
	// a setting, which --yes does not choose
	let enable_download = ui.prompt_yes_no("Enable download?", false)?;
	link.set_enable_download(enable_download);

	client
		.update_dir_link(dir, &link)
		.await
		.context("Failed to update public link")?;
	ui.print_success("Public link saved.");
	print_public_link_url(ui, dir_public_link_url(&link).as_deref());

	Ok(())
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

	/// The item type as the `--json` listings name it.
	fn json_type(&self) -> &'static str {
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

struct LinkExpirationOption(&'static str, PublicLinkExpiration);

fn prompt_expiration(ui: &mut UI) -> Result<PublicLinkExpiration> {
	let expiration_options = vec![
		LinkExpirationOption("Never", PublicLinkExpiration::Never),
		LinkExpirationOption("1 hour", PublicLinkExpiration::OneHour),
		LinkExpirationOption("6 hours", PublicLinkExpiration::SixHours),
		LinkExpirationOption("1 day", PublicLinkExpiration::OneDay),
		LinkExpirationOption("3 days", PublicLinkExpiration::ThreeDays),
		LinkExpirationOption("1 week", PublicLinkExpiration::OneWeek),
		LinkExpirationOption("2 weeks", PublicLinkExpiration::TwoWeeks),
		LinkExpirationOption("30 days", PublicLinkExpiration::ThirtyDays),
	];
	let selection = ui
		.prompt_select(
			"Expires after:",
			expiration_options
				.iter()
				.map(|option| option.0.to_owned())
				.collect(),
		)?
		.ok_or_else(|| UI::failure("Canceled"))?;
	expiration_options
		.into_iter()
		.find(|option| option.0 == selection)
		.map(|option| option.1)
		.context("Failed to resolve selected expiration")
}

#[cfg(test)]
mod tests {
	use std::borrow::Cow;

	use filen_types::crypto::LinkHashedPassword;

	use super::*;
	use crate::ui::test_support::Printed;

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
	fn password_label_says_yes_for_a_password_the_server_reports() {
		let hash = LinkHashedPassword(Cow::Borrowed("0123456789abcdef"));
		assert_eq!(password_label(&PasswordState::Hashed(hash)), "Yes");
	}

	#[test]
	fn password_label_says_yes_for_a_password_set_in_this_session() {
		assert_eq!(
			password_label(&PasswordState::Known("hunter2".to_owned())),
			"Yes"
		);
	}

	#[test]
	fn password_label_says_no_without_a_password() {
		assert_eq!(password_label(&PasswordState::None), "No");
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
