use anyhow::{Context, Result};
use dialoguer::console::style;
use log::{error, info};
use tiny_gradient::{GradientStr, RGB};
use unicode_width::UnicodeWidthStr;

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
		println!(
			"{} {} {} {}",
			formatted_timestamp,
			formatted_level,
			formatted_target,
			record.args()
		);
	}

	fn flush(&self) {}
}

pub(crate) struct UI {
	quiet: bool,
	theme: dialoguer::theme::ColorfulTheme,
	repl_input_theme: dialoguer::theme::ColorfulTheme,
	history: dialoguer::BasicHistory,
}

impl UI {
	pub(crate) fn new(quiet: bool) -> Self {
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

	/// Print a colorful banner at the top of the application (contains app name and version)
	pub(crate) fn print_banner(&self) {
		let banner_text = format!("Filen CLI v{}", FILEN_CLI_VERSION);
		let width: usize = termsize::get().map(|size| size.cols).unwrap_or(10).into();
		let banner = "=".repeat((width - banner_text.len() - 2) / 2)
			+ " " + &banner_text
			+ " " + &"=".repeat((width - banner_text.len() - 2) / 2);
		let filen_blue = RGB::new(0x1d, 0x57, 0xb9);
		let filen_violet = RGB::new(0x99, 0x66, 0xCC);
		let filen_green = RGB::new(0x50, 0xC8, 0x78);
		let banner = banner.gradient([filen_blue, filen_violet, filen_green]);
		// the string outputted by tiny_gradient has many "0m" sequences that reset formatting, so we're applying the bold formatting manually
		let banner = banner.to_string().replace("0m", "1m");
		println!("{}", banner);
	}

	pub(crate) fn print(&self, msg: &str) {
		println!("{}", msg);
		// todo: use ui.println() everywhere?
		info!("[PRINT] {}", msg);
	}
	pub(crate) fn print_hidden(&self, msg: &str) {
		info!("[PRINT] {}", msg);
	}
	pub(crate) fn eprint(&self, msg: &str) {
		eprintln!("{}", msg);
		error!("[PRINT] {}", msg);
	}

	// print with formatting

	/// Print a message with a success icon
	pub(crate) fn print_success(&self, msg: &str) {
		if !self.quiet {
			self.print(&format!("{} {}", style("✔").green(), msg));
		} else {
			self.print_hidden(&format!("{} {}", style("✔").green(), msg));
		}
	}

	/// Print a message with a failure icon
	pub(crate) fn print_failure(&self, msg: &str) {
		self.eprint(&format!("{} {}", style("✘").red(), msg));
	}

	/// Return an error with a user-friendly error message
	pub(crate) fn failure(msg: &str) -> Result<()> {
		Err(anyhow::anyhow!("{} {}", style("✘").red(), msg))
	}

	/// Print an error or failure message
	/// User-friendly failures created with `UI::failure` will be printed as-is,
	/// other errors will be printed with a generic message and a link to report bugs.
	pub(crate) fn print_failure_or_error(&self, err: &anyhow::Error) {
		let err_msg = format!("{}", err);
		let is_failure = err_msg.starts_with(&format!("{}", style("✘").red()));
		if is_failure {
			self.eprint(&err_msg);
		} else {
			self.print_failure(&format!("An unexpected error occurred: {}", err));
			self.print_failure("If you believe this is a bug, please report it at https://github.com/FilenCloudDienste/filen-rs/issues");
		}
	}

	/// Print an error with a failure icon
	pub(crate) fn print_err(&self, err: &anyhow::Error) {
		self.print_failure(&format!("{}", err));
	}

	pub(crate) fn print_muted(&self, msg: &str) {
		self.print(&style(msg).dim().to_string());
	}

	/// Print a table of key-value pairs
	pub(crate) fn print_key_value_table(&self, table: &[(&str, &str)]) {
		let key_width = table.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
		for (key, value) in table {
			self.print(&format!("{:>key_width$} {}", style(key).dim(), value));
		}
	}

	/// Print a grid of strings (like an ls command's output)
	pub(crate) fn print_grid(&self, items: &[&str]) {
		if items.is_empty() {
			self.print("");
			return;
		}
		let terminal_width = termsize::get().map(|size| size.cols).unwrap_or(10) as usize;
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
}

pub(crate) fn format_date(date: &chrono::DateTime<chrono::Utc>) -> String {
	date.format("%Y-%m-%d %H:%M:%S (UTC)").to_string()
}

pub(crate) fn format_size(size: u64) -> String {
	humansize::format_size(size, humansize::BINARY)
}
