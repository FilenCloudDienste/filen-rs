//! Live tests of the contact and share commands, between the test account and the share account.
//! Each takes the standing contact between them (`ensure_contact`, which sends no request when
//! the accounts are contacts already), shares items of its own test directory, and stops every
//! share it made, whether it passes or not. The one exception is the contact-request flow, which
//! removes the standing contact first and sets it up again at the end.

use std::{borrow::Cow, future::Future, panic::AssertUnwindSafe, time::Duration};

use assert_fs::prelude::{FileWriteStr as _, PathChild as _};
use filen_macros::shared_test_runtime;
use filen_sdk_rs::{
	auth::Client,
	connect::fs::{SharedRootDirectory, SharedRootFile},
	fs::{
		HasName as _, HasUUID as _,
		categories::{RootItemType, Shared},
	},
	io::{RemoteDirectory, RemoteFile},
};
use filen_types::fs::Uuid;
use futures::FutureExt as _;
use serde_json::Value;
use test_utils::authenticated_cli_with_args;

fn stdout_of(assert: &assert_cmd::assert::Assert) -> String {
	String::from_utf8(assert.get_output().stdout.clone()).unwrap()
}

fn stderr_of(assert: &assert_cmd::assert::Assert) -> String {
	String::from_utf8(assert.get_output().stderr.clone()).unwrap()
}

fn json_of(assert: &assert_cmd::assert::Assert) -> Value {
	let stdout = stdout_of(assert);
	serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("{e}: {stdout}"))
}

/// Runs the CLI logged in as the share account.
fn share_account_cli(share_client: &Client, args: &[&str]) -> assert_cmd::assert::Assert {
	let temp_dir = assert_fs::TempDir::new().unwrap();
	let auth_config = temp_dir.child("filen-cli-auth-config");
	auth_config
		.write_str(&filen_cli::serialize_auth_config(share_client).unwrap())
		.unwrap();
	let assert = assert_cmd::cargo::cargo_bin_cmd!("filen-cli")
		.args(["--auth-config-path", auth_config.to_str().unwrap(), "-v"])
		.args(args)
		.assert();
	// kept until the CLI has read the credentials, then deleted with them
	drop(temp_dir);
	assert
}

async fn upload_file(client: &Client, parent: &RemoteDirectory, name: &str) -> RemoteFile {
	let file = client.make_file_builder(name, parent.uuid()).unwrap();
	client.upload_file(file, b"shared").await.unwrap()
}

/// The absolute path of `name` in the test directory, which is a child of the root.
fn path_in(test_dir: &RemoteDirectory, name: &str) -> String {
	format!("/{}/{name}", test_dir.name().unwrap())
}

/// The entry with this UUID in a JSON array of items.
fn listed(json: &Value, uuid: Uuid) -> Option<&Value> {
	json.as_array()
		.unwrap()
		.iter()
		.find(|item| item["uuid"] == uuid.to_string())
}

async fn out_shares_of(
	client: &Client,
	uuids: &[Uuid],
) -> (Vec<SharedRootDirectory>, Vec<SharedRootFile>) {
	let (dirs, files) = client
		.list_out_shared(None, None::<&fn(u64, Option<u64>)>)
		.await
		.unwrap();
	(
		dirs.into_iter()
			.filter(|dir| uuids.contains(&dir.uuid()))
			.collect(),
		files
			.into_iter()
			.filter(|file| uuids.contains(&file.uuid()))
			.collect(),
	)
}

/// How long the server may go on listing a share after it answered its removal. Its share
/// listings catch up with a removal only shortly after it: a listing sent right after one still
/// held the removed share in about half of the tries, for up to about 160 ms.
const SHARE_REMOVAL_SETTLE: Duration = Duration::from_secs(10);

