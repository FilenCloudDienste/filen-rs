//! [cli-doc] auth-methods
//! The CLI logs in with the first of these that it finds:
//! 1. the CLI arguments `--email` and `--password`, optionally `--two-factor-code`,
//!    or the environment variables `FILEN_CLI_EMAIL`, `FILEN_CLI_PASSWORD` and `FILEN_CLI_2FA_CODE`
//!    (when none of these is set, the variables `FILEN_EMAIL`, `FILEN_PASSWORD` and
//!    `FILEN_2FA_CODE` of the old Filen CLI are used; when a two-factor code is required
//!    and not given, you are prompted for it; the recovery key is accepted as the code too)
//!
//!    To log in to an account with two-factor authentication without anyone to type a code,
//!    set `FILEN_CLI_2FA_SECRET` to its TOTP secret (the key your authenticator app was set up with):
//!    the CLI then makes the code itself. Keep in mind that anyone who can read the CLI's
//!    environment then has your second factor too, which defeats two-factor authentication
//!    on that machine.
//! 2. an auth config file, exported with `filen export-auth-config`: the one given with
//!    `--auth-config-path`, or else `./filen-cli-auth-config.txt` (in the current working
//!    directory), then `filen-cli-auth-config.txt` in the config dir
//!    (on Linux and macOS, an auth config found in these two places must not be accessible
//!    to other users: restrict it with `chmod 600`)
//! 3. the login kept in the system keychain (each config dir keeps a login of its own there)
//! 4. credentials saved by the old Filen CLI, which are imported
//!    (its `.filen-cli-auth-config` in the current working directory or its data directory,
//!    and, with the default config dir, the login it kept; its files are left as they are)
//! 5. otherwise, you are prompted for your email and password (with three attempts)
//!
//! After logging in by 4. or 5., you can keep the login in the system keychain,
//! or, where there is none (such as in the Docker image), in an auth config file in the config dir.
//!
//! The config dir is the one given with `--config-dir`, or by default:
//! - Linux: `$XDG_CONFIG_HOME/filen-cli`, or `~/.config/filen-cli` without it
//! - macOS: `~/Library/Application Support/filen-cli`
//! - Windows: `%APPDATA%\filen-cli`
//!
//! (development builds use `filen-cli-dev` instead)

mod account;
mod old_cli;
mod profile;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::{
	borrow::Cow,
	path::{Path, PathBuf},
	sync::Arc,
	time::Duration,
};

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use filen_sdk_rs::{
	ErrorKind,
	auth::{Client, TwoFASecret, unauth::UnauthClient},
	fs::HasUUID as _,
};

pub(crate) use self::account::{
	change_email, change_password, disable_two_factor, enable_two_factor, export_master_keys,
};
pub(crate) use self::profile::{ProfileName, Profiles, print_profiles};
use crate::{
	CliConfig, ClientConfigArgs, build_client_config, default_config_dir, deserialize_auth_config,
	serialize_auth_config,
	ui::UI,
	util::{ChunkStore as _, KeyringChunkStore, LongKeyringEntry},
};

/// A lazily authenticated client.
/// Since some commands (e. g. logout) don't need the user to be authenticated, we only authenticate when necessary.
pub(crate) enum LazyClient {
	Unauthenticated {
		config: CliConfig,
		email_arg: Option<String>,
		password_arg: Option<String>,
		auth_config_path_arg: Option<String>,
		two_factor_code_arg: Option<String>,
		client_config_args: ClientConfigArgs,
	},
	Authenticated {
		client: Arc<Client>,
		cache_callback_rx: tokio::sync::mpsc::UnboundedReceiver<filen_sdk_rs::cache::CacheMessage>,
		/// Kept for logout, which offers to delete the file.
		auth_config_path_arg: Option<String>,
		/// Where the login is kept, which a change of the credentials updates.
		kept_login: KeptLogin,
	},
}

/// Where the login a client was made from is kept, so that a command that changes the credentials
/// can update it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum KeptLogin {
	/// Given with `--email` and `--password`, or their environment variables, on each run.
	Arguments,
	/// In this auth config file.
	AuthConfig(PathBuf),
	/// In the system keyring's entry of the config dir.
	Keyring,
	/// In the credentials the old CLI saved at this path, which it was imported from on this run
	/// without being kept, so that it is imported from there again on the next run.
	OldCli(PathBuf),
	/// Nowhere: the login was typed at the prompt, and not kept.
	Nowhere,
}

/// A client that logged in, and where its login is kept.
struct Login {
	client: Client,
	kept: KeptLogin,
}

impl LazyClient {
	pub(crate) fn new(
		config: CliConfig,
		email_arg: Option<String>,
		password_arg: Option<String>,
		two_factor_code_arg: Option<String>,
		auth_config_path_arg: Option<String>,
		client_config_args: ClientConfigArgs,
	) -> Self {
		Self::Unauthenticated {
			config,
			email_arg,
			password_arg,
			two_factor_code_arg,
			auth_config_path_arg,
			client_config_args,
		}
	}

	pub(crate) async fn get(&mut self, ui: &mut UI) -> Result<&Arc<Client>> {
		Ok(self.get_with_kept_login(ui).await?.0)
	}

	/// [`LazyClient::get`], with where its login is kept.
	pub(crate) async fn get_with_kept_login(
		&mut self,
		ui: &mut UI,
	) -> Result<(&Arc<Client>, &KeptLogin)> {
		match self {
			Self::Authenticated {
				client, kept_login, ..
			} => Ok((client, kept_login)),
			Self::Unauthenticated {
				config,
				email_arg,
				password_arg,
				auth_config_path_arg,
				two_factor_code_arg,
				client_config_args,
			} => {
				let Login { client, kept } = authenticate_and_get_password(
					config,
					ui,
					email_arg.as_deref(),
					password_arg.as_deref(),
					two_factor_code_arg.as_deref(),
					auth_config_path_arg.as_deref(),
					client_config_args,
				)
				.await?;
				let (cache_callback_tx, cache_callback_rx) = tokio::sync::mpsc::unbounded_channel();
				client
					.configure_cache(config.config_dir.join(CACHE_FILE_NAME), move |msgs| {
						for msg in msgs {
							cache_callback_tx.send(msg).unwrap_or_else(|e| {
								log::warn!("Failed to send cache callback message: {:?}", e);
							});
						}
					})
					.await
					.context("Failed to configure cache")?;
				let auth_config_path_arg = auth_config_path_arg.take();
				*self = Self::Authenticated {
					client: Arc::new(client),
					cache_callback_rx,
					auth_config_path_arg,
					kept_login: kept,
				};
				let Self::Authenticated {
					client, kept_login, ..
				} = self
				else {
					unreachable!();
				};
				Ok((client, kept_login))
			}
		}
	}

	/// The auth config file given with `--auth-config-path`, if any.
	fn auth_config_path_arg(&self) -> Option<&Path> {
		match self {
			Self::Unauthenticated {
				auth_config_path_arg,
				..
			}
			| Self::Authenticated {
				auth_config_path_arg,
				..
			} => auth_config_path_arg.as_deref().map(Path::new),
		}
	}

	/// Whether this process may have the local cache open.
	fn cache_access(&self) -> CacheAccess {
		match self {
			Self::Unauthenticated { .. } => CacheAccess::Unused,
			// the cache was configured, and is opened by the first command that uses it
			Self::Authenticated { .. } => CacheAccess::ConfiguredHere,
		}
	}

	pub(crate) fn get_arc(&self) -> Option<Arc<Client>> {
		match self {
			Self::Authenticated { client, .. } => Some(client.clone()),
			Self::Unauthenticated { .. } => None,
		}
	}

	pub(crate) fn get_cache_callback_rx(
		&mut self,
	) -> Option<&mut tokio::sync::mpsc::UnboundedReceiver<filen_sdk_rs::cache::CacheMessage>> {
		match self {
			Self::Authenticated {
				cache_callback_rx, ..
			} => Some(cache_callback_rx),
			Self::Unauthenticated { .. } => None,
		}
	}
}

/// Authenticate by one of the available authentication methods.
async fn authenticate_and_get_password(
	config: &CliConfig,
	ui: &mut UI,
	email_arg: Option<&str>,
	password_arg: Option<&str>,
	two_factor_code_arg: Option<&str>,
	auth_config_path_arg: Option<&str>,
	client_config_args: &ClientConfigArgs,
) -> Result<Login> {
	if let Some(credentials) =
		cli_credentials(email_arg, password_arg, two_factor_code_arg, |name| {
			std::env::var(name).ok()
		})? {
		let client = login_and_optionally_prompt_two_factor_code(
			ui,
			&credentials.email,
			&credentials.password,
			credentials.two_factor_code.as_deref(),
			client_config_args,
		)
		.await?;
		log::info!("Authenticated from CLI arguments");
		Ok(Login {
			client,
			kept: KeptLogin::Arguments,
		})
	} else if let Some((client, export_path)) =
		authenticate_from_auth_config(config, ui, auth_config_path_arg, client_config_args).await?
	{
		log::info!(
			"Authenticated from auth config file {}",
			export_path.display()
		);
		Ok(Login {
			client,
			kept: KeptLogin::AuthConfig(export_path),
		})
	} else {
		match authenticate_from_keyring(config, ui, client_config_args).await {
			Ok(Some(client)) => {
				log::info!("Authenticated from keyring");
				return Ok(Login {
					client,
					kept: KeptLogin::Keyring,
				});
			}
			Ok(None) => {}
			Err(e) => log::warn!("Failed to authenticate from keyring: {:?}", e),
		}
		if let Some((login, path)) =
			authenticate_from_old_cli(config, ui, client_config_args).await?
		{
			log::info!(
				"Authenticated from credentials of the old CLI at {}",
				path.display()
			);
			return Ok(login);
		}
		authenticate_from_prompt(config, ui, client_config_args).await
	}
}

async fn login_and_optionally_prompt_two_factor_code(
	ui: &mut UI,
	email: &str,
	password: &str,
	two_factor_code: Option<&str>,
	client_config_args: &ClientConfigArgs,
) -> Result<Client> {
	try_login(ui, email, password, two_factor_code, client_config_args)
		.await?
		.map_err(RefusedLogin::into_error)
}

/// A login the server refused.
struct RefusedLogin {
	error: filen_sdk_rs::Error,
	/// What was being done, for an unexpected `error`.
	context: &'static str,
}

impl RefusedLogin {
	fn into_error(self) -> anyhow::Error {
		login_failure(self.error, self.context)
	}
}

/// [`login_and_optionally_prompt_two_factor_code`], with a login the server refused as the inner
/// error, so that the caller can tell it apart from a prompt that failed or was canceled.
async fn try_login(
	ui: &mut UI,
	email: &str,
	password: &str,
	two_factor_code: Option<&str>,
	client_config_args: &ClientConfigArgs,
) -> Result<std::result::Result<Client, RefusedLogin>> {
	let unauth_client = UnauthClient::from_config(build_client_config(client_config_args))?;

	let e = match unauth_client
		.login(
			email.to_owned(),
			password,
			two_factor_code.unwrap_or("XXXXXX"),
		)
		.await
	{
		Ok(client) => return Ok(Ok(client)),
		Err(e) => e,
	};
	if e.kind() == ErrorKind::Enter2fa
		&& let Some(secret) = totp_secret_from(|name| std::env::var(name).ok())
	{
		return login_with_totp_secret(ui, &unauth_client, email, password, &secret).await;
	}
	let can_prompt = ui.ensure_can_prompt(TWO_FACTOR_CODE_PROMPT).is_ok();
	match two_factor_prompt_after(e.kind(), can_prompt) {
		Some(TwoFactorPrompt::Required) => {}
		Some(TwoFactorPrompt::WrongCode) => ui.print_warning(WRONG_TWO_FACTOR_CODE),
		None => {
			return Ok(Err(RefusedLogin {
				error: e,
				context: "Failed to log in",
			}));
		}
	}
	let two_factor_code = ui.prompt_password(TWO_FACTOR_CODE_PROMPT)?;
	Ok(unauth_client
		.login(email.to_owned(), password, two_factor_code.trim())
		.await
		.map_err(|error| RefusedLogin {
			error,
			context: "Failed to log in (with 2fa)",
		}))
}

/// The variable holding the TOTP secret two-factor codes are made from, for logins without anyone
/// to type a code. Not a CLI argument, so that it never shows in the help or in a process list.
/// (! referenced in module docs)
const TOTP_SECRET_VAR: &str = "FILEN_CLI_2FA_SECRET";

/// The TOTP secret in [`TOTP_SECRET_VAR`], which `env` looks up, if it is set and not empty.
fn totp_secret_from(env: impl Fn(&str) -> Option<String>) -> Option<String> {
	env(TOTP_SECRET_VAR).filter(|secret| !secret.is_empty())
}

