use crate::{
	auth::LazyClient, commands::fs_cmds::print_items_with_full_paths, ui::UI, util::RemotePath,
};
use anyhow::{Context as _, Result};
use filen_sdk_rs::{
	ErrorKind,
	auth::Client,
	connect::{DirPublicLinkRW, FilePublicLink, PasswordState},
	fs::{HasName, categories::NonRootFileType, file::traits::HasFileInfo as _},
	io::{RemoteDirectory, RemoteFile},
};
use filen_types::api::v3::dir::link::PublicLinkExpiration;
use std::fmt::{Display, Write as _};

/// Where the web app opens public links, as filen-web's `PUBLIC_LINK_BASE_URL` builds them.
const PUBLIC_LINK_BASE_URL: &str = "https://app.filen.io/#/";

/// Shown in place of a public link's URL when the key in it could not be decrypted.
const LINK_URL_UNAVAILABLE: &str = "unavailable (the key could not be decrypted)";

pub(crate) async fn list_public_links(ui: &mut UI, client: &mut LazyClient) -> Result<()> {
	let client = client.get(ui).await?;
	let (dirs, files) = client
		.list_linked(Some(&(|_, _| {})))
		.await
		.context("Failed to list public links")?;

	print_items_with_full_paths(ui, client, dirs, files, "No public links").await?;

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
	let expiration = prompt_expiration()?;
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
	let expiration = prompt_expiration()?;
	link.set_expiration(expiration);
	let password = ui.prompt("Password (leave empty for no password):")?;
	if password.is_empty() {
		link.clear_password();
	} else {
		link.set_password(password);
	}
	let enable_download = ui.prompt_confirm("Enable download?", false)?;
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

impl std::fmt::Display for LinkExpirationOption {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "{}", self.0)
	}
}

fn prompt_expiration() -> Result<PublicLinkExpiration> {
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
	let expiration = inquire::Select::new("Expires after:", expiration_options).prompt()?;
	Ok(expiration.1)
}

#[cfg(test)]
mod tests {
	use std::borrow::Cow;

	use filen_types::crypto::LinkHashedPassword;

	use super::*;

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
