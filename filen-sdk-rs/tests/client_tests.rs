use core::panic;
use std::{
	borrow::Cow,
	collections::{HashMap, HashSet},
	net::{TcpStream, ToSocketAddrs},
	time::Duration,
};

use base64::{
	Engine,
	prelude::{BASE64_STANDARD_NO_PAD, BASE64_URL_SAFE_NO_PAD},
};
use chrono::DateTime;
use filen_macros::shared_test_runtime;
use filen_sdk_rs::{
	ErrorKind,
	auth::{Client, TwoFASecret, http::ClientConfig, unauth::UnauthClient},
	fs::{HasName, HasUUID},
	io::client_impl::IoSharedClientExt,
	socket::{DecryptedGeneralEvent, DecryptedSocketEvent},
};
use filen_types::traits::CowHelpersExt;
use futures::{FutureExt, StreamExt, stream::FuturesUnordered};
use regex::Regex;
use rustls_connector::RustlsConnector;
use scraper::{Html, Selector};
use test_utils::await_event;

// all tests must be multi_threaded, otherwise drop will deadlock for TestResources
#[shared_test_runtime]
async fn test_login() {
	test_utils::RESOURCES.client().await;
}

#[shared_test_runtime]
async fn test_login_with_api_key() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let stringified = client.to_stringified();
	let (_, password, _) = test_utils::RESOURCES.get_credentials();
	let relogged = client
		.get_unauthed()
		.login_with_api_key(stringified.email.clone(), &password, stringified.api_key)
		.await
		.expect("login_with_api_key failed");
	assert_eq!(relogged, **client);
}

#[shared_test_runtime]
async fn test_stringification() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let stringified = client.to_stringified();
	let unauthed = client.get_unauthed();
	assert_eq!(unauthed.from_stringified(stringified).unwrap(), **client)
}

#[shared_test_runtime]
async fn cleanup_test_dirs() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let _lock = client.lock_drive().await.unwrap();

	let (dirs, _) = client
		.list_dir(&(client.root()).into(), None::<&fn(u64, Option<u64>)>)
		.await
		.unwrap();
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
			.enable_2fa(&secret.make_totp_code(chrono::Utc::now()).unwrap())
			.await;
		match result {
			Err(e) => {
				tracing::warn!("Failed to enable 2FA: {}, retrying...", e);
				tokio::time::sleep(Duration::from_secs(5)).await;
				continue;
			}
			Ok(recovery_key) => return recovery_key,
		}
	}
	panic!("Failed to enable 2FA after multiple attempts");
}

#[shared_test_runtime]
async fn test_wrong_password() {
	let client = test_utils::RESOURCES.client().await;
	let unauthed = client.get_unauthed();

	// `test_2fa` is `#[ignore]`d out of the parallel matrix (it toggles 2FA on this shared account,
	// which would make a wrong-password login report Enter2fa instead of EmailOrPasswordWrong), so
	// in a normal run 2FA is never on here and this assertion is deterministic. The auth lock stays
	// as a secondary guard for the isolated `--ignored` 2FA job; it cannot fully protect against a
	// cross-worker 2FA toggle (other logins don't hold it), so the real isolation is keeping
	// `test_2fa` off the parallel matrix.
	let _lock = client.lock_auth().await.unwrap();

	let (email, _, _) = test_utils::RESOURCES.get_credentials();
	let err = unauthed
		.login(email, "wrongpassword", "XXXXXX")
		.await
		.unwrap_err();
	assert_eq!(err.kind(), ErrorKind::EmailOrPasswordWrong);
}