/// The two-factor code that the TOTP `secret` gives at `now`. `email` only names the account in
/// the secret's URL, which is not used.
fn totp_code(secret: &str, email: &str, now: DateTime<Utc>) -> Result<String> {
	// the SDK takes the secret by value
	TwoFASecret::new(secret.to_owned(), email)
		.make_totp_code(now)
		.map_err(|e| {
			// the error names neither the secret nor a code
			UI::failure(&format!(
				"{TOTP_SECRET_VAR} is not a valid two-factor authentication secret ({e})"
			))
		})
}

/// Log in with a code made from the TOTP `secret`, for an account that asked for a two-factor code.
///
/// The server takes each code once, so a code is wrong if an earlier login used it, such as the
/// previous run of a script within the same 30 second step, and it is also wrong if it was made
/// just before its step ended. Either way the next step's code is new: a wrong code is retried
/// once with it, waiting for that step to begin if it has not yet.
async fn login_with_totp_secret(
	ui: &mut UI,
	unauth_client: &UnauthClient,
	email: &str,
	password: &str,
	secret: &str,
) -> Result<std::result::Result<Client, RefusedLogin>> {
	// neither the secret nor the codes made from it are logged
	log::info!("Logging in with a two-factor code made from {TOTP_SECRET_VAR}");
	let mut code = totp_code(secret, email, Utc::now())?;
	for attempt in 1..=TOTP_LOGIN_ATTEMPTS {
		match unauth_client.login(email.to_owned(), password, &code).await {
			Ok(client) => return Ok(Ok(client)),
			Err(error) if error.kind() == ErrorKind::Wrong2fa => {}
			Err(error) => {
				return Ok(Err(RefusedLogin {
					error,
					context: "Failed to log in (with a code from the two-factor secret)",
				}));
			}
		}
		if attempt == TOTP_LOGIN_ATTEMPTS {
			break;
		}
		code = match next_totp_code(secret, email, &code, Utc::now())? {
			NextTotpCode::Now(next_code) => next_code,
			NextTotpCode::After(wait) => {
				ui.print_muted_to_stderr(&format!(
					"Waiting {} s for the next two-factor code",
					wait.as_secs()
				));
				tokio::time::sleep(wait).await;
				totp_code(secret, email, Utc::now())?
			}
		};
		log::info!("Retrying with the two-factor code of the next step");
	}
	// a failure, not a refused login: typing the email and password again would not help
	Err(UI::failure(&format!(
		"{WRONG_TWO_FACTOR_CODE}: check the secret in {TOTP_SECRET_VAR} and the system clock"
	)))
}

/// The code to retry a login with after the server refused `code`, made from the TOTP `secret`.
#[derive(Debug, PartialEq, Eq)]
enum NextTotpCode {
	/// The code of the step at the time, which differs from the refused one.
	Now(String),
	/// The refused code is the one of the step at the time: the next step begins after this long.
	After(Duration),
}

/// What to retry a login with after the server refused `code`, made from the TOTP `secret`, at
/// `now`.
fn next_totp_code(
	secret: &str,
	email: &str,
	code: &str,
	now: DateTime<Utc>,
) -> Result<NextTotpCode> {
	let next_code = totp_code(secret, email, now)?;
	Ok(if next_code == code {
		NextTotpCode::After(time_until_next_totp_step(now))
	} else {
		NextTotpCode::Now(next_code)
	})
}

/// How many codes made from the TOTP secret a login tries.
const TOTP_LOGIN_ATTEMPTS: u32 = 2;

/// How long a TOTP code of Filen is valid.
const TOTP_STEP_SECS: i64 = 30;

/// The time from `now` until the next TOTP step begins, at least a second and at most a step.
fn time_until_next_totp_step(now: DateTime<Utc>) -> Duration {
	// from the start of the second, which `timestamp` rounds down to, so never too short
	Duration::from_secs(
		(TOTP_STEP_SECS - now.timestamp().rem_euclid(TOTP_STEP_SECS)).unsigned_abs(),
	)
}

/// The prompt for a two-factor code, which the recovery key can stand in for.
const TWO_FACTOR_CODE_PROMPT: &str = "Two-factor authentication code or recovery key: ";
const WRONG_TWO_FACTOR_CODE: &str = "Two-factor authentication code wrong";

/// Why the user is asked for a two-factor code.
#[derive(Debug, PartialEq, Eq)]
enum TwoFactorPrompt {
	/// The account needs one, and none was given.
	Required,
	/// The one given with the flag or in the environment was wrong.
	WrongCode,
}

/// Whether to ask for a two-factor code after the first login attempt failed with `kind`. A wrong
/// code given up front is asked for again only if a prompt `can_prompt`: a script fails instead.
fn two_factor_prompt_after(kind: ErrorKind, can_prompt: bool) -> Option<TwoFactorPrompt> {
	match kind {
		ErrorKind::Enter2fa => Some(TwoFactorPrompt::Required),
		ErrorKind::Wrong2fa if can_prompt => Some(TwoFactorPrompt::WrongCode),
		_ => None,
	}
}

/// The error for a login that failed with `e`: the failure the user can fix by giving other
/// credentials, or `e` with `context` if it is unexpected.
fn login_failure(e: filen_sdk_rs::Error, context: &'static str) -> anyhow::Error {
	match login_failure_message(e.kind()) {
		Some(message) => UI::failure(message),
		None => anyhow!(e).context(context),
	}
}

/// The message for a login that failed with `kind` because of the credentials given, or `None` if
/// it failed for another reason.
fn login_failure_message(kind: ErrorKind) -> Option<&'static str> {
	match kind {
		ErrorKind::Enter2fa | ErrorKind::Wrong2fa => Some(WRONG_TWO_FACTOR_CODE),
		ErrorKind::EmailOrPasswordWrong => Some("Email or password wrong"),
		_ => None,
	}
}

/// Credentials given in the CLI arguments, borrowed from them, or in the legacy environment
/// variables, read from there.
struct CliCredentials<'a> {
	email: Cow<'a, str>,
	password: Cow<'a, str>,
	two_factor_code: Option<Cow<'a, str>>,
}

/// The variables the old Filen CLI read credentials from. (! referenced in module docs)
const LEGACY_EMAIL_VAR: &str = "FILEN_EMAIL";
const LEGACY_PASSWORD_VAR: &str = "FILEN_PASSWORD";
const LEGACY_TWO_FACTOR_CODE_VAR: &str = "FILEN_2FA_CODE";

/// The credentials to log in with from the CLI arguments, if an email or a password was given, or
/// else from the variables of the old Filen CLI, which `env` looks up.
/// A two-factor code alone, usually a `FILEN_CLI_2FA_CODE` left in the environment, is ignored,
/// so that it does not keep the login from going on through an auth config or the keyring.
fn cli_credentials<'a>(
	email_arg: Option<&'a str>,
	password_arg: Option<&'a str>,
	two_factor_code_arg: Option<&'a str>,
	env: impl Fn(&str) -> Option<String>,
) -> Result<Option<CliCredentials<'a>>> {
	match (email_arg, password_arg) {
		(Some(email), Some(password)) => Ok(Some(CliCredentials {
			email: Cow::Borrowed(email),
			password: Cow::Borrowed(password),
			two_factor_code: two_factor_code_arg.map(Cow::Borrowed),
		})),
		(Some(_), None) => Err(UI::failure("Password is required")),
		(None, Some(_)) => Err(UI::failure("Email is required")),
		(None, None) => legacy_env_credentials(two_factor_code_arg, env),
	}
}

/// The credentials in the variables of the old Filen CLI, which `env` looks up, if `FILEN_EMAIL`
/// is set. As there, `FILEN_PASSWORD` alone is ignored. A two-factor code from the arguments
/// wins over `FILEN_2FA_CODE`.
fn legacy_env_credentials<'a>(
	two_factor_code_arg: Option<&'a str>,
	env: impl Fn(&str) -> Option<String>,
) -> Result<Option<CliCredentials<'a>>> {
	let var = |name: &str| env(name).filter(|value| !value.is_empty());
	match (var(LEGACY_EMAIL_VAR), var(LEGACY_PASSWORD_VAR)) {
		(Some(email), Some(password)) => {
			log::info!("Using the credentials from {LEGACY_EMAIL_VAR} and {LEGACY_PASSWORD_VAR}");
			Ok(Some(CliCredentials {
				email: Cow::Owned(email),
				password: Cow::Owned(password),
				two_factor_code: two_factor_code_arg
					.map(Cow::Borrowed)
					.or_else(|| var(LEGACY_TWO_FACTOR_CODE_VAR).map(Cow::Owned)),
			}))
		}
		(Some(_), None) => Err(UI::failure(&format!(
			"{LEGACY_PASSWORD_VAR} is required with {LEGACY_EMAIL_VAR}"
		))),
		(None, password) => {
			if password.is_some() {
				log::info!(
					"Ignoring {LEGACY_PASSWORD_VAR}, which is only used with {LEGACY_EMAIL_VAR}"
				);
			}
			if two_factor_code_arg.is_some() {
				log::info!(
					"Ignoring the two-factor code, which is only used with an email and a password"
				);
			}
			Ok(None)
		}
	}
}

/// Authenticate using SDK config stored in a file ("auth config") exported from the CLI.
/// Checks the path provided via CLI argument (if any), or default locations: (! referenced in module docs)
/// - `./filen-cli-auth-config.txt` (current working directory)
/// - `{config_dir}/filen-cli-auth-config.txt`
///
/// A given auth config that is corrupt or whose login the server no longer accepts fails, so that
/// a script never stops at a prompt; an autodetected one is skipped with a warning, and the next
/// one is tried.
async fn authenticate_from_auth_config(
	config: &CliConfig,
	ui: &mut UI,
	path_arg: Option<&str>,
	client_config_args: &ClientConfigArgs,
) -> Result<Option<(Client, PathBuf)>> {
	let candidates = if let Some(path) = path_arg {
		// a path given explicitly must be used, never skipped for another account's credentials
		vec![(PathBuf::from(path), AuthConfigLocation::Given)]
	} else if testing_switch_is_on(TESTING_DISABLE_AUTODETECTED_AUTH_CONFIG) {
		return Ok(None);
	} else {
		get_auth_config_default_locations(config)
	};
	authenticate_from_auth_configs(ui, candidates, client_config_args).await
}

/// [`authenticate_from_auth_config`] with the auth config files `candidates`, in the order they
/// are tried in.
async fn authenticate_from_auth_configs(
	ui: &mut UI,
	candidates: Vec<(PathBuf, AuthConfigLocation)>,
	client_config_args: &ClientConfigArgs,
) -> Result<Option<(Client, PathBuf)>> {
	for (path, location) in candidates {
		if let Some(client) =
			authenticate_from_auth_config_file(ui, &path, location, client_config_args).await?
		{
			return Ok(Some((client, path)));
		}
	}
	Ok(None)
}

/// [`authenticate_from_auth_config`] with the auth config file at `path`, or `None` if it was
/// autodetected and cannot be used.
async fn authenticate_from_auth_config_file(
	ui: &mut UI,
	path: &Path,
	location: AuthConfigLocation,
	client_config_args: &ClientConfigArgs,
) -> Result<Option<Client>> {
	check_auth_config_access(ui, path, location)?;
	let sdk_config = read_auth_config(path)?;
	let client = match restore_login(&sdk_config, client_config_args).await {
		Ok(client) => client,
		Err(e) => {
			let problem = format!(
				"The auth config file {} cannot be used ({:#}): export a new one with \
				 `filen export-auth-config`",
				path.display(),
				e
			);
			return match location {
				AuthConfigLocation::Given => Err(UI::failure(&problem)),
				AuthConfigLocation::WorkingDir | AuthConfigLocation::ConfigDir => {
					ui.print_warning(&problem);
					Ok(None)
				}
			};
		}
	};
	if location == AuthConfigLocation::WorkingDir {
		// a file in whatever directory the CLI was started in is easy to overlook
		ui.print_muted_to_stderr(&format!(
			"Logged in as {} from the auth config file {}",
			client.email(),
			path.display()
		));
	}
	Ok(Some(client))
}

/// Where an auth config file was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AuthConfigLocation {
	/// Given with `--auth-config-path`
	Given,
	/// `./filen-cli-auth-config.txt`, in the current working directory
	WorkingDir,
	/// `{config_dir}/filen-cli-auth-config.txt`
	ConfigDir,
}

