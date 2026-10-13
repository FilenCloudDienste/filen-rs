use std::borrow::Cow;

use crate::{auth::LazyClient, ui::UI, util::RemotePath};
use anyhow::{Context as _, Result};
use filen_sdk_rs::{
	auth::Client,
	fs::{
		HasName as _, HasUUID,
		categories::{NonRootFileType, NonRootItemType, Normal},
		file::traits::HasFileInfo as _,
	},
	io::{RemoteDirectory, RemoteFile},
};

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

pub(crate) fn print_items_after_list(
	ui: &mut UI,
	dirs: Vec<RemoteDirectory>,
	files: Vec<RemoteFile>,
	directory_label: Option<&str>,
	long: bool,
) {
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
	ui.print_item_names(
		directories,
		files,
		&format!("{} is empty", directory_label.unwrap_or("Directory")),
		long,
	);
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
	ui.print_item_names(directories, files, empty_message, false);
	Ok(())
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
	print_items_after_list(ui, dirs, files, Some("Trash"), false);
	Ok(())
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
	let Some(selection) = ui.prompt_select(
		match action {
			TrashAction::Restore => "Select an item to restore",
			TrashAction::Delete => "Select an item to permanently delete",
		},
		options.clone(),
	)?
	else {
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