// IGNORED in the default (parallel) test matrix: this enables 2FA on the SHARED `TEST` account,
// and while 2FA is on EVERY fresh login to that account fails — including other CI workers'
// `RESOURCES.client()` initialization and `test_wrong_password`. No lock can fully cover that: a
// fresh login can't hold the auth lock (you need an authenticated client to take it). So run this
// ISOLATED via `cargo test -- --ignored` in a single-worker job that never overlaps the main
// matrix on this account, rather than letting it corrupt every concurrent login.
#[shared_test_runtime]
#[ignore = "toggles 2FA on the shared account; run isolated via --ignored, never alongside the parallel matrix"]
async fn test_2fa() {
	let client = test_utils::RESOURCES.client().await;
	let unauthed = client.get_unauthed();

	let _lock = client.lock_auth().await.unwrap();

	let secret = client.generate_2fa_secret().await.unwrap();

	let recovery_key = enable_2fa_for_client(&client, &secret).await;

	let (email, password, _) = test_utils::RESOURCES.get_credentials();
	let code = secret.make_totp_code(DateTime::default()).unwrap();

	let res = std::panic::AssertUnwindSafe(async move {
		let err = unauthed
			.login(email.clone(), &password, &code)
			.await
			.unwrap_err();
		assert_eq!(err.kind(), ErrorKind::Wrong2fa);

		let err = unauthed
			.login(email, &password, "XXXXXX")
			.await
			.unwrap_err();
		assert_eq!(err.kind(), ErrorKind::Enter2fa);
	})
	.catch_unwind()
	.await;

	// we use the recovery key here rather than the 2fa code
	// to make sure the test doesn't fail due to a race condition
	client.disable_2fa(&recovery_key).await.unwrap();
	res.unwrap();
}
const IMAP_DOMAIN: &str = "imappro.zoho.eu";
const IMAP_PORT: u16 = 993;
/// `imap`'s `ClientBuilder` sets no timeouts at all: its `TcpStream::connect` and its greeting read
/// both block indefinitely. On the 2026-09-20 nightly one `.connect()` sat 3m41s after a completed
/// TLS handshake before the peer's EOF finally surfaced.
const IMAP_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const IMAP_IO_TIMEOUT: Duration = Duration::from_secs(60);
const IMAP_CONNECT_ATTEMPTS: u32 = 3;
/// How many `Ok`-but-empty polls [`await_email`] tolerates before rebuilding the session.
const EMPTY_POLLS_BEFORE_RECONNECT: usize = 5;

type BoxedImapSession = imap::Session<std::boxed::Box<dyn imap::ImapConnection>>;