/// Refuse an autodetected auth config file that users other than its owner can access, and warn
/// about a given one: a file given explicitly is often mounted from elsewhere (a Docker secret is
/// readable by everyone by default), where the CLI cannot be the one to decide.
fn check_auth_config_access(ui: &mut UI, path: &Path, location: AuthConfigLocation) -> Result<()> {
	let open_to_others =
		is_open_to_other_users(path).map_err(|e| auth_config_read_failure(path, &e))?;
	if !open_to_others {
		return Ok(());
	}
	let problem = format!(
		"Other users can access the auth config file {}, which holds your account's keys: \
		 restrict it with `chmod 600 {}`",
		path.display(),
		path.display()
	);
	match location {
		AuthConfigLocation::Given => {
			ui.print_warning(&problem);
			Ok(())
		}
		AuthConfigLocation::WorkingDir | AuthConfigLocation::ConfigDir => {
			Err(UI::failure(&problem))
		}
	}
}

/// Whether the permissions of the file at `path` let users other than its owner access it.
#[cfg(unix)]
fn is_open_to_other_users(path: &Path) -> std::io::Result<bool> {
	Ok(std::fs::metadata(path)?.permissions().mode() & 0o077 != 0)
}

/// Whether the permissions of the file at `path` let users other than its owner access it. Not
/// checked outside Unix, where the file's ACL decides.
#[cfg(not(unix))]
fn is_open_to_other_users(_path: &Path) -> std::io::Result<bool> {
	Ok(false)
}

/// Read the auth config file at `path`.
fn read_auth_config(path: &Path) -> Result<String> {
	std::fs::read_to_string(path).map_err(|e| auth_config_read_failure(path, &e))
}

/// The failure for an auth config file at `path` that could not be read.
fn auth_config_read_failure(path: &Path, e: &std::io::Error) -> anyhow::Error {
	if e.kind() == std::io::ErrorKind::NotFound {
		UI::failure(&format!("Auth config file not found: {}", path.display()))
	} else {
		UI::failure(&format!(
			"Failed to read auth config file {}: {}",
			path.display(),
			e
		))
	}
}

/// Get the default locations for auth config files that exist: inside the current working
/// directory, and the config directory, in the order they are used in.
pub(crate) fn get_auth_config_default_locations(
	config: &CliConfig,
) -> Vec<(PathBuf, AuthConfigLocation)> {
	let mut locations = Vec::new();
	match std::env::current_dir() {
		Ok(current_dir) => locations.push((
			current_dir.join(AUTH_CONFIG_FILENAME),
			AuthConfigLocation::WorkingDir,
		)),
		Err(e) => log::warn!(
			"Failed to get current working directory, so not checking there for an auth config file: {:?}",
			e
		),
	}
	locations.push((
		config.config_dir.join(AUTH_CONFIG_FILENAME),
		AuthConfigLocation::ConfigDir,
	));
	locations
		.into_iter()
		.filter(|(path, _)| path.exists())
		.collect()
}

/// The keyring service of release builds, which keeps the existing logins.
const KEYRING_SERVICE: &str = "filen-cli-rs";
/// The keyring service of debug builds, so that they never use or delete a release login.
const KEYRING_SERVICE_DEV: &str = "filen-cli-rs-dev";
/// The keyring entry of the credentials used with the default config dir.
const KEYRING_SDK_CONFIG_NAME: &str = "sdk-config";

/// Where the keyring keeps the credentials used with a config dir.
#[derive(Debug, PartialEq, Eq)]
struct KeyringLocation {
	service: &'static str,
	entry_name: String,
}

impl KeyringLocation {
	/// The location of the credentials used with `config`'s config dir in this build.
	fn of(config: &CliConfig) -> Self {
		let is_dev = cfg!(debug_assertions);
		let default_dir = canonical_default_config_dir(is_dev);
		Self::for_config_dir(
			is_dev,
			&canonical_or_given(&config.config_dir),
			default_dir.as_deref(),
		)
	}

	/// The location of the credentials used with `config_dir`: the default config dir keeps the
	/// entry the CLI always used, and any other one an entry named after it, so that a
	/// `--config-dir` keeps its own login. (! referenced in module docs)
	fn for_config_dir(is_dev: bool, config_dir: &Path, default_config_dir: Option<&Path>) -> Self {
		let service = match is_dev {
			true => KEYRING_SERVICE_DEV,
			false => KEYRING_SERVICE,
		};
		let entry_name = if default_config_dir == Some(config_dir) {
			KEYRING_SDK_CONFIG_NAME.to_owned()
		} else {
			format!("{KEYRING_SDK_CONFIG_NAME}@{}", config_dir.display())
		};
		KeyringLocation {
			service,
			entry_name,
		}
	}

	fn store(&self) -> KeyringChunkStore {
		KeyringChunkStore::new(self.service)
	}
}

/// The config dir used without `--config-dir` in this build, resolved as [`canonical_or_given`].
fn canonical_default_config_dir(is_dev: bool) -> Option<PathBuf> {
	default_config_dir(is_dev).map(|dir| canonical_or_given(&dir))
}

/// Whether `config` uses the config dir used without `--config-dir`.
fn is_default_config_dir(config: &CliConfig, is_dev: bool) -> bool {
	canonical_default_config_dir(is_dev) == Some(canonical_or_given(&config.config_dir))
}

/// `path` with symlinks and relative components resolved, so that one directory always names one
/// keyring entry, or `path` itself if it cannot be resolved.
fn canonical_or_given(path: &Path) -> PathBuf {
	path.canonicalize().unwrap_or_else(|e| {
		log::warn!("Failed to resolve the path {}: {:?}", path.display(), e);
		path.to_path_buf()
	})
}

// The CLI's tests set these environment variables to `1`, so that a test run never reads or
// deletes the credentials of whoever runs it. Each one only turns a source of stored credentials
// off: someone who sets one outside the tests is at worst asked to log in, so they are harmless in
// a release build, and stay in it rather than behind a cfg the tests' binary would need too.

/// Turns the system keyring off for reading: no login is read from it or deleted from it at
/// logout, no key of the old CLI is read from it, and an imported login of the old CLI is not
/// offered to be kept in it.
const TESTING_DISABLE_KEYRING: &str = "FILEN_CLI_TESTING_DISABLE_KEYRING";
/// Turns off the auth config files in the default locations and the credentials the old CLI
/// saved. An auth config given with `--auth-config-path` is still used, and logout still offers to
/// delete the files in the default locations.
const TESTING_DISABLE_AUTODETECTED_AUTH_CONFIG: &str =
	"FILEN_CLI_TESTING_DISABLE_AUTODETECTED_AUTH_CONFIG";

/// Whether the tests set the switch `name` (see [`TESTING_DISABLE_KEYRING`]).
fn testing_switch_is_on(name: &str) -> bool {
	std::env::var(name).is_ok_and(|value| value == "1")
}

/// Whether the tests turned the keyring off, so that they neither read nor delete the
/// credentials of whoever runs them.
fn keyring_disabled_for_testing() -> bool {
	testing_switch_is_on(TESTING_DISABLE_KEYRING)
}

/// Authenticate using SDK config stored in the keyring.
async fn authenticate_from_keyring(
	config: &CliConfig,
	ui: &mut UI,
	client_config_args: &ClientConfigArgs,
) -> Result<Option<Client>> {
	if keyring_disabled_for_testing() {
		return Ok(None);
	}
	let location = KeyringLocation::of(config);
	authenticate_from_keyring_entry(
		&LongKeyringEntry::new(&location.store(), &location.entry_name),
		ui,
		client_config_args,
	)
	.await
}

/// [`authenticate_from_keyring`] with the SDK config in `entry`. Credentials that are corrupt, or
/// whose login the server no longer accepts, are deleted with a warning, so that the user logs in
/// again, whether or not they keep the new login.
async fn authenticate_from_keyring_entry(
	entry: &LongKeyringEntry<'_>,
	ui: &mut UI,
	client_config_args: &ClientConfigArgs,
) -> Result<Option<Client>> {
	let sdk_config = entry
		.read()
		.context("Failed to read SDK config from keyring")?;
	let Some(sdk_config) = sdk_config else {
		return Ok(None);
	};
	match restore_login(&sdk_config, client_config_args).await {
		Ok(client) => Ok(Some(client)),
		Err(e) => {
			log::warn!(
				"The credentials saved in the keyring cannot be used: {:#}",
				e
			);
			// so that the warning is not repeated on every run until a new login is kept
			if let Err(e) = entry.delete() {
				log::warn!(
					"Failed to delete the credentials saved in the keyring: {:#}",
					e
				);
			}
			ui.print_warning("Saved credentials are no longer valid, please log in again");
			Ok(None)
		}
	}
}

/// The client of the stored credentials `sdk_config`, once the server accepted them.
async fn restore_login(sdk_config: &str, client_config_args: &ClientConfigArgs) -> Result<Client> {
	let client = deserialize_auth_config(sdk_config, client_config_args)?;
	check_login_accepted(&client).await?;
	Ok(client)
}

/// The name looked up by [`check_login_accepted`]. Whether a directory of this name exists does not
/// matter.
const LOGIN_CHECK_DIR_NAME: &str = "filen-cli-login-check";

/// Fails if the server rejects the API key of `client`, which was built from stored credentials
/// without asking the server: the key may have been revoked since, by a password change or a
/// logout elsewhere. Any other error, such as being offline, lets the login go on, so that an
/// unreachable server never makes the user log in again.
async fn check_login_accepted(client: &Client) -> Result<()> {
	login_check_outcome(
		// the cheapest authenticated request: one name hash lookup, no listing, nothing to decrypt
		client.find_dir_uuid(client.root().uuid(), LOGIN_CHECK_DIR_NAME),
		LOGIN_CHECK_TIMEOUT,
	)
	.await
}

/// How long [`check_login_accepted`] waits for the server. The SDK retries a request that cannot
/// connect and waits minutes for a slow answer, which would hold up every command while offline.
const LOGIN_CHECK_TIMEOUT: Duration = Duration::from_secs(10);

/// [`check_login_accepted`] with the request `check`, which is given up on after `timeout` like
/// any other error that is not a rejected API key.
async fn login_check_outcome<T>(
	check: impl Future<Output = std::result::Result<T, filen_sdk_rs::Error>>,
	timeout: Duration,
) -> Result<()> {
	match tokio::time::timeout(timeout, check).await {
		Ok(Ok(_)) => Ok(()),
		Ok(Err(e)) if e.kind() == ErrorKind::Unauthenticated => {
			Err(anyhow!("the server no longer accepts this login"))
		}
		Ok(Err(e)) => {
			log::warn!(
				"Failed to check the stored credentials with the server, using them anyway: {:?}",
				e
			);
			Ok(())
		}
		Err(_) => {
			log::warn!(
				"The server did not answer the check of the stored credentials within {} s, \
				 using them anyway",
				timeout.as_secs()
			);
			Ok(())
		}
	}
}

/// Authenticate using credentials provided interactively.
async fn authenticate_from_prompt(
	config: &CliConfig,
	ui: &mut UI,
	client_config_args: &ClientConfigArgs,
) -> Result<Login> {
	ui.ensure_can_prompt("Email:")
		.context("Not logged in: log in with --email and --password, or with --auth-config-path")?;
	let client = login_at_prompt(
		ui,
		|ui| Ok((ui.prompt("Email:")?, ui.prompt_password("Password: ")?)),
		async |ui, email, password| {
			// the password as typed: it can start or end with a space, as one given with
			// --password can
			try_login(ui, email.trim(), password, None, client_config_args).await
		},
	)
	.await?;
	let kept = offer_to_keep_logged_in(config, ui, &client)?;
	Ok(Login { client, kept })
}

/// Log in with the email and password `ask` asks for, by `login`, asking again after a login
/// refused as [`prompted_login_retry`] allows.
async fn login_at_prompt<T>(
	ui: &mut UI,
	mut ask: impl FnMut(&mut UI) -> Result<(String, String)>,
	mut login: impl AsyncFnMut(&mut UI, &str, &str) -> Result<std::result::Result<T, RefusedLogin>>,
) -> Result<T> {
	let mut attempt = 1;
	loop {
		let (email, password) = ask(ui)?;
		let refused = match login(ui, &email, &password).await? {
			Ok(client) => return Ok(client),
			Err(refused) => refused,
		};
		match prompted_login_retry(refused.error.kind(), attempt) {
			Some(message) => {
				ui.print_warning(&format!("{message}, please try again"));
				attempt += 1;
			}
			None => return Err(refused.into_error()),
		}
	}
}

/// How often the user may type their credentials at the prompt. (! referenced in module docs)
const PROMPTED_LOGIN_ATTEMPTS: u32 = 3;

