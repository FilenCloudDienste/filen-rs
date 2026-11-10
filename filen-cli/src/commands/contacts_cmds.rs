//! [cli-doc] contacts-overview
//! Your contacts are the accounts you can share files and directories with. To add one, send them
//! a contact request with `contact-request <email>`; once they accept it, they are listed by
//! `contacts`.
//!
//! `contacts` lists your contacts and the contact requests that are still pending, both the ones
//! sent to you and the ones you sent. `contacts --blocked` lists the accounts you blocked instead.
//! With `--json`, `contacts` prints an object with the arrays `contacts`, `incomingRequests` and
//! `outgoingRequests`, and `contacts --blocked` prints an array.
//!
//! Every other contact command takes the email address of the account it concerns, which it looks
//! up in the list it acts on (case does not matter), so you never need a UUID; the UUID printed by
//! `contacts --json` is accepted as well. `contact-accept` and `contact-deny` answer a request sent
//! to you, `contact-cancel` withdraws one you sent, `contact-remove` removes a contact (after
//! asking to confirm, which `--yes` answers), and `contact-block` / `contact-unblock` block an
//! account from sending you requests or lift that block.

use anyhow::{Context as _, Result};
use clap::Subcommand;
use filen_types::{
	api::v3::contacts::{
		Contact,
		blocked::BlockedContact,
		requests::{r#in::ContactRequestIn, out::ContactRequestOut},
	},
	fs::Uuid,
};
use serde_json::json;

use crate::{
	CommandResult,
	auth::LazyClient,
	commands::CommandContext,
	docs::DocElement,
	ui::{self, UI},
};

#[derive(Debug, Subcommand)]
pub(crate) enum ContactsCommands {
	/// List your contacts and pending contact requests
	Contacts {
		/// List the accounts you blocked instead
		#[arg(long)]
		blocked: bool,
	},
	/// Send a contact request
	ContactRequest {
		/// The email address of the account to send it to
		// not `email`, which is the id of the global --email option
		#[arg(id = "contact_email", value_name = "EMAIL")]
		email: String,
	},
	/// Accept a contact request sent to you
	ContactAccept {
		/// The email address (or UUID) of the request
		request: String,
	},
	/// Deny a contact request sent to you
	ContactDeny {
		/// The email address (or UUID) of the request
		request: String,
	},
	/// Withdraw a contact request you sent
	ContactCancel {
		/// The email address (or UUID) of the request
		request: String,
	},
	/// Remove a contact (asks to confirm, which --yes answers)
	ContactRemove {
		/// The email address (or UUID) of the contact
		contact: String,
	},
	/// Block an account from sending you contact requests
	ContactBlock {
		/// The email address of the account
		#[arg(id = "contact_email", value_name = "EMAIL")]
		email: String,
	},
	/// Lift a block
	ContactUnblock {
		/// The email address (or UUID) of the blocked account
		blocked: String,
	},
}

/// The contact commands' docs, placed in the "Sharing" section.
pub(crate) const DOCS: &[DocElement] = &[
	DocElement::DocFragment("contacts-overview"),
	DocElement::CommandHelp("contacts"),
	DocElement::CommandHelp("contact-request"),
	DocElement::CommandHelp("contact-accept"),
	DocElement::CommandHelp("contact-deny"),
	DocElement::CommandHelp("contact-cancel"),
	DocElement::CommandHelp("contact-remove"),
	DocElement::CommandHelp("contact-block"),
	DocElement::CommandHelp("contact-unblock"),
];

impl ContactsCommands {
	pub(super) async fn execute(self, ctx: CommandContext<'_>) -> Result<CommandResult> {
		let CommandContext { ui, client, .. } = ctx;
		match self {
			Self::Contacts { blocked: false } => print_contacts(ui, client).await?,
			Self::Contacts { blocked: true } => print_blocked(ui, client).await?,
			Self::ContactRequest { email } => {
				let client = client.get(ui).await?;
				client
					.send_contact_request(email.trim())
					.await
					.with_context(|| format!("Failed to send a contact request to {email}"))?;
				ui.print_success(&format!("Sent a contact request to {}", email.trim()));
			}
			Self::ContactAccept { request } => {
				let client = client.get(ui).await?;
				let requests = client
					.list_incoming_contact_requests()
					.await
					.context("Failed to list incoming contact requests")?;
				let request = find_entry(&requests, &request).ok_or_else(|| {
					UI::failure(&format!("No contact request from {request} to accept"))
				})?;
				client
					.accept_contact_request(request.uuid)
					.await
					.context("Failed to accept the contact request")?;
				ui.print_success(&format!(
					"Accepted the contact request from {}",
					request.email
				));
			}
			Self::ContactDeny { request } => {
				let client = client.get(ui).await?;
				let requests = client
					.list_incoming_contact_requests()
					.await
					.context("Failed to list incoming contact requests")?;
				let request = find_entry(&requests, &request).ok_or_else(|| {
					UI::failure(&format!("No contact request from {request} to deny"))
				})?;
				client
					.deny_contact_request(request.uuid)
					.await
					.context("Failed to deny the contact request")?;
				ui.print_success(&format!(
					"Denied the contact request from {}",
					request.email
				));
			}
			Self::ContactCancel { request } => {
				let client = client.get(ui).await?;
				let requests = client
					.list_outgoing_contact_requests()
					.await
					.context("Failed to list outgoing contact requests")?;
				let request = find_entry(&requests, &request).ok_or_else(|| {
					UI::failure(&format!("No contact request to {request} to withdraw"))
				})?;
				client
					.cancel_contact_request(request.uuid)
					.await
					.context("Failed to withdraw the contact request")?;
				ui.print_success(&format!(
					"Withdrew the contact request to {}",
					request.email
				));
			}
			Self::ContactRemove { contact } => remove_contact(ui, client, &contact).await?,
			Self::ContactBlock { email } => {
				let client = client.get(ui).await?;
				client
					.block_contact(email.trim())
					.await
					.with_context(|| format!("Failed to block {email}"))?;
				ui.print_success(&format!("Blocked {}", email.trim()));
			}
			Self::ContactUnblock { blocked } => {
				let client = client.get(ui).await?;
				let blocks = client
					.get_blocked_contacts()
					.await
					.context("Failed to list blocked accounts")?;
				let block = find_entry(&blocks, &blocked)
					.ok_or_else(|| UI::failure(&format!("{blocked} is not blocked")))?;
				client
					.unblock_contact(block.uuid)
					.await
					.with_context(|| format!("Failed to unblock {}", block.email))?;
				ui.print_success(&format!("Unblocked {}", block.email));
			}
		}
		Ok(CommandResult::default())
	}
}

/// A contact, contact request or block, which a command names by its email address or UUID.
pub(crate) trait ContactEntry {
	fn uuid(&self) -> Uuid;
	fn email(&self) -> &str;
}

impl ContactEntry for Contact<'_> {
	fn uuid(&self) -> Uuid {
		self.uuid
	}
	fn email(&self) -> &str {
		&self.email
	}
}

