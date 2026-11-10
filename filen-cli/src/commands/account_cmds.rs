//! [cli-doc] account-overview
//! `account` (or `account info`) shows your account: its email, plan, storage usage, and whether
//! two-factor authentication, file versioning and login alerts are enabled. `statfs` shows the
//! storage usage alone. Both make one request for it, so they stay fast on a large drive (unlike
//! `stat /`, which also counts every file and directory).
//!
//! `account versioning <on|off>` turns file versioning on or off: with it on, uploading a file
//! over an existing one keeps the old content as a version. `account login-alerts <on|off>` turns
//! the emails about new logins on or off.
//!
//! `account delete-versions` permanently deletes the old versions of every file. In a terminal it
//! asks you to type `delete all versions` to confirm; `--yes` confirms it without asking. Without
//! a terminal and without `--yes` it refuses, before logging in.
//!
//! `events` lists your account's activity log, newest first: logins, uploads, moves, shares and
//! other changes, one per line with its time (UTC), its type and the item or address it concerns.
//! `--filter <type>` asks the server for one type of event. Without `--limit` it lists the newest
//! page the server returns, about 100 events; `--limit <n>` prints only the newest `n`, fetching
//! older events as needed (at most 10 pages of about 100). The server pages by whole seconds, so
//! when more events share one second than one page holds, the rest of that second cannot be
//! fetched: `--limit` lists the events before it all the same, with a warning naming the second.
//! With `--json`, the events are printed as an array.

use std::{collections::HashSet, fmt::Display, num::NonZeroUsize};

use anyhow::{Context as _, Result};
use chrono::{DateTime, TimeDelta, Utc};
use clap::{Subcommand, ValueEnum};
use filen_sdk_rs::user::{
	UserInfo,
	events::{DecryptedUserEvent, DecryptedUserEventKind as Kind},
};
use filen_types::api::v3::user::account::UserAccountPlan;
use serde_json::json;

use crate::{
	CommandResult,
	auth::LazyClient,
	commands::CommandContext,
	docs::DocElement,
	ui::{self, UI},
};

#[derive(Debug, Subcommand)]
pub(crate) enum AccountCommands {
	/// Show how much of your storage is used
	Statfs,
	/// Show or change your account's settings (default: show the account)
	Account {
		#[command(subcommand)]
		command: Option<AccountSubcommand>,
	},
	/// List your account's activity log, newest first
	Events {
		/// Only list events of this type, as the server names them (default: all)
		#[arg(long)]
		filter: Option<String>,
		/// Print only the newest N events, fetching older events as needed (at most 10 pages of
		/// about 100); a second holding more events than one page may be listed in part, with a
		/// warning
		#[arg(long, value_name = "N")]
		limit: Option<NonZeroUsize>,
	},
}

#[derive(Debug, Subcommand)]
pub(crate) enum AccountSubcommand {
	/// Show your account's email, plan, storage usage and settings
	Info,
	/// Turn file versioning on or off
	Versioning { state: Toggle },
	/// Turn the emails about new logins on or off
	LoginAlerts { state: Toggle },
	/// Permanently delete the old versions of every file (asks to confirm, which --yes answers)
	DeleteVersions,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum Toggle {
	On,
	Off,
}

impl Toggle {
	fn enabled(self) -> bool {
		match self {
			Self::On => true,
			Self::Off => false,
		}
	}

	fn label(self) -> &'static str {
		match self {
			Self::On => "on",
			Self::Off => "off",
		}
	}
}

/// The docs' "Account" section.
pub(crate) const DOCS: &[DocElement] = &[
	DocElement::DocFragment("account-overview"),
	DocElement::CommandHelp("account"),
	DocElement::CommandHelp("statfs"),
	DocElement::CommandHelp("events"),
];

impl AccountCommands {
	pub(super) async fn execute(self, ctx: CommandContext<'_>) -> Result<CommandResult> {
		let CommandContext { ui, client, .. } = ctx;
		match self {
			Self::Statfs => print_storage_usage(ui, client).await?,
			Self::Account { command } => match command.unwrap_or(AccountSubcommand::Info) {
				AccountSubcommand::Info => print_account_info(ui, client).await?,
				AccountSubcommand::Versioning { state } => {
					let client = client.get(ui).await?;
					client
						.set_versioning_enabled(state.enabled())
						.await
						.context("Failed to change file versioning")?;
					ui.print_success(&format!("Turned file versioning {}", state.label()));
				}
				AccountSubcommand::LoginAlerts { state } => {
					let client = client.get(ui).await?;
					client
						.set_login_alerts_enabled(state.enabled())
						.await
						.context("Failed to change login alerts")?;
					ui.print_success(&format!("Turned login alerts {}", state.label()));
				}
				AccountSubcommand::DeleteVersions => delete_all_versions(ui, client).await?,
			},
			Self::Events { filter, limit } => {
				print_events(ui, client, filter.as_deref(), limit).await?;
			}
		}
		Ok(CommandResult::default())
	}
}

// storage usage

/// The storage that is left, which is none when more than the maximum is used.
fn free_storage(used: u64, max: u64) -> u64 {
	max.saturating_sub(used)
}

