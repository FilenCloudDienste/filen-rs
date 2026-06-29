//! Observability: the `tracing` subscriber stack for the SDK.
//!
//! This module owns the *default* `tracing` subscriber and the runtime log-level control.
//! Design decisions (locked):
//!
//! - **Init model — SDK default, host can override.** [`try_init`] installs a per-target
//!   subscriber via `tracing`'s `try_init`, which is a no-op if a global subscriber is
//!   already set. A host that wants SDK logs routed into its own pipeline (Timber,
//!   Crashlytics, os_log) installs its subscriber *first* and [`try_init`] defers to it.
//!   This also collapses the two historical init sites into one and removes the
//!   competing-global-logger panic hazard.
//!
//! - **Filtering — global `EnvFilter` + runtime reload.** A single process-wide
//!   [`EnvFilter`] (seeded from a [`LogLevel`], honouring `RUST_LOG` where an env exists)
//!   is wrapped in a [`reload`] layer. [`set_log_level`] flips verbosity live via the
//!   stored [`reload::Handle`] — e.g. a "verbose logging" toggle on a device.
//!
//! - **Sinks (per target).** native desktop → `fmt` to stdout · iOS → `tracing-oslog` ·
//!   Android → our own [`LogcatMakeWriter`](android) over the NDK `__android_log_write`
//!   (no unmaintained crate) · wasm → `wasm-tracing` (console + Performance API; avoids
//!   `fmt`'s `Instant::now()` panic on `wasm32-unknown-unknown`). On native targets,
//!   `tracing-log`'s `LogTracer` captures `log` records emitted by dependencies
//!   (reqwest, tokio-tungstenite) into the same pipeline.
//!
//! - **Timing + hang signal.** The `fmt` layer emits span busy/idle on close
//!   (`FmtSpan::CLOSE`) on native/Android. The [`inflight`] layer additionally tracks
//!   every open span so a periodic driver can `warn!` about operations that have been
//!   running too long — the forward-looking "still running after Ns" signal that lands in
//!   logcat. (Not compiled on wasm: no runtime hangs to watch there, and `Instant` traps.)

use std::sync::OnceLock;

use tracing_subscriber::{EnvFilter, Registry, filter::LevelFilter, reload};

use crate::auth::http::LogLevel;

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
pub use inflight::{inflight_count, warn_stale_operations};

/// Handle to the reloadable global filter, populated iff *we* installed the subscriber.
static RELOAD_HANDLE: OnceLock<reload::Handle<EnvFilter, Registry>> = OnceLock::new();

fn level_filter(level: LogLevel) -> LevelFilter {
	match level {
		LogLevel::Off => LevelFilter::OFF,
		LogLevel::Error => LevelFilter::ERROR,
		LogLevel::Warn => LevelFilter::WARN,
		LogLevel::Info => LevelFilter::INFO,
		LogLevel::Debug => LevelFilter::DEBUG,
		LogLevel::Trace => LevelFilter::TRACE,
	}
}

fn build_filter(level: LogLevel) -> EnvFilter {
	// Scope the requested level to first-party crates and keep everything else (reqwest, hyper,
	// tokio-tungstenite, ...) at a quiet WARN floor. Raising SDK verbosity to Debug/Trace must not
	// escalate third-party HTTP-stack logs — whose request URLs/headers can carry the auth bearer
	// token — into the device log sink. RUST_LOG still overrides everything where an env exists
	// (native dev); `parse_lossy` never panics on a malformed directive or a target without an env.
	let lvl = level_filter(level);
	let directives = std::env::var("RUST_LOG").unwrap_or_else(|_| {
		format!("warn,filen_sdk_rs={lvl},filen_mobile_native_cache={lvl},filen_types={lvl}")
	});
	EnvFilter::builder()
		.with_default_directive(LevelFilter::WARN.into())
		.parse_lossy(directives)
}

/// Install the SDK's default `tracing` subscriber for this process.
///
/// Returns `true` if this call installed the subscriber, `false` if a global subscriber was
/// already set (i.e. a host installed its own first, or this was called twice) — in which
/// case nothing is changed and [`set_log_level`] will be inert.
pub fn try_init(default_level: LogLevel) -> bool {
	let (filter_layer, reload_handle) = reload::Layer::new(build_filter(default_level));
	let base = {
		use tracing_subscriber::layer::SubscriberExt as _;
		tracing_subscriber::registry().with(filter_layer)
	};

	let installed = build_and_init(base);
	if installed {
		let _ = RELOAD_HANDLE.set(reload_handle);
		// Capture `log` records from dependencies into tracing. No-op error if a `log`
		// logger is already installed. Not available on wasm.
		#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
		{
			let _ = tracing_log::LogTracer::init();
			// Start the hang watchdog if we are already on a runtime; otherwise a runtime-setup
			// site (e.g. the cache's get_runtime) calls spawn_inflight_watchdog itself.
			spawn_inflight_watchdog();
		}
	}
	installed
}