impl ContactEntry for ContactRequestIn<'_> {
	fn uuid(&self) -> Uuid {
		self.uuid
	}
	fn email(&self) -> &str {
		&self.email
	}
}

impl ContactEntry for ContactRequestOut<'_> {
	fn uuid(&self) -> Uuid {
		self.uuid
	}
	fn email(&self) -> &str {
		&self.email
	}
}

impl ContactEntry for BlockedContact<'_> {
	fn uuid(&self) -> Uuid {
		self.uuid
	}
	fn email(&self) -> &str {
		&self.email
	}
}

/// The entry `query` names: the one with that UUID, else the one with that email address, in any
/// case. An account appears at most once in each list, so there is never more than one.
pub(crate) fn find_entry<'a, E: ContactEntry>(entries: &'a [E], query: &str) -> Option<&'a E> {
	let query = query.trim();
	if let Ok(uuid) = query.parse::<Uuid>()
		&& let Some(entry) = entries.iter().find(|entry| entry.uuid() == uuid)
	{
		return Some(entry);
	}
	entries
		.iter()
		.find(|entry| entry.email().eq_ignore_ascii_case(query))
}

/// An email address with the nickname after it, when there is one.
fn describe(email: &str, nick_name: Option<&str>) -> String {
	match nick_name.filter(|nick_name| !nick_name.is_empty()) {
		Some(nick_name) => format!("{email} ({nick_name})"),
		None => email.to_owned(),
	}
}

fn contact_json(contact: &Contact<'_>) -> serde_json::Value {
	json!({
		"uuid": contact.uuid.to_string(),
		"userId": contact.user_id,
		"email": contact.email,
		"nickName": contact.nick_name,
		"lastActive": contact.last_active,
		"timestamp": contact.timestamp,
	})
}

