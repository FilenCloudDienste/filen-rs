use std::{
	fmt::Display,
	fs,
	io::{self, IsTerminal, Write},
	path::PathBuf,
	sync::{
		Arc, Mutex, PoisonError,
		atomic::{AtomicUsize, Ordering},
	},
};

use anyhow::{Context, Result};
use clap::{CommandFactory, builder::Styles};
use console::{self, Term, style};
use filen_sdk_rs::auth::Client;
use inquire::{
	Autocomplete, CustomUserError, InquireError,
	history::{History, Replacement, SimpleHistory},
	ui::{RenderConfig, Styled},
};
use log::{LevelFilter, error, info, warn};
use serde::Serialize;
use tiny_gradient::{GradientStr, RGB};
use unicode_width::{UnicodeWidthChar as _, UnicodeWidthStr};

use crate::{
	CliArgs,
	completion::async_completions_available,
	error::{self, CliError, find_cli_error},
	interrupt,
	repl_history::ReplHistory,
	util::RemotePath,
};

/// The level of the log lines printed to the terminal. A REPL line's `-v` or `-q` changes it for
/// that line's command.
pub(crate) struct ConsoleLogLevel {
	/// The [`LevelFilter`], as its index in [`LevelFilter::iter`].
	level: AtomicUsize,
	/// The level of the log files, which [`log::max_level`] must not drop below.
	file_level: LevelFilter,
}

impl ConsoleLogLevel {
	/// Starts at `level`. Takes effect once [`ConsoleLogLevel::set`] has set the logger's maximum.
	pub(crate) fn new(level: LevelFilter, file_level: LevelFilter) -> Self {
		Self {
			level: AtomicUsize::new(level as usize),
			file_level,
		}
	}

	pub(crate) fn get(&self) -> LevelFilter {
		LevelFilter::iter()
			.nth(self.level.load(Ordering::Relaxed))
			.expect("only a LevelFilter is stored (should be impossible)")
	}

	/// Sets the level, and the maximum the `log` macros pass on, which the log files' level
	/// keeps from dropping below theirs.
	pub(crate) fn set(&self, level: LevelFilter) {
		self.level.store(level as usize, Ordering::Relaxed);
		log::set_max_level(level.max(self.file_level));
	}
}

pub(crate) struct CustomLogger {
	pub(crate) config: ftail::Config,
	// An `Arc` because ftail keeps the logger for the rest of the process, and the REPL changes
	// the level for each line's command.
	pub(crate) level: Arc<ConsoleLogLevel>,
	/// Whether to redact things that depend on the runtime environment, like: paths, ...
	pub(crate) make_output_environment_agnostic_for_replay_testing: bool,
}

impl log::Log for CustomLogger {
	fn enabled(&self, metadata: &log::Metadata) -> bool {
		metadata.level() <= self.level.get()
	}

	fn log(&self, record: &log::Record) {
		if !self.enabled(record.metadata()) {
			return;
		}
		let now = chrono::Local::now().format(&self.config.datetime_format);
		let formatted_timestamp = if !self.make_output_environment_agnostic_for_replay_testing {
			style(format!("[{}]", now)).dim()
		} else {
			style("[omit]".to_string()).dim()
		};
		let formatted_level = match record.level() {
			log::Level::Error => style("[ERROR]").red(),
			log::Level::Warn => style("[WARN] ").yellow(),
			log::Level::Info => style("[INFO] ").green(),
			log::Level::Debug => style("[DEBUG]").blue(),
			log::Level::Trace => style("[TRACE]").dim(),
		};
		let formatted_target = style(format!("({})", record.target())).dim();
		let msg = record.args().to_string();
		let msg = if self.make_output_environment_agnostic_for_replay_testing {
			make_output_environment_agnostic_for_replay_testing(msg)
		} else {
			msg
		};
		// Log lines go to stderr, apart from the output that can be piped into another program.
		// With nowhere left to report a failed write, it is dropped.
		let _ = writeln!(
			io::stderr().lock(),
			"{} {} {} {}",
			formatted_timestamp,
			formatted_level,
			formatted_target,
			msg
		);
	}

	fn flush(&self) {}
}

/// Where to report a bug.
const BUG_REPORT_URL: &str = "https://github.com/FilenCloudDienste/filen-rs/issues";
const TERMINAL_INPUT_FAILURE: &str = "Failed to read input from terminal. Please ensure that the terminal supports interactive input.";
const DOCKER_TERMINAL_INPUT_HINT: &str = "It seems you are running the CLI in a Docker container. Make sure to run the container with the -it flags to enable interactive input.";

/// [cli-doc] json-output
/// With `--json`, the CLI prints for programs to read:
/// - A command's result goes to stdout as JSON with camelCase keys, such as
///   `{"directories": [...], "files": [...]}` for `filen ls --json`.
/// - A failure goes to stderr as one line of JSON instead of text:
///   `{"error":{"message":"...","kind":"failure","exitCode":1}}`. `kind` is `failure` (the command
///   could not do what was asked), `usage` (the command line was invalid), `interrupted`
///   (Ctrl-C), `terminal_input` (a prompt could not be shown or answered) or `unexpected`
///   (possibly a bug, with a `reportUrl` to report it at). `exitCode` is the code the CLI exits
///   with, or in the REPL the code the command would have exited with.
/// - Nothing is printed when Rclone fails, as it prints its own error, or when stdout was closed.
/// - Warnings stay text on stderr; announcements and release notes are left out.
///
/// A command line the CLI cannot parse is reported as text, as are failures to set up the config
/// directory or the log file before the command runs.
#[derive(Debug, Serialize)]
struct JsonFailure {
	error: JsonError,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct JsonError {
	message: String,
	kind: FailureKind,
	exit_code: i32,
	#[serde(skip_serializing_if = "Option::is_none")]
	report_url: Option<&'static str>,
}

/// What kind of failure the CLI reports, as `--json` names it.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum FailureKind {
	Failure,
	Usage,
	Interrupted,
	TerminalInput,
	Unexpected,
}

/// Where the answers to the CLI's prompts come from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PromptInput {
	/// A terminal: the one on stdin, or the controlling terminal when stdin is not one.
	Terminal,
	/// Nowhere, as `--no-input` asks.
	NoInputFlag,
	/// Nowhere, as stdin is not a terminal and there is no other to ask at.
	NotATerminal,
}

impl PromptInput {
	/// Where prompts are answered: at the terminal on stdin, or, when stdin is not one (a file
	/// redirected to it, or commands piped in), at the controlling terminal, as git, ssh and sudo
	/// ask, if there is one (`controlling_terminal`) and stderr, which the prompts are shown on, is
	/// a terminal too. inquire reads the keys from the controlling terminal itself when stdin is not
	/// a terminal.
	fn detect(
		stdin_is_terminal: bool,
		stderr_is_terminal: bool,
		controlling_terminal: impl FnOnce() -> bool,
	) -> Self {
		if stdin_is_terminal || (stderr_is_terminal && controlling_terminal()) {
			PromptInput::Terminal
		} else {
			PromptInput::NotATerminal
		}
	}
}

/// Whether the process has a controlling terminal to read answers from: `/dev/tty`, or on Windows
/// the console's input, `CONIN$`, where inquire reads keys from when stdin is not a terminal.
fn has_controlling_terminal() -> bool {
	let terminal = if cfg!(windows) { "CONIN$" } else { "/dev/tty" };
	fs::OpenOptions::new()
		.read(true)
		.write(true)
		.open(terminal)
		.is_ok()
}

// Independent output settings, each set from its own CLI flag.
#[allow(clippy::struct_excessive_bools)]
pub(crate) struct UI {
	quiet: bool,
	history: ReplHistory,
	override_terminal_width: Option<usize>,
	/// Where printed lines go: stdout, except in the tests.
	out: Box<dyn Write + Send + Sync>,
	/// Where failures and warnings go: stderr, except in the tests.
	err: Box<dyn Write + Send + Sync>,
	/// Whether stdout is a terminal, so a person reads it rather than a program.
	stdout_is_terminal: bool,
	/// Whether to output machine-readable JSON where applicable
	pub(crate) json: bool,
	/// Whether `-v` asks for more detail, such as each file a transfer moved.
	verbose: bool,
	/// Whether to answer yes to every confirmation, as `--yes` asks.
	assume_yes: bool,
	prompt_input: PromptInput,
	disable_suggestions: bool,
	/// Whether to redact things that depend on the runtime environment, like: paths, ...
	make_output_environment_agnostic_for_replay_testing: bool,
	/// Whether stdin carries the commands the CLI runs, as in the REPL and for commands piped
	/// into it.
	commands_on_stdin: bool,
	/// Why the first line [`UI::print`] dropped since [`UI::start_command_output`] could not be
	/// written to stdout, other than because stdout was closed ([`write_stdout_line`]).
	stdout_write_failure: Option<io::Error>,
}

impl UI {
	pub(crate) fn new() -> Self {
		let mut ui = Self::with_writers(io::stdout(), io::stderr());
		ui.stdout_is_terminal = io::stdout().is_terminal();
		ui.prompt_input = PromptInput::detect(
			io::stdin().is_terminal(),
			io::stderr().is_terminal(),
			has_controlling_terminal,
		);
		ui
	}

