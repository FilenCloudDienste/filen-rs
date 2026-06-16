macro_rules! def_sql_user_version {
	($ver:literal) => {
		pub(crate) const SQL_USER_VERSION: i64 = $ver;
		pub(crate) const SET_USER_VERSION: &str = concat!("PRAGMA user_version = ", $ver, ";");
	};
}

// Bumped whenever the schema changes. `init_db` reacts to a mismatch with a destructive wipe + rebuild
// (the cache is fully reconstructible from the server, so this is safe). A non-destructive migration is
// only worth building once the DB holds non-reconstructible local state (conflicts, local trash, etc.).
def_sql_user_version!(2);

pub(crate) const VACUUM: &str = "VACUUM;";
pub(crate) const GET_USER_VERSION: &str = "PRAGMA user_version;";

pub(crate) const EVENT_INSERT: &str = include_str!("raw/event_insert.sql");
pub(crate) const EVENT_LOAD_BATCH: &str = include_str!("raw/event_load_batch.sql");
#[cfg(test)] // only used by the test-only `delete_event`; the drain deletes inline
pub(crate) const EVENT_DELETE: &str = include_str!("raw/event_delete.sql");

pub(crate) const CACHE_META_GET: &str = include_str!("raw/cache_meta_get.sql");
pub(crate) const CACHE_META_SET: &str = include_str!("raw/cache_meta_set.sql");

// Resync staging table. RESET is a batch (CREATE TEMP TABLE IF NOT EXISTS + DELETE FROM).
pub(crate) const DIFF_INCOMING_RESET: &str = include_str!("raw/diff_incoming_reset.sql");
pub(crate) const DIFF_INCOMING_INSERT: &str = include_str!("raw/diff_incoming_insert.sql");

// Sync-root membership: the upward ancestor chain of one item.
pub(crate) const ANCESTRY_OF_UUID: &str = include_str!("raw/ancestry_of_uuid.sql");

// Sync-root eviction: delete the evicted root's subtree minus the protected nested roots.
pub(crate) const EVICT_SYNC_ROOT: &str = include_str!("raw/evict_sync_root.sql");
// The protected-roots TEMP table is populated then read by EVICT_SYNC_ROOT. These three are single
// trivial clauses, so they are inlined here rather than each given a dedicated `raw/*.sql` file.
pub(crate) const EVICT_PROTECTED_ROOTS_CREATE: &str =
	"CREATE TEMP TABLE IF NOT EXISTS evict_protected_roots (uuid BLOB PRIMARY KEY NOT NULL)";
pub(crate) const EVICT_PROTECTED_ROOTS_CLEAR: &str = "DELETE FROM evict_protected_roots";
pub(crate) const EVICT_PROTECTED_ROOTS_INSERT: &str =
	"INSERT OR IGNORE INTO evict_protected_roots (uuid) VALUES (?1)";

// Resync diff queries: compare the staged listing against cached `items`.
pub(crate) const DIFF_SUBTREE_ABSENT: &str = include_str!("raw/diff_subtree_absent.sql");
pub(crate) const DIFF_ORPHANS_ABSENT: &str = include_str!("raw/diff_orphans_absent.sql");
pub(crate) const DIFF_CREATES: &str = include_str!("raw/diff_creates.sql");
pub(crate) const DIFF_MOVES: &str = include_str!("raw/diff_moves.sql");
pub(crate) const DIFF_CONTENT_CHANGES: &str = include_str!("raw/diff_content_changes.sql");

/// Key for the contiguous-prefix watermark stored in `cache_meta`.
pub(crate) const WATERMARK_KEY: &str = "last_drive_message_id";
/// Key for the durable "a resync is needed" flag in `cache_meta` (set when an event is lost — a hole,
/// a corrupt row, or a failed persist — so a resync recovers it even across a restart). Set to `1`;
/// cleared by the resync.
pub(crate) const NEEDS_RESYNC_KEY: &str = "needs_resync";

pub(crate) const DIR_UPDATE_COLOR: &str = include_str!("raw/dir_update_color.sql");
pub(crate) const DIR_UPDATE_NAME: &str = include_str!("raw/dir_update_name.sql");
// `items`/`files`/`dirs` upserts are built dynamically as multi-row `VALUES` statements in `bulk.rs`
// (one prepared statement per batch instead of one per row), so there are no static *_UPSERT consts.

pub(crate) const FILE_UPDATE_META: &str = include_str!("raw/file_update_meta.sql");

pub(crate) const INIT: &str = include_str!("raw/init.sql");

pub(crate) const ITEM_DELETE: &str = include_str!("raw/item_delete.sql");
pub(crate) const ITEM_DELETE_ALL_NON_ROOT: &str = include_str!("raw/item_delete_all_non_root.sql");
pub(crate) const ITEM_UPDATE_OWN_ROOT_ID: &str = include_str!("raw/item_update_own_root_id.sql");

// The single account-root rowid (the lone `roots` row), cached on `CacheState` at open so the bulk
// upsert binds it directly instead of re-deriving it per row.
pub(crate) const SELECT_ROOT_ID: &str = "SELECT id FROM roots ORDER BY id LIMIT 1";

pub(crate) const ROOT_INSERT: &str = include_str!("raw/root_insert.sql");
pub(crate) const ROOT_ITEM_INSERT: &str = include_str!("raw/root_item_insert.sql");