/// How much of the storage is used, in percent, or `None` for an account without storage.
fn used_percent(used: u64, max: u64) -> Option<f64> {
	// u64 to f64 loses precision above 2^53 bytes, far below what a percentage shows
	(max != 0).then(|| used as f64 / max as f64 * 100.0)
}

fn format_percent(percent: Option<f64>) -> String {
	percent.map_or_else(|| "-".to_owned(), |percent| format!("{percent:.1}%"))
}

/// A number of files and the storage they take, as `3 files (1.5 KiB)`.
fn format_files(count: u64, size: u64) -> String {
	let noun = if count == 1 { "file" } else { "files" };
	format!("{count} {noun} ({})", ui::format_size(size))
}

/// The storage usage as JSON fields, in bytes.
fn storage_usage_json(info: &UserInfo) -> serde_json::Map<String, serde_json::Value> {
	let mut object = serde_json::Map::new();
	object.insert("used".to_owned(), json!(info.storage_used));
	object.insert("max".to_owned(), json!(info.max_storage));
	object.insert(
		"free".to_owned(),
		json!(free_storage(info.storage_used, info.max_storage)),
	);
	object.insert(
		"percentUsed".to_owned(),
		json!(used_percent(info.storage_used, info.max_storage)),
	);
	object.insert("versionedFiles".to_owned(), json!(info.versioned_files));
	object.insert("versionedStorage".to_owned(), json!(info.versioned_storage));
	object.insert("unfinishedFiles".to_owned(), json!(info.unfinished_files));
	object.insert(
		"unfinishedStorage".to_owned(),
		json!(info.unfinished_storage),
	);
	object
}

/// The storage usage as table rows.
fn storage_usage_rows(info: &UserInfo) -> Vec<(&'static str, String)> {
	vec![
		("Used", ui::format_size(info.storage_used)),
		("Max", ui::format_size(info.max_storage)),
		(
			"Free",
			ui::format_size(free_storage(info.storage_used, info.max_storage)),
		),
		(
			"Percent used",
			format_percent(used_percent(info.storage_used, info.max_storage)),
		),
		(
			"Versions",
			format_files(info.versioned_files, info.versioned_storage),
		),
		(
			"Unfinished",
			format_files(info.unfinished_files, info.unfinished_storage),
		),
	]
}

fn print_rows(ui: &mut UI, rows: &[(&str, String)]) {
	let table = rows
		.iter()
		.map(|(key, value)| (*key, value.as_str()))
		.collect::<Vec<(&str, &str)>>();
	ui.print_key_value_table(&table);
}

async fn get_user_info(ui: &mut UI, client: &mut LazyClient) -> Result<UserInfo> {
	let client = client.get(ui).await?;
	client
		.get_user_info()
		.await
		.context("Failed to get account info")
}

async fn print_storage_usage(ui: &mut UI, client: &mut LazyClient) -> Result<()> {
	let info = get_user_info(ui, client).await?;
	if ui.json {
		ui.print_json(serde_json::Value::Object(storage_usage_json(&info)))?;
	} else {
		print_rows(ui, &storage_usage_rows(&info));
	}
	Ok(())
}

// account info

/// The plan to show: the names of the plans that have not ended, or "Premium" or "Free" when the
/// account has none.
fn plan_label(plans: &[UserAccountPlan], is_premium: bool, now: DateTime<Utc>) -> String {
	let mut names = Vec::<&str>::new();
	for plan in plans.iter().filter(|plan| plan.end_timestamp > now) {
		if !names.contains(&plan.name.as_str()) {
			names.push(&plan.name);
		}
	}
	match (names.is_empty(), is_premium) {
		(false, _) => names.join(", "),
		(true, true) => "Premium".to_owned(),
		(true, false) => "Free".to_owned(),
	}
}

fn enabled_label(enabled: bool) -> String {
	if enabled { "enabled" } else { "disabled" }.to_owned()
}

async fn print_account_info(ui: &mut UI, client: &mut LazyClient) -> Result<()> {
	let info = get_user_info(ui, client).await?;
	// Only these settings are shown: `two_factor_key` is the secret to set two-factor
	// authentication up with while it is off, and the personal and billing fields are nobody
	// else's business, in a terminal that may be shared or recorded.
	let plan = plan_label(&info.plans, info.is_premium, Utc::now());
	if ui.json {
		let mut object = serde_json::Map::new();
		object.insert("email".to_owned(), json!(info.email));
		object.insert("plan".to_owned(), json!(plan));
		object.append(&mut storage_usage_json(&info));
		object.insert(
			"twoFactorEnabled".to_owned(),
			json!(info.two_factor_enabled),
		);
		object.insert(
			"versioningEnabled".to_owned(),
			json!(info.versioning_enabled),
		);
		object.insert(
			"loginAlertsEnabled".to_owned(),
			json!(info.login_alerts_enabled),
		);
		ui.print_json(serde_json::Value::Object(object))?;
	} else {
		let usage = storage_usage_rows(&info);
		let mut rows = vec![("Email", info.email), ("Plan", plan)];
		rows.extend(usage);
		rows.extend([
			("Two-factor auth", enabled_label(info.two_factor_enabled)),
			("Versioning", enabled_label(info.versioning_enabled)),
			("Login alerts", enabled_label(info.login_alerts_enabled)),
		]);
		print_rows(ui, &rows);
	}
	Ok(())
}

