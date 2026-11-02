//! [cli-doc] main-usage
//!
//! Welcome to Filen CLI v{{VERSION}}!
//!
//! You can find out more about the Filen CLI at https://github.com/FilenCloudDienste/filen-cli-releases
//!
//! Invoke the Filen CLI with no command specified to enter interactive mode (REPL).
//! There, you can specify absolute paths (starting with "/") or relative paths (supports "." and "..").
//!
//! Options can go before or after the command (`filen --json ls` or `filen ls --json`),
//! except for `--password` and `--two-factor-code`, which go before it.

use std::{ffi::OsString, fs, num::NonZeroU32, path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context, Result, anyhow};
use base64::{Engine as _, engine::general_purpose::STANDARD as base64};
use clap::{CommandFactory as _, FromArgMatches as _, Parser};
use filen_sdk_rs::auth::{Client, StringifiedClient, http::ClientConfig, unauth::UnauthClient};
use ftail::Ftail;
use log::{LevelFilter, info, warn};

use crate::{
	commands::{
		Commands, execute_command,
		fs_cmds::{self, FsCommands},
	},
	docs::{generate_markdown_docs, print_in_app_docs},
	log_files::LogFiles,
	ui::{CustomLogger, ReplPromptResult},
	updater::check_for_updates,
	util::RemotePath,
};

mod auth;
mod commands;
mod completion;
mod docs;
mod error;
mod log_files;
mod memory;
mod ui;
mod updater;
mod util;

#[derive(Debug, Parser)]
#[clap(
	name = "Filen CLI",
	version,
	disable_help_flag = true,
	disable_help_subcommand = true
)]
// Each bool is an independent clap flag.
#[allow(clippy::struct_excessive_bools)]
pub(crate) struct CliArgs {
	/// Print help about a command or topic
	// global: after a command, it asks for that command's help (see `parse_args`)
	#[arg(
		short,
		long,
		num_args = 0..=1,
		default_missing_value = "",
		hide = true,
		global = true
	)]
	help: Option<String>,

	/// Increase verbosity (-v, -vv, -vvv)
	#[arg(short, long, action = clap::ArgAction::Count, global = true)]
	verbose: u8,

	/// Hide progress bars and other non-essential output (overrides -v)
	#[arg(short, long, global = true)]
	quiet: bool,

	/// Config directory (overrides system default)
	#[arg(long, global = true)]
	config_dir: Option<PathBuf>,

	/// Filen account email (requires --password)
	#[arg(short, long, env = "FILEN_CLI_EMAIL", global = true)]
	email: Option<String>,

	// The password and the two-factor code go before the command, unlike the other options: `-p`
	// after it is `rm -p` or `sync -p`, and `--password` is `serve --password`.
	/// Filen account password (requires --email)
	#[arg(short, long, env = "FILEN_CLI_PASSWORD")]
	password: Option<String>,

	/// Filen account two-factor code (optional, requires --email and --password)
	#[arg(short, long, env = "FILEN_CLI_2FA_CODE")]
	two_factor_code: Option<String>,

	/// Path to auth config file (exported via `filen export-auth-config`)
	#[arg(long, global = true)]
	auth_config_path: Option<String>,

	/// Limit concurrent API connections
	#[arg(long, global = true)]
	concurrency: Option<usize>,

	/// Maximum number of API requests per second
	#[arg(long, global = true)]
	requests_per_sec: Option<NonZeroU32>,

	/// Cap upload bandwidth, in kilobytes per second
	#[arg(long, global = true)]
	upload_bandwidth_kbps: Option<NonZeroU32>,

	/// Cap download bandwidth, in kilobytes per second
	#[arg(long, global = true)]
	download_bandwidth_kbps: Option<NonZeroU32>,

	/// Memory budget for file I/O, in bytes
	#[arg(long, global = true)]
	memory_budget_bytes: Option<usize>,

	/// Connection timeout, in seconds
	#[arg(long, global = true)]
	connect_timeout_secs: Option<u64>,

	/// Skip checking for updates
	#[arg(long, global = true)]
	skip_update: bool,

	/// Force checking for updates
	#[arg(long, global = true)]
	force_update_check: bool,

	/// Force checking for updates and install them automatically.
	/// Usually, updates are only installed in REPL mode
	#[arg(long, global = true)]
	always_update: bool,

	/// Sets autocomplete to be less eager, only completing remote paths after pressing tab
	#[arg(long, global = true)]
	reluctant_autocomplete: bool,

	/// Format command output as machine-readable JSON (where applicable)
	#[arg(long, global = true)]
	json: bool,

	#[command(subcommand)]
	command: Option<Commands>,

	#[arg(long, hide = true)]
	export_markdown_docs: bool,

	#[arg(long, hide = true)]
	make_output_environment_agnostic_for_replay_testing: bool,

	/// The initial working path for the REPL or non-interactive commands (defaults to "/")
	#[arg(long, global = true)]
	working_path: Option<String>,
}