/// Waits until neither what `client` shared nor what `share_client` was shared lists an item in
/// `uuids`, once their shares were removed, and fails when they are still listed after
/// [`SHARE_REMOVAL_SETTLE`].
async fn wait_until_unshared(client: &Client, share_client: &Client, uuids: &[Uuid]) {
	let deadline = tokio::time::Instant::now() + SHARE_REMOVAL_SETTLE;
	loop {
		let (out_dirs, out_files) = out_shares_of(client, uuids).await;
		let (in_dirs, in_files) = share_client
			.list_in_shared_root(None::<&fn(u64, Option<u64>)>)
			.await
			.unwrap();
		let still_in = in_dirs
			.iter()
			.map(|dir| dir.uuid())
			.chain(in_files.iter().map(|file| file.uuid()))
			.filter(|uuid| uuids.contains(uuid))
			.count();
		if out_dirs.is_empty() && out_files.is_empty() && still_in == 0 {
			return;
		}
		assert!(
			tokio::time::Instant::now() < deadline,
			"{} out-shares and {still_in} in-shares of {uuids:?} still listed {SHARE_REMOVAL_SETTLE:?} \
			 after their removal",
			out_dirs.len() + out_files.len()
		);
		tokio::time::sleep(Duration::from_millis(100)).await;
	}
}

/// Runs `test` while the two accounts are contacts, then stops sharing every item in `uuids`,
/// even when the test panicked, and passes the panic on.
async fn with_contact_and_shares_cleaned_up<F, Fut>(
	client: &Client,
	share_client: &Client,
	uuids: &[Uuid],
	test: F,
) where
	F: FnOnce() -> Fut,
	Fut: Future<Output = ()>,
{
	let (_contact, _locks) = test_utils::ensure_contact(client, share_client).await;
	let result = AssertUnwindSafe(test()).catch_unwind().await;
	let (dirs, files) = out_shares_of(client, uuids).await;
	let items = dirs
		.into_iter()
		.map(|dir| RootItemType::<Shared>::Dir(Cow::Owned(dir)))
		.chain(
			files
				.into_iter()
				.map(|file| RootItemType::<Shared>::File(Cow::Owned(file))),
		);
	for item in items {
		client.remove_shared_item(&item).await.unwrap();
	}
	if let Err(panic) = result {
		std::panic::resume_unwind(panic);
	}
}