/// How often the watchdog scans for stalled operations, and how long an operation may run
/// before it is reported.
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
const WATCHDOG_CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(15);
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
const WATCHDOG_WARN_AFTER: std::time::Duration = std::time::Duration::from_secs(30);

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
static WATCHDOG_SPAWNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Periodically report operations that have been in flight too long (the forward-looking
/// "still running after Ns" hang signal). Runs forever; spawn it on a long-lived runtime via
/// [`spawn_inflight_watchdog`].
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
pub async fn run_inflight_watchdog() {
	loop {
		tokio::time::sleep(WATCHDOG_CHECK_INTERVAL).await;
		warn_stale_operations(WATCHDOG_WARN_AFTER);
	}
}

/// Spawn [`run_inflight_watchdog`] on the current tokio runtime, at most once per process. A
/// no-op (leaving it for a later call) when invoked outside a runtime, so it is safe to call
/// from both [`try_init`] and runtime-setup code.
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
pub fn spawn_inflight_watchdog() {
	use std::sync::atomic::Ordering;
	if tokio::runtime::Handle::try_current().is_err() {
		return;
	}
	if !WATCHDOG_SPAWNED.swap(true, Ordering::Relaxed) {
		tokio::spawn(run_inflight_watchdog());
	}
}

/// Adjust the global log level at runtime. Returns `false` if [`try_init`] never installed
/// the subscriber (so there is no reload handle to drive).
pub fn set_log_level(level: LogLevel) -> bool {
	let Some(handle) = RELOAD_HANDLE.get() else {
		return false;
	};
	handle.reload(build_filter(level)).is_ok()
}

/// Attach the per-target sink (and, off-wasm, the in-flight tracker) and install the result
/// as the global default. Generic over the concrete `Layered<…>` type so we never have to
/// name it; exactly one `cfg` branch compiles per target.
fn build_and_init<S>(base: S) -> bool
where
	S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a> + Send + Sync,
{
	use tracing_subscriber::layer::SubscriberExt as _;
	use tracing_subscriber::util::SubscriberInitExt as _;

	// Native desktop / server: fmt to stdout with busy/idle timing on span close.
	#[cfg(all(
		not(all(target_family = "wasm", target_os = "unknown")),
		not(target_os = "android"),
		not(target_os = "ios")
	))]
	{
		base.with(
			tracing_subscriber::fmt::layer()
				.with_ansi(false)
				.with_span_events(tracing_subscriber::fmt::format::FmtSpan::CLOSE),
		)
		.with(inflight::InflightLayer)
		.try_init()
		.is_ok()
	}

	// Android: fmt rendered into logcat via our NDK MakeWriter, with busy/idle timing.
	#[cfg(target_os = "android")]
	{
		base.with(
			tracing_subscriber::fmt::layer()
				.with_ansi(false)
				.with_writer(android::LogcatMakeWriter)
				.with_span_events(tracing_subscriber::fmt::format::FmtSpan::CLOSE),
		)
		.with(inflight::InflightLayer)
		.try_init()
		.is_ok()
	}

	// iOS: Apple unified logging (os_log) via tracing-oslog.
	#[cfg(target_os = "ios")]
	{
		base.with(tracing_oslog::OsLogger::new(
			"io.filen.filen-sdk-rs",
			"default",
		))
		.with(inflight::InflightLayer)
		.try_init()
		.is_ok()
	}

	// wasm: console + Performance API. No in-flight watchdog (no runtime hangs; Instant traps).
	#[cfg(all(target_family = "wasm", target_os = "unknown"))]
	{
		base.with(wasm_tracing::WasmLayer::default())
			.try_init()
			.is_ok()
	}
}

/// In-flight operation tracking: the forward-looking "still running after Ns" hang signal.
///
/// A [`tracing::Span`] is recorded here on creation and removed on close, so the set of
/// currently-open spans is enumerable. [`warn_stale_operations`] emits a one-shot `warn!`
/// per operation that has outlived a threshold; a periodic driver (added with the runtime
/// wiring) calls it. Split out from any timer so this module needs no async runtime.
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
mod inflight {
	use std::collections::HashMap;
	use std::fmt::Write as _;
	use std::sync::{LazyLock, Mutex};
	use std::time::{Duration, Instant};

	use tracing::field::{Field, Visit};
	use tracing::span::{Attributes, Id};
	use tracing_subscriber::layer::{Context, Layer};
	use tracing_subscriber::registry::LookupSpan;

	struct Record {
		started: Instant,
		target: String,
		name: &'static str,
		fields: String,
		warned: bool,
	}

	static INFLIGHT: LazyLock<Mutex<HashMap<u64, Record>>> =
		LazyLock::new(|| Mutex::new(HashMap::new()));