#[derive(Clone)]
pub(crate) struct CliConfig {
	pub(crate) config_dir: PathBuf,
}

pub(crate) static CTRLC_TX: std::sync::LazyLock<tokio::sync::broadcast::Sender<()>> =
	std::sync::LazyLock::new(|| {
		let (tx, _) = tokio::sync::broadcast::channel(1);
		let tx_clone = tx.clone();
		ctrlc::set_handler(move || {
			let _ = tx_clone.send(());
		})
		.expect("Error setting Ctrl-C handler");
		tx
	});
// todo: might we also be able to use this for general cancellability? (e.g. for long-running commands like upload/download)

/// The most blocking threads the runtime runs at once, in place of tokio's default of 512.
///
/// Every `tokio::fs` call (each read and write of a transferred file, its open, metadata and
/// rename), every DNS lookup and every `spawn_blocking` takes one, and an idle one lives on for
/// 10 s. With `--concurrency 256` a transfer or `sync` would burst to hundreds of threads, and on
/// Linux glibc gives each new thread its own malloc arena (up to 8 per core) and keeps what is
/// freed there for reuse, so resident memory ratchets up with every burst. Tasks past the cap
/// queue instead of failing.
///
/// The cap only has to stay above the threads that can sit waiting on work that needs one too.
/// The blocking tasks on the sync path touch the disk or the baseline database and never wait on
/// async work. The held-deletion and search prompts hold one each until the user answers, and on
/// Windows a running rclone's stdout and stderr readers hold one each. A directory download's
/// walker holds one while it blocks on a channel that its downloads drain, and those downloads
/// write through `tokio::fs`, so they need threads of their own.
///
/// That walker only holds its thread while its download runs, and the REPL runs one command at a
/// time. Ctrl-C at the REPL drops the download, which aborts its downloads and closes the walker's
/// channel, so the walker stops at its next entry and gives the thread back rather than staying
/// blocked after the REPL has moved on.
const MAX_BLOCKING_THREADS: usize = 32;

/// Runs the CLI: parses the command line, then runs one command or the REPL.
pub fn run() {
	// Before the runtime starts its threads, so that none of them has an arena of its own yet.
	let arenas_limited = memory::limit_arenas();
	tokio::runtime::Builder::new_multi_thread()
		.enable_all()
		.max_blocking_threads(MAX_BLOCKING_THREADS)
		.build()
		// The panic `#[tokio::main]` raised: nothing runs without the runtime.
		.expect("Failed building the Runtime")
		.block_on(async_main(arenas_limited));
}

/// `arenas_limited`: whether [`memory::limit_arenas`] took, reported once logging is set up.
async fn async_main(arenas_limited: bool) {
	let mut ui = ui::UI::new();
	// call ui.initialize() later after parsing args

	// translate errors to non-zero exit code
	if let Err(e) = inner_main(&mut ui, arenas_limited).await {
		ui.print_failure_or_error(&e);
		std::process::exit(error::exit_code(&e));
	}
}

