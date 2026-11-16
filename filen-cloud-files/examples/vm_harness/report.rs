//! The result file every reader subcommand writes, and what they share: waiting for a marker file
//! and reading the provider's events.

use std::{
	error::Error,
	fmt::Display,
	fs::{self, File},
	io::Write,
	path::Path,
	thread,
	time::Duration,
};

use serde_json::Value;

use crate::stall::Budget;

/// What a subcommand returns: whether every check passed, or why the harness itself failed.
pub(crate) type Outcome = Result<bool, Box<dyn Error>>;

/// A result file, written and flushed line by line, so a run that dies leaves what it got to.
pub(crate) struct Report {
	file: File,
	failed: bool,
}

impl Report {
	/// A new report at `path`, replacing any earlier one.
	pub(crate) fn create(path: &Path) -> std::io::Result<Self> {
		Ok(Self {
			file: File::create(path)?,
			failed: false,
		})
	}

	fn line(&mut self, verdict: &str, step: &str, detail: impl Display) {
		let line = format!("{verdict} {step} {detail}");
		println!("{line}");
		if let Err(error) = writeln!(self.file, "{line}").and_then(|()| self.file.flush()) {
			eprintln!("writing the report failed: {error}");
		}
	}

	pub(crate) fn pass(&mut self, step: &str, detail: impl Display) {
		self.line("PASS", step, detail);
	}

	pub(crate) fn fail(&mut self, step: &str, detail: impl Display) {
		self.failed = true;
		self.line("FAIL", step, detail);
	}

	pub(crate) fn info(&mut self, step: &str, detail: impl Display) {
		self.line("INFO", step, detail);
	}

	/// PASS or FAIL `step` by `ok`.
	pub(crate) fn check(&mut self, step: &str, ok: bool, detail: impl Display) {
		if ok {
			self.pass(step, detail);
		} else {
			self.fail(step, detail);
		}
	}

	/// Whether every check passed.
	pub(crate) fn passed(&self) -> bool {
		!self.failed
	}
}

/// Waits up to `limit` for `path` to exist; whether it does.
pub(crate) fn wait_for(path: &Path, limit: Duration) -> bool {
	let mut budget = Budget::new(limit);
	while !path.exists() {
		if budget.spent() {
			return false;
		}
		thread::sleep(Duration::from_millis(250));
	}
	true
}

/// Every event the provider logged to `<out>\events.jsonl` so far.
pub(crate) fn events(out: &Path) -> Vec<Value> {
	fs::read_to_string(out.join("events.jsonl"))
		.unwrap_or_default()
		.lines()
		.filter_map(|line| serde_json::from_str(line).ok())
		.collect()
}

/// The string field `name` of `event`, as the subscriber wrote it.
pub(crate) fn field<'a>(event: &'a Value, name: &str) -> Option<&'a str> {
	event.get(name).and_then(Value::as_str)
}

/// Whether `event` is about the item at `path`, a path below the root: by its `path` field, or by
/// its `item` tag, which the provider's "an item's path" events in `events` tie to its path (its
/// lines at INFO and WARN name an item by its tag alone).
pub(crate) fn names_path(events: &[Value], event: &Value, path: &str) -> bool {
	field(event, "path") == Some(path)
		|| field(event, "item").is_some_and(|tag| {
			events.iter().any(|tagged| {
				field(tagged, "message") == Some("an item's path")
					&& field(tagged, "item") == Some(tag)
					&& field(tagged, "path") == Some(path)
			})
		})
}

/// The integer field `name` of `event`.
pub(crate) fn int(event: &Value, name: &str) -> Option<i64> {
	event.get(name).and_then(Value::as_i64)
}
