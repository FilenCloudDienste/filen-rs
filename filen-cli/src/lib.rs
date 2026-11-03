//! [cli-doc] main-usage
//!
//! Welcome to Filen CLI v{{VERSION}}!
//!
//! You can find out more about the Filen CLI at https://github.com/FilenCloudDienste/filen-cli-releases
//!
//! Invoke the Filen CLI with no command specified to enter interactive mode (REPL).
//! There, you can specify absolute paths (starting with "/") or relative paths (supports "." and "..").
//!
//! Without a terminal on stdin, it runs the commands piped into it instead, one per line,
//! and exits with the code of the last one that failed.
//!
//! A command asks at the terminal for what it needs and was not given, such as a password or a
//! confirmation. When stdin is not a terminal, as with `filen rm -p file < list` or commands piped
//! in, it asks at the controlling terminal instead (`/dev/tty`, or the console on Windows), as git
//! and ssh do. `--yes` answers confirmations without asking, and `--no-input` never asks. A question that cannot be asked, with `--no-input` or without any terminal, fails the
//! command with exit code 2, naming the option that answers it instead where there is one.
//!
//! Options can go before or after the command (`filen --json ls` or `filen ls --json`),
//! except for `--password` and `--two-factor-code`, which go before it.
//! In the REPL, a line can add `--json`, `--quiet`, `-v` or `--yes` for its own command.

use std::{
	ffi::OsString,
	fs,
	io::{self, IsTerminal as _},
	num::NonZeroU32,
	path::{Path, PathBuf},
	sync::Arc,
	time::Duration,
};

use anyhow::{Context, Result, anyhow};
use base64::{Engine as _, engine::general_purpose::STANDARD as base64};
use clap::{ArgMatches, CommandFactory as _, FromArgMatches as _, Parser, parser::ValueSource};
use filen_sdk_rs::auth::{Client, StringifiedClient, http::ClientConfig, unauth::UnauthClient};
use ftail::Ftail;
use log::{LevelFilter, info, warn};

use crate::{
	commands::{
		Commands, execute_command,
		fs_cmds::{self, FsCommands},
	},
	docs::{generate_markdown_docs, print_in_app_docs},
	log_files::{ChosenLogFile, LogFiles, LogLimits},
	ui::{ConsoleLogLevel, CustomLogger, ReplPrompt},
	updater::check_for_updates,
	util::RemotePath,
};

mod auth;
mod commands;
mod completion;
mod docs;
mod error;
mod interrupt;
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

	/// Append the log to this file instead of writing it to the config directory
	#[arg(long, global = true, value_name = "PATH")]
	log_file: Option<PathBuf>,

	/// Do not write a log file
	#[arg(long, global = true, conflicts_with = "log_file")]
	no_log_file: bool,

	/// Filen account email (requires --password)
	#[arg(
		short,
		long,
		env = "FILEN_CLI_EMAIL",
		hide_env_values = true,
		global = true
	)]
	email: Option<String>,

	// The password and the two-factor code go before the command, unlike the other options: `-p`
	// after it is `rm -p` or `sync -p`, and `--password` is `serve --password`.
	/// Filen account password (requires --email)
	#[arg(short, long, env = "FILEN_CLI_PASSWORD", hide_env_values = true)]
	password: Option<String>,

	/// Filen account two-factor code (optional, requires --email and --password)
	#[arg(short, long, env = "FILEN_CLI_2FA_CODE", hide_env_values = true)]
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

	/// Answer yes to every confirmation prompt, such as `rm -p`'s
	#[arg(short, long, global = true)]
	yes: bool,

	/// Never prompt: fail where a prompt would be needed (pass --yes to answer confirmations)
	#[arg(long, global = true)]
	no_input: bool,

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

/// How large the log files grow and how long they are kept: a file is moved aside past 10 MiB,
/// and they are deleted after three days, or the oldest first once they hold 100 MiB together.
const LOG_LIMITS: LogLimits = LogLimits {
	max_file_size: 10 * 1024 * 1024,
	retention_days: 3,
	max_total_size: 100 * 1024 * 1024,
};