async fn inner_main(ui: &mut ui::UI, arenas_limited: bool) -> Result<()> {
	let cli_args = parse_args(std::env::args_os()).unwrap_or_else(|e| e.exit());

	let is_dev = cfg!(debug_assertions);
	let config = CliConfig {
		config_dir: match cli_args.config_dir {
			Some(ref dir) => {
				if !dir.exists() {
					return Err(ui::UI::usage_failure("Config dir does not exist"));
				}
				dir.clone()
			}
			None => {
				let dir = dirs::config_dir()
					.context("Failed to get config dir")?
					.join(match is_dev {
						true => "filen-cli-dev",
						false => "filen-cli",
					});
				fs::create_dir_all(&dir).context("Failed to create config dir")?;
				dir
			}
		},
	};

	// setup logging
	let log_dir = config.config_dir.join("logs");
	fs::create_dir_all(&log_dir).context("Failed to create logs dir")?;
	let logging_level = if cli_args.quiet {
		LevelFilter::Off
	} else {
		match cli_args.verbose {
			0 => LevelFilter::Off,
			1 => LevelFilter::Info,
			2 => LevelFilter::Debug,
			_ => LevelFilter::Trace,
		}
	};
	let log_file = log_dir.join(log_files::LATEST);
	let log_files_level = LevelFilter::Debug;
	// An `Arc` because ftail builds each channel from a `'static` `Fn`, which can only hand out
	// clones of it.
	let log_files = Arc::new(
		LogFiles::open(&log_dir, log_files_level, 10 * 1024 * 1024, 3)
			.context("Failed to open log files")?,
	);
	Ftail::new()
		.custom(
			move |config| {
				Box::new(CustomLogger {
					config,
					make_output_environment_agnostic_for_replay_testing: cli_args
						.make_output_environment_agnostic_for_replay_testing,
				}) as Box<dyn log::Log + Send + Sync>
			},
			logging_level,
		)
		.custom(
			move |_| Box::new(Arc::clone(&log_files)) as Box<dyn log::Log + Send + Sync>,
			log_files_level,
		)
		.init()
		.context("Failed to initialize logger")?;
	// ftail passes every record on to its channels, and no channel takes one past both levels.
	log::set_max_level(logging_level.max(log_files_level));
	info!("Logging level: {}", logging_level);
	info!("Full log file: {}", log_file.display());

	if cfg!(debug_assertions) {
		info!("Filen CLI (version omitted for replay testing)")
	} else {
		info!("Filen CLI v{}", env!("CARGO_PKG_VERSION"));
	}
	if !arenas_limited {
		warn!("glibc did not take the malloc arena cap, so freed memory may stay with the process");
	}

	ui.initialize(
		cli_args.quiet,
		cli_args.json,
		None,
		cli_args.reluctant_autocomplete,
		cli_args.make_output_environment_agnostic_for_replay_testing,
	);

	// --export-markdown-docs
	if cli_args.export_markdown_docs {
		return generate_markdown_docs();
	}

	// --help
	if let Some(help_topic) = cli_args.help {
		return print_in_app_docs(ui, help_topic_or_general(help_topic));
	}

	// a single command has no working directory to change
	if let Some(Commands::Fs(FsCommands::Cd { .. })) = cli_args.command {
		return Err(ui::UI::usage_failure(
			"`cd` only works in the REPL. To run a command in another directory, pass \
			 --working-path, or run `filen` without a command to enter the REPL.",
		));
	}

	if !cli_args.skip_update {
		check_for_updates(
			ui,
			cli_args.force_update_check || cli_args.always_update,
			cli_args.always_update,
			&config.config_dir,
			cli_args.command.is_none(),
		)
		.await?;
	}

	let client_config_args = ClientConfigArgs {
		concurrency: cli_args.concurrency,
		requests_per_sec: cli_args.requests_per_sec,
		upload_bandwidth_kbps: cli_args.upload_bandwidth_kbps,
		download_bandwidth_kbps: cli_args.download_bandwidth_kbps,
		memory_budget_bytes: cli_args.memory_budget_bytes,
		connect_timeout_secs: cli_args.connect_timeout_secs,
	};

	let mut client = auth::LazyClient::new(
		config.clone(),
		cli_args.email,
		cli_args.password,
		cli_args.two_factor_code,
		cli_args.auth_config_path,
		client_config_args,
	);

	if let Some(command) = cli_args.command {
		let working_path =
			initial_working_path(ui, &mut client, cli_args.working_path.as_deref()).await?;
		let _ = execute_command(&config, ui, &mut client, &working_path, command).await?;
		Ok(())
	} else {
		ui.print_banner();

		client.get(ui).await?;
		// authenticate, so the username is shown in the prompt.
		// this essentially defeats the purpose of LazyClient, but:
		// it does make a difference so non-authenticated commands (e.g. logout) can still be run ..
		// .. without authentication when called directly (no REPL)

		let mut working_path =
			initial_working_path(ui, &mut client, cli_args.working_path.as_deref()).await?;

		loop {
			let repl_result = ui.prompt_repl(client.get_arc().unwrap(), &working_path)?;
			let line = match repl_result {
				ReplPromptResult {
					input: Some(line), ..
				} if line.is_empty() => continue,
				ReplPromptResult {
					input: Some(line), ..
				} => line,
				ReplPromptResult {
					input: None,
					exit: true,
				} => break,
				ReplPromptResult {
					input: None,
					exit: false,
				} => continue,
			};
			let cli_args = match parse_repl_line(&line) {
				Ok(cli_args) => cli_args,
				Err(ReplLineError::Quoting) => {
					ui.print_failure("Invalid quoting");
					continue;
				}
				Err(ReplLineError::Arguments(e)) => {
					// as clap prints it when the CLI is invoked with the same arguments
					if let Err(print_error) = e.print() {
						warn!("Failed to print an argument error: {print_error}");
					}
					continue;
				}
			};
			if let Some(help_topic) = cli_args.help {
				if let Err(e) = print_in_app_docs(ui, help_topic_or_general(help_topic)) {
					ui.print_failure_or_error(&e);
				}
				continue;
			}
			let Some(command) = cli_args.command else {
				continue;
			};
			match execute_command(&config, ui, &mut client, &working_path, command).await {
				Ok(result) => {
					if result.exit {
						break;
					}
					working_path = result.working_path.unwrap_or(working_path);
				}
				Err(e) => {
					ui.print_failure_or_error(&e);
				}
			}
		}
		Ok(())
	}
}

