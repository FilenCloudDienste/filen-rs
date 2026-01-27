use std::{ffi::OsString, sync::Arc};

use anyhow::{Context, Result};
use clap::{CommandFactory, builder::Styles};
use dialoguer::console::{self, style};
use filen_sdk_rs::auth::Client;
use log::{error, info, warn};
use tiny_gradient::{GradientStr, RGB};
use unicode_width::UnicodeWidthStr;

use crate::{CliArgs, EXIT_CODE_ERROR_PREFIX, custom_arg_values::FilenCompleter, util::RemotePath};

const FILEN_CLI_VERSION: &str = env!("CARGO_PKG_VERSION");

pub(crate) struct CustomLogger {
	pub(crate) config: ftail::Config,
}

impl log::Log for CustomLogger {
	fn enabled(&self, metadata: &log::Metadata) -> bool {
		metadata.level() <= self.config.level_filter
	}

	fn log(&self, record: &log::Record) {
		if !self.enabled(record.metadata()) {
			return;
		}
		let now = chrono::Local::now().format(&self.config.datetime_format);
		let formatted_timestamp = style(format!("[{}]", now)).dim();
		let formatted_level = match record.level() {
			log::Level::Error => style("[ERROR]").red(),
			log::Level::Warn => style("[WARN] ").yellow(),
			log::Level::Info => style("[INFO] ").green(),
			log::Level::Debug => style("[DEBUG]").blue(),
			log::Level::Trace => style("[TRACE]").dim(),
		};
		let formatted_target = style(format!("({})", record.target())).dim();
		let msg = record.args().to_string();
		if !msg.starts_with("[PRINT]") {
			println!(
				"{} {} {} {}",
				formatted_timestamp, formatted_level, formatted_target, msg
			);
		}
	}

	fn flush(&self) {}
}

const FAILED_TO_READ_INPUT_PROMPT: &str = "Failed to read input prompt";

pub(crate) struct UI {
	quiet: bool,
	theme: dialoguer::theme::ColorfulTheme,
	repl_input_theme: dialoguer::theme::ColorfulTheme,
	history: dialoguer::BasicHistory,
	overwrite_terminal_width: Option<usize>,
	output: Vec<String>,

	/// Whether to output machine-readable JSON where applicable
	pub(crate) json: bool,
}

impl UI {
	pub(crate) fn new() -> Self {
		UI {
			quiet: false,
			theme: dialoguer::theme::ColorfulTheme {
				prompt_prefix: style("›".to_string()).cyan().bold(),
				prompt_suffix: style("›".to_string()).dim().bold(),
				success_prefix: style("›".to_string()).dim().bold(),
				..Default::default()
			},
			repl_input_theme: Self::repl_input_theme_for_user(None),
			history: dialoguer::BasicHistory::new().no_duplicates(true),
			overwrite_terminal_width: None,
			output: Vec::new(),
			json: false,
		}
	}

	pub(crate) fn initialize(
		&mut self,
		quiet: bool,
		json: bool,
		overwrite_terminal_width: Option<usize>,
	) {
		self.quiet = quiet;
		self.json = json;
		self.overwrite_terminal_width = overwrite_terminal_width;
	}

	pub(crate) fn set_user(&mut self, user: Option<&str>) {
		self.repl_input_theme = Self::repl_input_theme_for_user(user);
	}
	fn repl_input_theme_for_user(user: Option<&str>) -> dialoguer::theme::ColorfulTheme {
		let prefix = if let Some(user) = user {
			style(format!("({})", user)).cyan().bold()
		} else {
			style("(...)".to_string()).dim().bold()
		};
		let suffix = style("›".to_string()).dim();
		dialoguer::theme::ColorfulTheme {
			prompt_prefix: prefix.clone(),
			success_prefix: prefix,
			prompt_suffix: suffix.clone(),
			success_suffix: suffix,
			..Default::default()
		}
	}

	fn get_terminal_width(&self) -> Option<usize> {
		match self.overwrite_terminal_width {
			Some(size) => Some(size),
			None => termsize::get().map(|size| size.cols as usize),
		}
	}