// delete all versions

/// What the user types to confirm deleting every file version.
const DELETE_VERSIONS_PHRASE: &str = "delete all versions";

fn delete_versions_question() -> String {
	format!(
		"This permanently deletes the old versions of every file. Type \"{DELETE_VERSIONS_PHRASE}\" to confirm:"
	)
}

/// Whether `answer` confirms deleting every file version: it is the phrase, give or take
/// surrounding whitespace.
fn confirms_deleting_versions(answer: &str) -> bool {
	answer.trim() == DELETE_VERSIONS_PHRASE
}

/// Fails unless the deletion can be confirmed, by `--yes` or at a terminal. Runs before logging
/// in, so that a script learns it needs `--yes` before anything is looked up.
fn ensure_can_confirm_deleting_versions(ui: &UI) -> Result<()> {
	ui.ensure_can_confirm(&delete_versions_question())
}

/// Asks the user to type the confirmation phrase, which `--yes` answers.
fn confirm_deleting_versions(ui: &mut UI) -> Result<bool> {
	if ui.assumes_yes() {
		return Ok(true);
	}
	let answer = ui.prompt(&delete_versions_question())?;
	Ok(confirms_deleting_versions(&answer))
}

async fn delete_all_versions(ui: &mut UI, client: &mut LazyClient) -> Result<()> {
	ensure_can_confirm_deleting_versions(ui)?;
	let client = client.get(ui).await?;
	if !confirm_deleting_versions(ui)? {
		return Err(UI::failure(&format!(
			"The answer was not \"{DELETE_VERSIONS_PHRASE}\": no versions were deleted"
		)));
	}
	client
		.delete_all_versions()
		.await
		.context("Failed to delete file versions")?;
	ui.print_success("Deleted the old versions of every file");
	Ok(())
}

// events

/// What an event concerns, in a few words: the item's name, or the address a login came from.
/// `None` when the event says nothing more than its type, or its item's name cannot be decrypted.
fn event_summary(kind: &Kind) -> Option<String> {
	match kind {
		Kind::FileUploaded(info)
		| Kind::FileVersioned(info)
		| Kind::FileRestored(info)
		| Kind::VersionedFileRestored(info)
		| Kind::FileMoved(info)
		| Kind::FileTrash(info)
		| Kind::FileRm(info)
		| Kind::DeleteFilePermanently(info) => info.metadata.name().map(str::to_owned),
		Kind::FileRenamed(info) | Kind::FileMetadataChanged(info) => {
			renamed(info.old_metadata.name(), info.metadata.name())
		}
		Kind::FileShared(info) => info
			.metadata
			.name()
			.map(|name| format!("{name} with {}", info.receiver_email)),
		Kind::FileLinkEdited(info) => info.metadata.name().map(str::to_owned),
		Kind::FolderTrash(info)
		| Kind::FolderMoved(info)
		| Kind::SubFolderCreated(info)
		| Kind::BaseFolderCreated(info)
		| Kind::FolderRestored(info)
		| Kind::DeleteFolderPermanently(info) => info.name.name().map(str::to_owned),
		Kind::FolderRenamed(info) | Kind::FolderMetadataChanged(info) => {
			renamed(info.old_name.name(), info.name.name())
		}
		Kind::FolderShared(info) => info
			.name
			.name()
			.map(|name| format!("{name} with {}", info.receiver_email)),
		Kind::FolderColorChanged(info) => info.name.name().map(str::to_owned),
		Kind::Login(info)
		| Kind::FailedLogin(info)
		| Kind::PasswordChanged(info)
		| Kind::TwoFaEnabled(info)
		| Kind::TwoFaDisabled(info)
		| Kind::RequestAccountDeletion(info)
		| Kind::TrashEmptied(info)
		| Kind::DeleteAll(info)
		| Kind::DeleteVersioned(info)
		| Kind::DeleteUnfinished(info) => Some(format!("from {}", info.ip)),
		// the code is left out: it may still be redeemable
		Kind::CodeRedeemed(info) => Some(format!("from {}", info.ip)),
		Kind::EmailChanged(info) => Some(format!("to {}", info.email)),
		Kind::EmailChangeAttempt(info) => {
			Some(format!("from {} to {}", info.old_email, info.new_email))
		}
		Kind::RemovedSharedInItems(info) => Some(format!(
			"{} shared by {}",
			items(info.count),
			info.sharer_email
		)),
		Kind::RemovedSharedOutItems(info) => Some(format!(
			"{} shared with {}",
			items(info.count),
			info.receiver_email
		)),
		Kind::FolderLinkEdited(_) => None,
		Kind::ItemFavorite(info) => {
			let action = if info.value {
				"favorited"
			} else {
				"unfavorited"
			};
			Some(match info.metadata.name() {
				Some(name) => format!("{name} {action}"),
				None => action.to_owned(),
			})
		}
	}
}

fn renamed(old: Option<&str>, new: Option<&str>) -> Option<String> {
	match (old, new) {
		(Some(old), Some(new)) if old != new => Some(format!("{old} -> {new}")),
		(_, Some(new)) => Some(new.to_owned()),
		(Some(old), None) => Some(old.to_owned()),
		(None, None) => None,
	}
}