/// The working path to start in: the drive's root, or `--working-path`, which must be a
/// directory on the drive.
async fn initial_working_path(
	ui: &mut ui::UI,
	client: &mut auth::LazyClient,
	working_path: Option<&str>,
) -> Result<RemotePath> {
	let root = RemotePath::new("");
	match working_path {
		None => Ok(root),
		Some(path) => fs_cmds::cd(ui, client, &root, path)
			.await
			.context("Invalid --working-path"),
	}
}

/// Why a REPL line could not be parsed into a command.
#[derive(Debug)]
enum ReplLineError {
	/// A quote is not closed.
	Quoting,
	/// The arguments are not a valid command line.
	Arguments(clap::Error),
}

/// Parses a REPL line as the arguments the CLI would be invoked with.
fn parse_repl_line(line: &str) -> Result<CliArgs, ReplLineError> {
	let mut args = shlex::split(line.trim()).ok_or(ReplLineError::Quoting)?;
	args.insert(0, String::from("filen"));
	parse_args(args).map_err(ReplLineError::Arguments)
}

/// Parses the command line (the program name first).
///
/// `-h`/`--help` after a command asks for that command's help, which [`CliArgs::help`] then
/// names, and leaves [`CliArgs::command`] empty. It does so even when the rest of the command
/// is incomplete, as `filen cat --help` is without a file, as clap's own help flag would.
fn parse_args<I, T>(args: I) -> Result<CliArgs, clap::Error>
where
	I: IntoIterator<Item = T>,
	T: Into<OsString> + Clone,
{
	let args = args.into_iter().collect::<Vec<T>>();
	// Without validation, so that the help is found on an incomplete command.
	if let Ok(mut matches) = CliArgs::command()
		.ignore_errors(true)
		.try_get_matches_from(args.iter().cloned())
		&& let Some(help_topic) = matches.get_one::<String>("help").cloned()
	{
		let command = matches.remove_subcommand().map(|(name, _)| name);
		let mut cli_args = CliArgs::from_arg_matches(&matches)?;
		cli_args.help = Some(command.unwrap_or(help_topic));
		return Ok(cli_args);
	}
	CliArgs::try_parse_from(args)
}

/// The help topic `--help` asked for, or `None` for the general help.
fn help_topic_or_general(help_topic: String) -> Option<String> {
	(!help_topic.is_empty()).then_some(help_topic)
}

/// Completes a REPL input line as the REPL's tab completion does, waiting for remote paths to
/// be listed. Public only for the integration tests, which need a logged-in client.
#[doc(hidden)]
pub fn complete_repl_input(input: &str, client: Arc<Client>, working_path: &str) -> Vec<String> {
	completion::completer(input, client, &RemotePath::new(working_path), true)
}

/// Information returned by a command execution.
#[derive(Default)]
pub(crate) struct CommandResult {
	/// Change the REPL's working path.
	working_path: Option<RemotePath>,
	/// Exit the REPL.
	exit: bool,
}