/// The warning to try again with, if the prompted login `attempt` (counting from 1) failed with
/// `kind` and the user has attempts left: only credentials typed wrong are asked for again, any
/// other failure ends the login. Credentials given in the arguments, the environment or a file are
/// never asked for again, so that a script fails at once.
fn prompted_login_retry(kind: ErrorKind, attempt: u32) -> Option<&'static str> {
	if attempt >= PROMPTED_LOGIN_ATTEMPTS {
		return None;
	}
	login_failure_message(kind)
}

const KEEP_LOGGED_IN_PROMPT: &str = "Keep me logged in?";

/// Offer to save the login of `client` in the keyring, or, where that is not possible, in an auth
/// config file. The answer defaults to yes, unlike in the old Filen CLI, since the login is kept
/// in the system keyring, not in a file. Returns where the login was kept.
fn offer_to_keep_logged_in(config: &CliConfig, ui: &mut UI, client: &Client) -> Result<KeptLogin> {
	if !ui.prompt_confirm(KEEP_LOGGED_IN_PROMPT, true)? {
		return Ok(KeptLogin::Nowhere);
	}
	let sdk_config = serialize_auth_config(client)?;
	let location = KeyringLocation::of(config);
	let store = location.store();
	// a container has no keyring service to keep a login in
	let keyring_entry =
		(!cfg!(feature = "is_docker")).then(|| LongKeyringEntry::new(&store, &location.entry_name));
	keep_login(
		&config.config_dir,
		ui,
		&sdk_config,
		keyring_entry.as_ref(),
		confirm_auth_config,
	)
}

/// Asks whether to save the login in an auth config file with `msg`, which `--yes` answers.
fn confirm_auth_config(ui: &mut UI, msg: &str) -> Result<bool> {
	ui.prompt_confirm(msg, false)
}

/// Save the login `sdk_config` in `keyring_entry`, or, without one or if that fails, explain the
/// other ways to stay logged in and offer, through `ask`, to save it in an auth config file in
/// `config_dir`. Returns where the login was kept. (! referenced in module docs)
fn keep_login(
	config_dir: &Path,
	ui: &mut UI,
	sdk_config: &str,
	keyring_entry: Option<&LongKeyringEntry<'_>>,
	ask: impl FnOnce(&mut UI, &str) -> Result<bool>,
) -> Result<KeptLogin> {
	let problem = match keyring_entry.map(|entry| entry.write(sdk_config)) {
		Some(Ok(())) => {
			ui.print_success("Saved credentials");
			return Ok(KeptLogin::Keyring);
		}
		Some(Err(e)) => {
			log::warn!("Failed to save the credentials in the keyring: {:#}", e);
			"Failed to save the credentials in the system keyring"
		}
		None => "The system keyring is not used in the Docker image",
	};
	ui.print_warning(problem);
	ui.print_to_stderr(&keep_login_without_keyring_hint(config_dir));
	if !ask(
		ui,
		&format!(
			"Save an unencrypted auth config file in {} instead?",
			config_dir.display()
		),
	)? {
		return Ok(KeptLogin::Nowhere);
	}
	let path = config_dir.join(AUTH_CONFIG_FILENAME);
	write_private_file(&path, sdk_config)
		.with_context(|| format!("Failed to write auth config to file {}", path.display()))?;
	ui.print_success(&format!("Saved credentials to {}", path.display()));
	Ok(KeptLogin::AuthConfig(path))
}

/// The ways to stay logged in without the system keyring, with the config dir `config_dir`.
fn keep_login_without_keyring_hint(config_dir: &Path) -> String {
	format!(
		"To stay logged in without it, either:\n\
		 - save an auth config file in {}, which only you can read, but which holds your \
		 credentials unencrypted\n\
		 - pass --email and --password (or set FILEN_CLI_EMAIL and FILEN_CLI_PASSWORD) on each run\n\
		 - export an auth config with `filen export-auth-config` and pass --auth-config-path on \
		 each run",
		config_dir.display()
	)
}

/// Authenticate with credentials the old Filen CLI saved, the first of them that can be imported,
/// offering to keep the login in the keyring. Its files are left as they are, for the old CLI.
/// Its data directory, with the login it kept, is only looked at with the default config dir,
/// since another config dir keeps a login of its own.
async fn authenticate_from_old_cli(
	config: &CliConfig,
	ui: &mut UI,
	client_config_args: &ClientConfigArgs,
) -> Result<Option<(Login, PathBuf)>> {
	if testing_switch_is_on(TESTING_DISABLE_AUTODETECTED_AUTH_CONFIG) {
		return Ok(None);
	}
	let is_dev = cfg!(debug_assertions);
	let data_dir = is_default_config_dir(config, is_dev)
		.then(|| {
			let home_install_dir = dirs::home_dir()
				.map(|home| home.join(".filen-cli"))
				.filter(|dir| dir.exists());
			old_cli::data_dir(
				is_dev,
				dirs::config_dir().as_deref(),
				home_install_dir.as_deref(),
			)
		})
		.flatten();
	let working_dir = std::env::current_dir()
		.inspect_err(|e| {
			log::warn!(
				"Failed to get current working directory, so not checking there for credentials of the old CLI: {:?}",
				e
			)
		})
		.ok();
	let saved = old_cli::find(working_dir.as_deref(), data_dir.as_deref());
	import_old_cli_login(config, ui, saved, client_config_args, |name| {
		if keyring_disabled_for_testing() {
			return Ok(None);
		}
		KeyringChunkStore::new(old_cli::KEYCHAIN_SERVICE).get(name)
	})
	.await
}

/// [`authenticate_from_old_cli`] with the `saved` logins, and `keychain` looking up a key the old
/// CLI kept in the keychain.
async fn import_old_cli_login(
	config: &CliConfig,
	ui: &mut UI,
	saved: Vec<old_cli::SavedLogin>,
	client_config_args: &ClientConfigArgs,
	keychain: impl Fn(&str) -> Result<Option<String>>,
) -> Result<Option<(Login, PathBuf)>> {
	let key_name = old_cli::keychain_key_name(cfg!(debug_assertions));
	for saved in saved {
		match restore_old_cli_login(&saved, client_config_args, || keychain(key_name)).await {
			Ok(client) => {
				ui.print_muted_to_stderr(&format!(
					"Logged in as {} with the credentials saved by the old Filen CLI at {}",
					client.email(),
					saved.path.display()
				));
				// without a terminal or --yes, the login is used once and imported again next time
				let kept = if !keyring_disabled_for_testing()
					&& ui.ensure_can_confirm(KEEP_LOGGED_IN_PROMPT).is_ok()
				{
					offer_to_keep_logged_in(config, ui, &client)?
				} else {
					KeptLogin::Nowhere
				};
				let kept = match kept {
					KeptLogin::Nowhere => KeptLogin::OldCli(saved.path.clone()),
					kept => kept,
				};
				return Ok(Some((Login { client, kept }, saved.path)));
			}
			Err(e) => {
				log::warn!(
					"Failed to import the credentials of the old CLI at {}: {:#}",
					saved.path.display(),
					e
				);
				ui.print_warning(&format!(
					"Found credentials saved by the old Filen CLI at {} that could not be \
					 imported; please log in again",
					saved.path.display()
				));
			}
		}
	}
	Ok(None)
}

/// The client of the login the old CLI saved in `saved`, once the server accepted it.
async fn restore_old_cli_login(
	saved: &old_cli::SavedLogin,
	client_config_args: &ClientConfigArgs,
	key: impl FnOnce() -> Result<Option<String>>,
) -> Result<Client> {
	let stringified = saved.read(key)?;
	let client = UnauthClient::from_config(build_client_config(client_config_args))?
		.from_stringified(stringified)
		.context("Failed to create client from SDK config")?;
	check_login_accepted(&client).await?;
	Ok(client)
}

const AUTH_CONFIG_FILENAME: &str = "filen-cli-auth-config.txt"; // (!) referenced in module docs

/// Write `contents` to `path`, creating or truncating it with owner-only permissions
/// (mode `0o600` on Unix). The auth config holds the master keys, private key and API key,
/// so it must never be left group- or world-readable for other users on the machine.
fn write_private_file(path: &Path, contents: &str) -> std::io::Result<()> {
	use std::io::Write;

	let mut options = std::fs::OpenOptions::new();
	options.write(true).create(true).truncate(true);
	#[cfg(unix)]
	{
		use std::os::unix::fs::OpenOptionsExt;
		options.mode(0o600);
	}
	let mut file = options.open(path)?;
	// A pre-existing file keeps its previous mode through create+truncate, so tighten it
	// explicitly while it is still empty, before any secret bytes are written.
	#[cfg(unix)]
	{
		use std::os::unix::fs::PermissionsExt;
		file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
	}
	file.write_all(contents.as_bytes())
}

/// Export an auth config of `client`'s login to the file `path`.
fn export_auth_config(client: &Client, path: &Path) -> Result<()> {
	let sdk_config = serialize_auth_config(client)?;
	write_private_file(path, &sdk_config)
		.with_context(|| format!("Failed to write auth config to file {}", path.display()))
}

const EXPORT_RISK_PROMPT: &str = "The auth config holds your full, unencrypted credentials: \
	anyone who can read the file can access your account. Export?";

/// Export an auth config to `path_arg`, a file or a directory to write
/// `filen-cli-auth-config.txt` into, or to that file in the working directory without one, after
/// the user confirmed the risk and, if the file exists, overwriting it. Returns the file written,
/// or `None` if the user declined.
pub(crate) async fn export_auth_config_command(
	client: &mut LazyClient,
	ui: &mut UI,
	path_arg: Option<&Path>,
) -> Result<Option<PathBuf>> {
	// before logging in, so that a script learns it needs --yes before anything else happens
	ui.ensure_can_confirm(EXPORT_RISK_PROMPT)?;
	let path = auth_config_export_path(path_arg, || {
		std::env::current_dir().context("Failed to get current working directory")
	})?;
	if !confirm_export(ui, &path)? {
		ui.print_muted("Auth config not exported");
		return Ok(None);
	}
	let client = client.get(ui).await?;
	export_auth_config(client, &path)?;
	Ok(Some(path))
}

/// The file `export-auth-config` writes to: `path_arg`, or `filen-cli-auth-config.txt` in it if it
/// is a directory, or in the `working_dir` without one.
fn auth_config_export_path(
	path_arg: Option<&Path>,
	working_dir: impl FnOnce() -> Result<PathBuf>,
) -> Result<PathBuf> {
	Ok(match path_arg {
		None => working_dir()?.join(AUTH_CONFIG_FILENAME),
		Some(path) if path.is_dir() => path.join(AUTH_CONFIG_FILENAME),
		Some(path) => path.to_path_buf(),
	})
}

/// Asks the user to confirm exporting the credentials to `path`, and overwriting it if it exists.
/// `--yes` answers both.
fn confirm_export(ui: &mut UI, path: &Path) -> Result<bool> {
	if !ui.prompt_confirm(EXPORT_RISK_PROMPT, false)? {
		return Ok(false);
	}
	if path.exists() {
		return ui.prompt_confirm(
			&format!("{} already exists. Overwrite it?", path.display()),
			false,
		);
	}
	Ok(true)
}

const API_KEY_PRINT_PROMPT: &str =
	"This prints your API key, which gives full access to your account. Proceed?";

/// Asks a person who reads the output to confirm printing the account's API key, before logging in.
/// Returns whether to print it, and says so if not.
pub(crate) fn confirm_api_key_print(ui: &mut UI) -> Result<bool> {
	if confirm_secret_print(ui, API_KEY_PRINT_PROMPT)? {
		return Ok(true);
	}
	ui.print_muted("API key not printed");
	Ok(false)
}

/// Asks the user to confirm printing a secret with `msg`, which `--yes` answers, if a person reads
/// the output and can be asked. Output to a pipe or a file, JSON, or a script without a terminal
/// was asked for on purpose, and gets the secret without a prompt that would fail it.
fn confirm_secret_print(ui: &mut UI, msg: &str) -> Result<bool> {
	if !secret_print_needs_confirmation(ui, msg) {
		return Ok(true);
	}
	ui.prompt_confirm(msg, false)
}

/// Whether printing a secret is confirmed first with `msg`, see [`confirm_secret_print`].
fn secret_print_needs_confirmation(ui: &UI, msg: &str) -> bool {
	ui.output_is_read_by_a_person() && ui.ensure_can_confirm(msg).is_ok()
}

/// What [`logout`] did.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum LogoutOutcome {
	/// There were no stored credentials.
	NothingFound,
	/// Every deletion was declined.
	NothingDeleted,
	/// At least some stored credentials were deleted.
	Deleted,
}

/// The local cache of the drive's file listings, in the config dir: an SQLite database.
const CACHE_FILE_NAME: &str = "filen-sdk-rs-cache";

