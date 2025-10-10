use core::panic;
use std::collections::{HashMap, HashSet};

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
use scraper::{Html, Selector};

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

fn match_regex_in_email_body(body: &str, regex: &Regex) -> (String, Vec<String>) {
	let message = mail_parser::MessageParser::default().parse(body).unwrap();

	for i in 0..message.text_body_count() {
		let body = message.body_text(i).unwrap();
		if let Some(captures) = regex.captures(&body) {
			let mut captures_iter = captures.iter();
			let overall = captures_iter.next().unwrap().unwrap().as_str().to_string();

			let groups = captures_iter
				.filter_map(|c| c.map(|m| m.as_str().to_string()))
				.collect();

			return (overall, groups);
		}
	}
	panic!("No link found in email body");
}

struct RegisterTest {
	imap: ImapSession,
	password: String,
	email: String,
}

fn init_register_test() -> RegisterTest {
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

	let imap = imap_login(&email_username);
	RegisterTest {
		imap,
		password,
		email,
	}
}

fn activate_account(register_data: &mut RegisterTest) -> Client {
	test_utils::rt().block_on(async {
		RegisteredInfo::register(
			register_data.email.clone(),
			&register_data.password,
			None,
			None,
		)
		.await
		.unwrap();
	});

	let body = await_email(
		&mut register_data.imap,
		&register_data.email,
		"Account activation",
		20,
		std::time::Duration::from_secs(30),
	);

	let activate_regex = Regex::new(r"https:\/\/filen\.io\/activate\/\w+").unwrap();

	let (activate_link, _) = match_regex_in_email_body(&body, &activate_regex);
	test_utils::rt().block_on(async {
		reqwest::get(activate_link).await.unwrap();
		let client = Client::login(
			register_data.email.clone(),
			&register_data.password,
			"XXXXXX",
		)
		.await
		.unwrap();
		client.list_dir(client.root()).await.unwrap();
		client
	})
}