const AUTH_CONFIG_PREFIX: &str = "filen_cli_auth_config_1:";

/// Raw CLI-provided overrides for [`ClientConfig`]. Kept separate from `ClientConfig` itself
/// (which isn't `Clone`) so it can be cheaply threaded through the several auth code paths and
/// used to build a fresh `ClientConfig` at whichever one actually ends up authenticating.
#[derive(Clone, Copy, Default)]
pub struct ClientConfigArgs {
	pub concurrency: Option<usize>,
	pub requests_per_sec: Option<NonZeroU32>,
	pub upload_bandwidth_kbps: Option<NonZeroU32>,
	pub download_bandwidth_kbps: Option<NonZeroU32>,
	pub memory_budget_bytes: Option<usize>,
	/// `Some(0)` disables the connect timeout; `Some(n)` (n > 0) sets it to `n` seconds; `None`
	/// leaves the SDK default in place.
	pub connect_timeout_secs: Option<u64>,
}

pub fn build_client_config(args: &ClientConfigArgs) -> ClientConfig {
	let mut config = ClientConfig::default();
	if let Some(v) = args.concurrency {
		config = config.with_concurrency(v);
	}
	if let Some(v) = args.requests_per_sec {
		config = config.with_rate(v);
	}
	config = config.with_upload(args.upload_bandwidth_kbps);
	config = config.with_download(args.download_bandwidth_kbps);
	if let Some(v) = args.memory_budget_bytes {
		config = config.with_memory_budget(v);
	}
	if let Some(secs) = args.connect_timeout_secs {
		config = config.with_connect_timeout((secs > 0).then(|| Duration::from_secs(secs)));
	}
	config
}
// todo: verify this really does work

pub fn serialize_auth_config(client: &Client) -> Result<String> {
	let sdk_config = serde_json::to_string(&client.to_stringified()).unwrap();
	let sdk_config = format!("{}{}", AUTH_CONFIG_PREFIX, base64.encode(sdk_config));
	Ok(sdk_config)
}

pub fn deserialize_auth_config(
	sdk_config: &str,
	client_config_args: &ClientConfigArgs,
) -> Result<Client> {
	let sdk_config = sdk_config
		.strip_prefix(AUTH_CONFIG_PREFIX)
		.ok_or_else(|| anyhow!("Invalid auth config format (missing or invalid prefix)"))?;
	let sdk_config = base64.decode(sdk_config)?;
	let sdk_config = serde_json::from_slice::<StringifiedClient>(&sdk_config)
		.context("Failed to parse auth config (it may be corrupt)")?;
	let client = UnauthClient::from_config(build_client_config(client_config_args))?;
	let client = client
		.from_stringified(sdk_config)
		.context("Failed to create client from SDK config")?;
	Ok(client)
}

#[cfg(test)]
mod tests {
	use clap::error::ErrorKind;

	use super::*;
	use crate::commands::rclone_cmds::RcloneCommands;

	#[test]
	fn a_repl_line_with_an_unclosed_quote_is_a_quoting_error() {
		assert!(matches!(
			parse_repl_line("ls 'my dir"),
			Err(ReplLineError::Quoting)
		));
	}

	#[test]
	fn a_repl_line_with_an_unknown_command_is_an_argument_error() {
		let Err(ReplLineError::Arguments(e)) = parse_repl_line("unknowncommand") else {
			panic!("parsed an unknown command");
		};
		assert_eq!(e.kind(), ErrorKind::InvalidSubcommand);
	}

	#[test]
	fn help_after_a_command_asks_for_that_commands_help() {
		let cli_args = parse_args(["filen", "ls", "--help"]).unwrap();
		assert_eq!(cli_args.help.as_deref(), Some("ls"));
		assert!(cli_args.command.is_none());
	}

	#[test]
	fn help_after_an_incomplete_command_asks_for_that_commands_help() {
		let cli_args = parse_args(["filen", "cat", "-h"]).unwrap();
		assert_eq!(cli_args.help.as_deref(), Some("cat"));
		assert!(cli_args.command.is_none());
	}

	#[test]
	fn help_after_a_command_ignores_what_follows_it() {
		let cli_args = parse_args(["filen", "ls", "--help", "/some/dir"]).unwrap();
		assert_eq!(cli_args.help.as_deref(), Some("ls"));
	}

