use anyhow::{Context, Result};
use dialoguer::console::style;
use tiny_gradient::{GradientStr, RGB};

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
				..Default::default()
			},
			repl_input_theme: Self::repl_input_theme_for_user(None),
			history: dialoguer::BasicHistory::new().no_duplicates(true),
		}
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

	pub(crate) fn println(&self, msg: &str) {
		println!("{}", msg);
		// todo: use ui.println() everywhere?
	}

	pub(crate) fn print_banner(&self) {
		let banner_text = "Filen CLI v0.0.0";
		let width: usize = termsize::get().map(|size| size.cols).unwrap_or(10).into();
		let banner = "=".repeat((width - banner_text.len() - 2) / 2)
			+ " " + banner_text
			+ " " + &"=".repeat((width - banner_text.len() - 2) / 2);
		let filen_blue = RGB::new(0x1d, 0x57, 0xb9);
		let filen_violet = RGB::new(0x99, 0x66, 0xCC);
		let filen_green = RGB::new(0x50, 0xC8, 0x78);
		let banner = banner.gradient([filen_blue, filen_violet, filen_green]);
		// todo: make banner bold
		println!("{}", banner);
	}

	pub(crate) fn prompt_repl(&mut self, path: &str) -> Result<String> {
		let input = dialoguer::Input::with_theme(&self.repl_input_theme)
			.history_with(&mut self.history)
			.with_prompt(path.trim())
			.interact()
			.context("Failed to read input prompt")?;
		Ok(input)
	}

	pub(crate) fn prompt(&mut self, msg: &str) -> Result<String> {
		let input = dialoguer::Input::with_theme(&self.theme)
			.history_with(&mut self.history)
			.with_prompt(msg.trim())
			.interact()
			.context("Failed to read input prompt")?;
		Ok(input)
	}

	pub(crate) fn prompt_password(&mut self, msg: &str) -> Result<String> {
		let password = dialoguer::Password::with_theme(&self.theme)
			.with_prompt(msg.trim())
			.interact()
			.context("Failed to read password input prompt")?;
		Ok(password)
	}

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
