//! Opening SQLite connections. Every rusqlite connection in the workspace, test code included,
//! opens through these functions, so that SQLite runs its first-time initialization on one thread
//! before any other thread reaches it (see [`initialize_sqlite_once`]).

// The workspace's `clippy.toml` disallows opening a `Connection` directly anywhere else; this
// module is the one place that does, after initializing SQLite.
#![allow(clippy::disallowed_methods)]

use std::{path::Path, sync::Once};

use rusqlite::{Connection, OpenFlags};

/// Open (creating it if needed) the SQLite database at `path`, as [`Connection::open`] does.
///
/// Use this, or one of its siblings, instead of opening a [`Connection`] directly. The first call
/// in the process initializes SQLite on its own thread while any concurrent call waits: SQLite's
/// own first-time initialization is not safe to race on weakly ordered CPUs such as arm64, where
/// two threads making their first SQLite call at the same moment can crash.
pub fn open_sqlite(path: impl AsRef<Path>) -> rusqlite::Result<Connection> {
	initialize_sqlite_once();
	Connection::open(path)
}

/// Open the SQLite database at `path` with `flags`, as [`Connection::open_with_flags`] does,
/// initializing SQLite first as [`open_sqlite`] does.
pub fn open_sqlite_with_flags(
	path: impl AsRef<Path>,
	flags: OpenFlags,
) -> rusqlite::Result<Connection> {
	initialize_sqlite_once();
	Connection::open_with_flags(path, flags)
}

/// Open a new in-memory SQLite database, as [`Connection::open_in_memory`] does, initializing
/// SQLite first as [`open_sqlite`] does.
pub fn open_sqlite_in_memory() -> rusqlite::Result<Connection> {
	initialize_sqlite_once();
	Connection::open_in_memory()
}

/// Run SQLite's first-time initialization on this thread, exactly once per process, before any
/// connection opens.
///
/// SQLite's `sqlite3MutexInit` publishes the default mutex methods with a barrier on the writer's
/// side only: it stores the method table and then `xMutexAlloc` last, but a reader that sees
/// `xMutexAlloc` set goes on to load and call `xMutexInit` with no barrier between the two loads.
/// On a weakly ordered CPU (arm64: Apple Silicon, iOS, Android) the loads can be reordered, so a
/// second thread making its first SQLite call at the same moment can call a NULL `xMutexInit` and
/// crash. That is the code in SQLite 3.51.3, which libsqlite3-sys 0.37 bundles, and in every
/// release through 3.54.1.
///
/// Opening and dropping an in-memory connection runs `sqlite3_initialize` to completion on the
/// thread that wins the [`Once`]; every other caller blocks until it returns, and the `Once`'s
/// completion orders the whole mutex table before them.
///
/// SQLite fixed the race on trunk in check-in 37b83ad2f5245217 (2026-10-20), which adds the
/// missing barrier on the reader's side (<https://sqlite.org/bugs/forumpost/03b0bc9a0b>). Once a
/// SQLite release containing that check-in is what libsqlite3-sys bundles, this function, the
/// `open_sqlite*` wrappers above and the `clippy.toml` rule that routes every open through them can
/// go, and callers can open a [`Connection`] directly again.
///
/// On wasm, rusqlite runs on sqlite-wasm-rs, which builds SQLite with `SQLITE_THREADSAFE=0`: there
/// is no mutex table to race on, and the in-memory open only initializes SQLite early. The cache
/// opens a single connection there, so the `Once` is never contended either (a contended `Once`
/// would block, which the browser's main thread does not allow). It runs on every target so that
/// callers have one way to open a connection.
fn initialize_sqlite_once() {
	static INITIALIZED: Once = Once::new();
	INITIALIZED.call_once(|| {
		// A failure here means SQLite is unusable; the open that follows reports it to its caller.
		if let Err(error) = Connection::open_in_memory() {
			tracing::warn!("SQLite failed to initialize: {error}");
		}
	});
}
