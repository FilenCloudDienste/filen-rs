//! Result-column names for every query in `sql/*.sql`, so rows are read by NAME
//! (`row.get(ITEMS_UUID)`) instead of by position.
//!
//! This replaces an offset-based scheme: the wide-join selects (`select_object`,
//! `select_dir_children`, `select_trash_children`, `select_recents`) concatenate the `items`,
//! `dirs`, `dirs_meta`, `files`, `files_meta` and `roots` column blocks in a fixed order, and each
//! struct's `from_row` was handed the base offset of its own block (`ITEM_COLUMN_COUNT_NO_EXTRA`,
//! `+ DIRS_COLUMN_COUNT + DIRS_META_COLUMN_COUNT`, …). Adding one column to a `SELECT` silently
//! shifted every block after it. Reading by name removes the offsets — and lets the SAME
//! `from_row` serve both the wide joins and the narrow per-table selects.
//!
//! # Invariants
//!
//! - **Every value here is unique.** One const per distinct result-column NAME.
//! - **Every result set has unique column names.** `Statement::column_index` scans left to right
//!   and returns the FIRST case-insensitive match, so anything appearing on both `dirs*` and
//!   `files*` is aliased apart with a `dir_`/`file_` prefix in EVERY query that emits it —
//!   including the narrow selects, which would otherwise disagree with the wide ones.
//! - **Every non-trivial select expression is aliased.** SQLite names an unaliased expression
//!   column after its own text (`count(*)` → `"count(*)"`), which is not a stable identifier.

// -- `items` -----------------------------------------------------------------------------------

pub(crate) const ITEMS_ID: &str = "id";
pub(crate) const ITEMS_UUID: &str = "uuid";
pub(crate) const ITEMS_PARENT: &str = "parent";
pub(crate) const ITEMS_TRASHED: &str = "trashed";
pub(crate) const ITEMS_LOCAL_DATA: &str = "local_data";
pub(crate) const ITEMS_TYPE: &str = "type";

// -- `dirs` / `dirs_meta` ------------------------------------------------------------------------
//
// `favorite_rank`, `timestamp`, `metadata_state`, `raw_metadata`, `name` and `created` all exist
// on the `files*` tables too, so the dir side of every wide join carries a `dir_` prefix. `color`
// and `last_listed` are dir-only and stay unprefixed.

pub(crate) const DIR_FAVORITE_RANK: &str = "dir_favorite_rank";
pub(crate) const DIRS_COLOR: &str = "color";
pub(crate) const DIR_TIMESTAMP: &str = "dir_timestamp";
pub(crate) const DIRS_LAST_LISTED: &str = "last_listed";
pub(crate) const DIR_METADATA_STATE: &str = "dir_metadata_state";
pub(crate) const DIR_RAW_METADATA: &str = "dir_raw_metadata";
pub(crate) const DIR_NAME: &str = "dir_name";
pub(crate) const DIR_CREATED: &str = "dir_created";

// -- `files` / `files_meta` ----------------------------------------------------------------------

pub(crate) const FILES_SIZE: &str = "size";
pub(crate) const FILES_CHUNKS: &str = "chunks";
pub(crate) const FILE_FAVORITE_RANK: &str = "file_favorite_rank";
pub(crate) const FILES_REGION: &str = "region";
pub(crate) const FILES_BUCKET: &str = "bucket";
pub(crate) const FILE_TIMESTAMP: &str = "file_timestamp";
pub(crate) const FILE_METADATA_STATE: &str = "file_metadata_state";
pub(crate) const FILE_RAW_METADATA: &str = "file_raw_metadata";
pub(crate) const FILE_NAME: &str = "file_name";
pub(crate) const FILES_MIME: &str = "mime";
pub(crate) const FILES_KEY: &str = "file_key";
pub(crate) const FILES_KEY_VERSION: &str = "file_key_version";
pub(crate) const FILE_CREATED: &str = "file_created";
pub(crate) const FILES_MODIFIED: &str = "modified";
pub(crate) const FILES_HASH: &str = "hash";

// -- `roots` -------------------------------------------------------------------------------------
//
// A root's `last_listed` comes from its `dirs` row, which the wide join ALSO exposes unprefixed as
// [`DIRS_LAST_LISTED`]; the root block therefore reads it through its own alias so the two blocks
// stay independent.

pub(crate) const ROOTS_STORAGE_USED: &str = "storage_used";
pub(crate) const ROOTS_MAX_STORAGE: &str = "max_storage";
pub(crate) const ROOTS_LAST_UPDATED: &str = "last_updated";
pub(crate) const ROOT_LAST_LISTED: &str = "root_last_listed";

// -- Expression aliases --------------------------------------------------------------------------

/// `coalesce(files_meta.name, dirs_meta.name, uuid_text(items.uuid)) AS display_name` — the name a
/// listing shows for an item whatever its type (see `statements::convert_order_by`).
pub(crate) const DISPLAY_NAME: &str = "display_name";
/// `select_pos_not_in_uuids.sql`: the caller's index into the uuid list it passed in.
pub(crate) const POSITION: &str = "position";
/// `recursive_select_path_from_uuid.sql`: the assembled `a/b/c` path.
pub(crate) const PATH: &str = "path";