/// The level of the lines written to the log files.
const LOG_FILES_LEVEL: LevelFilter = LevelFilter::Debug;

/// The log the CLI writes besides what it prints.
struct OpenedLog {
	// An `Arc` because ftail builds each channel from a `'static` `Fn`, which can only hand out
	// clones of it.
	log: Arc<dyn log::Log + Send + Sync>,
	/// The file that holds this run's full log.
	path: PathBuf,
}

/// Opens the log the CLI writes: the file `--log-file` names, none with `--no-log-file`, or
/// otherwise the log files in `log_dir` ([`open_log_files`]). Fails only when the file
/// `--log-file` names cannot be opened.
fn open_log_destination(
	log_dir: &Path,
	log_file: Option<&Path>,
	no_log_file: bool,
) -> Result<(Option<OpenedLog>, Vec<String>)> {
	if no_log_file {
		return Ok((None, Vec::new()));
	}
	if let Some(path) = log_file {
		let file = ChosenLogFile::open(path, LOG_FILES_LEVEL).map_err(|e| {
			ui::UI::failure(&format!(
				"Failed to open the log file {}: {e}",
				path.display()
			))
		})?;
		let log = OpenedLog {
			log: Arc::new(file),
			path: path.to_owned(),
		};
		return Ok((Some(log), Vec::new()));
	}
	let (log_files, warnings) = open_log_files(log_dir);
	let log = log_files.map(|log_files| OpenedLog {
		log: Arc::new(log_files),
		path: log_dir.join(log_files::LATEST),
	});
	Ok((log, warnings))
}

/// Opens the log files in `log_dir`, creating it if needed. Returns them, or none if they cannot
/// be opened, and a warning for each thing that went wrong: the CLI runs on without log files
/// rather than failing over them, as on a read-only config directory.
fn open_log_files(log_dir: &Path) -> (Option<LogFiles>, Vec<String>) {
	let mut warnings = Vec::new();
	if let Err(e) = fs::create_dir_all(log_dir) {
		warnings.push(format!(
			"Not writing log files: failed to create {}: {e}",
			log_dir.display()
		));
		return (None, warnings);
	}
	if let Err(e) = log_files::close_to_others(log_dir) {
		warnings.push(format!(
			"Failed to make {} readable by you only: {e}",
			log_dir.display()
		));
	}
	match LogFiles::open(log_dir, LOG_FILES_LEVEL, LOG_LIMITS) {
		Ok(log_files) => (Some(log_files), warnings),
		Err(e) => {
			warnings.push(format!(
				"Not writing log files: failed to open them in {}: {e}",
				log_dir.display()
			));
			(None, warnings)
		}
	}
}

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

	let finished = inner_main(&mut ui, arenas_limited).await;
	// whatever the command made of being cut short, nobody reads what is left to print
	if interrupt::stdout_closed() {
		info!("Exiting, as stdout was closed");
		std::process::exit(0);
	}
	// translate errors to non-zero exit code
	match finished {
		Ok(Finished::Success) => {}
		Ok(Finished::ScriptLineFailed(code)) => std::process::exit(code),
		Err(e) => {
			ui.print_failure_or_error(&e);
			std::process::exit(error::exit_code(&e));
		}
	}
}

/// How a run of the CLI that did not fail ended.
enum Finished {
	Success,
	/// Commands piped into the CLI ran, and the last of those that failed exits with this code.
	/// It printed its failure already.
	ScriptLineFailed(i32),
}

