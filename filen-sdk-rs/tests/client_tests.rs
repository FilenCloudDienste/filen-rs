use std::collections::HashSet;

use base64::{
	Engine,
	prelude::{BASE64_STANDARD_NO_PAD, BASE64_URL_SAFE_NO_PAD},
};
use filen_macros::shared_test_runtime;
use filen_sdk_rs::{
	auth::{Client, RegisteredInfo, TwoFASecret},
	fs::HasName,
};
use futures::{StreamExt, stream::FuturesUnordered};
use regex::Regex;

// all tests must be multi_threaded, otherwise drop will deadlock for TestResources
#[shared_test_runtime]
async fn test_login() {
	test_utils::RESOURCES.client().await;
}

#[shared_test_runtime]
async fn test_stringification() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let stringified = client.to_stringified();
	assert_eq!(Client::from_stringified(stringified).unwrap(), **client)
}

#[shared_test_runtime]
async fn cleanup_test_dirs() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let _lock = client.lock_drive().await.unwrap();

	let (dirs, _) = client.list_dir(client.root()).await.unwrap();
	let mut futures = FuturesUnordered::new();
	let now = chrono::Utc::now();
	for dir in dirs {
		if dir.name().is_some_and(|n| n.starts_with("rs-"))
			&& dir
				.created()
				.is_none_or(|c| now - c > chrono::Duration::days(1))
		{
			futures.push(async { client.delete_dir_permanently(dir).await });
		}
	}

	while futures.next().await.is_some() {}
}

async fn enable_2fa_for_client(client: &Client, secret: &TwoFASecret) -> String {
	for _ in 0..10 {
		let result = client
			.enable_2fa(
				&secret
					.make_totp_code(chrono::Utc::now())
					.unwrap()
					.to_string(),
			)
			.await;
		if let Ok(recovery_key) = result {
			return recovery_key;
		}
	}
	panic!("Failed to enable 2FA after multiple attempts");
}

#[shared_test_runtime]
async fn test_2fa() {
	let client = test_utils::RESOURCES.client().await;

	let _lock = client.lock_auth().await.unwrap();

	let secret = client.generate_2fa_secret().await.unwrap();

	let recovery_key = enable_2fa_for_client(&client, &secret).await;

	// we print this in case we have to recover the account
	println!("Recovery key: {recovery_key:?}");

	// we use the recovery key here rather than the 2fa code
	// to make sure the test doesn't fail due to a race condition
	client.disable_2fa(&recovery_key).await.unwrap();
}
struct ImapSession {
	session: imap::Session<std::boxed::Box<dyn imap::ImapConnection>>,
}

impl Drop for ImapSession {
	fn drop(&mut self) {
		if let Err(e) = self.session.expunge() {
			eprintln!("Failed to expunge emails: {e}");
		}
		if let Err(e) = self.session.logout() {
			eprintln!("Failed to logout from IMAP: {e}");
		}
	}
}

fn imap_login(imap_username: &str) -> ImapSession {
	let imap_email = format!("{}@filen.io", imap_username);
	let imap_password = std::env::var("IMAP_EMAIL_PASSWORD").unwrap();

	let client = imap::ClientBuilder::new("imappro.zoho.eu", 993)
		.connect()
		.unwrap();

	let mut imap_session = client.login(&imap_email, &imap_password).unwrap();

	imap_session.select("INBOX").unwrap();
	ImapSession {
		session: imap_session,
	}
}

struct MsgsResponse<'a> {
	ids: HashSet<u32>,
	session: &'a mut ImapSession,
}

impl Drop for MsgsResponse<'_> {
	fn drop(&mut self) {
		if self.ids.is_empty() {
			return;
		}

		println!("Deleting {} emails", self.ids.len());

		if let Err(e) = self.session.session.uid_store(
			self.ids
				.iter()
				.map(|id| id.to_string())
				.collect::<Vec<_>>()
				.join(","),
			"+FLAGS.SILENT (\\Deleted)",
		) {
			eprintln!("Failed to mark emails as deleted: {e}");
		}
	}
}