	fn with_writers(
		out: impl Write + Send + Sync + 'static,
		err: impl Write + Send + Sync + 'static,
	) -> Self {
		UI {
			quiet: false,
			history: ReplHistory::default(),
			override_terminal_width: None,
			out: Box::new(out),
			err: Box::new(err),
			stdout_is_terminal: false,
			json: false,
			verbose: false,
			assume_yes: false,
			prompt_input: PromptInput::NotATerminal,
			disable_suggestions: false,
			make_output_environment_agnostic_for_replay_testing: false,
			commands_on_stdin: false,
			stdout_write_failure: None,
		}
	}

	pub(crate) fn initialize(
		&mut self,
		quiet: bool,
		json: bool,
		override_terminal_width: Option<usize>,
		disable_suggestions: bool,
		make_output_environment_agnostic_for_replay_testing: bool,
	) {
		self.quiet = quiet;
		self.json = json;
		self.override_terminal_width = override_terminal_width;
		self.disable_suggestions = disable_suggestions;
		self.make_output_environment_agnostic_for_replay_testing =
			make_output_environment_agnostic_for_replay_testing;
	}

	/// Never prompts from now on, as `--no-input` asks: a prompt fails instead, and a
	/// confirmation unless [`UI::set_assume_yes`] answers it.
	pub(crate) fn disable_prompts(&mut self) {
		self.prompt_input = PromptInput::NoInputFlag;
	}

	/// Loads the REPL's history from `path`, and saves the lines entered from now on to it.
	pub(crate) fn load_repl_history(&mut self, path: PathBuf) {
		self.history = ReplHistory::load(path);
	}

	/// Sets `--yes`, which a REPL line can turn on for its command.
	pub(crate) fn set_assume_yes(&mut self, assume_yes: bool) {
		self.assume_yes = assume_yes;
	}

	/// Whether `--yes` answers every confirmation, for a command that confirms in its own way.
	pub(crate) fn assumes_yes(&self) -> bool {
		self.assume_yes
	}

	pub(crate) fn is_quiet(&self) -> bool {
		self.quiet
	}

	/// Whether a command may draw a progress line: not with `--quiet`, and not while replay
	/// testing, where its redraws depend on timing and no recording could match them.
	pub(crate) fn may_draw_progress(&self) -> bool {
		!self.quiet && !self.make_output_environment_agnostic_for_replay_testing
	}

	/// Sets `-v`, which a REPL line can turn on for its command.
	pub(crate) fn set_verbose(&mut self, verbose: bool) {
		self.verbose = verbose;
	}

	/// Notes that stdin carries the commands the CLI runs, as in the REPL and for commands piped
	/// into it.
	pub(crate) fn set_commands_on_stdin(&mut self) {
		self.commands_on_stdin = true;
	}

	/// Whether stdin carries the commands the CLI runs, so that a command can neither read stdin
	/// nor write a file's contents to stdout among the other commands' output.
	pub(crate) fn commands_on_stdin(&self) -> bool {
		self.commands_on_stdin
	}

	/// Whether `-v` was given and `--quiet`, which overrides it, was not.
	pub(crate) fn is_verbose(&self) -> bool {
		self.verbose && !self.quiet
	}

	/// Whether a person reads the output: stdout is a terminal and the output is not JSON.
	pub(crate) fn output_is_read_by_a_person(&self) -> bool {
		self.stdout_is_terminal && !self.json
	}

	/// Sets `--quiet` and `--json`, which a REPL line can turn on for its command.
	pub(crate) fn set_quiet_and_json(&mut self, quiet: bool, json: bool) {
		self.quiet = quiet;
		self.json = json;
	}

	fn get_terminal_width(&self) -> Option<usize> {
		match self.override_terminal_width {
			Some(size) => Some(size),
			None => termsize::get().map(|size| size.cols as usize),
		}
	}

	/// Print a colorful banner at the top of the application (contains app name and version)
	pub(crate) fn print_banner(&mut self) {
		self.print_banner_(
			if self.make_output_environment_agnostic_for_replay_testing {
				"X.X.X"
			} else {
				env!("CARGO_PKG_VERSION")
			},
		);
	}
	fn print_banner_(&mut self, version: &str) {
		let banner_text = format!("Filen CLI v{}", version);
		let width = match self.get_terminal_width() {
			Some(w) if w > banner_text.len() + 2 => w,
			_ => banner_text.len() + 6,
		};
		let banner = "=".repeat((width - banner_text.len() - 2) / 2)
			+ " " + &banner_text
			+ " " + &"=".repeat((width - banner_text.len() - 2) / 2);
		if console::colors_enabled() {
			let filen_blue = RGB::new(0x1d, 0x57, 0xb9);
			let filen_violet = RGB::new(0x99, 0x66, 0xCC);
			let filen_green = RGB::new(0x50, 0xC8, 0x78);
			let banner = banner.gradient([filen_blue, filen_violet, filen_green]);
			// the string outputted by tiny_gradient has many "0m" sequences that reset formatting, so we're applying the bold formatting manually
			let banner = banner.to_string().replace("0m", "1m");
			// add reset at the end
			let banner = format!("{}{}", banner, "\x1b[0m");
			self.print(&banner);
		} else {
			self.print(&banner);
		}
	}

	/// Prints a line. It is not logged: printed output includes API keys, serve credentials and
	/// decrypted file contents, which must not reach the log files.
	pub(crate) fn print(&mut self, msg: &str) {
		let msg = self.redact_for_replay_testing(msg.to_string());
		if let Err(e) = write_stdout_line(&mut self.out, &msg) {
			// the first failure says why the output was lost; the later ones usually repeat it
			self.stdout_write_failure.get_or_insert(e);
		}
	}

	/// Starts a command's output, so that a line dropped before it, by an earlier command in the
	/// REPL, does not fail this one ([`UI::finish_command_output`]).
	pub(crate) fn start_command_output(&mut self) {
		self.stdout_write_failure = None;
	}

	/// Fails a command whose output lost a line since [`UI::start_command_output`], as on a full
	/// disk, so that it does not end as if it had succeeded; in the REPL, that fails the command,
	/// not the session. After an interrupt (`interrupted`) it does not: after SIGHUP from a
	/// closed terminal, stdout is the hung-up terminal and every write fails (`EIO` on Unix)
	/// while the command stops as Ctrl-C stops it, and a clean stop ends as it does after
	/// SIGTERM.
	pub(crate) fn finish_command_output(&mut self, interrupted: bool) -> Result<()> {
		match self.stdout_write_failure.take() {
			Some(e) if !interrupted => Err(CliError::Failure(format!(
				"Some output could not be written to stdout: {e}"
			))
			.into()),
			Some(_) | None => Ok(()),
		}
	}

	/// Writes `bytes` to stdout as they are, with no newline added, as a file's content is printed.
	/// Like [`UI::print`], it is not logged. A closed stdout ends the running command as it does for
	/// [`UI::print`] ([`write_stdout_line`]), and fails with [`CliError::StdoutClosed`] so that the
	/// caller stops writing at once.
	pub(crate) fn write_stdout_bytes(&mut self, bytes: &[u8]) -> Result<()> {
		// Flushed now: stdout buffers up to the next newline, which binary content may not have.
		match self.out.write_all(bytes).and_then(|()| self.out.flush()) {
			Ok(()) => Ok(()),
			Err(e) if e.kind() == io::ErrorKind::BrokenPipe => {
				interrupt::close_stdout();
				Err(CliError::StdoutClosed.into())
			}
			Err(e) => Err(CliError::Failure(format!("Failed to write to stdout: {e}")).into()),
		}
	}

	/// Prints a line to stderr, which is for failures, warnings and notices, so that they stay out
	/// of output piped into another program. It is not logged, like [`UI::print`].
	pub(crate) fn print_to_stderr(&mut self, msg: &str) {
		let msg = self.redact_for_replay_testing(msg.to_string());
		// With nowhere left to report a failed write, it is dropped.
		let _ = writeln!(self.err, "{}", msg);
	}

	// print with formatting

	/// Print an announcement message (used for important info from updates) to stderr. It is only
	/// shown to a person at a terminal: not with `--json` or `--quiet`, nor when stdout is piped
	/// or redirected.
	pub(crate) fn print_announcement(&mut self, msg: &str) {
		warn!("[ANNOUNCEMENT] {}", msg);
		if self.quiet || self.json || !self.stdout_is_terminal {
			return;
		}
		self.print_to_stderr(&format!(
			"{} {}",
			style("[i]").yellow().bold(),
			style(msg).yellow()
		));
	}

	/// Print a muted notice about the CLI itself, such as an update's release notes, to stderr. Like
	/// an announcement, it is only shown to a person at a terminal: not with `--json` or `--quiet`,
	/// nor when stdout is piped or redirected.
	pub(crate) fn print_muted_notice(&mut self, msg: &str) {
		if self.quiet || self.json || !self.stdout_is_terminal {
			return;
		}
		self.print_muted_to_stderr(msg);
	}

	/// Print a message with a success icon
	pub(crate) fn print_success(&mut self, msg: &str) {
		if !self.quiet {
			self.print(&format!("{} {}", style("✔").green(), msg));
		}
	}

	/// Print a message with a warning icon
	pub(crate) fn print_warning(&mut self, msg: &str) {
		self.print_to_stderr(&format!("{} {}", style("⚠").yellow(), style(msg).yellow()));
	}