#[shared_test_runtime]
async fn share_list_and_unshare_a_file_and_a_directory() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;
	let share_client = test_utils::SHARE_RESOURCES.client().await;
	let share_email = share_client.email().to_owned();

	// names only this run uses, as the share account's shared items include other runs'
	let file_name = format!("{}.txt", test_dir.name().unwrap());
	let dir_name = format!("{}-dir", test_dir.name().unwrap());
	let file = upload_file(client, test_dir, &file_name).await;
	let dir = client
		.create_dir(&test_dir.into(), &dir_name)
		.await
		.unwrap();
	let inner = upload_file(client, &dir, "inner.txt").await;
	let file_path = path_in(test_dir, &file_name);
	let dir_path = path_in(test_dir, &dir_name);
	let inner_shared_path = format!("{dir_name}/inner.txt");
	let missing_shared_path = format!("{dir_name}/missing");

	with_contact_and_shares_cleaned_up(
		client,
		&share_client,
		&[file.uuid(), dir.uuid()],
		|| async {
			authenticated_cli_with_args!("share", &file_path, &share_email)
				.success()
				.stdout(predicates::str::contains(format!(
					"Shared {file_path} with {share_email}"
				)));
			// the email is looked up in any case
			authenticated_cli_with_args!("share", &dir_path, &share_email.to_uppercase()).success();

			let (dirs, files) = out_shares_of(client, &[file.uuid(), dir.uuid()]).await;
			assert_eq!(dirs.len(), 1);
			assert_eq!(files.len(), 1);
			assert_eq!(files[0].sharing_role().email(), share_email);

			let by_me = json_of(&authenticated_cli_with_args!("shared-by-me", "--json").success());
			let listed_file = listed(&by_me, file.uuid()).expect("the shared file is listed");
			assert_eq!(listed_file["name"], file_name);
			assert_eq!(listed_file["path"], file_path);
			assert_eq!(listed_file["type"], "file");
			assert_eq!(listed_file["receivers"], serde_json::json!([share_email]));
			let listed_dir = listed(&by_me, dir.uuid()).expect("the shared directory is listed");
			assert_eq!(listed_dir["type"], "dir");
			assert_eq!(listed_dir["path"], dir_path);
			let plain = stdout_of(&authenticated_cli_with_args!("shared-by-me").success());
			assert!(
				plain.contains(&format!("{dir_path}/  shared with {share_email}")),
				"{plain}"
			);

			// the share account sees the directory and what is in it
			let with_me =
				json_of(&share_account_cli(&share_client, &["shared-with-me", "--json"]).success());
			let seen_dir =
				listed(&with_me, dir.uuid()).expect("the share account sees the directory");
			assert_eq!(seen_dir["sharer"], client.email());
			let inside = json_of(
				&share_account_cli(
					&share_client,
					&[
						"shared-with-me",
						&dir_name,
						"--from",
						client.email(),
						"--json",
					],
				)
				.success(),
			);
			let seen_inner =
				listed(&inside, inner.uuid()).expect("the file in the directory is listed");
			assert_eq!(seen_inner["name"], "inner.txt");
			assert_eq!(seen_inner["size"], 6);
			let shown = stdout_of(
				&share_account_cli(
					&share_client,
					&[
						"shared-with-me",
						&inner_shared_path,
						"--from",
						client.email(),
					],
				)
				.success(),
			);
			assert!(shown.contains("inner.txt"), "{shown}");
			assert!(shown.contains(&inner.uuid().to_string()), "{shown}");
			share_account_cli(
				&share_client,
				&[
					"shared-with-me",
					&missing_shared_path,
					"--from",
					client.email(),
				],
			)
			.code(1);

			// unsharing with the receiver named, then with everyone
			authenticated_cli_with_args!("unshare", &file_path, &share_email)
				.success()
				.stdout(predicates::str::contains(format!(
					"Stopped sharing {file_path} with {share_email}"
				)));
			authenticated_cli_with_args!("unshare", &dir_path).success();
			wait_until_unshared(client, &share_client, &[file.uuid(), dir.uuid()]).await;
			let not_shared = authenticated_cli_with_args!("unshare", &file_path).code(1);
			assert!(
				stderr_of(&not_shared).contains(&format!("{file_path} is not shared")),
				"{}",
				stderr_of(&not_shared)
			);
		},
	)
	.await;
}

#[shared_test_runtime]
async fn the_share_account_leaves_a_share() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;
	let share_client = test_utils::SHARE_RESOURCES.client().await;

	// a name only this run uses, as the share account's shared items include other runs'
	let name = format!("{}.txt", test_dir.name().unwrap());
	let file = upload_file(client, test_dir, &name).await;

	with_contact_and_shares_cleaned_up(client, &share_client, &[file.uuid()], || async {
		let contact = client
			.get_contacts()
			.await
			.unwrap()
			.into_iter()
			.find(|contact| contact.email == share_client.email())
			.unwrap();
		client.share_file(&file, &contact).await.unwrap();

		let refused = share_account_cli(&share_client, &["leave-share", &name]).code(2);
		assert!(
			stderr_of(&refused).contains("Pass --yes to answer yes."),
			"{}",
			stderr_of(&refused)
		);
		assert_eq!(out_shares_of(client, &[file.uuid()]).await.1.len(), 1);

		share_account_cli(
			&share_client,
			&["leave-share", &name, "--from", client.email(), "--yes"],
		)
		.success()
		.stdout(predicates::str::contains(format!(
			"Left the share {name} from {}",
			client.email()
		)));
		wait_until_unshared(client, &share_client, &[file.uuid()]).await;
		share_account_cli(&share_client, &["leave-share", &name, "--yes"]).code(1);
	})
	.await;
}