fn items(count: u64) -> String {
	let noun = if count == 1 { "item" } else { "items" };
	format!("{count} {noun}")
}

fn event_line(event: &DecryptedUserEvent) -> String {
	let mut line = format!(
		"{}  {}",
		ui::format_date(&event.timestamp),
		event.event_type()
	);
	if let Some(summary) = event_summary(&event.kind) {
		line.push_str("  ");
		line.push_str(&summary);
	}
	line
}

fn event_json(event: &DecryptedUserEvent) -> serde_json::Value {
	json!({
		"id": event.id,
		"timestamp": event.timestamp,
		"uuid": event.uuid,
		"type": event.event_type(),
		"summary": event_summary(&event.kind),
	})
}

/// How many pages of events `events --limit` fetches at most, about 100 events each.
const MAX_EVENT_PAGES: usize = 10;

/// The events of a page that could be read, and how many could not (each logged).
fn readable_events<E: Display>(
	page: Vec<Result<DecryptedUserEvent, E>>,
) -> (Vec<DecryptedUserEvent>, usize) {
	let mut events = Vec::with_capacity(page.len());
	let mut unreadable = 0usize;
	for result in page {
		match result {
			Ok(event) => events.push(event),
			Err(e) => {
				log::warn!("Skipped an event that could not be read: {e}");
				unreadable += 1;
			}
		}
	}
	(events, unreadable)
}

/// The events collected over one or more pages, newest first.
#[derive(Debug)]
struct CollectedEvents {
	events: Vec<DecryptedUserEvent>,
	/// How many events could not be read. An unreadable event that a page fetches again is
	/// counted again: it has no id to recognise it by. That is one in the second a page overlaps
	/// the previous one with, or one older than the oldest readable event of a page that added
	/// no new readable event.
	unreadable: usize,
	/// Why the collection stopped short of the limit, if it did before the history ran out.
	cut_short: Option<CutShort>,
	/// The seconds paging was stuck at, newest first: a page as large as the first added no new
	/// event, so it held only events of that second already listed, and the server, which pages
	/// by whole seconds, cannot return the rest of that second. Paging carried on before that
	/// second, so only the rest of each of these seconds is missing. A second holding exactly
	/// one page of events looks the same, so this says events may be missing, not that they are.
	stuck_seconds: Vec<DateTime<Utc>>,
}

/// Why [`collect_events`] stopped before the limit and before the oldest event.
#[derive(Debug, Clone, Copy, PartialEq)]
enum CutShort {
	/// It fetched as many pages as it may.
	Capped,
	/// A page held only events that could not be read, so it gave no timestamp to page back from.
	/// The first page, or one as large as the first: a smaller later one is the end of the
	/// history.
	Unreadable,
}

/// Collects the newest `limit` events, paging back through older ones: `fetch` returns the
/// newest page of events strictly before the cutoff it is given, or the newest page for `None`.
/// A page that adds no new event but is as large as the first (counting the events that could
/// not be read, as the server returned it) carries on strictly before its oldest second. It is
/// stuck at that second (see [`CollectedEvents::stuck_seconds`]) if it holds only events already
/// listed; one that also holds unreadable events may have reached older seconds with them, and is
/// not. Stops at the limit, at any other page that adds no new event (an empty one included), at
/// a page with no event that could be read ([`CutShort::Unreadable`]), or after `max_pages`
/// pages ([`CutShort::Capped`]).
async fn collect_events<E: Display>(
	limit: usize,
	max_pages: usize,
	mut fetch: impl AsyncFnMut(Option<DateTime<Utc>>) -> Result<Vec<Result<DecryptedUserEvent, E>>>,
) -> Result<CollectedEvents> {
	let mut collected = CollectedEvents {
		events: Vec::new(),
		unreadable: 0,
		cut_short: None,
		stuck_seconds: Vec::new(),
	};
	let mut seen = HashSet::new();
	let mut cutoff = None;
	let mut first_page_len = None;
	for _ in 0..max_pages {
		let raw_page = fetch(cutoff).await?;
		// Compared as the server returned it: dropping the unreadable events first would let
		// them hide a stuck page, or make a short page look as large as the first.
		let page_len = raw_page.len();
		let first_page_len = *first_page_len.get_or_insert(page_len);
		let (page, unreadable) = readable_events(raw_page);
		collected.unreadable += unreadable;
		let oldest = page.iter().map(|event| event.timestamp).min();
		let before = collected.events.len();
		for event in page {
			if collected.events.len() == limit {
				return Ok(collected);
			}
			if seen.insert(event.id) {
				collected.events.push(event);
			}
		}
		if collected.events.len() == limit {
			return Ok(collected);
		}
		let Some(oldest) = oldest else {
			// with no readable event, there is no timestamp to page back from
			if page_len > 0 && page_len == first_page_len {
				collected.cut_short = Some(CutShort::Unreadable);
			}
			return Ok(collected);
		};
		cutoff = if collected.events.len() == before {
			// A page that adds no readable event holds readable events of its oldest second or
			// newer, all listed already. A smaller one than the first is the end of the history;
			// one as large is likely cut off by the server's page size. Without unreadable events
			// it holds only that second's, the rest of which cannot be fetched; with them, those
			// may be older events. Either way paging carries on strictly before that second, a
			// cutoff older than this page's, so paging makes progress.
			if page_len != first_page_len {
				return Ok(collected);
			}
			if unreadable == 0 {
				collected.stuck_seconds.push(oldest);
			}
			Some(oldest)
		} else {
			// The server returns the events strictly before the cutoff's second (see
			// `filen_types::api::v3::user::events::Request`), so a cutoff at the oldest event's
			// second would skip the events of that second the page left out. One second later
			// fetches that second again, and the ids drop the events already listed.
			let Some(next) = oldest.checked_add_signed(TimeDelta::seconds(1)) else {
				return Ok(collected);
			};
			Some(next)
		};
	}
	collected.cut_short = Some(CutShort::Capped);
	Ok(collected)
}