	/// Print a warning about what the CLI does besides the command, such as its update check, which
	/// is logged, and printed as [`UI::print_warning`] prints one but not with `--quiet` or
	/// `--json`, like an announcement.
	pub(crate) fn print_side_warning(&mut self, msg: &str) {
		warn!("{msg}");
		if self.quiet || self.json {
			return;
		}
		self.print_warning(msg);
	}

	/// Print a message with a failure icon
	pub(crate) fn print_failure(&mut self, msg: &str) {
		self.print_to_stderr(&format!("{} {}", style("✘").red(), msg));
	}

	/// Return an error with a user-friendly error message
	pub(crate) fn failure(msg: &str) -> anyhow::Error {
		CliError::Failure(msg.to_owned()).into()
	}

	/// Return an error with a user-friendly message about an invalid command line, which exits
	/// with code 2
	pub(crate) fn usage_failure(msg: &str) -> anyhow::Error {
		CliError::Usage(msg.to_owned()).into()
	}

	/// Print an error or failure message
	/// User-friendly failures created with `UI::failure` will be printed as-is, along with any
	/// context they were wrapped in, other errors will be printed with a generic message and a
	/// link to report bugs. With `--json`, it is one [`JsonFailure`] line instead.
	pub(crate) fn print_failure_or_error(&mut self, err: &anyhow::Error) {
		error!("{:#}", err);
		let kind = match find_cli_error(err) {
			Some(CliError::Failure(_)) => FailureKind::Failure,
			Some(CliError::Usage(_)) => FailureKind::Usage,
			Some(CliError::Interrupted) => FailureKind::Interrupted,
			Some(CliError::TerminalInput(_)) => FailureKind::TerminalInput,
			// the child process reported its own error, and nobody reads the output that was cut
			// short
			Some(CliError::ChildExit(_) | CliError::StdoutClosed) => return,
			None => FailureKind::Unexpected,
		};
		if self.json {
			self.print_json_failure(err, kind);
			return;
		}
		match kind {
			FailureKind::Failure | FailureKind::Usage | FailureKind::Interrupted => {
				self.print_failure(&format!("{:#}", err))
			}
			FailureKind::TerminalInput => {
				self.print_failure(TERMINAL_INPUT_FAILURE);
				if cfg!(feature = "is_docker") {
					self.print_failure(DOCKER_TERMINAL_INPUT_HINT);
				}
			}
			FailureKind::Unexpected => {
				self.print_failure(&format!("An unexpected error occurred: {:#}", err));
				self.print_failure(&format!(
					"If you believe this is a bug, please report it at {BUG_REPORT_URL}"
				));
			}
		}
	}

	/// Prints `err`, a failure of `kind`, as one line of JSON to stderr.
	fn print_json_failure(&mut self, err: &anyhow::Error, kind: FailureKind) {
		let message = match kind {
			FailureKind::TerminalInput if cfg!(feature = "is_docker") => {
				format!("{TERMINAL_INPUT_FAILURE} {DOCKER_TERMINAL_INPUT_HINT}")
			}
			FailureKind::TerminalInput => TERMINAL_INPUT_FAILURE.to_owned(),
			FailureKind::Failure
			| FailureKind::Usage
			| FailureKind::Interrupted
			| FailureKind::Unexpected => format!("{err:#}"),
		};
		let failure = JsonFailure {
			error: JsonError {
				message,
				kind,
				exit_code: error::exit_code(err),
				report_url: matches!(kind, FailureKind::Unexpected).then_some(BUG_REPORT_URL),
			},
		};
		// a struct of strings and numbers always serializes
		let line = serde_json::to_string(&failure)
			.expect("a failure serializes to JSON (should be impossible)");
		self.print_to_stderr(&line);
	}

	pub(crate) fn print_muted(&mut self, msg: &str) {
		self.print(&style(msg).dim().to_string());
	}

	/// Print a muted message about the CLI itself, such as its update's progress, to stderr, out
	/// of the command's output.
	pub(crate) fn print_muted_to_stderr(&mut self, msg: &str) {
		self.print_to_stderr(&style(msg).dim().to_string());
	}

	/// Print a table of key-value pairs
	pub(crate) fn print_key_value_table(&mut self, table: &[(&str, &str)]) {
		let key_width = table.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
		for (key, value) in table {
			self.print(&format!("{:>key_width$} {}", style(key).dim(), value));
		}
	}

	/// Print a grid of strings (like an ls command's output), or one per line when stdout is not
	/// a terminal and no width is set, so that a program reading it gets one item per line
	pub(crate) fn print_grid(&mut self, items: &[&str]) {
		if items.is_empty() {
			self.print("");
			return;
		}
		if self.override_terminal_width.is_none() && !self.stdout_is_terminal {
			self.print(&items.join("\n"));
			return;
		}
		let terminal_width = self.get_terminal_width().unwrap_or(10);
		match grid_layout(items, terminal_width) {
			GridLayout::OnePerLine => self.print(&items.join("\n")),
			GridLayout::Columns {
				column_height,
				column_widths,
			} => {
				let columns = items.chunks(column_height).collect::<Vec<_>>();
				for row in 0..column_height {
					let items = columns
						.iter()
						.zip(&column_widths)
						.map(|(column, column_width)| {
							let cell = column.get(row).unwrap_or(&"");
							let padding = " ".repeat(column_width - ansi_width::ansi_width(cell));
							format!("{}{}", cell, padding)
						})
						.collect::<Vec<_>>();
					self.print(&items.join("  "));
				}
			}
		}
	}

	/// Prints `value`, a command's result, as JSON to stdout.
	pub(crate) fn print_json(&mut self, value: impl Serialize) -> Result<()> {
		self.print(&serde_json::to_string_pretty(&value).context("Failed to serialize JSON")?);
		Ok(())
	}

	// prompt

	/// Prompt the user for input in the REPL (contains some special formatting)
	pub(crate) fn prompt_repl(
		&mut self,
		client: Arc<Client>,
		working_path: &RemotePath,
	) -> Result<ReplPrompt> {
		info!("[PROMPT] prompt_repl with path: {}", working_path);
		let prompt_prefix = self.redact_for_replay_testing(format!("({})", client.email()));
		let render_config = RenderConfig::default()
			.with_prompt_prefix(
				Styled::new(prompt_prefix.as_str()).with_fg(inquire::ui::Color::LightCyan),
			)
			.with_answered_prompt_prefix(
				Styled::new(prompt_prefix.as_str()).with_fg(inquire::ui::Color::LightCyan),
			);
		let working_path_str = self.redact_for_replay_testing(format!("{}", working_path));
		let typed = TypedLine::default();
		match inquire::Text::new(&working_path_str)
			.with_render_config(render_config)
			.with_autocomplete(InquireCompleter {
				client,
				working_path: working_path.clone(),
				disable_suggestions: self.disable_suggestions,
				typed: typed.clone(),
			})
			.with_history(TypedHistory {
				// a copy, as inquire owns the history it is given
				history: SimpleHistory::new(self.history.entries().to_vec()),
				typed: typed.clone(),
			})
			.prompt_skippable()
		{
			Ok(Some(answer)) => {
				log_repl_line(&answer);
				self.history.add(&answer);
				Ok(ReplPrompt::Line(answer))
			}
			// Ctrl-D, which inquire cannot tell apart from Esc
			Ok(None) | Err(InquireError::OperationCanceled) => {
				info!("[PROMPT] canceled");
				Ok(ReplPrompt::cancelled(&typed))
			}
			// Ctrl-C
			Err(InquireError::OperationInterrupted) => {
				info!("[PROMPT] interrupted");
				let prompt = ReplPrompt::cancelled(&typed);
				if prompt == ReplPrompt::Cleared {
					// inquire leaves the cursor after the dropped line, where the next prompt would
					// start.
					self.print_to_stderr("");
				}
				Ok(prompt)
			}
			Err(e) => {
				error!("[PROMPT] failed to read input");
				Err(CliError::TerminalInput(e).into())
			}
		}
	}

	/// Prompt the user for text input.
	/// Returns None if the user cancels the prompt (Ctrl+C or Esc)
	pub(crate) fn prompt(&mut self, msg: &str) -> Result<String> {
		info!("[PROMPT] prompt with message: {}", msg);
		self.ensure_can_prompt(msg)?;
		match Self::handle_prompt_result(inquire::Text::new(msg.trim()).prompt()) {
			Ok(Some(answer)) => Ok(answer),
			Ok(None) => Err(UI::failure("Canceled")),
			Err(e) => Err(e)?,
		}
	}

	/// Prompt the user for a secret, such as a password or a two-factor code, masking what is
	/// typed.
	/// Returns None if the user cancels the prompt (Ctrl+C or Esc)
	pub(crate) fn prompt_password(&mut self, msg: &str) -> Result<String> {
		info!("[PROMPT] prompt_password with message: {}", msg);
		self.ensure_can_prompt(msg)?;
		match Self::handle_prompt_result(
			inquire::Password::new(msg.trim())
				.with_display_mode(inquire::PasswordDisplayMode::Masked)
				.without_confirmation()
				.prompt(),
		) {
			Ok(Some(answer)) => Ok(answer),
			Ok(None) => Err(UI::failure("Canceled")),
			Err(e) => Err(e)?,
		}
	}

	/// Text answers are never logged: a prompt may ask for a two-factor code or a password, even
	/// one whose input is shown.
	fn handle_prompt_result(result: Result<String, InquireError>) -> Result<Option<String>> {
		match result {
			Ok(answer) => {
				info!("[PROMPT] answer not displayed in logs");
				Ok(Some(answer))
			}
			Err(InquireError::OperationCanceled) | Err(InquireError::OperationInterrupted) => {
				info!("[PROMPT] canceled");
				Ok(None)
			}
			Err(e) => Err(CliError::TerminalInput(e).into()),
		}
	}

