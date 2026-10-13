use console::style;
use unicode_width::UnicodeWidthStr as _;

use crate::ui::{self, UI};

impl UI {
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

	/// Print a view like an `ls` command.
	pub(crate) fn print_item_names(
		&mut self,
		directories: Vec<String>,
		files_with_size: Vec<(String, u64)>,
		empty_message: &str,
		long: bool,
	) {
		let file_names = files_with_size
			.iter()
			.map(|(name, _)| name.clone())
			.collect::<Vec<String>>();
		if directories.is_empty() && files_with_size.is_empty() {
			self.print_muted(empty_message);
			return;
		}
		if long {
			// one item per line, with a right-aligned size column (directories have no size)
			let sizes = files_with_size
				.iter()
				.map(|(_, size)| ui::format_size(*size))
				.collect::<Vec<String>>();
			let size_width = sizes.iter().map(|s| s.len()).max().unwrap_or(0).max(1);
			for name in &directories {
				// pad before styling, so the ANSI codes don't count towards the column width
				let size = format!("{:>size_width$}", "-");
				self.print(&format!("{}  {}", style(size).dim(), style(name).blue()));
			}
			for ((name, _), size) in files_with_size.iter().zip(sizes.iter()) {
				self.print(&format!("{:>size_width$}  {}", size, name));
			}
			return;
		}
		// print directory names in blue
		let directories = directories
			.iter()
			.map(|s| style(s).blue().to_string())
			.collect::<Vec<String>>();
		let all_items = directories
			.iter()
			.chain(file_names.iter())
			.map(|s| s.as_ref())
			.collect::<Vec<&str>>();
		self.print_grid(&all_items);
	}
}

pub(crate) trait PrintEndpointOutput {
	fn print_output(self, ui: &mut UI);
}