async fn inner_main(ui: &mut ui::UI, arenas_limited: bool) -> Result<Finished> {
	let cli_args = parse_args(std::env::args_os())
		.unwrap_or_else(|e| e.exit())
		.args;
	interrupt::install().context("Failed to set up Ctrl-C")?;

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
	let command_options = CommandOptions::of(&cli_args);
	let logging_level = command_options.log_level();
	let (log_files, log_files_warnings) =
		open_log_destination(&log_dir, cli_args.log_file.as_deref(), cli_args.no_log_file)?;
	let log_files_level = match log_files {
		Some(_) => LOG_FILES_LEVEL,
		None => LevelFilter::Off,
	};
	// An `Arc` because the logger keeps it for the rest of the process, while the REPL changes it
	// for each line's command.
	let console_log_level = Arc::new(ConsoleLogLevel::new(logging_level, log_files_level));
	let logger_level = Arc::clone(&console_log_level);
	let mut logger = Ftail::new().custom(
		move |config| {
			Box::new(CustomLogger {
				config,
				level: Arc::clone(&logger_level),
				make_output_environment_agnostic_for_replay_testing: cli_args
					.make_output_environment_agnostic_for_replay_testing,
			}) as Box<dyn log::Log + Send + Sync>
		},
		logging_level,
	);
	let log_file_path = match log_files {
		Some(OpenedLog { log, path }) => {
			logger = logger.custom(
				move |_| Box::new(Arc::clone(&log)) as Box<dyn log::Log + Send + Sync>,
				log_files_level,
			);
			Some(path)
		}
		None => None,
	};
	logger.init().context("Failed to initialize logger")?;
	// ftail passes every record on to its channels, and no channel takes one past both levels.
	console_log_level.set(logging_level);
	info!("Logging level: {}", logging_level);
	if let Some(path) = log_file_path {
		info!("Full log file: {}", path.display());
	}
	for warning in &log_files_warnings {
		warn!("{warning}");
	}

	if cfg!(debug_assertions) {
		info!("Filen CLI (version omitted for replay testing)")
	} else {
		info!("Filen CLI v{}", env!("CARGO_PKG_VERSION"));
	}
	if !arenas_limited {
		warn!("glibc did not take the malloc arena cap, so freed memory may stay with the process");
	}

	ui.initialize(
		command_options.quiet,
		command_options.json,
		None,
		cli_args.reluctant_autocomplete,
		cli_args.make_output_environment_agnostic_for_replay_testing,
	);
	ui.set_assume_yes(command_options.assume_yes);
	for warning in &log_files_warnings {
		ui.print_warning(warning);
	}
	if cli_args.no_input {
		ui.disable_prompts();
	}

	// --export-markdown-docs
	if cli_args.export_markdown_docs {
		return generate_markdown_docs().map(|()| Finished::Success);
	}

	// --help
	if let Some(help_topic) = cli_args.help {
		return print_in_app_docs(ui, help_topic_or_general(help_topic))
			.map(|()| Finished::Success);
	}

	// a single command has no working directory to change
	if let Some(Commands::Fs(FsCommands::Cd { .. })) = cli_args.command {
		return Err(ui::UI::usage_failure(
			"`cd` only works in the REPL. To run a command in another directory, pass \
			 --working-path, or run `filen` without a command to enter the REPL.",
		));
	}

	// Without a command, the CLI runs the commands piped into it, or the REPL at a terminal.
	let script = cli_args.command.is_none() && !io::stdin().is_terminal();

	// the REPL reads its commands from the terminal
	if cli_args.no_input && cli_args.command.is_none() && !script {
		return Err(ui::UI::usage_failure(
			"--no-input needs a command: the REPL reads its commands from the terminal.",
		));
	}

	if !cli_args.skip_update {
		check_for_updates(
			ui,
			cli_args.force_update_check || cli_args.always_update,
			cli_args.always_update,
			&config.config_dir,
			cli_args.command.is_none() && !script,
		)
		.await;
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
		let _ = execute_cancellable(&config, ui, &mut client, &working_path, command).await?;
		Ok(Finished::Success)
	} else if script {
		let mut working_path =
			initial_working_path(ui, &mut client, cli_args.working_path.as_deref()).await?;
		let mut lines = Lines {
			config: &config,
			client: &mut client,
			command_options,
			console_log_level: &console_log_level,
		};
		let mut last_failure = None;
		let mut line = String::new();
		loop {
			line.clear();
			if io::stdin()
				.read_line(&mut line)
				.context("Failed to read the commands from stdin")?
				== 0
			{
				break;
			}
			match lines.run(ui, &mut working_path, &line).await {
				LineOutcome::Done => {}
				// Ctrl-C stops the commands piped in, not just the one it cancelled.
				LineOutcome::Failed(code @ interrupt::EXIT_INTERRUPTED) => {
					last_failure = Some(code);
					break;
				}
				LineOutcome::Failed(code) => last_failure = Some(code),
				LineOutcome::Exit => break,
			}
			if interrupt::stdout_closed() {
				break;
			}
		}
		Ok(last_failure.map_or(Finished::Success, Finished::ScriptLineFailed))
	} else {
		ui.print_banner();

		client.get(ui).await?;
		// authenticate, so the username is shown in the prompt.
		// this essentially defeats the purpose of LazyClient, but:
		// it does make a difference so non-authenticated commands (e.g. logout) can still be run ..
		// .. without authentication when called directly (no REPL)

		let mut working_path =
			initial_working_path(ui, &mut client, cli_args.working_path.as_deref()).await?;

		let mut lines = Lines {
			config: &config,
			client: &mut client,
			command_options,
			console_log_level: &console_log_level,
		};
		loop {
			let line = match ui.prompt_repl(lines.client.get_arc().unwrap(), &working_path)? {
				ReplPrompt::Line(line) => line,
				ReplPrompt::Cleared => continue,
				ReplPrompt::Exit => break,
			};
			match lines.run(ui, &mut working_path, &line).await {
				LineOutcome::Done | LineOutcome::Failed(_) => {}
				LineOutcome::Exit => break,
			}
			if interrupt::stdout_closed() {
				break;
			}
		}
		Ok(Finished::Success)
	}
}