	/// Collects a compact `key=value ` summary of a span's fields (capped, for the warning).
	struct FieldVisitor(String);

	impl Visit for FieldVisitor {
		fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
			// Soft cap: once over the budget we stop accepting new fields, but the field that
			// crosses it is written in full, so the buffer can exceed FIELD_BUDGET by one field's
			// length. Approximate by design — a hard truncate could split a UTF-8 boundary.
			const FIELD_BUDGET: usize = 200;
			if self.0.len() < FIELD_BUDGET {
				let _ = write!(self.0, "{}={:?} ", field.name(), value);
			}
		}
	}

	pub(super) struct InflightLayer;

	impl<S> Layer<S> for InflightLayer
	where
		S: tracing::Subscriber + for<'a> LookupSpan<'a>,
	{
		fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, _ctx: Context<'_, S>) {
			let meta = attrs.metadata();
			let mut visitor = FieldVisitor(String::new());
			attrs.record(&mut visitor);
			let record = Record {
				started: Instant::now(),
				target: meta.target().to_owned(),
				name: meta.name(),
				fields: visitor.0,
				warned: false,
			};
			if let Ok(mut map) = INFLIGHT.lock() {
				map.insert(id.into_u64(), record);
			}
		}

		fn on_close(&self, id: Id, _ctx: Context<'_, S>) {
			if let Ok(mut map) = INFLIGHT.lock() {
				map.remove(&id.into_u64());
			}
		}
	}

	/// Number of operations currently in flight (open spans).
	pub fn inflight_count() -> usize {
		INFLIGHT.lock().map(|m| m.len()).unwrap_or(0)
	}

	/// Emit a one-shot `warn!` for every in-flight operation older than `warn_after` that has
	/// not already been warned about. Returns the number of newly-warned operations.
	pub fn warn_stale_operations(warn_after: Duration) -> usize {
		// Collect under the lock, emit after releasing it (the warn events go through the
		// subscriber and we hold no other locks while logging).
		let mut to_warn: Vec<(String, &'static str, String, Duration)> = Vec::new();
		if let Ok(mut map) = INFLIGHT.lock() {
			for rec in map.values_mut() {
				let elapsed = rec.started.elapsed();
				if !rec.warned && elapsed >= warn_after {
					rec.warned = true;
					to_warn.push((rec.target.clone(), rec.name, rec.fields.clone(), elapsed));
				}
			}
		}
		for (target, name, fields, elapsed) in &to_warn {
			tracing::warn!(
				target: "filen_sdk_rs::obs",
				op_target = %target,
				op_name = name,
				op_fields = %fields,
				elapsed_ms = elapsed.as_millis() as u64,
				"operation still running",
			);
		}
		to_warn.len()
	}
}

/// Android logcat sink: a `MakeWriter` over the NDK `__android_log_write`, mapping the
/// tracing level to an Android log priority. No external crate.
#[cfg(target_os = "android")]
mod android {
	use std::ffi::CString;
	use std::io::{self, Write};
	use std::os::raw::{c_char, c_int};

	#[link(name = "log")]
	unsafe extern "C" {
		fn __android_log_write(prio: c_int, tag: *const c_char, text: *const c_char) -> c_int;
	}

	// android/log.h ANDROID_LOG_* priorities.
	const PRIO_DEFAULT: c_int = 4; // INFO
	const TAG: &str = "filen-sdk-rs";

	fn priority(level: &tracing::Level) -> c_int {
		match *level {
			tracing::Level::ERROR => 6,
			tracing::Level::WARN => 5,
			tracing::Level::INFO => 4,
			tracing::Level::DEBUG => 3,
			tracing::Level::TRACE => 2,
		}
	}

	pub(super) struct LogcatWriter {
		prio: c_int,
	}

	impl Write for LogcatWriter {
		fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
			let line = String::from_utf8_lossy(buf);
			let line = line.trim_end_matches('\n');
			// CString rejects interior NULs; scrub them rather than dropping the line.
			if let (Ok(tag), Ok(text)) = (CString::new(TAG), CString::new(line.replace('\0', " ")))
			{
				unsafe { __android_log_write(self.prio, tag.as_ptr(), text.as_ptr()) };
			}
			Ok(buf.len())
		}

		fn flush(&mut self) -> io::Result<()> {
			Ok(())
		}
	}

	pub(super) struct LogcatMakeWriter;

	impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogcatMakeWriter {
		type Writer = LogcatWriter;

		fn make_writer(&'a self) -> Self::Writer {
			LogcatWriter { prio: PRIO_DEFAULT }
		}

		fn make_writer_for(&'a self, meta: &tracing::Metadata<'_>) -> Self::Writer {
			LogcatWriter {
				prio: priority(meta.level()),
			}
		}
	}
}
