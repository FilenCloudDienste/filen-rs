//! `versions`: a file's earlier versions, which the server keeps when a file is uploaded again
//! under the same name, and restoring or deleting one of them.

use anyhow::{Context as _, Result};
use chrono::{DateTime, Utc};
use console::style;
use filen_sdk_rs::fs::{HasUUID as _, file::FileVersion};
use filen_types::fs::Uuid;
use serde_json::json;

use super::{ItemOperand, find_file};
use crate::{
	auth::LazyClient,
	ui::{self, UI},
};

/// What `versions` does with the file's versions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum VersionsAction {
	List,
	/// Makes the version with this UUID the file's current one.
	Restore(Uuid),
	/// Permanently deletes the version with this UUID.
	Delete(Uuid),
}

impl VersionsAction {
	/// The action of `--restore` and `--delete`, which clap lets only one of be given.
	pub(super) fn of(restore: Option<Uuid>, delete: Option<Uuid>) -> Self {
		match (restore, delete) {
			(Some(uuid), _) => Self::Restore(uuid),
			(None, Some(uuid)) => Self::Delete(uuid),
			(None, None) => Self::List,
		}
	}
}

/// One version of a file, as `versions` prints it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ListedVersion {
	uuid: Uuid,
	/// When the version was uploaded.
	timestamp: DateTime<Utc>,
	size: u64,
	/// The version is the file's current content.
	current: bool,
}

impl ListedVersion {
	fn of(version: &FileVersion, current: Uuid) -> Self {
		Self {
			uuid: version.uuid(),
			timestamp: version.timestamp(),
			size: version.size(),
			current: version.uuid() == current,
		}
	}

	fn to_json(&self) -> serde_json::Value {
		json!({
			"uuid": self.uuid,
			"timestamp": self.timestamp,
			"size": self.size,
			"current": self.current,
		})
	}
}

/// Lists the versions of the file `path` names, newest first, or restores or deletes one of them.
pub(super) async fn file_versions(
	ui: &mut UI,
	client: &mut LazyClient,
	path: &ItemOperand,
	action: VersionsAction,
) -> Result<()> {
	if let VersionsAction::Delete(_) = action {
		// before logging in, so that a script missing --yes fails at once
		ui.ensure_can_confirm(&format!("Permanently delete a version of {}?", path))?;
	}
	let client = client.get(ui).await?;
	let file = find_file(client, path).await?;
	let versions = client
		.list_file_versions(&file)
		.await
		.context("Failed to list file versions")?;
	let current = file.uuid();
	let mut versions = versions
		.into_iter()
		.map(|version| (ListedVersion::of(&version, current), version))
		.collect::<Vec<(ListedVersion, FileVersion)>>();
	match action {
		VersionsAction::List => {
			let listed = versions
				.into_iter()
				.map(|(listed, _)| listed)
				.collect::<Vec<ListedVersion>>();
			print_versions(ui, &listed)
		}
		VersionsAction::Restore(uuid) => {
			let (listed, version) = take_version(&mut versions, uuid, path)?;
			if listed.current {
				ui.print_muted(&format!("{} is the current version of {}", uuid, path));
				return Ok(());
			}
			let mut file = file.into_owned();
			client
				.restore_file_version(&mut file, version)
				.await
				.context("Failed to restore file version")?;
			ui.print_success(&format!(
				"Restored the version of {} from {}",
				path,
				ui::format_date(&listed.timestamp)
			));
			Ok(())
		}
		VersionsAction::Delete(uuid) => {
			let (listed, version) = take_version(&mut versions, uuid, path)?;
			if listed.current {
				return Err(UI::failure(&format!(
					"Cannot delete {}, the current version of {}: delete the file with rm instead",
					uuid, path
				)));
			}
			let timestamp = ui::format_date(&listed.timestamp);
			if !ui.prompt_confirm(
				&format!(
					"Permanently delete the version of {} from {}?",
					path, timestamp
				),
				false,
			)? {
				return Ok(());
			}
			client
				.delete_file_version(version)
				.await
				.context("Failed to delete file version")?;
			ui.print_success(&format!(
				"Permanently deleted the version of {} from {}",
				path, timestamp
			));
			Ok(())
		}
	}
}

/// Takes the version with `uuid` out of `versions`.
fn take_version<T>(
	versions: &mut Vec<(ListedVersion, T)>,
	uuid: Uuid,
	path: &ItemOperand,
) -> Result<(ListedVersion, T)> {
	let index = versions
		.iter()
		.position(|(listed, _)| listed.uuid == uuid)
		.ok_or_else(|| UI::failure(&format!("No version {} of {}", uuid, path)))?;
	Ok(versions.swap_remove(index))
}