/// What it takes to run the lines of the REPL, or of the commands piped into the CLI.
struct Lines<'a> {
	config: &'a CliConfig,
	client: &'a mut auth::LazyClient,
	/// The CLI's options, to which each line can add its own.
	command_options: CommandOptions,
	console_log_level: &'a ConsoleLogLevel,
}

/// How a line of the REPL, or of the commands piped into the CLI, ended.
#[derive(Debug, PartialEq)]
enum LineOutcome {
	/// It did what it asked, or was empty.
	Done,
	/// It failed with this exit code, and printed why.
	Failed(i32),
	/// It asked to leave the REPL.
	Exit,
}

impl Lines<'_> {
	/// Runs a line: prints the help it asks for, or runs its command, and prints its failure.
	async fn run(
		&mut self,
		ui: &mut ui::UI,
		working_path: &mut RemotePath,
		line: &str,
	) -> LineOutcome {
		if line.trim().is_empty() {
			return LineOutcome::Done;
		}
		let line_args = match parse_repl_line(line) {
			Ok(line_args) => line_args,
			Err(ReplLineError::Quoting) => {
				ui.print_failure("Invalid quoting");
				return LineOutcome::Failed(error::EXIT_USAGE);
			}
			Err(ReplLineError::Arguments(e)) => return print_argument_error(ui, &e),
			Err(ReplLineError::StartupOptions(options)) => {
				ui.print_failure(&format!(
					"{} can only be given when starting the CLI, not in the REPL",
					options.join(", ")
				));
				return LineOutcome::Failed(error::EXIT_USAGE);
			}
		};
		self.command_options
			.with_repl_line(&line_args)
			.apply(ui, self.console_log_level);
		let outcome = self.run_line_args(ui, working_path, line_args).await;
		self.command_options.apply(ui, self.console_log_level);
		outcome
	}

	async fn run_line_args(
		&mut self,
		ui: &mut ui::UI,
		working_path: &mut RemotePath,
		line_args: CliArgs,
	) -> LineOutcome {
		if let Some(help_topic) = line_args.help {
			return match print_in_app_docs(ui, help_topic_or_general(help_topic)) {
				Ok(()) => LineOutcome::Done,
				Err(e) => failed(ui, &e),
			};
		}
		let Some(command) = line_args.command else {
			return LineOutcome::Done;
		};
		match execute_cancellable(self.config, ui, self.client, working_path, command).await {
			Ok(result) => {
				if let Some(new_working_path) = result.working_path {
					*working_path = new_working_path;
				}
				match result.exit {
					true => LineOutcome::Exit,
					false => LineOutcome::Done,
				}
			}
			Err(e) => failed(ui, &e),
		}
	}
}