	#[test]
	fn help_after_a_command_keeps_the_options_before_it() {
		let cli_args = parse_args(["filen", "-v", "cat", "--help"]).unwrap();
		assert_eq!(cli_args.verbose, 1);
	}

	#[test]
	fn help_before_a_topic_asks_for_that_topic() {
		let cli_args = parse_args(["filen", "--help", "ls"]).unwrap();
		assert_eq!(cli_args.help.as_deref(), Some("ls"));
		assert!(cli_args.command.is_none());
	}

	#[test]
	fn help_alone_asks_for_the_general_help() {
		let cli_args = parse_args(["filen", "--help"]).unwrap();
		assert_eq!(cli_args.help.as_deref(), Some(""));
	}

	#[test]
	fn help_inside_an_rclone_command_goes_to_rclone() {
		let cli_args = parse_args(["filen", "rclone", "ls", "--help"]).unwrap();
		assert!(cli_args.help.is_none());
		let Some(Commands::Rclone(RcloneCommands::Rclone { cmd })) = cli_args.command else {
			panic!("parsed {:?}", cli_args.command);
		};
		assert_eq!(cmd, ["ls", "--help"]);
	}

	#[test]
	fn help_before_an_rclone_command_is_the_clis() {
		let cli_args = parse_args(["filen", "rclone", "--help"]).unwrap();
		assert_eq!(cli_args.help.as_deref(), Some("rclone"));
	}

	#[test]
	fn the_options_and_commands_do_not_collide() {
		CliArgs::command().debug_assert();
	}

	#[test]
	fn an_option_after_the_command_is_the_clis() {
		let cli_args = parse_args(["filen", "ls", "/dir", "--json", "-vv"]).unwrap();
		assert!(cli_args.json);
		assert_eq!(cli_args.verbose, 2);
		let Some(Commands::Fs(FsCommands::Ls { directory, .. })) = cli_args.command else {
			panic!("parsed {:?}", cli_args.command);
		};
		assert_eq!(directory.as_deref(), Some("/dir"));
	}

	#[test]
	fn a_password_after_the_command_is_the_commands_option() {
		let cli_args = parse_args(["filen", "rm", "file", "-p"]).unwrap();
		assert!(cli_args.password.is_none());
		let Some(Commands::Fs(FsCommands::Rm { permanent, .. })) = cli_args.command else {
			panic!("parsed {:?}", cli_args.command);
		};
		assert!(permanent);
	}

	#[test]
	fn a_password_after_a_command_without_that_option_is_an_error() {
		let Err(e) = parse_args(["filen", "ls", "-p", "secret"]) else {
			panic!("parsed a password after the command");
		};
		assert_eq!(e.kind(), ErrorKind::UnknownArgument);
	}

	#[test]
	fn an_option_before_the_rclone_command_is_the_clis() {
		let cli_args = parse_args(["filen", "rclone", "-v", "ls"]).unwrap();
		assert_eq!(cli_args.verbose, 1);
		let Some(Commands::Rclone(RcloneCommands::Rclone { cmd })) = cli_args.command else {
			panic!("parsed {:?}", cli_args.command);
		};
		assert_eq!(cmd, ["ls"]);
	}

	#[test]
	fn an_option_inside_an_rclone_command_goes_to_rclone() {
		let cli_args = parse_args(["filen", "rclone", "ls", "-v"]).unwrap();
		assert_eq!(cli_args.verbose, 0);
		let Some(Commands::Rclone(RcloneCommands::Rclone { cmd })) = cli_args.command else {
			panic!("parsed {:?}", cli_args.command);
		};
		assert_eq!(cmd, ["ls", "-v"]);
	}

	#[test]
	fn a_repl_line_asking_for_help_on_a_topic_is_parsed() {
		let cli_args = parse_repl_line("--help ls").unwrap();
		assert_eq!(cli_args.help.as_deref(), Some("ls"));
		assert!(cli_args.command.is_none());
	}

	#[test]
	fn an_empty_help_topic_asks_for_the_general_help() {
		assert_eq!(help_topic_or_general(String::new()), None);
		assert_eq!(
			help_topic_or_general("ls".to_owned()),
			Some("ls".to_owned())
		);
	}

	#[test]
	fn a_repl_line_with_a_quoted_argument_is_parsed() {
		let cli_args = parse_repl_line("ls 'my dir'").unwrap();
		assert!(format!("{:?}", cli_args.command).contains("\"my dir\""));
	}
}