	/// Fails unless a prompt asking `msg` can be shown: with `--no-input`, or without a terminal
	/// ([`PromptInput::detect`]), nobody could answer it. A usage error, since the command line has to give what the
	/// prompt would ask for instead.
	pub(crate) fn ensure_can_prompt(&self, msg: &str) -> Result<()> {
		self.ensure_prompt_input(msg, "")
	}

	/// [`UI::ensure_can_prompt`], with `hint` after the reason.
	fn ensure_prompt_input(&self, msg: &str, hint: &str) -> Result<()> {
		let reason = match self.prompt_input {
			PromptInput::Terminal => return Ok(()),
			PromptInput::NoInputFlag => "--no-input was given",
			PromptInput::NotATerminal => "stdin is not a terminal",
		};
		Err(UI::usage_failure(&format!(
			"Cannot ask \"{}\": {reason}.{hint}",
			msg.trim()
		)))
	}

	/// Ask the user a yes/no question, such as a confirmation of what the command is about to do.
	/// `--yes` answers yes without asking, whatever the question; without it, the question fails
	/// when it cannot be asked ([`UI::ensure_can_prompt`]), saying to pass `--yes`.
	pub(crate) fn prompt_confirm(&mut self, msg: &str, default: bool) -> Result<bool> {
		info!("[PROMPT] prompt_confirm with message: {}", msg);
		self.ensure_can_confirm(msg)?;
		if self.assume_yes {
			info!("[PROMPT] answered yes by --yes");
			return Ok(true);
		}
		self.ask_yes_no(msg, default)
	}

	/// Fails unless the confirmation `msg` can be answered, by `--yes` or at the terminal, as
	/// [`UI::prompt_confirm`] would. A command calls it before the work that leads up to the
	/// confirmation, so that a script learns it needs `--yes` before anything is looked up.
	pub(crate) fn ensure_can_confirm(&self, msg: &str) -> Result<()> {
		if self.assume_yes {
			return Ok(());
		}
		self.ensure_prompt_input(msg, " Pass --yes to answer yes.")
	}

	fn ask_yes_no(&mut self, msg: &str, default: bool) -> Result<bool> {
		let result = inquire::Confirm::new(&self.redact_for_replay_testing(msg.trim().to_string()))
			.with_default(default)
			.prompt();
		match result {
			Ok(answer) => {
				info!("[PROMPT] answer: {}", answer);
				Ok(answer)
			}
			Err(InquireError::OperationCanceled) | Err(InquireError::OperationInterrupted) => {
				info!("[PROMPT] canceled");
				Err(UI::failure("Canceled"))
			}
			Err(e) => Err(CliError::TerminalInput(e).into()),
		}
	}

	/// Prompt the user for a selection from a list of options
	pub(crate) fn prompt_select(
		&mut self,
		msg: &str,
		options: Vec<String>,
	) -> Result<Option<String>> {
		info!(
			"[PROMPT] prompt_select with message: {} and options: {:?}",
			msg, options
		);
		self.ensure_can_prompt(msg)?;
		let result = inquire::Select::new(
			&self.redact_for_replay_testing(msg.trim().to_string()),
			options,
		)
		.prompt();
		Self::handle_select_result(result)
	}

	/// Prompt the user to choose one of `options`, shown by their labels, with the cursor starting
	/// on `default` (or on the first option when `default` is not one of them).
	/// Returns None if the user cancels the prompt (Ctrl+C or Esc)
	pub(crate) fn prompt_choice<T: Copy + PartialEq>(
		&mut self,
		msg: &str,
		options: &[(&str, T)],
		default: T,
	) -> Result<Option<T>> {
		let labels = options.iter().map(|(label, _)| *label).collect::<Vec<_>>();
		info!(
			"[PROMPT] prompt_choice with message: {} and options: {:?}",
			msg, labels
		);
		self.ensure_can_prompt(msg)?;
		let starting_cursor = options
			.iter()
			.position(|(_, value)| *value == default)
			.unwrap_or(0);
		let result = inquire::Select::new(
			&self.redact_for_replay_testing(msg.trim().to_string()),
			labels,
		)
		.with_starting_cursor(starting_cursor)
		.raw_prompt();
		Ok(Self::handle_select_result(result)?.map(|choice| {
			options
				.get(choice.index)
				.expect(
					"inquire answers with the index of one of the options (should be impossible)",
				)
				.1
		}))
	}

	fn handle_select_result<T: Display>(result: Result<T, InquireError>) -> Result<Option<T>> {
		match result {
			Ok(answer) => {
				info!("[PROMPT] answer: {}", answer);
				Ok(Some(answer))
			}
			Err(InquireError::OperationCanceled) | Err(InquireError::OperationInterrupted) => {
				info!("[PROMPT] canceled");
				Ok(None)
			}
			Err(e) => Err(CliError::TerminalInput(e).into()),
		}
	}

	// format help text

	pub(crate) fn format_command_help(cmd: &mut clap::Command) -> String {
		let styled_str = cmd
			.clone()
			.styles(if console::colors_enabled() {
				Styles::styled()
					.literal(anstyle::AnsiColor::Green.on_default())
					.placeholder(anstyle::AnsiColor::Green.on_default())
					.context(anstyle::Style::new().dimmed())
					.header(anstyle::AnsiColor::Yellow.on_default())
			} else {
				Styles::plain()
			})
			.help_template("{positionals}\n{options}")
			.render_help();
		let formatted_usage = cmd
			.clone()
			.styles(if console::colors_enabled() {
				Styles::styled().literal(anstyle::AnsiColor::Green.on_default().underline())
			} else {
				Styles::plain()
			})
			.help_template("{usage}\n{about}")
			// the long help's `{about}` is the whole description, not only its first paragraph
			.render_long_help();
		format!(
			"{}{}",
			//style("◊").green().bold().bright(),
			formatted_usage.ansi(),
			styled_str
				.ansi()
				.to_string()
				.lines()
				.filter(|l| !l.contains("--help")) // filter out help flag line that is erraneously included by clap
				.map(|l| format!("{} {}", style("→").dim(), l.trim()))
				.collect::<Vec<_>>()
				.join("\n")
		)
	}

	pub(crate) fn format_global_options_help() -> String {
		CliArgs::command()
			.clone()
			.styles(if console::colors_enabled() {
				Styles::styled()
					.literal(anstyle::AnsiColor::Green.on_default())
					.placeholder(anstyle::AnsiColor::Green.on_default())
					.context(anstyle::Style::new().dimmed())
					.header(anstyle::AnsiColor::Yellow.on_default())
			} else {
				Styles::plain()
			})
			.help_template("{options}")
			.render_help()
			.ansi()
			.to_string()
			.lines()
			.filter(|l| !l.is_empty())
			.map(|l| {
				// if line starts with an ansi code, it's the definition line
				if l.trim().starts_with("[") || l.trim().starts_with("-") {
					format!("{} {}", style("→").dim(), l.trim())
				} else {
					format!("  {}", l.trim())
				}
			})
			.collect::<Vec<_>>()
			.join("\n")
	}

	pub(crate) fn format_text_blockquote(text: &str) -> String {
		text.lines()
			.map(|l| format!("┃ {}", l))
			.collect::<Vec<_>>()
			.join("\n")
	}

	pub(crate) fn format_text_heading(text: &str) -> String {
		style(text).bold().underlined().to_string()
	}
}

/// How [`UI::print_grid`] lays out its items.
#[derive(Debug, PartialEq, Eq)]
enum GridLayout {
	/// One item per line, unpadded.
	OnePerLine,
	/// Consecutive runs of `column_height` items side by side, each column padded to its width
	/// (in terminal cells) and separated by two spaces.
	Columns {
		column_height: usize,
		column_widths: Vec<usize>,
	},
}

/// Finds the fewest rows that fit `items` into `terminal_width` cells, measuring every item the
/// way the grid pads it ([`ansi_width::ansi_width`]), or one item per line when no height fits.
fn grid_layout(items: &[&str], terminal_width: usize) -> GridLayout {
	let widest = items
		.iter()
		.map(|item| ansi_width::ansi_width(item))
		.max()
		.unwrap_or(0);
	if widest > terminal_width {
		return GridLayout::OnePerLine;
	}
	for column_height in 1..=items.len() {
		let column_widths = items
			.chunks(column_height)
			.map(|column| {
				column
					.iter()
					.map(|item| ansi_width::ansi_width(item))
					.max()
					.unwrap_or(0)
			})
			.collect::<Vec<_>>();
		let spacing_width = (column_widths.len() - 1) * 2;
		if column_widths.iter().sum::<usize>() + spacing_width <= terminal_width {
			return GridLayout::Columns {
				column_height,
				column_widths,
			};
		}
	}
	GridLayout::OnePerLine
}