/// Prints `err`, clap's error for a line's arguments, with the text clap prints when the CLI is
/// invoked with the same arguments, and returns the outcome of the line. It is printed through the
/// UI like any other line's output: what `--version` asks for to stdout, a failure to stderr.
fn print_argument_error(ui: &mut ui::UI, err: &clap::Error) -> LineOutcome {
	let rendered = err.render();
	match err.exit_code() {
		0 => {
			ui.print(styled(&rendered, console::colors_enabled()).trim_end());
			LineOutcome::Done
		}
		code => {
			ui.print_to_stderr(styled(&rendered, console::colors_enabled_stderr()).trim_end());
			LineOutcome::Failed(code)
		}
	}
}

/// `text` with its styles as ANSI escapes if `colors`, or as plain text.
fn styled(text: &clap::builder::StyledStr, colors: bool) -> String {
	if colors {
		text.ansi().to_string()
	} else {
		text.to_string()
	}
}

/// Prints `err` and returns the outcome of the line that failed with it.
fn failed(ui: &mut ui::UI, err: &anyhow::Error) -> LineOutcome {
	ui.print_failure_or_error(err);
	LineOutcome::Failed(error::exit_code(err))
}

/// Runs `command` until it ends or Ctrl-C cancels it, unless it handles Ctrl-C itself
/// ([`interrupt::own`]).
async fn execute_cancellable(
	config: &CliConfig,
	ui: &mut ui::UI,
	client: &mut auth::LazyClient,
	working_path: &RemotePath,
	command: Commands,
) -> Result<CommandResult> {
	interrupt::cancellable(execute_command(config, ui, client, working_path, command)).await
}

/// The options that shape how a command runs, its output and its confirmations: the CLI's, and
/// in the REPL those its line adds.
#[derive(Debug, Clone, Copy, PartialEq)]
// Each bool is an independent clap flag.
#[allow(clippy::struct_excessive_bools)]
struct CommandOptions {
	quiet: bool,
	json: bool,
	/// How often `-v` was given.
	verbose: u8,
	/// `--yes`.
	assume_yes: bool,
}

impl CommandOptions {
	fn of(cli_args: &CliArgs) -> Self {
		Self {
			quiet: cli_args.quiet,
			json: cli_args.json,
			verbose: cli_args.verbose,
			assume_yes: cli_args.yes,
		}
	}

	/// These options with those a REPL line gives added: a line can turn `--quiet`, `--json` and
	/// `--yes` on, and raise the verbosity, but not undo the CLI's.
	fn with_repl_line(self, line_args: &CliArgs) -> Self {
		Self {
			quiet: self.quiet || line_args.quiet,
			json: self.json || line_args.json,
			verbose: self.verbose.max(line_args.verbose),
			assume_yes: self.assume_yes || line_args.yes,
		}
	}

	/// The level of the log lines printed to the terminal: none with `--quiet`, which overrides
	/// `-v`.
	fn log_level(self) -> LevelFilter {
		if self.quiet {
			return LevelFilter::Off;
		}
		match self.verbose {
			0 => LevelFilter::Off,
			1 => LevelFilter::Info,
			2 => LevelFilter::Debug,
			_ => LevelFilter::Trace,
		}
	}

