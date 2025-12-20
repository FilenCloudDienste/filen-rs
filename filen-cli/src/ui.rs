use anyhow::{Context, Result};
use clap::{CommandFactory, builder::Styles};
use dialoguer::console::style;
use log::{error, info};
use tiny_gradient::{GradientStr, RGB};
use unicode_width::UnicodeWidthStr;

use crate::CliArgs;

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

pub(crate) struct UI {
	quiet: bool,
	theme: dialoguer::theme::ColorfulTheme,
	repl_input_theme: dialoguer::theme::ColorfulTheme,
	history: dialoguer::BasicHistory,
	overwrite_terminal_width: Option<usize>,
	output: Vec<String>,
}

impl UI {
	pub(crate) fn new(quiet: bool, overwrite_terminal_width: Option<usize>) -> Self {
		UI {
			quiet,
			theme: dialoguer::theme::ColorfulTheme {
				prompt_prefix: style("›".to_string()).cyan().bold(),
				prompt_suffix: style("›".to_string()).dim().bold(),
				success_prefix: style("›".to_string()).dim().bold(),
				..Default::default()
			},
			repl_input_theme: Self::repl_input_theme_for_user(None),
			history: dialoguer::BasicHistory::new().no_duplicates(true),
			overwrite_terminal_width,
			output: Vec::new(),
		}
	}

	// todo: is this necessary anymore?
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
		let banner_text = format!("Filen CLI v{}", FILEN_CLI_VERSION);
		let width = match self.get_terminal_width() {
			Some(w) if w > banner_text.len() + 2 => w,
			_ => banner_text.len() + 6,
		};
		let banner = "=".repeat((width - banner_text.len() - 2) / 2)
			+ " " + &banner_text
			+ " " + &"=".repeat((width - banner_text.len() - 2) / 2);
		let filen_blue = RGB::new(0x1d, 0x57, 0xb9);
		let filen_violet = RGB::new(0x99, 0x66, 0xCC);
		let filen_green = RGB::new(0x50, 0xC8, 0x78);
		let banner = banner.gradient([filen_blue, filen_violet, filen_green]);
		// the string outputted by tiny_gradient has many "0m" sequences that reset formatting, so we're applying the bold formatting manually
		let banner = banner.to_string().replace("0m", "1m");
		self.print(&banner);
	}

	pub(crate) fn print(&mut self, msg: &str) {
		println!("{}", msg);
		self.output.push(msg.to_string());
		// todo: use ui.println() everywhere?
		info!("[PRINT] {}", msg);
	}
	pub(crate) fn print_hidden(&self, msg: &str) {
		info!("[PRINT] {}", msg);
	}

	// print with formatting

	/// Print a message with a success icon
	pub(crate) fn print_success(&mut self, msg: &str) {
		if !self.quiet {
			self.print(&format!("{} {}", style("✔").green(), msg));
		} else {
			self.print_hidden(&format!("{} {}", style("✔").green(), msg));
		}
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
		let is_failure = err_msg.starts_with(&format!("{}", style("✘").red()));
		if is_failure {
			self.print(&err_msg);
		} else {
			self.print_failure(&format!("An unexpected error occurred: {}", err));
			self.print_failure("If you believe this is a bug, please report it at https://github.com/FilenCloudDienste/filen-rs/issues");
		}
	}

	/// Print an error with a failure icon
	pub(crate) fn print_err(&mut self, err: &anyhow::Error) {
		self.print_failure(&format!("{}", err));
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

	// prompt

	/// Prompt the user for input in the REPL (contains some special formatting)
	pub(crate) fn prompt_repl(&mut self, path: &str) -> Result<String> {
		let input = dialoguer::Input::with_theme(&self.repl_input_theme)
			.history_with(&mut self.history)
			.with_prompt(path.trim())
			.interact()
			.context("Failed to read input prompt")?;
		Ok(input)
	}

	/// Prompt the user for text input
	pub(crate) fn prompt(&mut self, msg: &str) -> Result<String> {
		let input = dialoguer::Input::with_theme(&self.theme)
			.history_with(&mut self.history)
			.with_prompt(msg.trim())
			.interact()
			.context("Failed to read input prompt")?;
		Ok(input)
	}

	/// Prompt the user for a password (no echo)
	pub(crate) fn prompt_password(&mut self, msg: &str) -> Result<String> {
		let password = dialoguer::Password::with_theme(&self.theme)
			.with_prompt(msg.trim())
			.interact()
			.context("Failed to read password input prompt")?;
		Ok(password)
	}

	/// Prompt the user for a yes/no input
	pub(crate) fn prompt_confirm(&mut self, msg: &str, default: bool) -> Result<bool> {
		let result = dialoguer::Confirm::with_theme(&self.theme)
			.with_prompt(msg.trim())
			.default(default)
			.report(true)
			.interact()
			.context("Failed to read confirmation prompt")?;
		Ok(result)
	}

	// format help text

	pub(crate) fn format_command_help(cmd: &mut clap::Command) -> String {
		let styled_str = cmd
			.clone()
			.styles(
				Styles::styled()
					.literal(anstyle::AnsiColor::Green.on_default())
					.placeholder(anstyle::AnsiColor::Green.on_default())
					.context(anstyle::Style::new().dimmed())
					.header(anstyle::AnsiColor::Yellow.on_default()),
			)
			.help_template("{positionals}\n{options}")
			.render_help();
		let formatted_usage = cmd
			.clone()
			.styles(Styles::styled().literal(anstyle::AnsiColor::Green.on_default().underline()))
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
			.styles(
				Styles::styled()
					.literal(anstyle::AnsiColor::Green.on_default())
					.placeholder(anstyle::AnsiColor::Green.on_default())
					.context(anstyle::Style::new().dimmed())
					.header(anstyle::AnsiColor::Yellow.on_default()),
			)
			.help_template("{options}")
			.render_help()
			.ansi()
			.to_string()
			.lines()
			.filter(|l| !l.is_empty())
			.map(|l| {
				// if line contains an ansi code, it's the definition line
				if l.trim().contains("[") {
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

#[cfg(test)]
mod tests {
	use super::*;

	// run using: cargo insta test --review -- --test test_ui

	#[test]
	#[ignore = "fails in ci for platforms reasons, will fix later"] // todo: fix
	fn test_ui() {
		let test = |ui: &mut UI| {
			ui.print_banner();
			ui.print("Should just show");
			ui.print_hidden("Shouldn't show");
			ui.print_success("Success!");
			ui.print_failure("Something went wrong");
			ui.print_err(&anyhow::anyhow!("This is an error"));
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
		};

		// for different terminal sizes
		let mut small_ui = UI::new(false, Some(30));
		test(&mut small_ui);
		insta::assert_snapshot!(small_ui.output.join("\n"));
		let mut large_ui = UI::new(false, Some(100));
		test(&mut large_ui);
		insta::assert_snapshot!(large_ui.output.join("\n"));
	}
}
