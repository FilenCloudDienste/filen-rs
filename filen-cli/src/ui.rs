use anyhow::{Context, Result};
use dialoguer::console::style;
use tiny_gradient::{GradientStr, RGB};

const FILEN_CLI_VERSION: &str = env!("CARGO_PKG_VERSION");

pub(crate) struct UI {
	theme: dialoguer::theme::ColorfulTheme,
	repl_input_theme: dialoguer::theme::ColorfulTheme,
	history: dialoguer::BasicHistory,
}

impl UI {
	pub(crate) fn new() -> Self {
		UI {
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

	pub(crate) fn println(&self, msg: &str) {
		println!("{}", msg);
		// todo: use ui.println() everywhere?
	}
	pub(crate) fn eprintln(&self, msg: &str) {
		eprintln!("{}", msg);
	}

	// print with formatting

	/// Print a message with a success icon
	pub(crate) fn print_success(&self, msg: &str) {
		self.println(&format!("{} {}", style("✔").green(), msg));
	}

	/// Print a message with a failure icon
	pub(crate) fn print_failure(&self, msg: &str) {
		self.eprintln(&format!("{} {}", style("✘").red(), style(msg).dim()));
	}

	/// Print a table of key-value pairs
	pub(crate) fn print_key_value_table(&self, table: &[(&str, &str)]) {
		let key_width = table.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
		for (key, value) in table {
			self.println(&format!("{:>key_width$} {}", style(key).dim(), value));
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