	fn apply(self, ui: &mut ui::UI, console_log_level: &ConsoleLogLevel) {
		ui.set_quiet_and_json(self.quiet, self.json);
		ui.set_assume_yes(self.assume_yes);
		console_log_level.set(self.log_level());
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
	/// The line gives options that only apply when the CLI starts, such as `--config-dir`; these
	/// are their names.
	StartupOptions(Vec<String>),
}

/// The ids of the CLI's options a REPL line can give for its command. The others only apply when
/// the CLI starts.
const REPL_LINE_OPTIONS: &[&str] = &["help", "quiet", "json", "verbose", "yes"];

/// Parses a REPL line as the arguments the CLI would be invoked with.
fn parse_repl_line(line: &str) -> Result<CliArgs, ReplLineError> {
	let mut args = shlex::split(line.trim()).ok_or(ReplLineError::Quoting)?;
	args.insert(0, String::from("filen"));
	let ParsedArgs { args, matches } = parse_args(args).map_err(ReplLineError::Arguments)?;
	let startup_options = CliArgs::command()
		.get_arguments()
		.filter(|arg| !REPL_LINE_OPTIONS.contains(&arg.get_id().as_str()))
		.filter(|arg| matches.value_source(arg.get_id().as_str()) == Some(ValueSource::CommandLine))
		.filter_map(|arg| arg.get_long().map(|long| format!("--{long}")))
		.collect::<Vec<_>>();
	if !startup_options.is_empty() {
		return Err(ReplLineError::StartupOptions(startup_options));
	}
	Ok(args)
}

/// A parsed command line.
struct ParsedArgs {
	args: CliArgs,
	/// What clap parsed it from, which tells where each value came from.
	matches: ArgMatches,
}

/// Parses the command line (the program name first).
///
/// `-h`/`--help` after a command asks for that command's help, which [`CliArgs::help`] then
/// names, and leaves [`CliArgs::command`] empty. It does so even when the rest of the command
/// is incomplete, as `filen cat --help` is without a file, as clap's own help flag would.
fn parse_args<I, T>(args: I) -> Result<ParsedArgs, clap::Error>
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
		let mut cli_args = from_matches(&matches)?;
		cli_args.help = Some(command.unwrap_or(help_topic));
		return Ok(ParsedArgs {
			args: cli_args,
			matches,
		});
	}
	let matches = CliArgs::command().try_get_matches_from(args)?;
	Ok(ParsedArgs {
		args: from_matches(&matches)?,
		matches,
	})
}