fn incoming_request_json(request: &ContactRequestIn<'_>) -> serde_json::Value {
	json!({
		"uuid": request.uuid.to_string(),
		"userId": request.user_id,
		"email": request.email,
		"nickName": non_empty(&request.nick_name),
	})
}

fn outgoing_request_json(request: &ContactRequestOut<'_>) -> serde_json::Value {
	json!({
		"uuid": request.uuid.to_string(),
		"email": request.email,
		"nickName": non_empty(&request.nick_name),
	})
}

fn blocked_json(blocked: &BlockedContact<'_>) -> serde_json::Value {
	json!({
		"uuid": blocked.uuid.to_string(),
		"userId": blocked.user_id,
		"email": blocked.email,
		"nickName": non_empty(&blocked.nick_name),
		"timestamp": blocked.timestamp,
	})
}

/// A nickname as JSON, `null` when it is empty (the server sends an empty one for none).
fn non_empty(nick_name: &str) -> Option<&str> {
	(!nick_name.is_empty()).then_some(nick_name)
}

/// The lines `contacts` prints: a heading for each non-empty list, then its entries.
fn contacts_lines(
	contacts: &[Contact<'_>],
	incoming: &[ContactRequestIn<'_>],
	outgoing: &[ContactRequestOut<'_>],
) -> Vec<String> {
	let sections = [
		(
			"Contacts",
			contacts
				.iter()
				.map(|c| describe(&c.email, c.nick_name.as_deref()))
				.collect::<Vec<_>>(),
		),
		(
			"Contact requests sent to you (contact-accept / contact-deny)",
			incoming
				.iter()
				.map(|r| describe(&r.email, Some(&r.nick_name)))
				.collect(),
		),
		(
			"Contact requests you sent (contact-cancel)",
			outgoing
				.iter()
				.map(|r| describe(&r.email, Some(&r.nick_name)))
				.collect(),
		),
	];
	let mut lines = Vec::new();
	for (heading, entries) in sections {
		if entries.is_empty() {
			continue;
		}
		lines.push(format!("{heading}:"));
		lines.extend(entries.into_iter().map(|entry| format!("  {entry}")));
	}
	lines
}

async fn print_contacts(ui: &mut UI, client: &mut LazyClient) -> Result<()> {
	let client = client.get(ui).await?;
	let (contacts, incoming, outgoing) = futures::try_join!(
		async {
			client
				.get_contacts()
				.await
				.context("Failed to list contacts")
		},
		async {
			client
				.list_incoming_contact_requests()
				.await
				.context("Failed to list incoming contact requests")
		},
		async {
			client
				.list_outgoing_contact_requests()
				.await
				.context("Failed to list outgoing contact requests")
		},
	)?;
	if ui.json {
		return ui.print_json(json!({
			"contacts": contacts.iter().map(contact_json).collect::<Vec<_>>(),
			"incomingRequests": incoming.iter().map(incoming_request_json).collect::<Vec<_>>(),
			"outgoingRequests": outgoing.iter().map(outgoing_request_json).collect::<Vec<_>>(),
		}));
	}
	let lines = contacts_lines(&contacts, &incoming, &outgoing);
	if lines.is_empty() {
		ui.print_muted("No contacts and no pending contact requests");
	}
	for line in lines {
		ui.print(&line);
	}
	Ok(())
}

async fn print_blocked(ui: &mut UI, client: &mut LazyClient) -> Result<()> {
	let client = client.get(ui).await?;
	let blocks = client
		.get_blocked_contacts()
		.await
		.context("Failed to list blocked accounts")?;
	if ui.json {
		return ui.print_json(serde_json::Value::Array(
			blocks.iter().map(blocked_json).collect(),
		));
	}
	if blocks.is_empty() {
		ui.print_muted("No blocked accounts");
	}
	for blocked in &blocks {
		ui.print(&format!(
			"{}  blocked {}",
			describe(&blocked.email, Some(&blocked.nick_name)),
			ui::format_date(&blocked.timestamp)
		));
	}
	Ok(())
}

/// The confirmation asked before removing a contact.
fn remove_question(email: &str) -> String {
	format!("Remove {email} from your contacts?")
}

/// Fails unless removing the contact `query` names can be confirmed, by `--yes` or at a terminal.
/// Runs before logging in, so that a script learns it needs `--yes` before anything is looked up.
fn ensure_can_remove(ui: &UI, query: &str) -> Result<()> {
	ui.ensure_can_confirm(&remove_question(query.trim()))
}

async fn remove_contact(ui: &mut UI, client: &mut LazyClient, query: &str) -> Result<()> {
	ensure_can_remove(ui, query)?;
	let client = client.get(ui).await?;
	let contacts = client
		.get_contacts()
		.await
		.context("Failed to list contacts")?;
	let contact = find_entry(&contacts, query)
		.ok_or_else(|| UI::failure(&format!("{} is not a contact", query.trim())))?;
	if !ui.prompt_confirm(&remove_question(&contact.email), false)? {
		return Ok(());
	}
	client
		.delete_contact(contact.uuid)
		.await
		.with_context(|| format!("Failed to remove {}", contact.email))?;
	ui.print_success(&format!("Removed {} from your contacts", contact.email));
	Ok(())
}

#[cfg(test)]
mod tests {
	use std::borrow::Cow;

	use super::*;
	use crate::{commands::Commands, error::exit_code, parse_args, ui::test_support::Printed};

	const A: Uuid = Uuid::from_u128(1);
	const B: Uuid = Uuid::from_u128(2);
	const C: Uuid = Uuid::from_u128(3);

	fn request(uuid: Uuid, email: &'static str) -> ContactRequestOut<'static> {
		ContactRequestOut {
			uuid,
			email: Cow::Borrowed(email),
			avatar: None,
			nick_name: Cow::Borrowed(""),
		}
	}

	fn requests() -> [ContactRequestOut<'static>; 2] {
		[
			request(A, "alice@example.com"),
			request(B, "Bob@Example.com"),
		]
	}

	#[test]
	fn an_entry_is_found_by_its_email_in_any_case() {
		let requests = requests();
		assert_eq!(
			find_entry(&requests, "alice@example.com").map(|r| r.uuid),
			Some(A)
		);
		assert_eq!(
			find_entry(&requests, "bob@example.COM").map(|r| r.uuid),
			Some(B)
		);
		assert_eq!(
			find_entry(&requests, " alice@example.com\n").map(|r| r.uuid),
			Some(A)
		);
		assert!(find_entry(&requests, "carol@example.com").is_none());
		assert!(find_entry(&requests, "alice").is_none());
		assert!(find_entry(&requests, "").is_none());
	}

	#[test]
	fn an_entry_is_found_by_its_uuid() {
		let requests = requests();
		assert_eq!(
			find_entry(&requests, &B.to_string()).map(|r| r.uuid),
			Some(B)
		);
		assert_eq!(
			find_entry(&requests, &A.simple().to_string()).map(|r| r.uuid),
			Some(A)
		);
		assert!(find_entry(&requests, &C.to_string()).is_none());
	}

	#[test]
	fn nothing_is_found_in_an_empty_list() {
		assert!(find_entry::<ContactRequestOut<'_>>(&[], "alice@example.com").is_none());
		assert!(find_entry::<ContactRequestOut<'_>>(&[], &A.to_string()).is_none());
	}

	#[test]
	fn a_nickname_follows_the_email_when_there_is_one() {
		assert_eq!(describe("a@x.io", Some("Alice")), "a@x.io (Alice)");
		assert_eq!(describe("a@x.io", Some("")), "a@x.io");
		assert_eq!(describe("a@x.io", None), "a@x.io");
	}

	#[test]
	fn contacts_lines_list_only_the_non_empty_sections() {
		let outgoing = requests();
		assert_eq!(
			contacts_lines(&[], &[], &outgoing),
			[
				"Contact requests you sent (contact-cancel):",
				"  alice@example.com",
				"  Bob@Example.com",
			]
		);
		let incoming = [ContactRequestIn {
			uuid: C,
			user_id: 7,
			email: Cow::Borrowed("carol@example.com"),
			avatar: None,
			nick_name: Cow::Borrowed("Carol"),
		}];
		assert_eq!(
			contacts_lines(&[], &incoming, &[]),
			[
				"Contact requests sent to you (contact-accept / contact-deny):",
				"  carol@example.com (Carol)",
			]
		);
		assert!(contacts_lines(&[], &[], &[]).is_empty());
	}

	#[test]
	fn request_json_reports_an_empty_nickname_as_null() {
		assert_eq!(
			outgoing_request_json(&request(A, "alice@example.com")),
			json!({
				"uuid": A.to_string(),
				"email": "alice@example.com",
				"nickName": null,
			})
		);
	}

	// The 2048-bit SubjectPublicKeyInfo, base64, that filen-types' `serde::rsa` tests and the
	// SDK's crypto known-answer tests parse; rsa refuses keys under 1024 bits.
	const PUBLIC_KEY_SPKI: &str = "MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEAvg+AOhimAL4YHrPu9rqEZC2MUcT7vKWXWlGGyd2dtXhF6reeJvendCiPixh/pDXM+gFWHiwfcyF8DuKl954hvX2TZCA3+ipgrU02BV7yndT/yjwzy7SdfN1qQt2P6fLeRQ4FKSnENouFrDAuxG05Z02i151MZWArbTNFSfDwlicvq1+kqYv5D28M7tH7lwB0qh6SXCLloE5m1480U3elVkDwwlqaxyFiDg1uvaWnIfB3J3dHXuckq2W34Nmyl3HRvvY9ymXFFmi+D/LF0PyAR+Y8srkvWc2KgAR8brQuzzaXwwjy0PJ3U8fGOemIhOYzHYZl+p5QKmKOxvoWIVzWKwIDAQAB";

	#[test]
	fn contact_and_block_json_dates_are_utc_with_z() {
		let contact: Contact<'_> = serde_json::from_value(json!({
			"uuid": A.to_string(),
			"userId": 7,
			"email": "alice@example.com",
			"avatar": null,
			"nickName": "",
			"lastActive": 1_700_000_000,
			"timestamp": 1_700_000_000,
			"publicKey": PUBLIC_KEY_SPKI,
		}))
		.unwrap();
		let json = contact_json(&contact);
		assert_eq!(json["lastActive"], "2023-11-14T22:13:20Z");
		assert_eq!(json["timestamp"], "2023-11-14T22:13:20Z");
		let blocked: BlockedContact<'_> = serde_json::from_value(json!({
			"uuid": B.to_string(),
			"userId": 8,
			"email": "bob@example.com",
			"avatar": null,
			"nickName": "",
			"timestamp": 1_700_000_000,
		}))
		.unwrap();
		assert_eq!(blocked_json(&blocked)["timestamp"], "2023-11-14T22:13:20Z");
	}

	fn parsed(args: &[&str]) -> Result<ContactsCommands, clap::error::ErrorKind> {
		let command = std::iter::once("filen").chain(args.iter().copied());
		match parse_args(command).map_err(|e| e.kind())?.args.command {
			Some(Commands::Contacts(command)) => Ok(command),
			other => panic!("parsed {other:?}"),
		}
	}

	#[test]
	fn the_address_of_contact_request_and_block_is_not_the_login_email() {
		for command in ["contact-request", "contact-block"] {
			let args = ["filen", command, "a@x.io"];
			assert_eq!(parse_args(args).unwrap().args.email, None, "{command}");
			assert!(matches!(
				parsed(&args[1..]),
				Ok(ContactsCommands::ContactRequest { email } | ContactsCommands::ContactBlock { email })
					if email == "a@x.io"
			));
		}
	}

	#[test]
	fn the_contact_commands_parse() {
		assert!(matches!(
			parsed(&["contacts"]),
			Ok(ContactsCommands::Contacts { blocked: false })
		));
		assert!(matches!(
			parsed(&["contacts", "--blocked"]),
			Ok(ContactsCommands::Contacts { blocked: true })
		));
		assert!(matches!(
			parsed(&["contact-remove", "a@x.io", "--yes"]),
			Ok(ContactsCommands::ContactRemove { contact }) if contact == "a@x.io"
		));
		assert_eq!(
			parsed(&["contact-request"]).err(),
			Some(clap::error::ErrorKind::MissingRequiredArgument)
		);
	}

	#[test]
	fn contact_remove_without_a_terminal_or_yes_is_refused() {
		let mut ui = Printed::default().ui();
		let err = ensure_can_remove(&ui, " a@x.io ").unwrap_err();
		assert_eq!(exit_code(&err), 2);
		assert_eq!(
			err.to_string(),
			"Cannot ask \"Remove a@x.io from your contacts?\": stdin is not a terminal. Pass --yes \
			 to answer yes."
		);
		ui.set_assume_yes(true);
		ensure_can_remove(&ui, "a@x.io").unwrap();
	}
}