struct ImapSession {
	session: BoxedImapSession,
	imap_username: String,
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

impl ImapSession {
	/// Best-effort replacement of the underlying session. Only `session` is swapped: dropping a
	/// whole [`ImapSession`] would run the mailbox-wide `expunge` in [`Drop`] against a socket that
	/// is, in the case this exists for, already dead.
	fn reconnect(&mut self) {
		let password = match std::env::var("IMAP_EMAIL_PASSWORD") {
			Ok(password) => password,
			Err(e) => {
				tracing::debug!("IMAP reconnect skipped, no password in the environment: {e}");
				return;
			}
		};
		match imap_connect_once(&self.imap_username, &password) {
			Ok(session) => {
				tracing::debug!("Rebuilt the IMAP session");
				self.session = session;
			}
			Err(e) => tracing::debug!("IMAP reconnect failed, keeping the old session: {e}"),
		}
	}
}

/// One connect + login + select, with an explicit connect timeout and I/O timeout.
///
/// [`imap::ClientBuilder::connect`] cannot do this: it exposes no timeout knob and its
/// `connect_with` is private, so the TLS connection is built by hand the way the crate's own
/// `examples/timeout.rs` does. The read timeout is set on the `TcpStream` before the TLS wrap, so
/// it also bounds every later command on the returned session — including the `uid_search` loop in
/// [`await_email`], which otherwise polls a dead socket for its full budget.
fn imap_connect_once(
	imap_username: &str,
	imap_password: &str,
) -> Result<BoxedImapSession, Box<dyn std::error::Error>> {
	let imap_email = format!("{}@filen.io", imap_username);
	let mut last_err: Option<Box<dyn std::error::Error>> = None;

	for addr in (IMAP_DOMAIN, IMAP_PORT).to_socket_addrs()? {
		let tcp = match TcpStream::connect_timeout(&addr, IMAP_CONNECT_TIMEOUT) {
			Ok(tcp) => tcp,
			Err(e) => {
				last_err = Some(Box::new(e));
				continue;
			}
		};
		tcp.set_read_timeout(Some(IMAP_IO_TIMEOUT))?;
		tcp.set_write_timeout(Some(IMAP_IO_TIMEOUT))?;

		// Mirrors the crate's own `build_tls_rustls`, which builds its root store from
		// `load_native_certs()`: keep the trust anchors identical to `ClientBuilder::connect`.
		let tls = RustlsConnector::new_with_native_certs()?.connect(IMAP_DOMAIN, tcp)?;
		let connection: std::boxed::Box<dyn imap::ImapConnection> = Box::new(tls);
		let mut client = imap::Client::new(connection);
		client.read_greeting()?;
		let mut session = client
			.login(&imap_email, imap_password)
			.map_err(|(e, _)| e)?;
		session.select("INBOX")?;
		return Ok(session);
	}

	Err(last_err.unwrap_or_else(|| format!("no addresses resolved for {IMAP_DOMAIN}").into()))
}

fn imap_login(imap_username: &str) -> ImapSession {
	let imap_password = std::env::var("IMAP_EMAIL_PASSWORD").unwrap();
	let mut delay = Duration::from_secs(1);
	let mut last_err = None;

	for attempt in 1..=IMAP_CONNECT_ATTEMPTS {
		match imap_connect_once(imap_username, &imap_password) {
			Ok(session) => {
				return ImapSession {
					session,
					imap_username: imap_username.to_owned(),
				};
			}
			Err(e) => {
				tracing::debug!(
					"IMAP connect attempt {attempt}/{IMAP_CONNECT_ATTEMPTS} failed: {e}"
				);
				last_err = Some(e);
				if attempt < IMAP_CONNECT_ATTEMPTS {
					std::thread::sleep(delay);
					delay *= 2;
				}
			}
		}
	}

	panic!(
		"IMAP login failed after {IMAP_CONNECT_ATTEMPTS} attempts: {}",
		last_err.expect("at least one attempt ran")
	);
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

	// Stays at trace: the query embeds the mailbox address.
	tracing::trace!("Waiting for email with query: {}", query);

	let mut ok_empty = 0usize;
	let mut search_errors = 0usize;
	let mut empty_since_reconnect = 0usize;
	let mut last_error: Option<String> = None;

	for attempt in 1..=retry_count {
		// Bound the search borrow before the `Ok` arm reborrows `imap_session` for `MsgsResponse`.
		let search = imap_session.session.uid_search(&query);
		match search {
			Ok(msgs) => {
				// deletes all received messages on drop
				tracing::trace!("Found {} emails", msgs.len());
				let msgs = MsgsResponse {
					ids: msgs,
					session: imap_session,
				};

				let mut iter = msgs.ids.iter();
				if let Some(msg) = iter.next() {
					if iter.next().is_some() {
						panic!("More than one email received from noreply@notifications.filen.io");
					}
					tracing::debug!("Found email with id {}", msg);
					let msg = msgs
						.session
						.session
						.uid_fetch(msg.to_string(), "RFC822")
						.unwrap();
					if let Some(msg) = msg.iter().next() {
						let body = msg.body().expect("Email has no body");
						let body =
							std::str::from_utf8(body).expect("Email body is not valid UTF-8");
						return body.to_string();
					} else {
						tracing::warn!("Email disappeared");
					}
				} else {
					ok_empty += 1;
					empty_since_reconnect += 1;
					// debug!, not trace!: CI captures at RUST_LOG=debug, so a trace line here is
					// invisible in every nightly log ever recorded, and the panic below cannot be
					// told apart from the dead-session case (2026-09-20 triage).
					tracing::debug!("No email received ({attempt}/{retry_count}), retrying...");
				}
			}
			Err(e) => {
				search_errors += 1;
				last_error = Some(e.to_string());
				tracing::debug!(
					"Failed to search emails ({attempt}/{retry_count}): {e}, retrying..."
				);
				// A session whose socket has died answers `Err` for every remaining poll.
				imap_session.reconnect();
				empty_since_reconnect = 0;
			}
		}

		// A live session can also go blind to an already-delivered message: on a green run Zoho's
		// SEARCH withheld one uid for ~90s while a later-delivered uid was already visible to a
		// sibling session. An `Ok`-but-empty answer never takes the `Err` path above, so force a
		// fresh SELECT periodically as well.
		if empty_since_reconnect >= EMPTY_POLLS_BEFORE_RECONNECT {
			imap_session.reconnect();
			empty_since_reconnect = 0;
		}

		std::thread::sleep(retry_delay);
	}
	panic!(
		"No email received after {retry_count} polls ({ok_empty} empty, {search_errors} search errors, last error: {last_error:?})"
	);
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

fn activate_account(register_data: &mut RegisterTest, unauth_client: &UnauthClient) -> Client {
	test_utils::rt().block_on(async {
		unauth_client
			.register(
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

	test_utils::rt().block_on(async {
		unauth_client
			.resend_registration_confirmation(&register_data.email)
			.await
			.unwrap();
	});

	let body_2 = await_email(
		&mut register_data.imap,
		&register_data.email,
		"Confirm your email address",
		20,
		std::time::Duration::from_secs(30),
	);

	let activate_regex = Regex::new(r"https:\/\/filen\.io\/activate\/\w+").unwrap();

	let (activate_link, _) = match_regex_in_email_body(&body, &activate_regex);
	let (second_activate_link, _) = match_regex_in_email_body(&body_2, &activate_regex);
	assert_eq!(
		activate_link, second_activate_link,
		"Multiple different activation links found in email body"
	);

	test_utils::rt().block_on(async {
		reqwest::get(activate_link).await.unwrap();
		let client = unauth_client
			.login(
				register_data.email.clone(),
				&register_data.password,
				"XXXXXX",
			)
			.await
			.unwrap();
		client
			.list_dir(&(client.root()).into(), None::<&fn(u64, Option<u64>)>)
			.await
			.unwrap();
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
	let unauth_client = UnauthClient::from_config(ClientConfig::default()).unwrap();
	let mut register_data = init_register_test();

	let old_client = activate_account(&mut register_data, &unauth_client);

	test_utils::rt().block_on(async {
		unauth_client
			.start_password_reset(&register_data.email)
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
		let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();

		let _handle = old_client
			.add_event_listener(
				Box::new(move |event| {
					let _ = sender.send(event.to_owned_cow());
				}),
				Some(vec![Cow::Borrowed("passwordChanged")]),
			)
			.await
			.unwrap();

		let client = unauth_client
			.complete_password_reset(
				reset_token,
				register_data.email.clone(),
				&new_password,
				None,
				None,
			)
			.await
			.unwrap();

		await_event(
			&mut receiver,
			|e| {
				matches!(
					e,
					DecryptedSocketEvent::General {
						inner: DecryptedGeneralEvent::PasswordChanged,
						..
					}
				)
			},
			Duration::from_secs(10),
			"password changed event",
		)
		.await;

		client
			.list_dir(&(client.root()).into(), None::<&fn(u64, Option<u64>)>)
			.await
			.unwrap();
		unauth_client
			.login(register_data.email.clone(), &new_password, "XXXXXX")
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
	let unauth_client = UnauthClient::from_config(ClientConfig::default()).unwrap();
	let client = activate_account(&mut register_data, &unauth_client);

	let (recovery_key, files) = test_utils::rt().block_on(async {
		client
			.list_dir(&(client.root()).into(), None::<&fn(u64, Option<u64>)>)
			.await
			.unwrap();
		let first_file = client
			.make_file_builder("first.txt", client.root().uuid())
			.unwrap();

		let first_file = client
			.upload_file(first_file, b"Hello, world!")
			.await
			.unwrap();

		let new_password: [u8; 64] = rand::random();
		let new_password = BASE64_STANDARD_NO_PAD.encode(new_password);

		client
			.change_password(&register_data.password, &new_password)
			.await
			.unwrap();

		let second_file = client
			.make_file_builder("second", client.root().uuid())
			.unwrap();
		let second_file = client
			.upload_file(second_file, b"Hello, world!")
			.await
			.unwrap();

		let relogin_client = unauth_client
			.login(register_data.email.clone(), &new_password, "XXXXXX")
			.await
			.unwrap();

		let third_file = relogin_client
			.make_file_builder("third", client.root().uuid())
			.unwrap();

		let third_file = relogin_client
			.upload_file(third_file, b"Hello, world!")
			.await
			.unwrap();

		let (_, files) = relogin_client
			.list_dir(
				&(relogin_client.root()).into(),
				None::<&fn(u64, Option<u64>)>,
			)
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

		unauth_client
			.start_password_reset(&register_data.email)
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
		let client = unauth_client
			.complete_password_reset(
				reset_token,
				register_data.email.clone(),
				&new_password,
				Some(&recovery_key),
				None,
			)
			.await
			.unwrap();

		let (first_file, second_file, third_file) = files;

		let (_, files) = client
			.list_dir(&(client.root()).into(), None::<&fn(u64, Option<u64>)>)
			.await
			.unwrap();

		assert_eq!(files.len(), 3);
		assert!(files.contains(&first_file));
		assert!(files.contains(&second_file));
		assert!(files.contains(&third_file));
		client
	});

	delete_account(client, &mut register_data);
}
