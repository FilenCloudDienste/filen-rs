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

use anyhow::{Context as _, Result};
use chrono::{DateTime, Utc};
use clap::{Subcommand, ValueEnum};
use filen_sdk_rs::user::UserInfo;
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

#[cfg(test)]
mod tests {
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
}