/// Writes `line` to `out`, which is stdout outside the tests. A reader that closed stdout, as
/// `head` does once it has read its lines, wants no more output: the line is dropped, and the
/// running command ends as Ctrl-C would end it ([`interrupt::close_stdout`]), so that a sync,
/// mount or serve stops cleanly and a command cut short does not exit with 0.
///
/// Any other failed write drops the line too, is logged, and is returned, so that a command
/// whose output was lost, as on a full disk, does not exit with 0
/// ([`UI::finish_command_output`]). After SIGHUP from a closed terminal, stdout is the hung-up
/// terminal and every write fails (`EIO` on Unix) while the command stops as Ctrl-C stops it: a
/// panic there would cut that stop short, and closing stdout would turn its exit code 130 into 0.
pub(crate) fn write_stdout_line(out: &mut (impl Write + ?Sized), line: &str) -> io::Result<()> {
	match writeln!(out, "{}", line) {
		Ok(()) => Ok(()),
		Err(e) if e.kind() == io::ErrorKind::BrokenPipe => {
			interrupt::close_stdout();
			Ok(())
		}
		Err(e) => {
			warn!("Dropped a line of output, as writing to stdout failed: {e}");
			Err(e)
		}
	}
}

/// Logs the command a REPL line runs, without its arguments, which can hold a password (as in
/// `serve-webdav --password`).
fn log_repl_line(line: &str) {
	let command = line.split_whitespace().next().unwrap_or_default();
	info!("[PROMPT] command: {}", command);
}

/// What the REPL prompt read.
#[derive(Debug, PartialEq)]
pub(crate) enum ReplPrompt {
	/// A line to run.
	Line(String),
	/// The line was dropped before it was entered; prompt again.
	Cleared,
	/// Leave the REPL.
	Exit,
}

impl ReplPrompt {
	/// After Ctrl-C or Ctrl-D, which leave the REPL on an empty line, as Ctrl-D does in a shell,
	/// and otherwise drop the line.
	fn cancelled(typed: &TypedLine) -> Self {
		if typed.is_empty() {
			ReplPrompt::Exit
		} else {
			ReplPrompt::Cleared
		}
	}
}

/// The text typed at the REPL prompt so far, which inquire does not hand out when the prompt is
/// cancelled. The prompt's autocompletion and history see every change to it: typing goes through
/// [`Autocomplete::get_suggestions`], tab completion through [`Autocomplete::get_completion`] and
/// the arrow keys through [`TypedHistory`].
// An `Arc` because the autocompletion and the history inquire takes are clones of one another,
// read after the prompt has ended.
#[derive(Clone, Default)]
struct TypedLine(Arc<Mutex<String>>);

impl TypedLine {
	fn set(&self, line: &str) {
		let mut typed = self.0.lock().unwrap_or_else(PoisonError::into_inner);
		typed.clear();
		typed.push_str(line);
	}

	fn is_empty(&self) -> bool {
		self.0
			.lock()
			.unwrap_or_else(PoisonError::into_inner)
			.is_empty()
	}
}

/// The REPL's history, telling [`TypedLine`] of the line each arrow key puts at the prompt.
#[derive(Clone)]
struct TypedHistory {
	history: SimpleHistory,
	typed: TypedLine,
}

impl History for TypedHistory {
	fn earlier_element(&mut self) -> Result<Replacement, CustomUserError> {
		let replacement = self.history.earlier_element()?;
		if let Some(line) = &replacement {
			self.typed.set(line);
		}
		Ok(replacement)
	}

	fn later_element(&mut self) -> Result<Replacement, CustomUserError> {
		let replacement = self.history.later_element()?;
		// Past the latest line, inquire empties the prompt.
		self.typed.set(replacement.as_deref().unwrap_or_default());
		Ok(replacement)
	}

	fn prepend_element(&mut self, line: String) {
		self.history.prepend_element(line);
	}
}

pub(crate) fn format_date(date: &chrono::DateTime<chrono::Utc>) -> String {
	date.format("%Y-%m-%d %H:%M:%S (UTC)").to_string()
}

pub(crate) fn format_size(size: u64) -> String {
	humansize::format_size(size, humansize::BINARY)
}

/// A dim line at the bottom of the terminal on stderr, redrawn in place, which is cleared before
/// anything else is printed.
pub(crate) struct StderrLine {
	term: Term,
	/// Whether the line is on the screen, so that it needs clearing before another line is printed.
	shown: bool,
}

impl StderrLine {
	/// The line, when stderr is a terminal to draw it on.
	pub(crate) fn on_terminal() -> Option<Self> {
		let term = Term::stderr();
		term.is_term().then_some(Self { term, shown: false })
	}

	/// Redraws the line with `text`, cut to the terminal's width: a line that wrapped onto a
	/// second row would not be cleared.
	pub(crate) fn show(&mut self, text: &str) {
		let text = truncate_to_width(text, usize::from(self.term.size().1));
		// A failed write to the terminal leaves the line as it was; there is nowhere to report it.
		let _ = self.term.clear_line();
		let _ = self.term.write_str(&style(text).dim().to_string());
		let _ = self.term.flush();
		self.shown = true;
	}

	/// Clears the line, so that the next line printed starts on a clean row.
	pub(crate) fn clear(&mut self) {
		if self.shown {
			let _ = self.term.clear_line();
			let _ = self.term.flush();
			self.shown = false;
		}
	}
}

/// Cut `line` down to `width` display columns. An over-long line would wrap onto a second row,
/// and the in-place redraw only ever clears one row.
pub(crate) fn truncate_to_width(line: &str, width: usize) -> String {
	if line.width() <= width {
		return line.to_string();
	}
	if width == 0 {
		return String::new();
	}
	let mut truncated = String::new();
	let mut used = 0;
	for c in line.chars() {
		let char_width = c.width().unwrap_or(0);
		if used + char_width > width - 1 {
			break;
		}
		truncated.push(c);
		used += char_width;
	}
	truncated.push('…');
	truncated
}

#[derive(Clone)]
struct InquireCompleter {
	client: Arc<Client>,
	working_path: RemotePath,
	disable_suggestions: bool,
	typed: TypedLine,
}

impl Autocomplete for InquireCompleter {
	fn get_completion(
		&mut self,
		input: &str,
		highlighted_suggestion: Option<String>,
	) -> std::prelude::v1::Result<inquire::autocompletion::Replacement, inquire::CustomUserError> {
		let completion = if highlighted_suggestion.is_some() {
			highlighted_suggestion
		} else {
			let completions =
				crate::completion::completer(input, self.client.clone(), &self.working_path, true);
			completions.first().cloned()
		};
		if let Some(line) = &completion {
			self.typed.set(line);
		}
		Ok(completion)
	}

	fn get_suggestions(
		&mut self,
		input: &str,
	) -> std::prelude::v1::Result<Vec<String>, inquire::CustomUserError> {
		self.typed.set(input);
		if self.disable_suggestions {
			return Ok(Vec::new());
		}
		let completions =
			crate::completion::completer(input, self.client.clone(), &self.working_path, false);
		Ok(completions)
	}

	fn updated_suggestions_available(&mut self, _input: &str) -> bool {
		async_completions_available() // could depend on input, but it's okay to overestimate here
	}
}

/// Redact output that depends on the runtime environment, so it can be matched in Manuel replay testing:
/// - Environment variables: $HOME, $MANUEL_TMP, $MANUEL_REMOTE_CWD, $MANUEL_EMAIL
fn make_output_environment_agnostic_for_replay_testing(mut output: String) -> String {
	output = output.replace(
		env!("CARGO_PKG_VERSION"),
		&style(format!("{} version {}", "{{", "}}"))
			.yellow()
			.bold()
			.to_string(),
	);
	for environment_variable in ["HOME", "MANUEL_TMP", "MANUEL_REMOTE_CWD", "MANUEL_EMAIL"] {
		if let Ok(env_var_value) = std::env::var(environment_variable) {
			output = output.replace(
				&env_var_value,
				&style(format!("{} ${} {}", "{{", environment_variable, "}}"))
					.yellow()
					.bold()
					.to_string(),
			);
		}
	}
	output
}

impl UI {
	pub(crate) fn redact_for_replay_testing(&self, output: String) -> String {
		if self.make_output_environment_agnostic_for_replay_testing {
			make_output_environment_agnostic_for_replay_testing(output)
		} else {
			output
		}
	}

	/// `value`, or `[omit]` while replay testing: for output that differs on every run, such as
	/// a newly created id or key, which no recording could match.
	pub(crate) fn omit_for_replay_testing<'a>(&self, value: &'a str) -> &'a str {
		if self.make_output_environment_agnostic_for_replay_testing {
			"[omit]"
		} else {
			value
		}
	}
}

#[cfg(test)]
pub(crate) mod test_support {
	use std::sync::Mutex;

	use super::*;

	/// What a UI printed. Its clones are handles to one buffer, so the test keeps one to read back
	/// what the UI wrote through another.
	#[derive(Clone, Default)]
	pub(crate) struct Printed(Arc<Mutex<Vec<u8>>>);

	impl Write for Printed {
		fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
			self.0.lock().unwrap().extend_from_slice(buf);
			Ok(buf.len())
		}