/// Prints one version per line: its upload date, its size and its UUID, and `(current)` after
/// the current one.
fn print_versions(ui: &mut UI, versions: &[ListedVersion]) -> Result<()> {
	if ui.json {
		return ui.print_json(serde_json::Value::Array(
			versions.iter().map(ListedVersion::to_json).collect(),
		));
	}
	let sizes = versions
		.iter()
		.map(|version| ui::format_size(version.size))
		.collect::<Vec<String>>();
	let size_width = sizes.iter().map(String::len).max().unwrap_or(0);
	for (version, size) in versions.iter().zip(sizes) {
		let current = if version.current { "  (current)" } else { "" };
		ui.print(&format!(
			"{}  {:>size_width$}  {}{}",
			style(ui::format_date(&version.timestamp)).dim(),
			size,
			version.uuid,
			current
		));
	}
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::{commands::fs_cmds::FsCommands, ui::test_support::Printed, util::RemotePath};
	use chrono::TimeZone as _;
	use clap::Parser;

	fn version(uuid: u128, day: u32, size: u64, current: bool) -> ListedVersion {
		ListedVersion {
			uuid: Uuid::from_u128(uuid),
			timestamp: Utc.with_ymd_and_hms(2024, 3, day, 10, 20, 30).unwrap(),
			size,
			current,
		}
	}

	fn printed_versions(versions: &[ListedVersion], json: bool) -> String {
		let printed = Printed::default();
		let mut ui = printed.ui();
		ui.json = json;
		print_versions(&mut ui, versions).unwrap();
		console::strip_ansi_codes(&printed.lines()).into_owned()
	}

	#[test]
	fn versions_prints_each_versions_date_size_and_uuid_and_marks_the_current_one() {
		let versions = [version(1, 9, 2048, true), version(2, 8, 7, false)];
		assert_eq!(
			printed_versions(&versions, false),
			"2024-03-09 10:20:30 (UTC)  2 KiB  00000000-0000-0000-0000-000000000001  (current)\n\
			 2024-03-08 10:20:30 (UTC)    7 B  00000000-0000-0000-0000-000000000002"
		);
	}

	#[test]
	fn versions_json_prints_an_object_per_version() {
		let versions = [version(1, 9, 2048, true), version(2, 8, 7, false)];
		assert_eq!(
			serde_json::from_str::<serde_json::Value>(&printed_versions(&versions, true)).unwrap(),
			json!([
				{
					"uuid": "00000000-0000-0000-0000-000000000001",
					"timestamp": "2024-03-09T10:20:30Z",
					"size": 2048,
					"current": true,
				},
				{
					"uuid": "00000000-0000-0000-0000-000000000002",
					"timestamp": "2024-03-08T10:20:30Z",
					"size": 7,
					"current": false,
				},
			])
		);
	}

	#[test]
	fn take_version_takes_the_version_with_the_uuid() {
		let mut versions = vec![
			(version(1, 9, 2048, true), "one"),
			(version(2, 8, 7, false), "two"),
		];
		assert_eq!(
			take_version(
				&mut versions,
				Uuid::from_u128(2),
				&ItemOperand::Path(RemotePath::new("/a.txt"))
			)
			.unwrap(),
			(version(2, 8, 7, false), "two")
		);
	}

	#[test]
	fn take_version_of_a_uuid_that_is_no_version_of_the_file_fails() {
		let mut versions = vec![(version(1, 9, 2048, true), ())];
		assert_eq!(
			take_version(
				&mut versions,
				Uuid::from_u128(3),
				&ItemOperand::Path(RemotePath::new("/a.txt"))
			)
			.unwrap_err()
			.to_string(),
			"No version 00000000-0000-0000-0000-000000000003 of /a.txt"
		);
	}

	#[test]
	fn versions_action_is_listing_without_restore_or_delete() {
		assert_eq!(VersionsAction::of(None, None), VersionsAction::List);
		assert_eq!(
			VersionsAction::of(Some(Uuid::from_u128(1)), None),
			VersionsAction::Restore(Uuid::from_u128(1))
		);
		assert_eq!(
			VersionsAction::of(None, Some(Uuid::from_u128(1))),
			VersionsAction::Delete(Uuid::from_u128(1))
		);
	}

	#[derive(Debug, Parser)]
	struct Args {
		#[command(subcommand)]
		command: FsCommands,
	}

	#[test]
	fn restoring_and_deleting_a_version_together_is_refused() {
		assert_eq!(
			Args::try_parse_from([
				"filen",
				"versions",
				"a.txt",
				"--restore",
				"00000000-0000-0000-0000-000000000001",
				"--delete",
				"00000000-0000-0000-0000-000000000002",
			])
			.unwrap_err()
			.kind(),
			clap::error::ErrorKind::ArgumentConflict
		);
	}

	#[test]
	fn a_version_that_is_not_a_uuid_is_refused() {
		assert_eq!(
			Args::try_parse_from(["filen", "versions", "a.txt", "--restore", "x"])
				.unwrap_err()
				.kind(),
			clap::error::ErrorKind::ValueValidation
		);
	}
}