fn await_email(
	imap_session: &mut ImapSession,
	to: &str,
	subject: &str,
	retry_count: usize,
	retry_delay: std::time::Duration,
) -> String {
	let query = format!(
		r#"SUBJECT "{}" TO "{}" FROM "noreply@notifications.filen.io""#,
		subject, to
	);

	log::trace!("Waiting for email with query: {}", query);

	for _ in 0..retry_count {
		if let Ok(msgs) = imap_session.session.uid_search(&query) {
			// deletes all received messages on drop
			log::trace!("Found {} emails", msgs.len());
			let msgs = MsgsResponse {
				ids: msgs,
				session: imap_session,
			};

			let mut iter = msgs.ids.iter();
			if let Some(msg) = iter.next() {
				if iter.next().is_some() {
					panic!("More than one email received from noreply@notifications.filen.io");
				}
				log::debug!("Found email with id {}", msg);
				let msg = msgs
					.session
					.session
					.uid_fetch(msg.to_string(), "RFC822")
					.unwrap();
				if let Some(msg) = msg.iter().next() {
					let body = msg.body().expect("Email has no body");
					let body = std::str::from_utf8(body).expect("Email body is not valid UTF-8");
					return body.to_string();
				} else {
					log::warn!("Email disappeared");
				}
			} else {
				log::trace!("No email received, retrying...");
			}
		} else {
			log::trace!("Failed to search emails, retrying...");
		}
		std::thread::sleep(retry_delay);
	}
	panic!("No email received");
}

// this isn't async so we can block_on in drops
#[test]
fn register() {
	let _ = dotenv::dotenv();
	let password: [u8; 64] = rand::random();
	let password = BASE64_STANDARD_NO_PAD.encode(password);
	let suffix: [u8; 32] = rand::random();
	let email_username = std::env::var("IMAP_EMAIL_USER").unwrap();
	let email = format!(
		"{}+{}@filen.io",
		email_username,
		BASE64_URL_SAFE_NO_PAD.encode(suffix)
	);

	test_utils::rt().block_on(async {
		RegisteredInfo::register(email.clone(), &password, None, None)
			.await
			.unwrap();
	});

	let mut imap_session = imap_login(&email_username);
	let body = await_email(
		&mut imap_session,
		&email,
		"Account activation",
		20,
		std::time::Duration::from_secs(30),
	);

	let activate_regex = Regex::new("https://filen.io/activate/\\w+").unwrap();

	let activate_link = activate_regex
		.find(&body)
		.expect("No activation link found")
		.as_str();

	test_utils::rt().block_on(async {
		reqwest::get(activate_link).await.unwrap();
		let client = Client::login(email.clone(), &password, "XXXXXX")
			.await
			.unwrap();
		client.list_dir(client.root()).await.unwrap();
		client.delete_account("XXXXXX").await.unwrap();
	});

	let body = await_email(
		&mut imap_session,
		&email,
		"Confirm account deletion",
		20,
		std::time::Duration::from_secs(30),
	);

	let activate_regex = Regex::new("https://filen.io/delete-account/(\\w+)").unwrap();
	let delete_match = activate_regex.captures(&body).unwrap();
	let delete_link = delete_match.get(0).unwrap().as_str();

	// we print this in case we have to force delete the account
	println!("Deletion link: {}", delete_link);

	test_utils::rt().block_on(async {
		let (mut browser, mut handler) = chromiumoxide::Browser::launch(
			chromiumoxide::BrowserConfig::builder().build().unwrap(),
		)
		.await
		.unwrap();

		let handle = tokio::spawn(async move {
			while let Some(event) = handler.next().await {
				if event.is_err() {
					break;
				}
			}
		});

		let page = browser.new_page(delete_link).await.unwrap();

		let element = page.find_element(r#"button[type="submit"]"#).await.unwrap();
		element.click().await.unwrap();

		browser.close().await.unwrap();
		handle.await.unwrap();
	});
}