async fn print_events(
	ui: &mut UI,
	client: &mut LazyClient,
	filter: Option<&str>,
	limit: Option<NonZeroUsize>,
) -> Result<()> {
	let client = client.get(ui).await?;
	let (events, unreadable) = match limit {
		Some(limit) => {
			let collected = collect_events(
				limit.get(),
				MAX_EVENT_PAGES,
				async |cutoff: Option<DateTime<Utc>>| {
					client
						.get_user_events(filter, cutoff)
						.await
						.context("Failed to get account events")
				},
			)
			.await?;
			match collected.cut_short {
				Some(CutShort::Capped) => ui.print_warning(&format!(
					"Listed the newest {} events: older ones take more than {MAX_EVENT_PAGES} requests",
					collected.events.len()
				)),
				Some(CutShort::Unreadable) => ui.print_warning(&format!(
					"Listed the newest {} events: older ones were not fetched past a page of \
					 events that could not be read",
					collected.events.len()
				)),
				None => {}
			}
			for second in &collected.stuck_seconds {
				ui.print_warning(&format!(
					"Some events at {} may not have been listed: more events share that second \
					 than one request returns",
					ui::format_date(second)
				));
			}
			(collected.events, collected.unreadable)
		}
		None => readable_events(
			client
				.get_user_events(filter, None)
				.await
				.context("Failed to get account events")?,
		),
	};
	if unreadable > 0 {
		ui.print_warning(&format!(
			"Skipped {unreadable} {} that could not be read",
			if unreadable == 1 { "event" } else { "events" }
		));
	}
	if ui.json {
		ui.print_json(serde_json::Value::Array(
			events.iter().map(event_json).collect(),
		))?;
	} else if events.is_empty() {
		ui.print_muted("No events");
	} else {
		for event in &events {
			ui.print(&event_line(event));
		}
	}
	Ok(())
}

#[cfg(test)]
mod tests {
	use filen_sdk_rs::{fs::dir::meta::DirectoryMeta, user::events::UserEventFolderPairInfo};
	use filen_types::fs::Uuid;

	use super::*;
	use crate::{commands::Commands, error::CliError, parse_args, ui::test_support::Printed};

	fn parse_error(args: &[&str]) -> clap::error::ErrorKind {
		let command = std::iter::once("filen").chain(args.iter().copied());
		match parse_args(command) {
			Ok(parsed) => panic!("parsed {:?}", parsed.args.command),
			Err(e) => e.kind(),
		}
	}

	fn parsed(args: &[&str]) -> AccountCommands {
		let command = std::iter::once("filen").chain(args.iter().copied());
		match parse_args(command).unwrap().args.command {
			Some(Commands::Account(command)) => command,
			other => panic!("parsed {other:?}"),
		}
	}

	#[test]
	fn account_without_a_subcommand_parses_as_no_subcommand() {
		assert!(matches!(
			parsed(&["account"]),
			AccountCommands::Account { command: None }
		));
	}

	#[test]
	fn account_versioning_takes_on_or_off() {
		assert!(matches!(
			parsed(&["account", "versioning", "on"]),
			AccountCommands::Account {
				command: Some(AccountSubcommand::Versioning { state: Toggle::On })
			}
		));
		assert!(matches!(
			parsed(&["account", "login-alerts", "off"]),
			AccountCommands::Account {
				command: Some(AccountSubcommand::LoginAlerts { state: Toggle::Off })
			}
		));
		assert_eq!(
			parse_error(&["account", "versioning", "maybe"]),
			clap::error::ErrorKind::InvalidValue
		);
	}

	#[test]
	fn events_limit_must_be_positive() {
		assert!(matches!(
			parsed(&["events", "--filter", "login", "--limit", "3", "--json"]),
			AccountCommands::Events { filter: Some(filter), limit: Some(limit) }
				if filter == "login" && limit.get() == 3
		));
		assert_eq!(
			parse_error(&["events", "--limit", "0"]),
			clap::error::ErrorKind::ValueValidation
		);
	}

	#[test]
	fn free_storage_is_max_minus_used_and_never_negative() {
		assert_eq!(free_storage(300, 1000), 700);
		assert_eq!(free_storage(1000, 1000), 0);
		assert_eq!(free_storage(1500, 1000), 0);
		assert_eq!(free_storage(u64::MAX, 0), 0);
	}