	/// Print a colorful banner at the top of the application (contains app name and version)
	pub(crate) fn print_banner(&mut self) {
		self.print_banner_(FILEN_CLI_VERSION);
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

	pub(crate) fn print(&mut self, msg: &str) {
		println!("{}", msg);
		self.output.push(msg.to_string());
		info!("[PRINT] {}", msg);
	}
	pub(crate) fn print_hidden(&self, msg: &str) {
		info!("[PRINT] {}", msg);
	}

	// print with formatting

	/// Print an announcement message (used for important info from updates)
	pub(crate) fn print_announcement(&mut self, msg: &str) {
		warn!("[ANNOUNCEMENT] {}", msg);
		self.print(&format!(
			"{} {}",
			style("[i]").yellow().bold(),
			style(msg).yellow()
		));
	}

	/// Print a message with a success icon
	pub(crate) fn print_success(&mut self, msg: &str) {
		if !self.quiet {
			self.print(&format!("{} {}", style("✔").green(), msg));
		} else {
			self.print_hidden(&format!("{} {}", style("✔").green(), msg));
		}
	}

	/// Print a message with a warning icon
	pub(crate) fn print_warning(&mut self, msg: &str) {
		self.print(&format!("{} {}", style("⚠").yellow(), style(msg).yellow()));
	}

	/// Print a message with a failure icon
	pub(crate) fn print_failure(&mut self, msg: &str) {
		self.print(&format!("{} {}", style("✘").red(), msg));
	}

	/// Return an error with a user-friendly error message
	pub(crate) fn failure(msg: &str) -> anyhow::Error {
		anyhow::anyhow!("{} {}", style("✘").red(), msg)
	}

	/// Print an error or failure message
	/// User-friendly failures created with `UI::failure` will be printed as-is,
	/// other errors will be printed with a generic message and a link to report bugs.
	pub(crate) fn print_failure_or_error(&mut self, err: &anyhow::Error) {
		error!("{:#}", err);
		let err_msg = format!("{}", err);
		if err_msg.starts_with(EXIT_CODE_ERROR_PREFIX) {
			return;
		}
		let is_failure = err_msg.starts_with(&format!("{}", style("✘").red()));
		if is_failure {
			self.print(&err_msg);
		} else if err_msg.contains(FAILED_TO_READ_INPUT_PROMPT) {
			self.print_failure("Failed to read input from terminal. Please ensure that the terminal supports interactive input.");
			if cfg!(feature = "is_docker") {
				self.print_failure("It seems you are running the CLI in a Docker container. Make sure to run the container with the -it flags to enable interactive input.");
			}
		} else {
			self.print_failure(&format!("An unexpected error occurred: {}", err));
			self.print_failure("If you believe this is a bug, please report it at https://github.com/FilenCloudDienste/filen-rs/issues");
		}
	}

	pub(crate) fn print_muted(&mut self, msg: &str) {
		self.print(&style(msg).dim().to_string());
	}

	/// Print a table of key-value pairs
	pub(crate) fn print_key_value_table(&mut self, table: &[(&str, &str)]) {
		let key_width = table.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
		for (key, value) in table {
			self.print(&format!("{:>key_width$} {}", style(key).dim(), value));
		}
	}

	/// Print a grid of strings (like an ls command's output)
	pub(crate) fn print_grid(&mut self, items: &[&str]) {
		if items.is_empty() {
			self.print("");
			return;
		}
		let terminal_width = self.get_terminal_width().unwrap_or(10);
		let min_text_width = items.iter().map(|item| item.width()).max().unwrap_or(0);
		if min_text_width > terminal_width {
			// it won't fit, so just print one per line
			self.print(&items.join("\n"));
			return;
		}
		// try different column heights
		for column_height in 1.. {
			let columns = items.chunks(column_height).collect::<Vec<_>>();
			let num_columns = columns.len();
			let column_widths = columns
				.clone()
				.iter()
				.map(|column| {
					column
						.iter()
						.map(|item| ansi_width::ansi_width(item))
						.max()
						.unwrap()
				})
				.collect::<Vec<_>>();
			let spacing_width = (num_columns - 1) * 2;
			// check if it fits
			let total_text_width = column_widths.iter().sum::<usize>();
			if total_text_width + spacing_width <= terminal_width {
				for row in 0..column_height {
					let items = (0..columns.len())
						.map(|i| {
							let cell = columns[i].get(row).unwrap_or(&"");
							let padding =
								" ".repeat(column_widths[i] - ansi_width::ansi_width(cell));
							format!("{}{}", cell, padding)
						})
						.collect::<Vec<_>>();
					self.print(&items.join("  "));
				}
				break;
			}
		}
	}

	pub(crate) fn print_json(&mut self, value: serde_json::Value) -> Result<()> {
		self.print(&serde_json::to_string_pretty(&value).context("Failed to serialize JSON")?);
		Ok(())
	}

	// prompt

	/// Prompt the user for input in the REPL (contains some special formatting)
	pub(crate) fn prompt_repl(
		&mut self,
		client: Arc<Client>,
		working_path: &RemotePath,
	) -> Result<String> {
		dialoguer::Input::with_theme(&self.repl_input_theme)
			.history_with(&mut self.history)
			.completion_with(&DialoguerCompleter {
				client,
				working_path,
			})
			.with_prompt(working_path.to_string())
			.interact_text()
			.context(FAILED_TO_READ_INPUT_PROMPT)
		// todo: terminal has weird graphical glitches when using the completer
	}

	// for DialoguerCompleter
	fn completer(
		input: &str,
		client: Arc<Client>,
		working_path: &RemotePath,
	) -> Result<Option<String>> {
		let args = shlex::split(input)
			.context("Invalid quoting")?
			.iter()
			.map(|str| str.into())
			.collect::<Vec<OsString>>();
		if args.is_empty() {
			return Ok(None);
		}
		let args_index = args.len();
		let mut cli = CliArgs::command();
		cli = FilenCompleter::initialize_completers_in_command(cli, client, working_path);
		match clap_complete::engine::complete(
			&mut cli,
			vec!["filen"]
				.into_iter()
				.map(OsString::from)
				.chain(args.clone())
				.collect(),
			args_index,
			std::env::current_dir().ok().as_deref(),
		) {
			Ok(candidates) => Ok(candidates.first().and_then(|candidate| {
				let completion = candidate.get_value().to_string_lossy().to_string();
				let replace_word = args.last().unwrap();
				input
					.strip_suffix(replace_word.to_str().unwrap())
					.map(|prefix| format!("{}{}", prefix, completion))
			})),
			Err(_) => Ok(None),
		}
	}

	/// Prompt the user for text input
	pub(crate) fn prompt(&mut self, msg: &str) -> Result<String> {
		dialoguer::Input::with_theme(&self.theme)
			.history_with(&mut self.history)
			.with_prompt(msg.trim())
			.interact_text()
			.context(FAILED_TO_READ_INPUT_PROMPT)
	}

	/// Prompt the user for a password (no echo)
	pub(crate) fn prompt_password(&mut self, msg: &str) -> Result<String> {
		dialoguer::Password::with_theme(&self.theme)
			.with_prompt(msg.trim())
			.interact()
			.context(FAILED_TO_READ_INPUT_PROMPT)
	}

	/// Prompt the user for a yes/no input
	pub(crate) fn prompt_confirm(&mut self, msg: &str, default: bool) -> Result<bool> {
		dialoguer::Confirm::with_theme(&self.theme)
			.with_prompt(msg.trim())
			.default(default)
			.report(true)
			.interact()
			.context(FAILED_TO_READ_INPUT_PROMPT)
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
			.render_help();
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
				// if line contains an ansi code, it's the definition line
				if l.trim().contains("[") || l.trim().starts_with("-") {
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

pub(crate) fn format_date(date: &chrono::DateTime<chrono::Utc>) -> String {
	date.format("%Y-%m-%d %H:%M:%S (UTC)").to_string()
}

pub(crate) fn format_size(size: u64) -> String {
	humansize::format_size(size, humansize::BINARY)
}

struct DialoguerCompleter<'a> {
	client: Arc<Client>,
	working_path: &'a RemotePath,
}

impl dialoguer::Completion for DialoguerCompleter<'_> {
	fn get(&self, input: &str) -> Option<String> {
		UI::completer(input, self.client.clone(), self.working_path)
			.ok()
			.flatten()
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	// run using: cargo insta test --review -- --test test_ui

	#[test]
	fn test_ui() {
		console::set_colors_enabled(true); // even in CI

		fn test(ui: &mut UI) {
			ui.print_banner_("0.0.0-test");
			ui.print("Should just show");
			ui.print_hidden("Shouldn't show");
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

		// different terminal sizes
		let mut small_ui = UI::new();
		small_ui.initialize(false, false, Some(30));
		test(&mut small_ui);
		insta::assert_snapshot!(small_ui.output.join("\n"));
		let mut large_ui = UI::new();
		large_ui.initialize(false, false, Some(100));
		test(&mut large_ui);
		insta::assert_snapshot!(large_ui.output.join("\n"));

		// no color
		console::set_colors_enabled(false);
		let mut no_color_ui = UI::new();
		no_color_ui.initialize(false, false, Some(100));
		test(&mut no_color_ui);
		insta::assert_snapshot!(no_color_ui.output.join("\n"));
	}
}