fn delete_account(client: Client, register_data: &mut RegisterTest) {
	test_utils::rt().block_on(async {
		client.delete_account("XXXXXX").await.unwrap();
	});

	let body = await_email(
		&mut register_data.imap,
		&register_data.email,
		"Confirm account deletion",
		20,
		std::time::Duration::from_secs(30),
	);

	let delete_regex = Regex::new(r"https:\/\/filen\.io\/delete-account\/(\w+)").unwrap();
	let (delete_link, _) = match_regex_in_email_body(&body, &delete_regex);

	test_utils::rt().block_on(async {
		let client = reqwest::Client::builder()
			.cookie_store(true)
			.build()
			.unwrap();

		let response = client.get(&delete_link).send().await.unwrap();
		let html_content = response.text().await.unwrap();

		let document = Html::parse_document(&html_content);

		let form_selector = Selector::parse(r#"form[method="POST"]"#).unwrap();
		let input_selector = Selector::parse("input[type='hidden']").unwrap();

		let form = document
			.select(&form_selector)
			.find(|f| {
				let button_sel = Selector::parse("button").unwrap();
				f.select(&button_sel)
					.any(|b| b.text().any(|t| t.contains("Delete my account")))
			})
			.expect("Delete account form not found");

		let mut form_data: HashMap<String, String> = HashMap::new();

		for input in form.select(&input_selector) {
			if let Some(name) = input.value().attr("name") {
				let value = input.value().attr("value").unwrap_or("");
				form_data.insert(name.to_string(), value.to_string());
			}
		}

		let mut multipart_form = reqwest::multipart::Form::new();

		for input in form.select(&input_selector) {
			if let Some(name) = input.value().attr("name") {
				let value = input.value().attr("value").unwrap_or("");
				multipart_form = multipart_form.text(name.to_string(), value.to_string());
			}
		}

		let response = client
			.post(&delete_link)
			.multipart(multipart_form)
			.send()
			.await
			.unwrap();

		assert!(response.status().is_success() || response.status().is_redirection());
	})
}

#[test]
fn register_and_reset_password_no_export() {
	let mut register_data = init_register_test();

	activate_account(&mut register_data);

	test_utils::rt().block_on(async {
		filen_sdk_rs::auth::start_password_reset(&register_data.email)
			.await
			.unwrap();
	});

	let reset_password_body = await_email(
		&mut register_data.imap,
		&register_data.email,
		"Password reset",
		20,
		std::time::Duration::from_secs(30),
	);

	let reset_regex = Regex::new(r"https:\/\/filen\.io\/forgot-password\/([\w-]+)").unwrap();
	let (_, reset_token_match_vec) = match_regex_in_email_body(&reset_password_body, &reset_regex);
	let reset_token = &reset_token_match_vec[0];

	let new_password: [u8; 64] = rand::random();
	let new_password = BASE64_STANDARD_NO_PAD.encode(new_password);
	let client = test_utils::rt().block_on(async {
		let client = Client::complete_password_reset(
			reset_token,
			register_data.email.clone(),
			&new_password,
			None,
		)
		.await
		.unwrap();
		client.list_dir(client.root()).await.unwrap();
		Client::login(register_data.email.clone(), &new_password, "XXXXXX")
			.await
			.unwrap();
		client
	});

	delete_account(client, &mut register_data);
}

// this isn't async so we can block_on in drops
#[test]
fn register_change_and_reset_password_with_export() {
	let mut register_data = init_register_test();
	let client = activate_account(&mut register_data);

	let (recovery_key, files) = test_utils::rt().block_on(async {
		client.list_dir(client.root()).await.unwrap();
		let first_file = client.make_file_builder("first.txt", client.root()).build();

		let first_file = client
			.upload_file(first_file.into(), b"Hello, world!")
			.await
			.unwrap();

		let new_password: [u8; 64] = rand::random();
		let new_password = BASE64_STANDARD_NO_PAD.encode(new_password);

		client
			.change_password(&register_data.password, &new_password)
			.await
			.unwrap();

		let second_file = client.make_file_builder("second", client.root()).build();
		let second_file = client
			.upload_file(second_file.into(), b"Hello, world!")
			.await
			.unwrap();

		let relogin_client = Client::login(register_data.email.clone(), &new_password, "XXXXXX")
			.await
			.unwrap();

		let third_file = relogin_client
			.make_file_builder("third", client.root())
			.build();

		let third_file = relogin_client
			.upload_file(third_file.into(), b"Hello, world!")
			.await
			.unwrap();

		let (_, files) = relogin_client
			.list_dir(relogin_client.root())
			.await
			.unwrap();

		assert_eq!(files.len(), 3);
		assert!(files.contains(&first_file));
		assert!(files.contains(&second_file));
		assert!(files.contains(&third_file));

		for file in files {
			let contents = relogin_client.download_file(&file).await.unwrap();
			assert_eq!(contents, b"Hello, world!");
		}

		let exported_keys_string = relogin_client
			.export_master_keys()
			.await
			.unwrap_or_else(|e| panic!("Failed to export master keys: {}", e));

		filen_sdk_rs::auth::start_password_reset(&register_data.email)
			.await
			.unwrap();
		(exported_keys_string, (first_file, second_file, third_file))
	});

	let reset_password_body = await_email(
		&mut register_data.imap,
		&register_data.email,
		"Password reset",
		20,
		std::time::Duration::from_secs(30),
	);

	let reset_regex = Regex::new(r"https:\/\/filen\.io\/forgot-password\/([\w-]+)").unwrap();
	let (_, reset_token_match_vec) = match_regex_in_email_body(&reset_password_body, &reset_regex);
	let reset_token = &reset_token_match_vec[0];

	let new_password: [u8; 64] = rand::random();
	let new_password = BASE64_STANDARD_NO_PAD.encode(new_password);
	let client = test_utils::rt().block_on(async {
		let client = Client::complete_password_reset(
			reset_token,
			register_data.email.clone(),
			&new_password,
			Some(&recovery_key),
		)
		.await
		.unwrap();

		let (first_file, second_file, third_file) = files;

		let (_, files) = client.list_dir(client.root()).await.unwrap();

		assert_eq!(files.len(), 3);
		assert!(files.contains(&first_file));
		assert!(files.contains(&second_file));
		assert!(files.contains(&third_file));
		client
	});

	delete_account(client, &mut register_data);
}