	#[test]
	fn used_percent_is_none_without_storage() {
		assert_eq!(used_percent(0, 0), None);
		assert_eq!(used_percent(5, 0), None);
		assert_eq!(format_percent(used_percent(5, 0)), "-");
	}

	#[test]
	fn used_percent_is_formatted_with_one_decimal() {
		assert_eq!(used_percent(250, 1000), Some(25.0));
		assert_eq!(format_percent(used_percent(1, 3)), "33.3%");
		assert_eq!(format_percent(used_percent(1500, 1000)), "150.0%");
		assert_eq!(format_percent(used_percent(u64::MAX, u64::MAX)), "100.0%");
	}

	#[test]
	fn files_are_counted_with_their_size() {
		assert_eq!(format_files(0, 0), "0 files (0 B)");
		assert_eq!(format_files(1, 1024), "1 file (1 KiB)");
		assert_eq!(format_files(3, 1536), "3 files (1.50 KiB)");
	}

	fn plan(name: &str, end: DateTime<Utc>) -> UserAccountPlan {
		UserAccountPlan {
			cost: 1.0,
			end_timestamp: end,
			id: 1,
			length_type: "monthly".to_owned(),
			name: name.to_owned(),
			storage: 1 << 40,
		}
	}

	fn at(seconds: i64) -> DateTime<Utc> {
		DateTime::from_timestamp(seconds, 0).unwrap()
	}

	#[test]
	fn the_plan_is_the_names_of_the_plans_that_have_not_ended() {
		let plans = [
			plan("Pro I", at(2_000)),
			plan("Old", at(500)),
			plan("Pro I", at(3_000)),
			plan("Lifetime", at(9_000)),
		];
		assert_eq!(plan_label(&plans, true, at(1_000)), "Pro I, Lifetime");
	}

	#[test]
	fn an_account_without_a_current_plan_is_premium_or_free() {
		let plans = [plan("Old", at(500))];
		assert_eq!(plan_label(&plans, true, at(1_000)), "Premium");
		assert_eq!(plan_label(&plans, false, at(1_000)), "Free");
		assert_eq!(plan_label(&[], false, at(1_000)), "Free");
	}

	#[test]
	fn only_the_phrase_confirms_deleting_versions() {
		assert!(confirms_deleting_versions("delete all versions"));
		assert!(confirms_deleting_versions("  delete all versions\n"));
		assert!(!confirms_deleting_versions(""));
		assert!(!confirms_deleting_versions("y"));
		assert!(!confirms_deleting_versions("yes"));
		assert!(!confirms_deleting_versions("Delete all versions"));
		assert!(!confirms_deleting_versions("delete all"));
	}

	#[test]
	fn deleting_versions_without_a_terminal_or_yes_is_refused() {
		let ui = Printed::default().ui();
		let error = ensure_can_confirm_deleting_versions(&ui).unwrap_err();
		assert!(
			matches!(error.downcast_ref(), Some(CliError::Usage(_))),
			"{error:#}"
		);
		assert!(
			error.to_string().contains("Pass --yes to answer yes."),
			"{error:#}"
		);
	}

	#[test]
	fn yes_confirms_deleting_versions_without_asking() {
		let mut ui = Printed::default().ui();
		ui.set_assume_yes(true);
		ensure_can_confirm_deleting_versions(&ui).unwrap();
		// stdin is not a terminal, so asking would fail
		assert!(confirm_deleting_versions(&mut ui).unwrap());
	}

	#[test]
	fn a_terminal_can_confirm_deleting_versions() {
		let ui = Printed::default().prompting_ui();
		ensure_can_confirm_deleting_versions(&ui).unwrap();
	}