/// The [`CliArgs`] in `matches`, with an error formatted as clap formats one it parses.
fn from_matches(matches: &ArgMatches) -> Result<CliArgs, clap::Error> {
	CliArgs::from_arg_matches(matches).map_err(|e| e.format(&mut CliArgs::command()))
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
	use crate::{commands::rclone_cmds::RcloneCommands, ui::test_support::Printed};

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
	fn a_repl_lines_argument_error_is_printed_to_stderr_through_the_ui() {
		let Err(ReplLineError::Arguments(e)) = parse_repl_line("unknowncommand") else {
			panic!("parsed an unknown command");
		};
		let out = Printed::default();
		let err = Printed::default();
		let mut ui = Printed::split_ui(&out, &err);

		assert_eq!(print_argument_error(&mut ui, &e), LineOutcome::Failed(2));
		assert_eq!(out.lines_or_empty(), "");
		assert_eq!(
			console::strip_ansi_codes(&err.lines()),
			"error: unrecognized subcommand 'unknowncommand'\n\nUsage: filen [OPTIONS] [COMMAND]"
		);
	}

	#[test]
	fn a_repl_line_asking_for_the_version_prints_it_to_stdout() {
		let Err(ReplLineError::Arguments(e)) = parse_repl_line("--version") else {
			panic!("parsed --version as a command line");
		};
		let out = Printed::default();
		let err = Printed::default();
		let mut ui = Printed::split_ui(&out, &err);

		assert_eq!(print_argument_error(&mut ui, &e), LineOutcome::Done);
		assert_eq!(
			console::strip_ansi_codes(&out.lines()),
			format!("Filen CLI {}", env!("CARGO_PKG_VERSION"))
		);
		assert_eq!(err.lines_or_empty(), "");
	}

	#[test]
	fn help_after_a_command_asks_for_that_commands_help() {
		let cli_args = parse_args(["filen", "ls", "--help"]).unwrap().args;
		assert_eq!(cli_args.help.as_deref(), Some("ls"));
		assert!(cli_args.command.is_none());
	}

	#[test]
	fn help_after_an_incomplete_command_asks_for_that_commands_help() {
		let cli_args = parse_args(["filen", "cat", "-h"]).unwrap().args;
		assert_eq!(cli_args.help.as_deref(), Some("cat"));
		assert!(cli_args.command.is_none());
	}

	#[test]
	fn help_after_a_command_ignores_what_follows_it() {
		let cli_args = parse_args(["filen", "ls", "--help", "/some/dir"])
			.unwrap()
			.args;
		assert_eq!(cli_args.help.as_deref(), Some("ls"));
	}

	#[test]
	fn help_after_a_command_keeps_the_options_before_it() {
		let cli_args = parse_args(["filen", "-v", "cat", "--help"]).unwrap().args;
		assert_eq!(cli_args.verbose, 1);
	}

	#[test]
	fn help_before_a_topic_asks_for_that_topic() {
		let cli_args = parse_args(["filen", "--help", "ls"]).unwrap().args;
		assert_eq!(cli_args.help.as_deref(), Some("ls"));
		assert!(cli_args.command.is_none());
	}

	#[test]
	fn help_alone_asks_for_the_general_help() {
		let cli_args = parse_args(["filen", "--help"]).unwrap().args;
		assert_eq!(cli_args.help.as_deref(), Some(""));
	}

	#[test]
	fn help_inside_an_rclone_command_goes_to_rclone() {
		let cli_args = parse_args(["filen", "rclone", "ls", "--help"])
			.unwrap()
			.args;
		assert!(cli_args.help.is_none());
		let Some(Commands::Rclone(RcloneCommands::Rclone { cmd })) = cli_args.command else {
			panic!("parsed {:?}", cli_args.command);
		};
		assert_eq!(cmd, ["ls", "--help"]);
	}

	#[test]
	fn help_before_an_rclone_command_is_the_clis() {
		let cli_args = parse_args(["filen", "rclone", "--help"]).unwrap().args;
		assert_eq!(cli_args.help.as_deref(), Some("rclone"));
	}

	#[test]
	fn the_options_and_commands_do_not_collide() {
		CliArgs::command().debug_assert();
	}

	#[test]
	fn an_option_after_the_command_is_the_clis() {
		let cli_args = parse_args(["filen", "ls", "/dir", "--json", "-vv"])
			.unwrap()
			.args;
		assert!(cli_args.json);
		assert_eq!(cli_args.verbose, 2);
		let Some(Commands::Fs(FsCommands::Ls { directory, .. })) = cli_args.command else {
			panic!("parsed {:?}", cli_args.command);
		};
		assert_eq!(directory.as_deref(), Some("/dir"));
	}

	#[test]
	fn a_password_after_the_command_is_the_commands_option() {
		let cli_args = parse_args(["filen", "rm", "file", "-p"]).unwrap().args;
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
		let cli_args = parse_args(["filen", "rclone", "-v", "ls"]).unwrap().args;
		assert_eq!(cli_args.verbose, 1);
		let Some(Commands::Rclone(RcloneCommands::Rclone { cmd })) = cli_args.command else {
			panic!("parsed {:?}", cli_args.command);
		};
		assert_eq!(cmd, ["ls"]);
	}

	#[test]
	fn an_option_inside_an_rclone_command_goes_to_rclone() {
		let cli_args = parse_args(["filen", "rclone", "ls", "-v"]).unwrap().args;
		assert_eq!(cli_args.verbose, 0);
		let Some(Commands::Rclone(RcloneCommands::Rclone { cmd })) = cli_args.command else {
			panic!("parsed {:?}", cli_args.command);
		};
		assert_eq!(cmd, ["ls", "-v"]);
	}

	#[test]
	fn a_repl_line_gives_output_options_for_its_command() {
		let line_args = parse_repl_line("ls --json -q -vv").unwrap();
		assert!(line_args.json);
		assert!(line_args.quiet);
		assert_eq!(line_args.verbose, 2);
	}

	#[test]
	fn a_repl_line_giving_an_option_for_the_start_of_the_cli_is_rejected() {
		let Err(ReplLineError::StartupOptions(options)) =
			parse_repl_line("ls --config-dir /tmp --skip-update")
		else {
			panic!("parsed an option for the start of the CLI");
		};
		assert_eq!(options, ["--config-dir", "--skip-update"]);
	}

	#[test]
	fn a_repl_line_giving_an_account_option_is_rejected() {
		let Err(ReplLineError::StartupOptions(options)) =
			parse_repl_line("-e someone@example.com ls")
		else {
			panic!("parsed an account option");
		};
		assert_eq!(options, ["--email"]);
	}

	#[test]
	fn a_repl_line_with_help_and_an_option_for_the_start_of_the_cli_is_rejected() {
		let Err(ReplLineError::StartupOptions(options)) =
			parse_repl_line("ls --working-path /dir --help")
		else {
			panic!("parsed an option for the start of the CLI");
		};
		assert_eq!(options, ["--working-path"]);
	}

	#[test]
	fn a_repl_line_adds_to_the_clis_output_options() {
		let cli = CommandOptions {
			quiet: false,
			json: true,
			verbose: 1,
			assume_yes: false,
		};
		let line_args = parse_repl_line("ls -q -vvv").unwrap();
		assert_eq!(
			cli.with_repl_line(&line_args),
			CommandOptions {
				quiet: true,
				json: true,
				verbose: 3,
				assume_yes: false,
			}
		);
	}

	#[test]
	fn a_repl_line_can_answer_its_commands_confirmations() {
		let cli = CommandOptions::of(&parse_args(["filen"]).unwrap().args);
		let line_args = parse_repl_line("rm -p some_file -y").unwrap();
		assert!(cli.with_repl_line(&line_args).assume_yes);
		assert!(!cli.assume_yes);
	}

	#[test]
	fn yes_after_the_command_is_the_clis() {
		let cli_args = parse_args(["filen", "rm", "-p", "some_file", "--yes"])
			.unwrap()
			.args;
		assert!(cli_args.yes);
		let Some(Commands::Fs(FsCommands::Rm { permanent, .. })) = cli_args.command else {
			panic!("parsed {:?}", cli_args.command);
		};
		assert!(permanent);
	}

	#[test]
	fn a_repl_line_giving_no_input_is_rejected() {
		let Err(ReplLineError::StartupOptions(options)) = parse_repl_line("ls --no-input") else {
			panic!("parsed --no-input on a REPL line");
		};
		assert_eq!(options, ["--no-input"]);
	}

	#[test]
	fn a_repl_line_without_output_options_keeps_the_clis() {
		let cli = CommandOptions {
			quiet: true,
			json: true,
			verbose: 2,
			assume_yes: true,
		};
		let line_args = parse_repl_line("ls").unwrap();
		assert_eq!(cli.with_repl_line(&line_args), cli);
	}

	#[test]
	fn quiet_hides_the_log_lines_whatever_the_verbosity() {
		let options = |quiet, verbose| CommandOptions {
			quiet,
			json: false,
			verbose,
			assume_yes: false,
		};
		assert_eq!(options(false, 0).log_level(), LevelFilter::Off);
		assert_eq!(options(false, 1).log_level(), LevelFilter::Info);
		assert_eq!(options(false, 2).log_level(), LevelFilter::Debug);
		assert_eq!(options(false, 3).log_level(), LevelFilter::Trace);
		assert_eq!(options(false, 4).log_level(), LevelFilter::Trace);
		assert_eq!(options(true, 3).log_level(), LevelFilter::Off);
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

	#[test]
	fn the_help_never_prints_the_credentials_from_the_environment() {
		let command = CliArgs::command();
		for id in ["email", "password", "two_factor_code"] {
			let arg = command
				.get_arguments()
				.find(|arg| arg.get_id() == id)
				.unwrap();
			assert!(arg.get_env().is_some(), "{id} is read from the environment");
			assert!(arg.is_hide_env_values_set(), "{id}'s value is hidden");
		}
	}
}