#[shared_test_runtime]
async fn contacts_lists_the_share_account_and_share_needs_a_contact() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;
	let share_client = test_utils::SHARE_RESOURCES.client().await;
	upload_file(client, test_dir, "unshared.txt").await;
	let file_path = path_in(test_dir, "unshared.txt");

	with_contact_and_shares_cleaned_up(client, &share_client, &[], || async {
		let contacts = json_of(&authenticated_cli_with_args!("contacts", "--json").success());
		let listed_contact = contacts["contacts"]
			.as_array()
			.unwrap()
			.iter()
			.find(|contact| contact["email"] == share_client.email())
			.expect("the share account is a contact");
		assert!(listed_contact["uuid"].is_string());
		assert!(contacts["incomingRequests"].is_array());
		assert!(contacts["outgoingRequests"].is_array());
		assert!(
			json_of(&authenticated_cli_with_args!("contacts", "--blocked", "--json").success())
				.is_array()
		);

		let stranger = "not-a-contact@example.invalid";
		let refused = authenticated_cli_with_args!("share", &file_path, stranger).code(1);
		assert!(
			stderr_of(&refused).contains(&format!(
				"{stranger} is not a contact: send a contact request first"
			)),
			"{}",
			stderr_of(&refused)
		);
		authenticated_cli_with_args!("contact-cancel", stranger)
			.code(1)
			.stderr(predicates::str::contains(format!(
				"No contact request to {stranger} to withdraw"
			)));
		// A request to an account that is a contact already goes through, and the server answers it
		// with the existing contact rather than a pending request. It is one send against the
		// sender's hourly budget, which is why the test sends no request to a new account.
		authenticated_cli_with_args!("contact-request", share_client.email())
			.success()
			.stdout(predicates::str::contains(format!(
				"Sent a contact request to {}",
				share_client.email()
			)));
		let pending = client
			.list_outgoing_contact_requests()
			.await
			.unwrap()
			.into_iter()
			.find(|request| request.email == share_client.email());
		if let Some(pending) = &pending {
			client.cancel_contact_request(pending.uuid).await.unwrap();
		}
		assert!(pending.is_none(), "a request to a contact stays pending");

		// removing asks first, so nothing is removed without --yes
		authenticated_cli_with_args!("contact-remove", share_client.email()).code(2);
		assert!(
			client
				.get_contacts()
				.await
				.unwrap()
				.iter()
				.any(|contact| contact.email == share_client.email())
		);
	})
	.await;
}

/// Waits until `client` has an incoming contact request from `email`. A request takes a moment
/// to reach the receiving account.
async fn wait_for_incoming_request(client: &Client, email: &str) {
	for _ in 0..30 {
		let requests = client.list_incoming_contact_requests().await.unwrap();
		if requests.iter().any(|request| request.email == email) {
			return;
		}
		tokio::time::sleep(Duration::from_secs(1)).await;
	}
	panic!("no contact request from {email} arrived within 30 s");
}

/// Puts the two accounts back the way the other tests expect them, as contacts with nothing
/// pending and nothing blocked, while the caller still holds both `test:contact` locks.
async fn restore_contact(client: &Client, share_client: &Client) {
	for request in client.list_outgoing_contact_requests().await.unwrap() {
		if request.email == share_client.email() {
			client.cancel_contact_request(request.uuid).await.unwrap();
		}
	}
	for blocked in client.get_blocked_contacts().await.unwrap() {
		if blocked.email == share_client.email() {
			client.unblock_contact(blocked.uuid).await.unwrap();
		}
	}
	let is_contact = client
		.get_contacts()
		.await
		.unwrap()
		.iter()
		.any(|contact| contact.email == share_client.email());
	if !is_contact {
		test_utils::add_contact(client, share_client).await;
	}
}

