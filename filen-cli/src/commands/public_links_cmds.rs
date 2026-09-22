use crate::{
	auth::LazyClient, commands::fs_cmds::print_items_with_full_paths, ui::UI, util::RemotePath,
};
use anyhow::{Context as _, Result};
use filen_sdk_rs::{
	ErrorKind,
	auth::Client,
	connect::{DirPublicLinkRW, FilePublicLink, PasswordState},
	fs::{HasName, categories::NonRootFileType},
	io::{RemoteDirectory, RemoteFile},
};
use filen_types::api::v3::dir::link::PublicLinkExpiration;

// todo: write tests for this (!)

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
				Ok(link) => link.is_some(),
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
			if existing_link {
				let existing_link = client
					.public_link_file(&file)
					.await
					.context("Failed to modify public link")?;

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
		(
			"Password",
			match link.password() {
				PasswordState::None => "No",
				_ => "No",
			},
		),
	]);
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

	Ok(())
}

fn print_dir_public_link(ui: &mut UI, dir: &RemoteDirectory, link: &DirPublicLinkRW) {
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
		(
			"Password",
			match link.password() {
				PasswordState::None => "No",
				_ => "Yes",
			},
		),
		(
			"Download Enabled",
			if link.download_enabled() { "Yes" } else { "No" },
		),
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

	Ok(())
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