	fn folder_meta(name: &str) -> DirectoryMeta<'static> {
		DirectoryMeta::DecryptedUTF8(format!("not json: {name}").into())
	}

	#[test]
	fn a_rename_shows_the_old_and_new_name() {
		assert_eq!(
			renamed(Some("a.txt"), Some("b.txt")).as_deref(),
			Some("a.txt -> b.txt")
		);
		assert_eq!(renamed(Some("a"), Some("a")).as_deref(), Some("a"));
		assert_eq!(renamed(None, Some("b")).as_deref(), Some("b"));
		assert_eq!(renamed(Some("a"), None).as_deref(), Some("a"));
		assert_eq!(renamed(None, None), None);
	}

	#[test]
	fn an_event_line_has_the_time_type_and_summary() {
		let event = DecryptedUserEvent {
			id: 7,
			timestamp: at(1_700_000_000),
			uuid: Uuid::from_u128(1),
			kind: Kind::RemovedSharedOutItems(
				filen_sdk_rs::user::events::UserEventRemovedSharedOutItemsInfo {
					ip: "192.0.2.1".to_owned(),
					user_agent: "agent".to_owned(),
					count: 2,
					receiver_email: "friend@example.com".to_owned(),
				},
			),
		};
		assert_eq!(
			event_line(&event),
			"2023-11-14 22:13:20 (UTC)  removedSharedOutItems  2 items shared with friend@example.com"
		);
		assert_eq!(
			event_json(&event),
			json!({
				"id": 7,
				"timestamp": "2023-11-14T22:13:20Z",
				"uuid": "00000000-0000-0000-0000-000000000001",
				"type": "removedSharedOutItems",
				"summary": "2 items shared with friend@example.com",
			})
		);
	}

	#[test]
	fn an_event_whose_names_cannot_be_decrypted_has_no_summary() {
		let event = DecryptedUserEvent {
			id: 8,
			timestamp: at(1_700_000_000),
			uuid: Uuid::from_u128(2),
			kind: Kind::FolderRenamed(UserEventFolderPairInfo {
				ip: "192.0.2.1".to_owned(),
				user_agent: "agent".to_owned(),
				name: folder_meta("new"),
				old_name: folder_meta("old"),
				uuid: None,
			}),
		};
		assert_eq!(
			event_line(&event),
			"2023-11-14 22:13:20 (UTC)  folderRenamed"
		);
		assert_eq!(event_json(&event)["summary"], serde_json::Value::Null);
	}

	/// How many events the fake server returns per page.
	const PAGE_SIZE: usize = 100;

	/// The event with this id, `per_second` events sharing each second, newest (id 0) first.
	fn paged_event(id: u64, per_second: u64) -> DecryptedUserEvent {
		DecryptedUserEvent {
			id,
			timestamp: at(10_000 - i64::try_from(id / per_second).unwrap()),
			uuid: Uuid::from_u128(u128::from(id)),
			kind: Kind::RemovedSharedOutItems(
				filen_sdk_rs::user::events::UserEventRemovedSharedOutItemsInfo {
					ip: "192.0.2.1".to_owned(),
					user_agent: "agent".to_owned(),
					count: 1,
					receiver_email: "friend@example.com".to_owned(),
				},
			),
		}
	}

	/// What the server answers for `total` events, those with an id in `unreadable` failing to
	/// decrypt: the newest page strictly before the cutoff.
	fn page_before(
		event: impl Fn(u64) -> DecryptedUserEvent,
		total: u64,
		unreadable: &[u64],
		cutoff: Option<DateTime<Utc>>,
	) -> Vec<Result<DecryptedUserEvent, String>> {
		(0..total)
			.map(event)
			.filter(|event| cutoff.is_none_or(|cutoff| event.timestamp < cutoff))
			.take(PAGE_SIZE)
			.map(|event| {
				if unreadable.contains(&event.id) {
					Err(format!("event {} is unreadable", event.id))
				} else {
					Ok(event)
				}
			})
			.collect()
	}

	/// Collects with a fake server over `total` events, returning the cutoffs it was asked for.
	async fn collect_from(
		limit: usize,
		max_pages: usize,
		event: impl Fn(u64) -> DecryptedUserEvent,
		total: u64,
		unreadable: &[u64],
	) -> (CollectedEvents, Vec<Option<DateTime<Utc>>>) {
		let mut cutoffs = Vec::new();
		let collected = collect_events(limit, max_pages, async |cutoff| {
			cutoffs.push(cutoff);
			Ok(page_before(&event, total, unreadable, cutoff))
		})
		.await
		.unwrap();
		(collected, cutoffs)
	}

	/// Collects with a fake server over `total` readable events, `per_second` sharing each second.
	async fn collect_from_fake(
		limit: usize,
		max_pages: usize,
		total: u64,
		per_second: u64,
	) -> (CollectedEvents, Vec<Option<DateTime<Utc>>>) {
		collect_from(
			limit,
			max_pages,
			|id| paged_event(id, per_second),
			total,
			&[],
		)
		.await
	}

	/// 50 events a second for three seconds (ids 0 to 149), then the ids from 150 to
	/// `crowded_end` in the second 9000, then one event a second before that.
	fn event_with_crowded_second(id: u64, crowded_end: u64) -> DecryptedUserEvent {
		let mut event = paged_event(id, 50);
		if (150..crowded_end).contains(&id) {
			event.timestamp = at(9_000);
		} else if id >= crowded_end {
			event.timestamp = at(9_000 - i64::try_from(id - crowded_end + 1).unwrap());
		}
		event
	}

	fn ids(collected: &CollectedEvents) -> Vec<u64> {
		collected.events.iter().map(|event| event.id).collect()
	}

	#[tokio::test]
	async fn events_limit_pages_back_without_duplicates() {
		// three events per second: the first page ends at id 99, whose second also holds 100
		// and 101, so the second page starts again at 99
		let (collected, cutoffs) = collect_from_fake(150, MAX_EVENT_PAGES, 300, 3).await;
		assert_eq!(ids(&collected), (0..150).collect::<Vec<_>>());
		assert_eq!(cutoffs, [None, Some(at(10_000 - 33 + 1))]);
		assert_eq!(collected.unreadable, 0);
		assert_eq!(collected.cut_short, None);
		assert!(collected.stuck_seconds.is_empty());
	}

	#[tokio::test]
	async fn events_paging_moves_past_a_second_holding_more_than_a_page() {
		// every event in one second: the second page is the first one again, and the third,
		// strictly before that second, is empty
		let (collected, cutoffs) = collect_from_fake(500, MAX_EVENT_PAGES, 300, 300).await;
		assert_eq!(ids(&collected), (0..100).collect::<Vec<_>>());
		assert_eq!(cutoffs, [None, Some(at(10_001)), Some(at(10_000))]);
		assert_eq!(collected.cut_short, None);
		assert_eq!(collected.stuck_seconds, [at(10_000)]);
	}

	#[tokio::test]
	async fn events_paging_lists_the_events_older_than_a_crowded_second() {
		// 250 events in the second 9000, then 50 older ones: once paging reaches that second,
		// the next page repeats its newest 100 and adds nothing, so paging carries on before it
		let (collected, cutoffs) = collect_from(
			1_000,
			MAX_EVENT_PAGES,
			|id| event_with_crowded_second(id, 400),
			450,
			&[],
		)
		.await;
		assert_eq!(
			ids(&collected),
			(0..250).chain(400..450).collect::<Vec<_>>()
		);
		assert_eq!(
			cutoffs,
			[
				None,
				Some(at(10_000)),
				Some(at(9_999)),
				Some(at(9_001)),
				Some(at(9_001)),
				Some(at(9_000)),
				Some(at(8_951)),
			]
		);
		assert_eq!(collected.stuck_seconds, [at(9_000)]);
		assert_eq!(collected.cut_short, None);
	}

	#[tokio::test]
	async fn events_paging_compares_page_sizes_with_the_unreadable_events_counted() {
		// the first page holds 5 unreadable events, so only 95 readable ones; the stuck page
		// in the crowded second is a full 100 and must still count as stuck
		let (collected, cutoffs) = collect_from(
			1_000,
			MAX_EVENT_PAGES,
			|id| event_with_crowded_second(id, 400),
			400,
			&[0, 1, 2, 3, 4],
		)
		.await;
		assert_eq!(ids(&collected), (5..250).collect::<Vec<_>>());
		assert_eq!(collected.unreadable, 5);
		assert_eq!(collected.stuck_seconds, [at(9_000)]);
		assert_eq!(cutoffs.last(), Some(&Some(at(9_000))));
		assert_eq!(collected.cut_short, None);
	}

	#[tokio::test]
	async fn events_paging_stops_short_at_a_page_of_only_unreadable_events() {
		// one event a second, 100 to 199 unreadable: the second page adds only unreadable
		// events, and the third holds nothing else, so there is no timestamp to page back from
		let unreadable: Vec<u64> = (100..200).collect();
		let (collected, cutoffs) = collect_from(
			1_000,
			MAX_EVENT_PAGES,
			|id| paged_event(id, 1),
			300,
			&unreadable,
		)
		.await;
		assert_eq!(ids(&collected), (0..100).collect::<Vec<_>>());
		assert_eq!(
			cutoffs,
			[None, Some(at(10_000 - 99 + 1)), Some(at(10_000 - 99))]
		);
		assert_eq!(collected.cut_short, Some(CutShort::Unreadable));
		assert!(collected.stuck_seconds.is_empty());
	}

	#[tokio::test]
	async fn events_paging_does_not_take_a_page_adding_only_unreadable_events_for_stuck() {
		// one event a second, 100 to 198 unreadable: the second page repeats event 99 and adds
		// only unreadable events, which are older, so paging carries on to the readable ones
		let unreadable: Vec<u64> = (100..199).collect();
		let (collected, _) = collect_from(
			1_000,
			MAX_EVENT_PAGES,
			|id| paged_event(id, 1),
			300,
			&unreadable,
		)
		.await;
		assert_eq!(
			ids(&collected),
			(0..100).chain(199..300).collect::<Vec<_>>()
		);
		assert_eq!(collected.unreadable, 198);
		assert!(collected.stuck_seconds.is_empty());
		assert_eq!(collected.cut_short, None);
	}

	#[tokio::test]
	async fn events_paging_stops_when_the_events_run_out() {
		let (collected, cutoffs) = collect_from_fake(500, MAX_EVENT_PAGES, 150, 1).await;
		assert_eq!(ids(&collected), (0..150).collect::<Vec<_>>());
		assert_eq!(cutoffs.len(), 3);
		assert_eq!(collected.cut_short, None);
		assert!(collected.stuck_seconds.is_empty());
	}

	#[tokio::test]
	async fn events_history_ending_on_a_full_page_does_not_warn() {
		// exactly one full page, one event a second: the second page repeats only the oldest
		// event, a smaller page than the first, so it is the end and not a stuck second
		let (collected, cutoffs) = collect_from_fake(500, MAX_EVENT_PAGES, 100, 1).await;
		assert_eq!(ids(&collected), (0..100).collect::<Vec<_>>());
		assert_eq!(cutoffs, [None, Some(at(10_000 - 99 + 1))]);
		assert_eq!(collected.cut_short, None);
		assert!(collected.stuck_seconds.is_empty());
	}

	#[tokio::test]
	async fn events_paging_stops_at_the_page_cap() {
		let (collected, cutoffs) = collect_from_fake(1_000, 2, 3_000, 3).await;
		assert_eq!(ids(&collected), (0..199).collect::<Vec<_>>());
		assert_eq!(cutoffs.len(), 2);
		assert_eq!(collected.cut_short, Some(CutShort::Capped));
	}
}