/// The whole contact-request flow through the CLI, which needs the accounts not to be contacts
/// first: `set_up_contact_no_add` removes the standing contact and waits 300 s for the server to
/// settle. Four requests are sent against the sender's hourly budget: one denied, one withdrawn,
/// one accepted, and the one that restores the contact at the end.
#[shared_test_runtime]
async fn a_contact_request_is_denied_withdrawn_accepted_and_removed() {
	let client = &test_utils::RESOURCES.get_resources().await.client;
	let share_client = test_utils::SHARE_RESOURCES.client().await;
	let email = client.email().to_owned();
	let share_email = share_client.email().to_owned();

	let _locks = test_utils::set_up_contact_no_add(client, &share_client).await;
	let result = AssertUnwindSafe(async {
		authenticated_cli_with_args!("contact-request", &share_email)
			.success()
			.stdout(predicates::str::contains(format!(
				"Sent a contact request to {share_email}"
			)));
		let contacts = json_of(&authenticated_cli_with_args!("contacts", "--json").success());
		assert!(
			contacts["outgoingRequests"]
				.as_array()
				.unwrap()
				.iter()
				.any(|request| request["email"] == share_email),
			"{contacts}"
		);
		wait_for_incoming_request(&share_client, &email).await;
		let incoming =
			json_of(&share_account_cli(&share_client, &["contacts", "--json"]).success());
		assert!(
			incoming["incomingRequests"]
				.as_array()
				.unwrap()
				.iter()
				.any(|request| request["email"] == email),
			"{incoming}"
		);
		share_account_cli(&share_client, &["contact-deny", &email])
			.success()
			.stdout(predicates::str::contains(format!(
				"Denied the contact request from {email}"
			)));
		assert!(
			!share_client
				.list_incoming_contact_requests()
				.await
				.unwrap()
				.iter()
				.any(|request| request.email == email)
		);

		authenticated_cli_with_args!("contact-request", &share_email).success();
		authenticated_cli_with_args!("contact-cancel", &share_email.to_uppercase())
			.success()
			.stdout(predicates::str::contains(format!(
				"Withdrew the contact request to {share_email}"
			)));
		assert!(
			!client
				.list_outgoing_contact_requests()
				.await
				.unwrap()
				.iter()
				.any(|request| request.email == share_email)
		);

		authenticated_cli_with_args!("contact-request", &share_email).success();
		wait_for_incoming_request(&share_client, &email).await;
		share_account_cli(&share_client, &["contact-accept", &email])
			.success()
			.stdout(predicates::str::contains(format!(
				"Accepted the contact request from {email}"
			)));
		let contacts = json_of(&authenticated_cli_with_args!("contacts", "--json").success());
		assert!(
			contacts["contacts"]
				.as_array()
				.unwrap()
				.iter()
				.any(|contact| contact["email"] == share_email),
			"{contacts}"
		);

		authenticated_cli_with_args!("contact-remove", &share_email, "--yes")
			.success()
			.stdout(predicates::str::contains(format!(
				"Removed {share_email} from your contacts"
			)));
		assert!(
			!client
				.get_contacts()
				.await
				.unwrap()
				.iter()
				.any(|contact| contact.email == share_email)
		);

		authenticated_cli_with_args!("contact-block", &share_email)
			.success()
			.stdout(predicates::str::contains(format!("Blocked {share_email}")));
		let blocked =
			json_of(&authenticated_cli_with_args!("contacts", "--blocked", "--json").success());
		assert!(
			blocked
				.as_array()
				.unwrap()
				.iter()
				.any(|block| block["email"] == share_email),
			"{blocked}"
		);
		authenticated_cli_with_args!("contact-unblock", &share_email)
			.success()
			.stdout(predicates::str::contains(format!(
				"Unblocked {share_email}"
			)));
		assert!(
			!client
				.get_blocked_contacts()
				.await
				.unwrap()
				.iter()
				.any(|block| block.email == share_email)
		);
	})
	.catch_unwind()
	.await;
	restore_contact(client, &share_client).await;
	if let Err(panic) = result {
		std::panic::resume_unwind(panic);
	}
}