/// Whether this process may have the local cache open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CacheAccess {
	/// It never logged in, so it never opened the cache.
	Unused,
	/// It logged in and configured the cache, which may be open.
	ConfiguredHere,
}

/// The stored credentials [`logout`] offers to delete, besides the auth config files in the default
/// locations.
struct StoredCredentials<'a> {
	/// The keyring entry, unless the tests turned the keyring off.
	keyring_entry: Option<&'a LongKeyringEntry<'a>>,
	/// The auth config file given with `--auth-config-path`.
	given_auth_config: Option<&'a Path>,
}

/// Log out by deleting stored credentials from the keyring and auth config files, those in the
/// default locations and the one given with `--auth-config-path`, prompting the user to confirm
/// each (`--yes` confirms them all). Then offers to delete the local cache. The sync databases are
/// left alone: they hold the state of every sync pair, which deleting them would reset.
pub(crate) fn logout(
	config: &CliConfig,
	ui: &mut UI,
	client: &LazyClient,
) -> Result<LogoutOutcome> {
	let keyring_location = KeyringLocation::of(config);
	let keyring_store = keyring_location.store();
	let keyring_entry = (!keyring_disabled_for_testing())
		.then(|| LongKeyringEntry::new(&keyring_store, &keyring_location.entry_name));
	logout_from(
		config,
		ui,
		StoredCredentials {
			keyring_entry: keyring_entry.as_ref(),
			given_auth_config: client.auth_config_path_arg(),
		},
		client.cache_access(),
		|ui, msg| ui.prompt_confirm(msg, false),
	)
}

/// [`logout`] from the `stored` credentials and the auth config files in the default locations,
/// then from the local cache, which `cache` tells if this process may have open, asking `confirm`
/// before each deletion.
fn logout_from(
	config: &CliConfig,
	ui: &mut UI,
	stored: StoredCredentials<'_>,
	cache: CacheAccess,
	mut confirm: impl FnMut(&mut UI, &str) -> Result<bool>,
) -> Result<LogoutOutcome> {
	let outcome = delete_credentials(config, ui, &stored, &mut confirm)?;
	offer_to_delete_cache(
		ui,
		&config.config_dir.join(CACHE_FILE_NAME),
		cache,
		&mut confirm,
	)?;
	Ok(outcome)
}

/// The credentials part of [`logout_from`].
fn delete_credentials(
	config: &CliConfig,
	ui: &mut UI,
	stored: &StoredCredentials<'_>,
	confirm: &mut impl FnMut(&mut UI, &str) -> Result<bool>,
) -> Result<LogoutOutcome> {
	let mut found_any_credentials = false;
	let mut deleted_any_credentials = false;
	if let Some(keyring_entry) = stored.keyring_entry
		&& let Ok(Some(_)) = keyring_entry.read()
	{
		found_any_credentials = true;
		if confirm(ui, "Delete credentials stored in system keyring?")? {
			let deleted = keyring_entry
				.delete()
				.context("Failed to delete SDK config from keyring")?;
			if deleted {
				deleted_any_credentials = true;
				ui.print_success("Credentials deleted from keyring");
			} else {
				ui.print_failure("No credentials found in keyring");
			}
		}
	}
	for path in auth_config_files(stored.given_auth_config, config) {
		if path.exists() {
			found_any_credentials = true;
			if confirm(
				ui,
				&format!("Delete auth config file at {}?", path.display()),
			)? {
				std::fs::remove_file(&path).with_context(|| {
					format!("Failed to delete auth config file at {}", path.display())
				})?;
				deleted_any_credentials = true;
				ui.print_success(&format!("Deleted auth config file at {}", path.display()));
			}
		}
	}
	if deleted_any_credentials {
		Ok(LogoutOutcome::Deleted)
	} else if found_any_credentials {
		ui.print_muted("Nothing deleted");
		Ok(LogoutOutcome::NothingDeleted)
	} else {
		ui.print_muted("No credentials found in system keyring or auth config default locations");
		Ok(LogoutOutcome::NothingFound)
	}
}

/// The auth config files that exist: `given`, then those in the default locations, each once.
fn auth_config_files(given: Option<&Path>, config: &CliConfig) -> Vec<PathBuf> {
	let mut files = Vec::new();
	if let Some(given) = given {
		if given.exists() {
			files.push(given.to_path_buf());
		} else {
			log::info!(
				"The auth config file {} does not exist, so there is nothing to delete",
				given.display()
			);
		}
	}
	for (path, _) in get_auth_config_default_locations(config) {
		if !files.iter().any(|file| is_same_file(file, &path)) {
			files.push(path);
		}
	}
	files
}

/// Whether `a` and `b` name the same existing file.
fn is_same_file(a: &Path, b: &Path) -> bool {
	match (a.canonicalize(), b.canonicalize()) {
		(Ok(a), Ok(b)) => a == b,
		_ => false,
	}
}

/// Offer to delete the local cache at `cache_path`, unless this process may have it open (`cache`):
/// an open database must not be deleted under the process using it. Without a terminal and `--yes`,
/// the cache is left with a note rather than failing the logout, since it holds no credentials.
fn offer_to_delete_cache(
	ui: &mut UI,
	cache_path: &Path,
	cache: CacheAccess,
	confirm: &mut impl FnMut(&mut UI, &str) -> Result<bool>,
) -> Result<()> {
	if !cache_path.exists() {
		return Ok(());
	}
	match cache {
		CacheAccess::ConfiguredHere => {
			ui.print_muted(&format!(
				"The local cache of your drive at {} is in use; delete it after exiting to remove it",
				cache_path.display()
			));
		}
		CacheAccess::Unused => {
			let question = format!(
				"Also delete the local cache of your drive at {}?",
				cache_path.display()
			);
			if ui.ensure_can_confirm(&question).is_err() {
				ui.print_muted(&format!(
					"Kept the local cache of your drive at {} (pass --yes to delete it too)",
					cache_path.display()
				));
			} else if confirm(ui, &question)? {
				delete_sqlite_database(cache_path)?;
				ui.print_success(&format!(
					"Deleted the local cache at {}",
					cache_path.display()
				));
			}
		}
	}
	Ok(())
}

