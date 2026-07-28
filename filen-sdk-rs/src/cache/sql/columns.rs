//! Result-column names for every cache query, so rows are read by NAME (`row.get(UUID)`) instead of
//! by position (`row.get(0)`). A positional read silently returns the wrong column when a `SELECT`
//! list is reordered or a column is inserted mid-list; a named read fails loudly with
//! `InvalidColumnName` instead.
//!
//! # Invariants
//!
//! - **Every value in this module is unique.** One const per distinct result-column NAME, shared by
//!   every query that emits it — so `ITEMS_UUID` and a hypothetical second `"uuid"` const can never
//!   drift apart.
//! - **Every result set must have unique column names.** `Statement::column_index` scans left to
//!   right and returns the FIRST case-insensitive match, so a query joining two tables that both
//!   have a `name` column MUST alias them (see [`FILE_NAME`] / [`DIR_NAME`]).
//! - **Every non-trivial select expression must be aliased.** SQLite names an unaliased expression
//!   column after its own text (`count(*)` → `"count(*)"`), which is not a stable identifier —
//!   hence [`COUNT`] and friends, and the `AS` clauses in the inline test SQL.
//!
//! Covers the whole cache schema: the `sql` module's own queries AND the `search` module's window /
//! count queries, which project the same underlying columns (deduplicating them here is what keeps
//! the uniqueness invariant checkable in one place).

// -- `items` -----------------------------------------------------------------------------------

pub(in crate::cache) const ITEMS_ID: &str = "id";
pub(in crate::cache) const ITEMS_UUID: &str = "uuid";
pub(in crate::cache) const ITEMS_PARENT: &str = "parent";
pub(in crate::cache) const ITEMS_TYPE: &str = "type";
/// Written on every upsert and compared inside `diff_content_changes.sql`, but only ever SELECTed
/// back by the round-trip tests.
#[cfg(test)]
pub(in crate::cache) const ITEMS_CONTENT_HASH: &str = "content_hash";

// -- `files` -----------------------------------------------------------------------------------

/// The server-minted whole-life file id, constant across the content edits that re-mint `uuid`.
pub(in crate::cache) const FILES_STABLE_UUID: &str = "stable_uuid";
pub(in crate::cache) const FILES_CHUNKS_SIZE: &str = "chunks_size";
pub(in crate::cache) const FILES_CHUNKS: &str = "chunks";
pub(in crate::cache) const FILES_REGION: &str = "region";
pub(in crate::cache) const FILES_BUCKET: &str = "bucket";
pub(in crate::cache) const FILES_SIZE: &str = "size";
pub(in crate::cache) const FILES_MIME: &str = "mime";
pub(in crate::cache) const FILES_KEY: &str = "file_key";
pub(in crate::cache) const FILES_KEY_VERSION: &str = "file_key_version";
pub(in crate::cache) const FILES_MODIFIED: &str = "modified";
pub(in crate::cache) const FILES_HASH: &str = "hash";

// -- `dirs` ------------------------------------------------------------------------------------

pub(in crate::cache) const DIRS_COLOR: &str = "color";

// -- `files` / `dirs` collisions ----------------------------------------------------------------
//
// `favorite`, `timestamp`, `name` and `created` exist on BOTH tables, and the search window
// queries select from both in one row. Those four are therefore ALWAYS read through the aliased
// names below, which every such query spells out with `AS`.

pub(in crate::cache) const FILE_FAVORITE: &str = "file_favorite";
pub(in crate::cache) const FILE_TIMESTAMP: &str = "file_timestamp";
pub(in crate::cache) const FILE_NAME: &str = "file_name";
pub(in crate::cache) const FILE_CREATED: &str = "file_created";

pub(in crate::cache) const DIR_FAVORITE: &str = "dir_favorite";
pub(in crate::cache) const DIR_TIMESTAMP: &str = "dir_timestamp";
pub(in crate::cache) const DIR_NAME: &str = "dir_name";
pub(in crate::cache) const DIR_CREATED: &str = "dir_created";

// -- `events` ----------------------------------------------------------------------------------

pub(in crate::cache) const EVENTS_SEQ: &str = "seq";
pub(in crate::cache) const EVENTS_DRIVE_MESSAGE_ID: &str = "drive_message_id";
/// Only the drain ORDER BY uses it in production; the ordering tests SELECT it back.
#[cfg(test)]
pub(in crate::cache) const EVENTS_SYNTHETIC: &str = "synthetic";
pub(in crate::cache) const EVENTS_PAYLOAD: &str = "payload";

// -- `cache_meta` ------------------------------------------------------------------------------

pub(in crate::cache) const CACHE_META_VALUE: &str = "value";

// -- `sqlite_master` (schema introspection in tests) --------------------------------------------

#[cfg(test)]
pub(in crate::cache) const SQLITE_MASTER_NAME: &str = "name";

// -- Search window / count projections ----------------------------------------------------------
//
// Emitted only by `search/raw/search_window_*.sql`. The remaining window columns reuse the
// `items`/`files`/`dirs` names above.

pub(in crate::cache) const SEARCH_TOTAL: &str = "total";
pub(in crate::cache) const SEARCH_PARENT_PATH: &str = "parent_path";

// -- Aggregate / expression aliases -------------------------------------------------------------
//
// SQLite gives an unaliased expression column the expression's own text as its name, so every
// query producing one spells out `AS <alias>` and reads it back through the const below.

pub(in crate::cache) const COUNT: &str = "count";
/// `SELECT EXISTS (...) AS item_exists` — `exists` itself is a SQL keyword.
#[cfg(test)]
pub(in crate::cache) const ITEM_EXISTS: &str = "item_exists";

// -- PRAGMA result columns ----------------------------------------------------------------------
//
// A value-returning PRAGMA yields a single column named after the pragma itself.

pub(in crate::cache) const PRAGMA_USER_VERSION: &str = "user_version";
#[cfg(test)]
pub(in crate::cache) const PRAGMA_SYNCHRONOUS: &str = "synchronous";
#[cfg(test)]
pub(in crate::cache) const PRAGMA_CACHE_SIZE: &str = "cache_size";
#[cfg(test)]
pub(in crate::cache) const PRAGMA_MMAP_SIZE: &str = "mmap_size";