		fn flush(&mut self) -> io::Result<()> {
			Ok(())
		}
	}

	impl Printed {
		/// A UI that prints into this buffer, as a terminal shows stdout and stderr together.
		pub(crate) fn ui(&self) -> UI {
			UI::with_writers(self.clone(), self.clone())
		}

		/// [`Printed::ui`], as if stdin were a terminal, so that a prompt could be asked.
		pub(crate) fn prompting_ui(&self) -> UI {
			let mut ui = self.ui();
			ui.prompt_input = PromptInput::Terminal;
			ui
		}

		/// [`Printed::ui`], as if stdout were a terminal.
		pub(crate) fn terminal_ui(&self) -> UI {
			let mut ui = self.ui();
			ui.stdout_is_terminal = true;
			ui
		}

		/// [`Printed::ui`], as if stdin and stdout were a terminal.
		pub(crate) fn prompting_terminal_ui(&self) -> UI {
			let mut ui = self.prompting_ui();
			ui.stdout_is_terminal = true;
			ui
		}

		/// A UI that prints stdout into `out` and stderr into `err`.
		pub(crate) fn split_ui(out: &Printed, err: &Printed) -> UI {
			UI::with_writers(out.clone(), err.clone())
		}

		/// [`Printed::split_ui`], as if stdout were a terminal.
		pub(crate) fn split_terminal_ui(out: &Printed, err: &Printed) -> UI {
			let mut ui = Printed::split_ui(out, err);
			ui.stdout_is_terminal = true;
			ui
		}

		/// The printed lines joined by `\n`, without the one ending the last line.
		pub(crate) fn lines(&self) -> String {
			let bytes = self.0.lock().unwrap();
			let text = str::from_utf8(&bytes).unwrap();
			text.strip_suffix('\n').unwrap().to_owned()
		}

		/// The printed bytes, exactly as they were written.
		pub(crate) fn bytes(&self) -> Vec<u8> {
			// copied out, as the UI may still write into the buffer
			self.0.lock().unwrap().clone()
		}

		/// [`Printed::lines`], or an empty string if nothing was printed.
		pub(crate) fn lines_or_empty(&self) -> String {
			if self.0.lock().unwrap().is_empty() {
				String::new()
			} else {
				self.lines()
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use std::{env, process::Command};

	use super::{test_support::Printed, *};

	#[test]
	fn no_progress_is_drawn_with_quiet_or_while_replay_testing() {
		let mut ui = Printed::default().ui();
		assert!(ui.may_draw_progress());
		ui.initialize(true, false, None, true, false);
		assert!(!ui.may_draw_progress());
		ui.initialize(false, false, None, true, true);
		assert!(!ui.may_draw_progress());
	}

	/// Tests that read back what was logged. `log` takes one logger per process and other tests in
	/// this binary install theirs (test-utils' tracing subscriber), so each of these runs alone in a
	/// process of its own, started by the test of the same name in the parent module.
	mod alone {
		use std::sync::Mutex;

		use log::{LevelFilter, Log, Metadata, Record};

		use crate::ui::{UI, log_repl_line, test_support::Printed};

		static LOGGED: Mutex<Vec<String>> = Mutex::new(Vec::new());

		struct CapturingLogger;

		impl Log for CapturingLogger {
			fn enabled(&self, _: &Metadata) -> bool {
				true
			}

			fn log(&self, record: &Record) {
				LOGGED.lock().unwrap().push(record.args().to_string());
			}

			fn flush(&self) {}
		}

		fn capture_logs() {
			log::set_logger(&CapturingLogger).unwrap();
			log::set_max_level(LevelFilter::Trace);
		}

		fn logged() -> Vec<String> {
			LOGGED.lock().unwrap().clone()
		}

		#[test]
		#[ignore = "run in a process of its own by ui::tests::printed_lines_are_not_logged"]
		fn printed_lines_are_not_logged() {
			capture_logs();
			let printed = Printed::default();
			let mut ui = printed.ui();
			ui.print("api-key-1234");
			ui.print_success("Saved");
			ui.initialize(true, false, None, true, false);
			ui.print_success("Saved quietly");

			// Whether the tick is colored depends on the terminal.
			let printed = printed.lines();
			assert!(printed.starts_with("api-key-1234\n"), "{printed:?}");
			assert!(printed.ends_with(" Saved"), "{printed:?}");
			assert_eq!(logged(), Vec::<String>::new());
		}

		#[test]
		#[ignore = "run in a process of its own by ui::tests::prompt_answers_are_not_logged"]
		fn prompt_answers_are_not_logged() {
			capture_logs();
			let answer = UI::handle_prompt_result(Ok("123456".to_owned())).unwrap();

			assert_eq!(answer.as_deref(), Some("123456"));
			assert_eq!(logged(), ["[PROMPT] answer not displayed in logs"]);
		}

		#[test]
		#[ignore = "run in a process of its own by ui::tests::a_repl_line_is_logged_without_its_arguments"]
		fn a_repl_line_is_logged_without_its_arguments() {
			capture_logs();
			log_repl_line("serve-webdav --user me --password hunter2");

			assert_eq!(logged(), ["[PROMPT] command: serve-webdav"]);
		}
	}

	/// Runs the ignored test `ui::tests::alone::<name>`, and only it, in a new process of this
	/// test binary.
	fn run_alone(name: &str) {
		let output = Command::new(env::current_exe().unwrap())
			.args([
				"--exact",
				&format!("ui::tests::alone::{name}"),
				"--ignored",
				"--test-threads=1",
			])
			.output()
			.unwrap();
		let stdout = String::from_utf8_lossy(&output.stdout);
		assert!(
			output.status.success() && stdout.contains("test result: ok. 1 passed"),
			"{stdout}{}",
			String::from_utf8_lossy(&output.stderr)
		);
	}

	#[test]
	fn the_console_log_level_reads_back_each_level() {
		for level in LevelFilter::iter() {
			assert_eq!(ConsoleLogLevel::new(level, LevelFilter::Debug).get(), level);
		}
	}

	#[test]
	fn printed_lines_are_not_logged() {
		run_alone("printed_lines_are_not_logged");
	}

	#[test]
	fn prompt_answers_are_not_logged() {
		run_alone("prompt_answers_are_not_logged");
	}

	#[test]
	fn a_repl_line_is_logged_without_its_arguments() {
		run_alone("a_repl_line_is_logged_without_its_arguments");
	}

	// run using: cargo insta test --review -- --test test_ui

	#[test]
	fn test_ui() {
		console::set_colors_enabled(true); // even in CI

		fn test(ui: &mut UI) {
			ui.print_banner_("0.0.0-test");
			ui.print("Should just show");
			ui.print_success("Success!");
			ui.print_failure("Something went wrong");
			ui.print_muted("This is muted text");
			ui.print_key_value_table(&[("Key1", "Value1"), ("LongerKey2", "Value2")]);
			ui.print_grid(&[
				"file1.txt",
				"file2.txt",
				"a_very_long_filename_document.pdf",
				"image.png",
				"video.mp4",
				"archive.zip",
			]);
		}

		fn printed(terminal_width: usize) -> String {
			let printed = Printed::default();
			let mut ui = printed.ui();
			ui.initialize(false, false, Some(terminal_width), true, false);
			test(&mut ui);
			printed.lines()
		}

		// different terminal sizes
		insta::assert_snapshot!(printed(30));
		insta::assert_snapshot!(printed(100));

		// no color
		console::set_colors_enabled(false);
		insta::assert_snapshot!(printed(100));
	}

	#[test]
	fn a_grid_prints_one_item_per_line_when_stdout_is_not_a_terminal() {
		let printed = Printed::default();
		printed.ui().print_grid(&["a", "b", "c"]);
		assert_eq!(printed.lines(), "a\nb\nc");
	}

	#[test]
	fn a_grid_fills_a_set_width_when_stdout_is_not_a_terminal() {
		let printed = Printed::default();
		let mut ui = printed.ui();
		ui.initialize(false, false, Some(80), true, false);
		ui.print_grid(&["a", "b", "c"]);
		assert_eq!(printed.lines(), "a  b  c");
	}

	#[test]
	fn a_grid_fills_a_set_width_on_a_terminal() {
		let printed = Printed::default();
		let mut ui = printed.terminal_ui();
		ui.initialize(false, false, Some(80), true, false);
		ui.print_grid(&["a", "b", "c"]);
		assert_eq!(printed.lines(), "a  b  c");
	}

	#[test]
	fn a_grid_of_zwj_emoji_wider_than_the_terminal_is_laid_out_one_per_line() {
		let family = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}\u{200D}\u{1F466}";
		let name = family.repeat(14);
		assert_eq!(
			grid_layout(&[name.as_str(), "a"], 40),
			GridLayout::OnePerLine
		);
	}

	#[test]
	fn a_grid_takes_the_fewest_rows_that_fit() {
		assert_eq!(
			grid_layout(&["a", "bb", "ccc", "dddd", "e"], 10),
			GridLayout::Columns {
				column_height: 3,
				column_widths: vec![3, 4],
			}
		);
	}

	#[test]
	fn failures_and_warnings_go_to_stderr() {
		let out = Printed::default();
		let err = Printed::default();
		let mut ui = Printed::split_ui(&out, &err);

		ui.print("result");
		ui.print_warning("careful");
		ui.print_failure("No such file: /a");
		ui.print_failure_or_error(&UI::failure("Canceled"));

		assert_eq!(out.lines(), "result");
		// Whether the icons are colored depends on the terminal.
		assert_eq!(
			console::strip_ansi_codes(&err.lines()),
			"⚠ careful\n✘ No such file: /a\n✘ Canceled"
		);
	}

	/// What `print_announcement` prints to stdout and to stderr, after `set_up`.
	fn printed_announcement(set_up: impl FnOnce(&mut UI)) -> (String, String) {
		let out = Printed::default();
		let err = Printed::default();
		let mut ui = Printed::split_ui(&out, &err);
		ui.stdout_is_terminal = true;
		set_up(&mut ui);
		ui.print_announcement("Version 1 is deprecated");
		let printed = |p: &Printed| console::strip_ansi_codes(&p.lines_or_empty()).into_owned();
		(printed(&out), printed(&err))
	}

	#[test]
	fn an_announcement_goes_to_stderr_at_a_terminal() {
		assert_eq!(
			printed_announcement(|_| {}),
			(String::new(), "[i] Version 1 is deprecated".to_owned())
		);
	}

	#[test]
	fn an_announcement_is_skipped_with_json_output() {
		assert_eq!(
			printed_announcement(|ui| ui.json = true),
			(String::new(), String::new())
		);
	}

	#[test]
	fn an_announcement_is_skipped_when_quiet() {
		assert_eq!(
			printed_announcement(|ui| ui.quiet = true),
			(String::new(), String::new())
		);
	}

	fn printed_side_warning(set_up: impl FnOnce(&mut UI)) -> (String, String) {
		let out = Printed::default();
		let err = Printed::default();
		let mut ui = Printed::split_ui(&out, &err);
		set_up(&mut ui);
		ui.print_side_warning("Update check failed: offline");
		let printed = |p: &Printed| console::strip_ansi_codes(&p.lines_or_empty()).into_owned();
		(printed(&out), printed(&err))
	}

	#[test]
	fn a_side_warning_goes_to_stderr() {
		assert_eq!(
			printed_side_warning(|_| {}),
			(String::new(), "⚠ Update check failed: offline".to_owned())
		);
	}

	#[test]
	fn a_side_warning_is_skipped_with_json_output() {
		assert_eq!(
			printed_side_warning(|ui| ui.json = true),
			(String::new(), String::new())
		);
	}

	#[test]
	fn a_side_warning_is_skipped_when_quiet() {
		assert_eq!(
			printed_side_warning(|ui| ui.quiet = true),
			(String::new(), String::new())
		);
	}

	#[test]
	fn an_announcement_is_skipped_when_stdout_is_not_a_terminal() {
		assert_eq!(
			printed_announcement(|ui| ui.stdout_is_terminal = false),
			(String::new(), String::new())
		);
	}

	#[test]
	fn a_failure_wrapped_in_context_is_printed_as_a_failure() {
		let printed = Printed::default();
		let mut ui = printed.ui();
		let err = Err::<(), _>(UI::failure("Canceled"))
			.context("Failed to read confirmation")
			.unwrap_err();

		ui.print_failure_or_error(&err);

		// Whether the cross is colored depends on the terminal.
		assert_eq!(
			console::strip_ansi_codes(&printed.lines()),
			"✘ Failed to read confirmation: Canceled"
		);
	}

	#[test]
	fn an_unexpected_error_is_printed_with_a_bug_report_link() {
		let printed = Printed::default();
		let mut ui = printed.ui();

		ui.print_failure_or_error(&anyhow::anyhow!("connection reset"));

		assert_eq!(
			console::strip_ansi_codes(&printed.lines()),
			"✘ An unexpected error occurred: connection reset\n\
			 ✘ If you believe this is a bug, please report it at https://github.com/FilenCloudDienste/filen-rs/issues"
		);
	}

	/// A writer whose every write fails as a full disk fails it.
	struct FullDisk;

	impl Write for FullDisk {
		fn write(&mut self, _: &[u8]) -> io::Result<usize> {
			Err(io::Error::from(io::ErrorKind::StorageFull))
		}

		fn flush(&mut self) -> io::Result<()> {
			Ok(())
		}
	}

	/// A UI whose stdout fails every write as a full disk does, and the buffer its stderr goes to.
	fn full_disk_ui(json: bool) -> (UI, Printed) {
		let err = Printed::default();
		let mut ui = UI::with_writers(FullDisk, err.clone());
		ui.json = json;
		(ui, err)
	}

	#[test]
	fn a_line_stdout_could_not_take_fails_the_command() {
		let (mut ui, _) = full_disk_ui(false);
		ui.start_command_output();
		ui.print("lost");
		let err = ui.finish_command_output(false).unwrap_err();
		assert_eq!(
			format!("{err:#}"),
			format!(
				"Some output could not be written to stdout: {}",
				io::Error::from(io::ErrorKind::StorageFull)
			)
		);
		assert!(matches!(find_cli_error(&err), Some(CliError::Failure(_))));
		assert_eq!(error::exit_code(&err), error::EXIT_FAILURE);
	}

	#[test]
	fn a_lost_line_fails_only_the_command_that_printed_it() {
		let (mut ui, _) = full_disk_ui(false);
		ui.start_command_output();
		ui.print("lost");
		ui.finish_command_output(false).unwrap_err();
		// the next REPL command prints nothing, and succeeds
		ui.start_command_output();
		ui.finish_command_output(false).unwrap();
	}

	#[test]
	fn a_line_lost_before_a_command_does_not_fail_it() {
		let (mut ui, _) = full_disk_ui(false);
		ui.print("lost before the command, as by an earlier REPL line");
		ui.start_command_output();
		ui.finish_command_output(false).unwrap();
	}

	#[test]
	fn a_lost_line_with_json_is_reported_as_a_json_failure() {
		let (mut ui, err_out) = full_disk_ui(true);
		ui.start_command_output();
		ui.print("lost");
		let err = ui.finish_command_output(false).unwrap_err();
		ui.print_failure_or_error(&err);
		let printed: serde_json::Value = serde_json::from_str(&err_out.lines()).unwrap();
		assert_eq!(
			printed,
			serde_json::json!({"error": {
				"message": format!(
					"Some output could not be written to stdout: {}",
					io::Error::from(io::ErrorKind::StorageFull)
				),
				"kind": "failure",
				"exitCode": 1,
			}})
		);
	}

	#[test]
	fn a_lost_line_after_an_interrupt_does_not_fail_the_command() {
		let (mut ui, _) = full_disk_ui(false);
		ui.start_command_output();
		ui.print("lost to a hung-up terminal");
		ui.finish_command_output(true).unwrap();
		// and is not left over for the next command either
		ui.start_command_output();
		ui.finish_command_output(false).unwrap();
	}

	#[test]
	fn an_unexpected_error_is_printed_with_its_context() {
		let printed = Printed::default();
		let mut ui = printed.ui();

		ui.print_failure_or_error(&anyhow::anyhow!("connection reset").context("Failed to list"));

		assert_eq!(
			console::strip_ansi_codes(&printed.lines()),
			"✘ An unexpected error occurred: Failed to list: connection reset\n\
			 ✘ If you believe this is a bug, please report it at https://github.com/FilenCloudDienste/filen-rs/issues"
		);
	}

	/// What `print_failure_or_error` prints for `err` to stdout and to stderr, with or without
	/// `--json`.
	fn printed_failure(err: &anyhow::Error, json: bool) -> (String, String) {
		let out = Printed::default();
		let err_out = Printed::default();
		let mut ui = Printed::split_ui(&out, &err_out);
		ui.json = json;
		ui.print_failure_or_error(err);
		(
			out.lines_or_empty(),
			console::strip_ansi_codes(&err_out.lines_or_empty()).into_owned(),
		)
	}

	/// The JSON `print_failure_or_error` prints for `err` with `--json`, read back.
	fn json_failure(err: &anyhow::Error) -> serde_json::Value {
		let (out, err) = printed_failure(err, true);
		assert_eq!(out, "");
		assert_eq!(err.lines().count(), 1, "{err}");
		serde_json::from_str(&err).unwrap()
	}

	#[test]
	fn a_failure_with_json_is_an_error_object_on_stderr() {
		let err = Err::<(), _>(UI::failure("No such file: /a"))
			.context("Failed to copy")
			.unwrap_err();

		assert_eq!(
			json_failure(&err),
			serde_json::json!({"error": {
				"message": "Failed to copy: No such file: /a",
				"kind": "failure",
				"exitCode": 1,
			}})
		);
	}

	#[test]
	fn a_usage_failure_with_json_exits_with_2() {
		let err = UI::usage_failure("Give both --user and --password, or neither");

		assert_eq!(
			json_failure(&err),
			serde_json::json!({"error": {
				"message": "Give both --user and --password, or neither",
				"kind": "usage",
				"exitCode": 2,
			}})
		);
	}

	#[test]
	fn an_interrupted_command_with_json_exits_with_130() {
		let err = anyhow::Error::from(CliError::Interrupted).context("Failed to download");

		assert_eq!(
			json_failure(&err),
			serde_json::json!({"error": {
				"message": "Failed to download: Canceled",
				"kind": "interrupted",
				"exitCode": 130,
			}})
		);
	}

	#[test]
	fn a_terminal_input_failure_with_json_explains_itself() {
		let err = anyhow::Error::from(CliError::TerminalInput(InquireError::NotTTY));

		let failure = json_failure(&err);

		assert_eq!(failure["error"]["kind"], "terminal_input");
		assert_eq!(failure["error"]["exitCode"], 1);
		assert!(
			failure["error"]["message"]
				.as_str()
				.unwrap()
				.starts_with(TERMINAL_INPUT_FAILURE),
			"{failure}"
		);
		assert!(failure["error"].get("reportUrl").is_none(), "{failure}");
	}

	#[test]
	fn an_io_error_at_a_prompt_is_a_terminal_input_failure() {
		let err = UI::handle_prompt_result(Err(InquireError::IO(io::Error::other("broken pipe"))))
			.unwrap_err();

		assert!(
			matches!(
				find_cli_error(&err),
				Some(CliError::TerminalInput(InquireError::IO(_)))
			),
			"{err:?}"
		);
		assert_eq!(error::exit_code(&err), 1);
	}

	#[test]
	fn an_unexpected_error_with_json_has_the_report_link_apart() {
		let err = anyhow::anyhow!("connection reset").context("Failed to list");

		assert_eq!(
			json_failure(&err),
			serde_json::json!({"error": {
				"message": "Failed to list: connection reset",
				"kind": "unexpected",
				"exitCode": 1,
				"reportUrl": "https://github.com/FilenCloudDienste/filen-rs/issues",
			}})
		);
	}

	#[test]
	fn a_child_exit_and_a_closed_stdout_print_nothing_with_or_without_json() {
		for json in [false, true] {
			for err in [CliError::ChildExit(3), CliError::StdoutClosed] {
				assert_eq!(
					printed_failure(&anyhow::Error::from(err), json),
					(String::new(), String::new()),
					"{json}"
				);
			}
		}
	}

	#[test]
	fn failures_without_json_stay_text() {
		assert_eq!(
			printed_failure(&UI::usage_failure("Invalid flag"), false),
			(String::new(), "✘ Invalid flag".to_owned())
		);
		assert_eq!(
			printed_failure(&anyhow::Error::from(CliError::Interrupted), false),
			(String::new(), "✘ Canceled".to_owned())
		);
		let (out, err) = printed_failure(
			&anyhow::Error::from(CliError::TerminalInput(InquireError::NotTTY)),
			false,
		);
		assert_eq!(out, "");
		assert!(
			err.starts_with(&format!("✘ {TERMINAL_INPUT_FAILURE}")),
			"{err}"
		);
	}

	#[test]
	fn a_result_prints_as_json_on_stdout() {
		#[derive(Serialize)]
		#[serde(rename_all = "camelCase")]
		struct Moved {
			from_path: &'static str,
		}
		let out = Printed::default();
		let err = Printed::default();
		let mut ui = Printed::split_ui(&out, &err);

		ui.print_json(Moved { from_path: "/a" }).unwrap();

		assert_eq!(out.lines(), "{\n  \"fromPath\": \"/a\"\n}");
		assert_eq!(err.lines_or_empty(), "");
	}

	#[test]
	fn omit_for_replay_testing_keeps_the_value_outside_replay_testing() {
		let mut ui = Printed::default().ui();
		ui.initialize(false, false, None, true, false);
		assert_eq!(ui.omit_for_replay_testing("value"), "value");
	}

	#[test]
	fn omit_for_replay_testing_hides_the_value_while_replay_testing() {
		let mut ui = Printed::default().ui();
		ui.initialize(false, false, None, true, true);
		assert_eq!(ui.omit_for_replay_testing("value"), "[omit]");
	}

	#[test]
	fn a_failed_write_of_bytes_is_a_failure() {
		/// A stdout whose every write fails as on a full disk.
		struct StorageFull;

		impl Write for StorageFull {
			fn write(&mut self, _: &[u8]) -> io::Result<usize> {
				Err(io::Error::from(io::ErrorKind::StorageFull))
			}

			fn flush(&mut self) -> io::Result<()> {
				Ok(())
			}
		}

		let mut ui = UI::with_writers(StorageFull, Printed::default());
		let err = ui.write_stdout_bytes(b"content").unwrap_err();
		assert!(matches!(find_cli_error(&err), Some(CliError::Failure(_))));
		assert_eq!(
			format!("{err:#}"),
			format!(
				"Failed to write to stdout: {}",
				io::Error::from(io::ErrorKind::StorageFull)
			)
		);
	}

	#[test]
	fn yes_answers_a_confirmation_without_asking() {
		let mut ui = Printed::default().ui();
		ui.disable_prompts();
		ui.set_assume_yes(true);

		assert!(ui.prompt_confirm("Permanently delete /a?", false).unwrap());
	}

	#[test]
	fn a_confirmation_with_no_input_fails_with_a_usage_error_naming_yes() {
		let mut ui = Printed::default().ui();
		ui.disable_prompts();

		let err = ui
			.prompt_confirm("Permanently delete /a?", false)
			.unwrap_err();
		assert_eq!(
			err.to_string(),
			"Cannot ask \"Permanently delete /a?\": --no-input was given. Pass --yes to answer yes."
		);
		assert_eq!(crate::error::exit_code(&err), 2);
	}

	#[test]
	fn a_prompt_can_be_asked_with_a_terminal_on_stdin() {
		let ui = Printed::default().prompting_ui();

		ui.ensure_can_prompt("Email:").unwrap();
		ui.ensure_can_confirm("Permanently delete /a?").unwrap();
	}

	#[test]
	fn a_confirmation_can_be_answered_by_yes_without_input() {
		let mut ui = Printed::default().ui();
		ui.disable_prompts();
		ui.set_assume_yes(true);

		ui.ensure_can_confirm("Permanently delete /a?").unwrap();
		assert!(ui.ensure_can_prompt("Email:").is_err());
	}

	#[test]
	fn prompts_are_answered_at_the_terminal_on_stdin() {
		assert_eq!(
			PromptInput::detect(true, false, || panic!("stdin is the terminal")),
			PromptInput::Terminal
		);
	}

	#[test]
	fn without_a_terminal_on_stdin_prompts_are_answered_at_the_controlling_terminal() {
		assert_eq!(
			PromptInput::detect(false, true, || true),
			PromptInput::Terminal
		);
	}

	#[test]
	fn without_a_terminal_anywhere_prompts_cannot_be_answered() {
		assert_eq!(
			PromptInput::detect(false, true, || false),
			PromptInput::NotATerminal
		);
	}

	#[test]
	fn prompts_shown_on_a_redirected_stderr_are_not_asked_at_the_controlling_terminal() {
		assert_eq!(
			PromptInput::detect(false, false, || panic!("nobody would see the prompt")),
			PromptInput::NotATerminal
		);
	}

	#[test]
	fn a_confirmation_without_a_terminal_on_stdin_fails_naming_yes() {
		let mut ui = Printed::default().ui();

		let err = ui.prompt_confirm("Delete credentials?", false).unwrap_err();
		assert_eq!(
			err.to_string(),
			"Cannot ask \"Delete credentials?\": stdin is not a terminal. Pass --yes to answer yes."
		);
	}

	#[test]
	fn yes_answers_a_question_about_a_setting_too() {
		let mut ui = Printed::default().ui();
		ui.set_assume_yes(true);

		assert!(ui.prompt_confirm("Enable download?", false).unwrap());
	}

	#[test]
	fn a_text_prompt_with_no_input_fails_even_with_yes() {
		let mut ui = Printed::default().ui();
		ui.disable_prompts();
		ui.set_assume_yes(true);

		for err in [
			ui.prompt("Email:").unwrap_err(),
			ui.prompt_password("Password: ").unwrap_err(),
			ui.prompt_select("Expires after:", vec!["Never".to_owned()])
				.unwrap_err(),
			ui.prompt_choice("Expires after:", &[("Never", 0)], 0)
				.unwrap_err(),
		] {
			assert!(
				err.to_string().ends_with(": --no-input was given."),
				"{err}"
			);
			assert_eq!(crate::error::exit_code(&err), 2);
		}
	}

	#[test]
	fn ctrl_c_or_ctrl_d_on_an_empty_repl_line_leaves_the_repl() {
		assert_eq!(
			ReplPrompt::cancelled(&TypedLine::default()),
			ReplPrompt::Exit
		);
	}

	#[test]
	fn ctrl_c_or_ctrl_d_after_typing_drops_the_line() {
		let typed = TypedLine::default();
		typed.set("ls");
		assert_eq!(ReplPrompt::cancelled(&typed), ReplPrompt::Cleared);
	}

	#[test]
	fn a_line_brought_back_from_the_history_counts_as_typed() {
		let typed = TypedLine::default();
		let mut history = TypedHistory {
			history: SimpleHistory::new(vec!["ls".to_owned()]),
			typed: typed.clone(),
		};

		assert_eq!(history.earlier_element().unwrap().as_deref(), Some("ls"));
		assert_eq!(ReplPrompt::cancelled(&typed), ReplPrompt::Cleared);
		// past the latest line, the prompt is empty again
		assert_eq!(history.later_element().unwrap(), None);
		assert_eq!(ReplPrompt::cancelled(&typed), ReplPrompt::Exit);
	}

	#[test]
	fn truncate_to_width_keeps_short_lines() {
		assert_eq!(truncate_to_width("Uploading", 20), "Uploading");
		assert_eq!(truncate_to_width("Uploading", 9), "Uploading");
	}

	#[test]
	fn truncate_to_width_cuts_long_lines() {
		let truncated = truncate_to_width("Uploading 1 MiB / 2 MiB", 10);
		assert_eq!(truncated, "Uploading…");
		assert_eq!(truncated.width(), 10);
	}

	#[test]
	fn truncate_to_width_handles_wide_chars() {
		// Each of these is two columns wide, so only two of them fit alongside the ellipsis.
		let truncated = truncate_to_width("上传上传上传", 5);
		assert_eq!(truncated, "上传…");
		assert!(truncated.width() <= 5);
	}

	#[test]
	fn truncate_to_width_zero_is_empty() {
		assert_eq!(truncate_to_width("Uploading", 0), "");
	}
}
