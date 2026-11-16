//! A tracing subscriber writing every event at DEBUG or above from this crate or the harness as
//! one JSON object per line, flushed per line, so readers in other processes can follow it.
//! Hand-written: the workspace has no subscriber crate this package may depend on.
//!
//! A thread of its own writes the lines, and echoes those at INFO or above on stderr: a write the
//! guest's disk holds up (seconds at a time in a loaded VM, see `stall`) then holds up no callback
//! or worker that logs.

use std::{
	fmt,
	fs::File,
	io::{self, BufWriter, Write},
	path::Path,
	sync::{OnceLock, mpsc},
	thread,
	time::Instant,
};

use serde_json::{Map, Value};
use tracing::{
	Event, Level, Metadata, Subscriber,
	field::{Field, Visit},
	level_filters::LevelFilter,
	span,
};

/// What the writer thread is handed.
enum Line {
	/// A line to write, and to echo on stderr too when `echo`.
	Event { line: String, echo: bool },
	/// Answered once every line handed over before is written.
	Flush(mpsc::SyncSender<()>),
}

/// The writer thread's queue, for [`flush`].
static WRITER: OnceLock<mpsc::Sender<Line>> = OnceLock::new();

/// The JSON-lines subscriber.
struct JsonLines {
	out: mpsc::Sender<Line>,
	start: Instant,
}

/// An event's fields, as JSON.
struct Fields(Map<String, Value>);

impl Visit for Fields {
	fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
		self.0
			.insert(field.name().to_owned(), Value::String(format!("{value:?}")));
	}

	fn record_str(&mut self, field: &Field, value: &str) {
		self.0
			.insert(field.name().to_owned(), Value::String(value.to_owned()));
	}

	fn record_i64(&mut self, field: &Field, value: i64) {
		self.0.insert(field.name().to_owned(), Value::from(value));
	}

	fn record_u64(&mut self, field: &Field, value: u64) {
		self.0.insert(field.name().to_owned(), Value::from(value));
	}

	fn record_bool(&mut self, field: &Field, value: bool) {
		self.0.insert(field.name().to_owned(), Value::Bool(value));
	}
}

impl Subscriber for JsonLines {
	fn enabled(&self, metadata: &Metadata<'_>) -> bool {
		*metadata.level() <= Level::DEBUG
			&& (metadata.target().starts_with("filen_cloud_files")
				|| metadata.target().starts_with("vm_harness"))
	}

	fn max_level_hint(&self) -> Option<LevelFilter> {
		Some(LevelFilter::DEBUG)
	}

	// Spans carry nothing the harness reads: every span is the same one.
	fn new_span(&self, _: &span::Attributes<'_>) -> span::Id {
		span::Id::from_u64(1)
	}

	fn record(&self, _: &span::Id, _: &span::Record<'_>) {}

	fn record_follows_from(&self, _: &span::Id, _: &span::Id) {}

	fn event(&self, event: &Event<'_>) {
		let metadata = event.metadata();
		let mut fields = Fields(Map::new());
		event.record(&mut fields);
		let mut line = fields.0;
		line.insert(
			"t_ms".to_owned(),
			Value::from(u64::try_from(self.start.elapsed().as_millis()).unwrap_or(u64::MAX)),
		);
		line.insert(
			"level".to_owned(),
			Value::String(metadata.level().to_string()),
		);
		line.insert(
			"target".to_owned(),
			Value::String(metadata.target().to_owned()),
		);
		let line = Value::Object(line).to_string();
		let echo = *metadata.level() <= Level::INFO;
		if self.out.send(Line::Event { line, echo }).is_err() {
			eprintln!("the event writer is gone");
		}
	}

	fn enter(&self, _: &span::Id) {}

	fn exit(&self, _: &span::Id) {}
}

/// Sends every event of the process to `path`, replacing any earlier file.
pub(crate) fn install(path: &Path) -> io::Result<()> {
	let mut file = BufWriter::new(File::create(path)?);
	let (out, lines) = mpsc::channel();
	thread::Builder::new()
		.name("event-writer".to_owned())
		.spawn(move || {
			for line in lines {
				match line {
					Line::Event { line, echo } => {
						if echo {
							eprintln!("{line}");
						}
						if let Err(error) = writeln!(file, "{line}").and_then(|()| file.flush()) {
							eprintln!("writing an event failed: {error}");
						}
					}
					Line::Flush(done) => {
						// The flusher may have stopped waiting.
						let _ = done.send(());
					}
				}
			}
		})?;
	// Installed once per process: a second install fails below as the subscriber's does.
	let _ = WRITER.set(out.clone());
	let subscriber = JsonLines {
		out,
		start: Instant::now(),
	};
	tracing::subscriber::set_global_default(subscriber).map_err(io::Error::other)
}

/// Waits until every event logged so far is in the file, before a marker tells the readers the
/// run got this far.
pub(crate) fn flush() {
	let Some(writer) = WRITER.get() else {
		return;
	};
	let (done, written) = mpsc::sync_channel(1);
	if writer.send(Line::Flush(done)).is_ok() {
		// Nothing is lost when the writer is gone: the line says so on stderr.
		let _ = written.recv();
	}
}