/// Delete the SQLite database at `path` with the journal files SQLite keeps beside it.
fn delete_sqlite_database(path: &Path) -> Result<()> {
	for suffix in ["", "-wal", "-shm", "-journal"] {
		let mut file = path.as_os_str().to_owned();
		file.push(suffix);
		let file = PathBuf::from(file);
		match std::fs::remove_file(&file) {
			Ok(()) => {}
			Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
			Err(e) => {
				return Err(e).with_context(|| format!("Failed to delete {}", file.display()));
			}
		}
	}
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::{
		error::exit_code,
		ui::test_support::Printed,
		util::{ChunkStore, test_support::MemoryChunkStore},
	};

	/// A config in a fresh config dir.
	fn config_in_temp_dir() -> (assert_fs::TempDir, CliConfig) {
		let dir = assert_fs::TempDir::new().unwrap();
		let config = CliConfig {
			config_dir: dir.path().to_path_buf(),
			profiles: Profiles::new(dir.path(), None),
		};
		(dir, config)
	}

	#[test]
	fn declining_to_delete_the_keyring_login_deletes_nothing() {
		let (_dir, config) = config_in_temp_dir();
		let store = MemoryChunkStore::default();
		let entry = LongKeyringEntry::new(&store, "sdk-config");
		entry.write("credentials").unwrap();
		let printed = Printed::default();

		let outcome = logout_from(
			&config,
			&mut printed.ui(),
			keyring_only(Some(&entry)),
			CacheAccess::Unused,
			|_, _| Ok(false),
		)
		.unwrap();

		assert_eq!(outcome, LogoutOutcome::NothingDeleted);
		assert_eq!(entry.read().unwrap().as_deref(), Some("credentials"));
		assert_eq!(
			console::strip_ansi_codes(&printed.lines()),
			"Nothing deleted"
		);
	}

	#[test]
	fn declining_to_delete_an_auth_config_deletes_nothing() {
		let (_dir, config) = config_in_temp_dir();
		let path = config.config_dir.join(AUTH_CONFIG_FILENAME);
		std::fs::write(&path, "secret-config").unwrap();

		let outcome = logout_from(
			&config,
			&mut Printed::default().ui(),
			keyring_only(None),
			CacheAccess::Unused,
			|_, _| Ok(false),
		)
		.unwrap();

		assert_eq!(outcome, LogoutOutcome::NothingDeleted);
		assert!(path.exists());
	}

	#[test]
	fn confirming_deletes_the_keyring_login_and_the_auth_config() {
		let (_dir, config) = config_in_temp_dir();
		let path = config.config_dir.join(AUTH_CONFIG_FILENAME);
		std::fs::write(&path, "secret-config").unwrap();
		let store = MemoryChunkStore::default();
		let entry = LongKeyringEntry::new(&store, "sdk-config");
		entry.write("credentials").unwrap();
		let mut ui = Printed::default().ui();
		ui.set_assume_yes(true);

		let outcome = logout_from(
			&config,
			&mut ui,
			keyring_only(Some(&entry)),
			CacheAccess::Unused,
			|ui, msg| ui.prompt_confirm(msg, false),
		)
		.unwrap();

		assert_eq!(outcome, LogoutOutcome::Deleted);
		assert!(store.keys().is_empty());
		assert!(!path.exists());
	}

	#[test]
	fn logging_out_without_stored_credentials_finds_nothing() {
		let (_dir, config) = config_in_temp_dir();
		let store = MemoryChunkStore::default();
		let entry = LongKeyringEntry::new(&store, "sdk-config");

		let outcome = logout_from(
			&config,
			&mut Printed::default().ui(),
			keyring_only(Some(&entry)),
			CacheAccess::Unused,
			|_, _| panic!("nothing to confirm"),
		)
		.unwrap();

		assert_eq!(outcome, LogoutOutcome::NothingFound);
	}

	/// The stored credentials of a run without `--auth-config-path`.
	fn keyring_only<'a>(keyring_entry: Option<&'a LongKeyringEntry<'a>>) -> StoredCredentials<'a> {
		StoredCredentials {
			keyring_entry,
			given_auth_config: None,
		}
	}

	#[test]
	fn logout_deletes_the_auth_config_given_with_the_flag() {
		let (_dir, config) = config_in_temp_dir();
		let elsewhere = assert_fs::TempDir::new().unwrap();
		let given = elsewhere.path().join("my-auth-config.txt");
		std::fs::write(&given, "secret-config").unwrap();
		let mut asked = Vec::new();

		let outcome = logout_from(
			&config,
			&mut Printed::default().ui(),
			StoredCredentials {
				keyring_entry: None,
				given_auth_config: Some(&given),
			},
			CacheAccess::Unused,
			|_, msg| {
				asked.push(msg.to_owned());
				Ok(true)
			},
		)
		.unwrap();

		assert_eq!(outcome, LogoutOutcome::Deleted);
		assert_eq!(
			asked,
			[format!("Delete auth config file at {}?", given.display())]
		);
		assert!(!given.exists());
	}

	#[test]
	fn an_auth_config_given_and_in_a_default_location_is_offered_once() {
		let (_dir, config) = config_in_temp_dir();
		let path = config.config_dir.join(AUTH_CONFIG_FILENAME);
		std::fs::write(&path, "secret-config").unwrap();
		let mut asked = 0;

		logout_from(
			&config,
			&mut Printed::default().ui(),
			StoredCredentials {
				keyring_entry: None,
				given_auth_config: Some(&path),
			},
			CacheAccess::Unused,
			|_, _| {
				asked += 1;
				Ok(false)
			},
		)
		.unwrap();

		assert_eq!(asked, 1);
	}

	#[test]
	fn a_missing_auth_config_given_with_the_flag_is_not_offered() {
		let (_dir, config) = config_in_temp_dir();
		let missing = config.config_dir.join("missing.txt");

		let outcome = logout_from(
			&config,
			&mut Printed::default().ui(),
			StoredCredentials {
				keyring_entry: None,
				given_auth_config: Some(&missing),
			},
			CacheAccess::Unused,
			|_, _| panic!("nothing to confirm"),
		)
		.unwrap();

		assert_eq!(outcome, LogoutOutcome::NothingFound);
	}

	/// A cache database with its write-ahead log in `config`'s config dir.
	fn cache_in(config: &CliConfig) -> (PathBuf, PathBuf) {
		let cache = config.config_dir.join(CACHE_FILE_NAME);
		let wal = config.config_dir.join("filen-sdk-rs-cache-wal");
		std::fs::write(&cache, "cache").unwrap();
		std::fs::write(&wal, "wal").unwrap();
		(cache, wal)
	}

	#[test]
	fn confirming_deletes_the_cache_with_its_journal_files() {
		let (_dir, config) = config_in_temp_dir();
		let (cache, wal) = cache_in(&config);

		let outcome = logout_from(
			&config,
			&mut Printed::default().prompting_ui(),
			keyring_only(None),
			CacheAccess::Unused,
			|_, msg| {
				assert_eq!(
					msg,
					format!(
						"Also delete the local cache of your drive at {}?",
						cache.display()
					)
				);
				Ok(true)
			},
		)
		.unwrap();

		// the cache holds no credentials
		assert_eq!(outcome, LogoutOutcome::NothingFound);
		assert!(!cache.exists());
		assert!(!wal.exists());
	}

	#[test]
	fn declining_keeps_the_cache() {
		let (_dir, config) = config_in_temp_dir();
		let (cache, wal) = cache_in(&config);

		logout_from(
			&config,
			&mut Printed::default().prompting_ui(),
			keyring_only(None),
			CacheAccess::Unused,
			|_, _| Ok(false),
		)
		.unwrap();

		assert!(cache.exists());
		assert!(wal.exists());
	}

	#[test]
	fn without_a_terminal_or_yes_the_cache_is_kept_without_failing() {
		let (_dir, config) = config_in_temp_dir();
		let (cache, _wal) = cache_in(&config);
		let printed = Printed::default();

		let outcome = logout_from(
			&config,
			&mut printed.ui(),
			keyring_only(None),
			CacheAccess::Unused,
			|_, _| panic!("a confirmation nobody can answer must not be asked"),
		)
		.unwrap();

		assert_eq!(outcome, LogoutOutcome::NothingFound);
		assert!(cache.exists());
		assert_eq!(
			console::strip_ansi_codes(&printed.lines()),
			format!(
				"No credentials found in system keyring or auth config default locations\nKept the \
				 local cache of your drive at {} (pass --yes to delete it too)",
				cache.display()
			)
		);
	}

	#[test]
	fn a_cache_this_process_configured_is_not_offered_for_deletion() {
		let (_dir, config) = config_in_temp_dir();
		let (cache, _wal) = cache_in(&config);
		let printed = Printed::default();

		logout_from(
			&config,
			&mut printed.ui(),
			keyring_only(None),
			CacheAccess::ConfiguredHere,
			|_, _| panic!("an open cache must not be offered for deletion"),
		)
		.unwrap();

		assert!(cache.exists());
		assert_eq!(
			console::strip_ansi_codes(&printed.lines()),
			format!(
				"No credentials found in system keyring or auth config default locations\nThe local \
				 cache of your drive at {} is in use; delete it after exiting to remove it",
				cache.display()
			)
		);
	}

	/// An auth config file in a fresh directory, with the permissions `mode`.
	#[cfg(unix)]
	fn auth_config_with_mode(mode: u32) -> (assert_fs::TempDir, PathBuf) {
		let dir = assert_fs::TempDir::new().unwrap();
		let path = dir.path().join(AUTH_CONFIG_FILENAME);
		std::fs::write(&path, "secret-config").unwrap();
		std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
		(dir, path)
	}

	#[cfg(unix)]
	#[test]
	fn a_file_readable_by_the_group_and_others_is_open_to_other_users() {
		let (_dir, path) = auth_config_with_mode(0o644);
		assert!(is_open_to_other_users(&path).unwrap());
	}

	#[cfg(unix)]
	#[test]
	fn a_file_only_its_owner_can_access_is_not_open_to_other_users() {
		let (_dir, path) = auth_config_with_mode(0o600);
		assert!(!is_open_to_other_users(&path).unwrap());
	}

	#[cfg(unix)]
	#[test]
	fn an_autodetected_auth_config_open_to_other_users_is_refused() {
		let (_dir, path) = auth_config_with_mode(0o644);
		for location in [
			AuthConfigLocation::WorkingDir,
			AuthConfigLocation::ConfigDir,
		] {
			let printed = Printed::default();
			let mut ui = printed.ui();
			let err = check_auth_config_access(&mut ui, &path, location).unwrap_err();
			assert_eq!(
				format!("{err:#}"),
				format!(
					"Other users can access the auth config file {}, which holds your account's \
					 keys: restrict it with `chmod 600 {}`",
					path.display(),
					path.display()
				)
			);
			assert_eq!(exit_code(&err), 1);
			assert_eq!(printed.lines_or_empty(), "");
		}
	}

	#[cfg(unix)]
	#[test]
	fn a_given_auth_config_open_to_other_users_is_used_with_a_warning() {
		let (_dir, path) = auth_config_with_mode(0o644);
		let out = Printed::default();
		let err = Printed::default();
		let mut ui = Printed::split_ui(&out, &err);
		check_auth_config_access(&mut ui, &path, AuthConfigLocation::Given).unwrap();
		assert_eq!(out.lines_or_empty(), "");
		assert_eq!(
			console::strip_ansi_codes(&err.lines()),
			format!(
				"⚠ Other users can access the auth config file {}, which holds your account's \
				 keys: restrict it with `chmod 600 {}`",
				path.display(),
				path.display()
			)
		);
	}

	#[cfg(unix)]
	#[test]
	fn an_autodetected_auth_config_only_its_owner_can_access_is_used_silently() {
		let (_dir, path) = auth_config_with_mode(0o600);
		let printed = Printed::default();
		let mut ui = printed.ui();
		check_auth_config_access(&mut ui, &path, AuthConfigLocation::WorkingDir).unwrap();
		assert_eq!(printed.lines_or_empty(), "");
	}

	#[test]
	fn the_default_config_dir_keeps_the_keyring_entry_of_earlier_versions() {
		let dir = Path::new("/home/user/.config/filen-cli");
		assert_eq!(
			KeyringLocation::for_config_dir(false, dir, Some(dir)),
			KeyringLocation {
				service: "filen-cli-rs",
				entry_name: "sdk-config".to_owned(),
			}
		);
	}

	#[test]
	fn another_config_dir_has_a_keyring_entry_of_its_own() {
		assert_eq!(
			KeyringLocation::for_config_dir(
				false,
				Path::new("/srv/filen"),
				Some(Path::new("/home/user/.config/filen-cli"))
			),
			KeyringLocation {
				service: "filen-cli-rs",
				entry_name: "sdk-config@/srv/filen".to_owned(),
			}
		);
	}

	#[test]
	fn a_config_dir_without_a_default_has_a_keyring_entry_of_its_own() {
		assert_eq!(
			KeyringLocation::for_config_dir(false, Path::new("/srv/filen"), None).entry_name,
			"sdk-config@/srv/filen"
		);
	}

	#[test]
	fn a_debug_build_uses_a_keyring_service_of_its_own() {
		let dir = Path::new("/home/user/.config/filen-cli-dev");
		assert_eq!(
			KeyringLocation::for_config_dir(true, dir, Some(dir)),
			KeyringLocation {
				service: "filen-cli-rs-dev",
				entry_name: "sdk-config".to_owned(),
			}
		);
	}

	#[test]
	fn a_missing_two_factor_code_is_asked_for() {
		for can_prompt in [true, false] {
			assert_eq!(
				two_factor_prompt_after(ErrorKind::Enter2fa, can_prompt),
				Some(TwoFactorPrompt::Required)
			);
		}
	}

	#[test]
	fn a_wrong_two_factor_code_is_asked_for_again_at_a_terminal() {
		assert_eq!(
			two_factor_prompt_after(ErrorKind::Wrong2fa, true),
			Some(TwoFactorPrompt::WrongCode)
		);
	}

	#[test]
	fn a_wrong_two_factor_code_without_a_terminal_fails_saying_it_was_wrong() {
		assert_eq!(two_factor_prompt_after(ErrorKind::Wrong2fa, false), None);
		assert_eq!(
			login_failure_message(ErrorKind::Wrong2fa),
			Some("Two-factor authentication code wrong")
		);
	}

	#[test]
	fn a_wrong_password_is_not_asked_for_a_two_factor_code() {
		assert_eq!(
			two_factor_prompt_after(ErrorKind::EmailOrPasswordWrong, true),
			None
		);
		assert_eq!(
			login_failure_message(ErrorKind::EmailOrPasswordWrong),
			Some("Email or password wrong")
		);
	}

	#[test]
	fn an_unexpected_login_error_keeps_its_cause() {
		assert_eq!(two_factor_prompt_after(ErrorKind::Server, true), None);
		assert_eq!(login_failure_message(ErrorKind::Server), None);
	}

	#[test]
	fn a_wrong_password_at_the_prompt_is_asked_for_again() {
		assert_eq!(
			prompted_login_retry(ErrorKind::EmailOrPasswordWrong, 1),
			Some("Email or password wrong")
		);
		assert_eq!(
			prompted_login_retry(ErrorKind::EmailOrPasswordWrong, 2),
			Some("Email or password wrong")
		);
	}

	#[test]
	fn a_wrong_prompted_two_factor_code_is_asked_for_again() {
		assert_eq!(
			prompted_login_retry(ErrorKind::Wrong2fa, 1),
			Some("Two-factor authentication code wrong")
		);
	}

	#[test]
	fn the_third_wrong_password_at_the_prompt_fails() {
		assert_eq!(
			prompted_login_retry(ErrorKind::EmailOrPasswordWrong, 3),
			None
		);
	}

	#[test]
	fn an_unexpected_failure_at_the_prompt_is_not_retried() {
		assert_eq!(prompted_login_retry(ErrorKind::Server, 1), None);
	}

	/// A login refused for `kind`, as [`try_login`] returns it.
	fn refused(kind: ErrorKind) -> std::result::Result<&'static str, RefusedLogin> {
		Err(RefusedLogin {
			error: filen_sdk_rs::Error::custom(kind, "refused"),
			context: "Failed to log in",
		})
	}

	/// [`login_at_prompt`] with logins answered by `answers` in turn, with what was printed, and the
	/// passwords typed for the logins.
	async fn login_at_prompt_answered(
		answers: Vec<std::result::Result<&'static str, RefusedLogin>>,
	) -> (Result<&'static str>, String, Vec<String>) {
		let printed = Printed::default();
		let mut answers = answers.into_iter();
		let mut typed = 0;
		let mut passwords = Vec::new();

		let result = login_at_prompt(
			&mut printed.ui(),
			|_| {
				typed += 1;
				Ok((" user@example.com ".to_owned(), format!("password {typed}")))
			},
			async |_, email, password| {
				assert_eq!(email, " user@example.com ");
				passwords.push(password.to_owned());
				Ok(answers.next().expect("no more logins than answers"))
			},
		)
		.await;

		(result, printed.lines_or_empty(), passwords)
	}

	#[tokio::test]
	async fn a_login_refused_at_the_prompt_is_asked_for_again_and_can_succeed() {
		let (result, printed, passwords) = login_at_prompt_answered(vec![
			refused(ErrorKind::EmailOrPasswordWrong),
			Ok("the client"),
		])
		.await;

		assert_eq!(result.unwrap(), "the client");
		assert_eq!(passwords, ["password 1", "password 2"]);
		assert_eq!(
			console::strip_ansi_codes(&printed),
			"⚠ Email or password wrong, please try again"
		);
	}

	#[tokio::test]
	async fn the_third_login_refused_at_the_prompt_fails() {
		let (result, printed, passwords) = login_at_prompt_answered(vec![
			refused(ErrorKind::EmailOrPasswordWrong),
			refused(ErrorKind::Wrong2fa),
			refused(ErrorKind::EmailOrPasswordWrong),
		])
		.await;

		let err = result.unwrap_err();
		assert_eq!(format!("{err:#}"), "Email or password wrong");
		assert_eq!(exit_code(&err), 1);
		assert_eq!(passwords, ["password 1", "password 2", "password 3"]);
		assert_eq!(
			console::strip_ansi_codes(&printed),
			"⚠ Email or password wrong, please try again\n\
			 ⚠ Two-factor authentication code wrong, please try again"
		);
	}

	#[tokio::test]
	async fn a_login_failing_at_the_prompt_for_another_reason_is_not_asked_for_again() {
		let (result, printed, passwords) =
			login_at_prompt_answered(vec![refused(ErrorKind::Server)]).await;

		assert_eq!(
			format!("{:#}", result.unwrap_err()),
			"Failed to log in: Error of kind Server: context: refused"
		);
		assert_eq!(passwords, ["password 1"]);
		assert_eq!(printed, "");
	}

	#[tokio::test]
	async fn a_prompt_canceled_while_logging_in_ends_the_login() {
		let printed = Printed::default();

		let err = login_at_prompt::<()>(
			&mut printed.ui(),
			|_| Err(UI::failure("Canceled")),
			async |_, _, _| panic!("nothing to log in with"),
		)
		.await
		.unwrap_err();

		assert_eq!(format!("{err:#}"), "Canceled");
		assert_eq!(printed.lines_or_empty(), "");
	}

	/// The SHA-1 seed of the RFC 6238 test vectors, `12345678901234567890`, in base32.
	const RFC_6238_SECRET: &str = "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ";

	fn at(timestamp: i64) -> DateTime<Utc> {
		DateTime::from_timestamp(timestamp, 0).expect("a timestamp in range")
	}

	#[test]
	fn the_two_factor_secret_gives_the_code_of_the_time() {
		// the last six digits of the RFC 6238 test vectors
		for (timestamp, code) in [
			(59, "287082"),
			(1111111109, "081804"),
			(1234567890, "005924"),
		] {
			assert_eq!(
				totp_code(RFC_6238_SECRET, "user@example.com", at(timestamp)).unwrap(),
				code
			);
		}
	}

	#[test]
	fn a_code_from_the_two_factor_secret_changes_only_with_the_next_step() {
		let code =
			|timestamp| totp_code(RFC_6238_SECRET, "user@example.com", at(timestamp)).unwrap();
		assert_eq!(code(30), code(59));
		assert_ne!(code(59), code(60));
	}

	#[test]
	fn the_wait_for_the_next_code_ends_when_its_step_begins() {
		assert_eq!(time_until_next_totp_step(at(30)), Duration::from_secs(30));
		assert_eq!(time_until_next_totp_step(at(31)), Duration::from_secs(29));
		assert_eq!(time_until_next_totp_step(at(59)), Duration::from_secs(1));
		let late_in_the_last_second = DateTime::from_timestamp(59, 999_000_000).unwrap();
		assert_eq!(
			time_until_next_totp_step(late_in_the_last_second),
			Duration::from_secs(1)
		);
	}

	#[test]
	fn a_refused_code_of_the_current_step_waits_for_the_next_step() {
		assert_eq!(
			next_totp_code(RFC_6238_SECRET, "user@example.com", "287082", at(59)).unwrap(),
			NextTotpCode::After(Duration::from_secs(1))
		);
		assert_eq!(
			next_totp_code(RFC_6238_SECRET, "user@example.com", "287082", at(30)).unwrap(),
			NextTotpCode::After(Duration::from_secs(30))
		);
	}

	#[test]
	fn a_refused_code_of_an_earlier_step_is_retried_with_the_current_one_at_once() {
		assert_eq!(
			next_totp_code(
				RFC_6238_SECRET,
				"user@example.com",
				"287082",
				at(1111111109)
			)
			.unwrap(),
			NextTotpCode::Now("081804".to_owned())
		);
	}

	#[test]
	fn an_invalid_two_factor_secret_fails_without_naming_it() {
		let secret = "not base32!";
		let error = totp_code(secret, "user@example.com", at(59)).unwrap_err();
		let message = format!("{error:#}");
		assert!(
			message.starts_with(
				"FILEN_CLI_2FA_SECRET is not a valid two-factor authentication secret"
			),
			"{message}"
		);
		assert!(!message.contains(secret), "{message}");
	}

	#[test]
	fn the_two_factor_secret_is_read_from_its_variable() {
		assert_eq!(
			totp_secret_from(env_with(&[("FILEN_CLI_2FA_SECRET", RFC_6238_SECRET)])).as_deref(),
			Some(RFC_6238_SECRET)
		);
	}

	#[test]
	fn an_empty_or_unset_two_factor_secret_is_none() {
		assert_eq!(totp_secret_from(no_env), None);
		assert_eq!(
			totp_secret_from(env_with(&[("FILEN_CLI_2FA_SECRET", "")])),
			None
		);
	}

	/// A keyring that fails every access, as one without a keyring service does.
	struct UnavailableChunkStore;

	impl ChunkStore for UnavailableChunkStore {
		fn get(&self, _key: &str) -> Result<Option<String>> {
			Err(anyhow!("no keyring service"))
		}

		fn set(&self, _key: &str, _value: &str) -> Result<()> {
			Err(anyhow!("no keyring service"))
		}

		fn delete(&self, _key: &str) -> Result<bool> {
			Err(anyhow!("no keyring service"))
		}
	}

	#[test]
	fn a_kept_login_is_saved_in_the_keyring() {
		let (dir, _config) = config_in_temp_dir();
		let store = MemoryChunkStore::default();
		let entry = LongKeyringEntry::new(&store, "sdk-config");
		let printed = Printed::default();

		let kept = keep_login(
			dir.path(),
			&mut printed.ui(),
			"the sdk config",
			Some(&entry),
			|_, _| panic!("a login saved in the keyring needs no other place"),
		)
		.unwrap();

		assert_eq!(kept, KeptLogin::Keyring);

		assert_eq!(entry.read().unwrap().as_deref(), Some("the sdk config"));
		assert!(!dir.path().join(AUTH_CONFIG_FILENAME).exists());
		assert_eq!(
			console::strip_ansi_codes(&printed.lines()),
			"✔ Saved credentials"
		);
	}

	#[test]
	fn without_a_keyring_the_options_are_explained_and_an_auth_config_is_offered() {
		let (dir, _config) = config_in_temp_dir();
		let store = UnavailableChunkStore;
		let entry = LongKeyringEntry::new(&store, "sdk-config");
		let printed = Printed::default();
		let mut asked = None;

		let kept = keep_login(
			dir.path(),
			&mut printed.ui(),
			"the sdk config",
			Some(&entry),
			|_, msg| {
				asked = Some(msg.to_owned());
				Ok(false)
			},
		)
		.unwrap();

		assert_eq!(kept, KeptLogin::Nowhere);

		assert_eq!(
			asked,
			Some(format!(
				"Save an unencrypted auth config file in {} instead?",
				dir.path().display()
			))
		);
		assert!(!dir.path().join(AUTH_CONFIG_FILENAME).exists());
		assert_eq!(
			console::strip_ansi_codes(&printed.lines()),
			format!(
				"⚠ Failed to save the credentials in the system keyring\n\
				 To stay logged in without it, either:\n\
				 - save an auth config file in {}, which only you can read, but which holds your \
				 credentials unencrypted\n\
				 - pass --email and --password (or set FILEN_CLI_EMAIL and FILEN_CLI_PASSWORD) on \
				 each run\n\
				 - export an auth config with `filen export-auth-config` and pass \
				 --auth-config-path on each run",
				dir.path().display()
			)
		);
	}

	#[test]
	fn yes_accepts_the_offer_of_an_auth_config_without_a_terminal() {
		let (dir, _config) = config_in_temp_dir();
		let mut ui = Printed::default().ui();
		ui.set_assume_yes(true);

		let kept = keep_login(
			dir.path(),
			&mut ui,
			"the sdk config",
			None,
			confirm_auth_config,
		)
		.unwrap();

		let path = dir.path().join(AUTH_CONFIG_FILENAME);
		assert_eq!(kept, KeptLogin::AuthConfig(path.clone()));
		assert_eq!(std::fs::read_to_string(&path).unwrap(), "the sdk config");
	}

	#[cfg(unix)]
	#[test]
	fn without_the_keyring_an_accepted_offer_saves_an_owner_only_auth_config() {
		let (dir, _config) = config_in_temp_dir();
		let printed = Printed::default();

		let kept = keep_login(
			dir.path(),
			&mut printed.ui(),
			"the sdk config",
			None,
			|_, _| Ok(true),
		)
		.unwrap();

		let path = dir.path().join(AUTH_CONFIG_FILENAME);
		assert_eq!(kept, KeptLogin::AuthConfig(path.clone()));
		assert_eq!(std::fs::read_to_string(&path).unwrap(), "the sdk config");
		assert_eq!(
			std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
			0o600
		);
		let printed = console::strip_ansi_codes(&printed.lines()).into_owned();
		assert!(printed.starts_with("⚠ The system keyring is not used in the Docker image\n"));
		assert!(printed.ends_with(&format!("✔ Saved credentials to {}", path.display())));
	}

	#[test]
	fn a_secret_printed_to_a_terminal_is_confirmed_first() {
		let printed = Printed::default();
		let mut ui = printed.terminal_ui();
		ui.set_assume_yes(true);
		assert!(secret_print_needs_confirmation(&ui, API_KEY_PRINT_PROMPT));
		assert!(confirm_api_key_print(&mut ui).unwrap());
		assert_eq!(printed.lines_or_empty(), "");
	}

	#[test]
	fn a_secret_printed_into_a_pipe_is_not_confirmed() {
		let mut ui = Printed::default().prompting_ui();
		assert!(!secret_print_needs_confirmation(&ui, API_KEY_PRINT_PROMPT));
		assert!(confirm_api_key_print(&mut ui).unwrap());
	}

	#[test]
	fn a_secret_printed_as_json_is_not_confirmed() {
		let mut ui = Printed::default().terminal_ui();
		ui.set_assume_yes(true);
		ui.set_quiet_and_json(false, true);
		assert!(!secret_print_needs_confirmation(&ui, API_KEY_PRINT_PROMPT));
	}

	#[test]
	fn a_secret_printed_without_a_way_to_confirm_is_not_confirmed() {
		let mut ui = Printed::default().terminal_ui();
		assert!(!secret_print_needs_confirmation(&ui, API_KEY_PRINT_PROMPT));
		assert!(confirm_api_key_print(&mut ui).unwrap());
	}

	/// An environment without any variables.
	fn no_env(_: &str) -> Option<String> {
		None
	}

	/// An environment with only `vars`.
	fn env_with<'a>(vars: &'a [(&str, &str)]) -> impl Fn(&str) -> Option<String> + 'a {
		|name| {
			vars.iter()
				.find(|(var, _)| *var == name)
				.map(|(_, value)| (*value).to_owned())
		}
	}

	#[test]
	fn no_credentials_in_the_arguments_are_none() {
		assert!(cli_credentials(None, None, None, no_env).unwrap().is_none());
	}

	#[test]
	fn a_two_factor_code_alone_is_ignored() {
		assert!(
			cli_credentials(None, None, Some("123456"), no_env)
				.unwrap()
				.is_none()
		);
	}

	#[test]
	fn the_variables_of_the_old_cli_are_the_credentials_without_arguments() {
		let credentials = cli_credentials(
			None,
			None,
			None,
			env_with(&[
				("FILEN_EMAIL", "old@example.com"),
				("FILEN_PASSWORD", "old-secret"),
				("FILEN_2FA_CODE", "654321"),
			]),
		)
		.unwrap()
		.unwrap();
		assert_eq!(credentials.email, "old@example.com");
		assert_eq!(credentials.password, "old-secret");
		assert_eq!(credentials.two_factor_code.as_deref(), Some("654321"));
	}

	#[test]
	fn the_arguments_win_over_the_variables_of_the_old_cli() {
		let credentials = cli_credentials(
			Some("a@example.com"),
			Some("secret"),
			None,
			env_with(&[
				("FILEN_EMAIL", "old@example.com"),
				("FILEN_PASSWORD", "old-secret"),
				("FILEN_2FA_CODE", "654321"),
			]),
		)
		.unwrap()
		.unwrap();
		assert_eq!(credentials.email, "a@example.com");
		assert_eq!(credentials.password, "secret");
		assert_eq!(credentials.two_factor_code, None);
	}

	#[test]
	fn a_two_factor_code_argument_wins_over_the_one_of_the_old_cli() {
		let credentials = cli_credentials(
			None,
			None,
			Some("123456"),
			env_with(&[
				("FILEN_EMAIL", "old@example.com"),
				("FILEN_PASSWORD", "old-secret"),
				("FILEN_2FA_CODE", "654321"),
			]),
		)
		.unwrap()
		.unwrap();
		assert_eq!(credentials.two_factor_code.as_deref(), Some("123456"));
	}

	#[test]
	fn an_old_cli_email_without_a_password_fails() {
		let Err(err) = cli_credentials(
			None,
			None,
			None,
			env_with(&[("FILEN_EMAIL", "old@example.com")]),
		) else {
			panic!("FILEN_EMAIL without FILEN_PASSWORD must fail");
		};
		assert_eq!(
			format!("{err:#}"),
			"FILEN_PASSWORD is required with FILEN_EMAIL"
		);
		assert_eq!(exit_code(&err), 1);
	}

	#[test]
	fn an_old_cli_password_alone_is_ignored() {
		assert!(
			cli_credentials(
				None,
				None,
				None,
				env_with(&[("FILEN_PASSWORD", "old-secret")])
			)
			.unwrap()
			.is_none()
		);
	}

	#[test]
	fn empty_variables_of_the_old_cli_are_unset() {
		assert!(
			cli_credentials(
				None,
				None,
				None,
				env_with(&[("FILEN_EMAIL", ""), ("FILEN_PASSWORD", "")])
			)
			.unwrap()
			.is_none()
		);
	}

	#[test]
	fn an_email_without_a_password_fails() {
		let Err(err) = cli_credentials(Some("a@example.com"), None, None, no_env) else {
			panic!("an email without a password must fail");
		};
		assert_eq!(format!("{err:#}"), "Password is required");
		assert_eq!(exit_code(&err), 1);
	}

	#[test]
	fn a_password_without_an_email_fails() {
		let Err(err) = cli_credentials(None, Some("secret"), Some("123456"), no_env) else {
			panic!("a password without an email must fail");
		};
		assert_eq!(format!("{err:#}"), "Email is required");
	}

	#[test]
	fn an_email_and_a_password_are_the_credentials() {
		let credentials = cli_credentials(
			Some("a@example.com"),
			Some("secret"),
			Some("123456"),
			no_env,
		)
		.unwrap()
		.unwrap();
		assert_eq!(credentials.email, "a@example.com");
		assert_eq!(credentials.password, "secret");
		assert_eq!(credentials.two_factor_code.as_deref(), Some("123456"));
	}

	#[tokio::test]
	async fn a_missing_auth_config_path_fails_instead_of_falling_back() {
		let config_dir = assert_fs::TempDir::new().unwrap();
		// the autodetected file, which must not be read in its place
		std::fs::write(config_dir.path().join(AUTH_CONFIG_FILENAME), "not a config").unwrap();
		let missing = config_dir.path().join("missing.txt");
		let config = CliConfig {
			config_dir: config_dir.path().to_path_buf(),
			profiles: Profiles::new(config_dir.path(), None),
		};

		let Err(err) = authenticate_from_auth_config(
			&config,
			&mut Printed::default().ui(),
			Some(missing.to_str().unwrap()),
			&ClientConfigArgs::default(),
		)
		.await
		else {
			panic!("a missing auth config path must fail");
		};

		assert_eq!(
			format!("{err:#}"),
			format!("Auth config file not found: {}", missing.display())
		);
		assert_eq!(exit_code(&err), 1);
	}

	/// What the auth config files of these tests hold: no auth config at all.
	const CORRUPT_AUTH_CONFIG: &str = "not a config";

	#[tokio::test]
	async fn a_corrupt_given_auth_config_fails_saying_to_export_a_new_one() {
		let (_dir, config) = config_in_temp_dir();
		let path = config.config_dir.join("given.txt");
		std::fs::write(&path, CORRUPT_AUTH_CONFIG).unwrap();
		#[cfg(unix)]
		std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();

		let Err(err) = authenticate_from_auth_config(
			&config,
			&mut Printed::default().ui(),
			Some(path.to_str().unwrap()),
			&ClientConfigArgs::default(),
		)
		.await
		else {
			panic!("a corrupt given auth config must fail");
		};

		assert_eq!(
			format!("{err:#}"),
			format!(
				"The auth config file {} cannot be used (Invalid auth config format (missing or \
				 invalid prefix)): export a new one with `filen export-auth-config`",
				path.display()
			)
		);
		assert_eq!(exit_code(&err), 1);
	}

	#[tokio::test]
	async fn a_corrupt_auth_config_in_the_config_dir_is_skipped_with_a_warning() {
		let (_dir, config) = config_in_temp_dir();
		let path = config.config_dir.join(AUTH_CONFIG_FILENAME);
		std::fs::write(&path, CORRUPT_AUTH_CONFIG).unwrap();
		#[cfg(unix)]
		std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
		let out = Printed::default();
		let err = Printed::default();

		let found = authenticate_from_auth_config(
			&config,
			&mut Printed::split_ui(&out, &err),
			None,
			&ClientConfigArgs::default(),
		)
		.await
		.unwrap();

		assert!(found.is_none());
		assert_eq!(out.lines_or_empty(), "");
		assert_eq!(
			console::strip_ansi_codes(&err.lines()),
			format!(
				"⚠ The auth config file {} cannot be used (Invalid auth config format (missing or \
				 invalid prefix)): export a new one with `filen export-auth-config`",
				path.display()
			)
		);
	}

	#[tokio::test]
	async fn an_unusable_auth_config_in_the_working_dir_falls_back_to_the_one_in_the_config_dir() {
		let (_dir, config) = config_in_temp_dir();
		let working_dir = assert_fs::TempDir::new().unwrap();
		let candidates = [
			(working_dir.path(), AuthConfigLocation::WorkingDir),
			(config.config_dir.as_path(), AuthConfigLocation::ConfigDir),
		]
		.map(|(dir, location)| {
			let path = dir.join(AUTH_CONFIG_FILENAME);
			std::fs::write(&path, CORRUPT_AUTH_CONFIG).unwrap();
			#[cfg(unix)]
			std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
			(path, location)
		});
		let warning = |path: &Path| {
			format!(
				"⚠ The auth config file {} cannot be used (Invalid auth config format (missing or \
				 invalid prefix)): export a new one with `filen export-auth-config`",
				path.display()
			)
		};
		let expected = format!(
			"{}\n{}",
			warning(&candidates[0].0),
			warning(&candidates[1].0)
		);
		let out = Printed::default();
		let err = Printed::default();

		let found = authenticate_from_auth_configs(
			&mut Printed::split_ui(&out, &err),
			candidates.to_vec(),
			&ClientConfigArgs::default(),
		)
		.await
		.unwrap();

		assert!(found.is_none());
		assert_eq!(out.lines_or_empty(), "");
		assert_eq!(console::strip_ansi_codes(&err.lines()), expected);
	}

	#[tokio::test]
	async fn corrupt_credentials_in_the_keyring_are_deleted_with_a_warning() {
		let store = MemoryChunkStore::default();
		let entry = LongKeyringEntry::new(&store, "sdk-config");
		entry.write(CORRUPT_AUTH_CONFIG).unwrap();
		let out = Printed::default();
		let err = Printed::default();

		let found = authenticate_from_keyring_entry(
			&entry,
			&mut Printed::split_ui(&out, &err),
			&ClientConfigArgs::default(),
		)
		.await
		.unwrap();

		assert!(found.is_none());
		assert_eq!(entry.read().unwrap(), None);
		assert_eq!(store.keys(), Vec::<String>::new());
		assert_eq!(out.lines_or_empty(), "");
		assert_eq!(
			console::strip_ansi_codes(&err.lines()),
			"⚠ Saved credentials are no longer valid, please log in again"
		);
	}

	#[tokio::test]
	async fn an_empty_keyring_has_no_credentials() {
		let store = MemoryChunkStore::default();
		let printed = Printed::default();

		let found = authenticate_from_keyring_entry(
			&LongKeyringEntry::new(&store, "sdk-config"),
			&mut printed.ui(),
			&ClientConfigArgs::default(),
		)
		.await
		.unwrap();

		assert!(found.is_none());
		assert_eq!(printed.lines_or_empty(), "");
	}

	/// The error the server answers a request with an API key it no longer accepts with.
	fn unauthenticated() -> filen_sdk_rs::Error {
		filen_sdk_rs::Error::custom(ErrorKind::Unauthenticated, "api key not found")
	}

	#[tokio::test]
	async fn a_login_whose_api_key_the_server_rejects_fails_the_check() {
		let err = login_check_outcome(
			async { Err::<(), _>(unauthenticated()) },
			LOGIN_CHECK_TIMEOUT,
		)
		.await
		.unwrap_err();

		assert_eq!(
			format!("{err:#}"),
			"the server no longer accepts this login"
		);
	}

	#[tokio::test]
	async fn a_login_the_server_could_not_check_is_kept() {
		let check = async {
			Err::<(), _>(filen_sdk_rs::Error::custom(
				ErrorKind::Server,
				"connection refused",
			))
		};

		login_check_outcome(check, LOGIN_CHECK_TIMEOUT)
			.await
			.unwrap();
	}

	#[tokio::test]
	async fn a_login_the_server_does_not_answer_the_check_of_in_time_is_kept() {
		// a request that never ends, as one retrying while offline does not for minutes
		login_check_outcome(
			std::future::pending::<std::result::Result<(), filen_sdk_rs::Error>>(),
			Duration::from_millis(10),
		)
		.await
		.unwrap();
	}

	#[tokio::test]
	async fn credentials_of_the_old_cli_that_cannot_be_imported_are_skipped_with_a_hint() {
		let (_dir, config) = config_in_temp_dir();
		let working_dir = assert_fs::TempDir::new().unwrap();
		let path = working_dir.path().join(".filen-cli-auth-config");
		std::fs::write(&path, CORRUPT_AUTH_CONFIG).unwrap();
		let out = Printed::default();
		let err = Printed::default();

		let found = import_old_cli_login(
			&config,
			&mut Printed::split_ui(&out, &err),
			old_cli::find(Some(working_dir.path()), None),
			&ClientConfigArgs::default(),
			|_| panic!("an auth config needs no key"),
		)
		.await
		.unwrap();

		assert!(found.is_none());
		assert_eq!(out.lines_or_empty(), "");
		assert_eq!(
			console::strip_ansi_codes(&err.lines()),
			format!(
				"⚠ Found credentials saved by the old Filen CLI at {} that could not be imported; \
				 please log in again",
				path.display()
			)
		);
		assert!(path.exists());
	}

	#[test]
	fn without_a_path_the_auth_config_is_exported_to_the_working_dir() {
		let path = auth_config_export_path(None, || Ok(PathBuf::from("/work"))).unwrap();
		assert_eq!(path, Path::new("/work/filen-cli-auth-config.txt"));
	}

	#[test]
	fn a_directory_path_gets_the_auth_config_file_inside_it() {
		let dir = assert_fs::TempDir::new().unwrap();
		let path =
			auth_config_export_path(Some(dir.path()), || panic!("a path was given")).unwrap();
		assert_eq!(path, dir.path().join("filen-cli-auth-config.txt"));
	}

	#[test]
	fn a_file_path_is_the_file_the_auth_config_is_exported_to() {
		let dir = assert_fs::TempDir::new().unwrap();
		let file = dir.path().join("my-auth-config.txt");
		let path = auth_config_export_path(Some(&file), || panic!("a path was given")).unwrap();
		assert_eq!(path, file);
	}

	#[test]
	fn exporting_without_a_terminal_or_yes_fails_asking_for_yes() {
		let dir = assert_fs::TempDir::new().unwrap();
		let err = confirm_export(
			&mut Printed::default().ui(),
			&dir.path().join(AUTH_CONFIG_FILENAME),
		)
		.unwrap_err();
		assert_eq!(
			format!("{err:#}"),
			format!(
				"Cannot ask \"{EXPORT_RISK_PROMPT}\": stdin is not a terminal. Pass --yes to answer \
				 yes."
			)
		);
		assert_eq!(exit_code(&err), 2);
	}

	#[test]
	fn yes_confirms_exporting_over_an_existing_file() {
		let dir = assert_fs::TempDir::new().unwrap();
		let path = dir.path().join(AUTH_CONFIG_FILENAME);
		std::fs::write(&path, "old").unwrap();
		let mut ui = Printed::default().ui();
		ui.set_assume_yes(true);
		assert!(confirm_export(&mut ui, &path).unwrap());
	}

	#[test]
	fn write_private_file_writes_contents() {
		let dir = assert_fs::TempDir::new().unwrap();
		let path = dir.path().join(AUTH_CONFIG_FILENAME);
		write_private_file(&path, "secret-config").unwrap();
		assert_eq!(std::fs::read_to_string(&path).unwrap(), "secret-config");
	}

	#[cfg(unix)]
	#[test]
	fn write_private_file_creates_owner_only() {
		use std::os::unix::fs::PermissionsExt;
		let dir = assert_fs::TempDir::new().unwrap();
		let path = dir.path().join(AUTH_CONFIG_FILENAME);
		write_private_file(&path, "secret-config").unwrap();
		let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
		assert_eq!(
			mode, 0o600,
			"auth config must not be group- or world-readable"
		);
	}

	#[cfg(unix)]
	#[test]
	fn write_private_file_tightens_existing_lax_file() {
		use std::os::unix::fs::PermissionsExt;
		let dir = assert_fs::TempDir::new().unwrap();
		let path = dir.path().join(AUTH_CONFIG_FILENAME);
		// Simulate a config left behind by an older CLI version with lax permissions.
		std::fs::write(&path, "old").unwrap();
		std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
		write_private_file(&path, "new-secret").unwrap();
		let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
		assert_eq!(mode, 0o600);
		assert_eq!(std::fs::read_to_string(&path).unwrap(), "new-secret");
	}
}
