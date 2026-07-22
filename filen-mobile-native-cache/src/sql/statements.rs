use filen_types::crypto::Blake3Hash;
use lazy_static::lazy_static;

// Generic
pub(crate) const INIT: &str = include_str!("../../sql/init.sql");
lazy_static! {
	pub static ref DB_INIT_HASH: Blake3Hash = blake3::hash(INIT.as_bytes()).into();
}
pub(crate) const SELECT_ID_BY_UUID: &str = "SELECT id FROM items WHERE uuid = ?;";
/// Resolves a stable uuid to the row's current real uuid. Returns nothing when the argument is not
/// any row's `stable_uuid` (e.g. it is already a real uuid, or unknown).
pub(crate) const SELECT_UUID_BY_STABLE_UUID: &str = "SELECT uuid FROM items WHERE stable_uuid = ?;";
pub(crate) const DELETE_BY_UUID: &str = "DELETE FROM items WHERE uuid = ?;";
/// Reassigns a row's stable_uuid by row id. Used to carry the original stable identity onto the row
/// that ends up holding a re-minted content uuid when the upsert did not land on the original row.
pub(crate) const UPDATE_ITEM_STABLE_UUID: &str = "UPDATE items SET stable_uuid = ? WHERE id = ?;";
pub(crate) const RECURSIVE_SELECT_PATH_FROM_UUID: &str =
	include_str!("../../sql/recursive_select_path_from_uuid.sql");

// Item
pub(crate) const UPSERT_ITEM: &str = include_str!("../../sql/upsert_item.sql");
pub(crate) const SELECT_ITEM_BY_PARENT_NAME: &str =
	include_str!("../../sql/select_item_by_parent_name.sql");
pub(crate) const SELECT_UUID_TYPE_NAME_BY_PARENT: &str =
	include_str!("../../sql/select_uuid_type_name_by_parent.sql");
pub(crate) const UPDATE_LOCAL_DATA_BY_UUID: &str =
	"UPDATE items SET local_data = ? WHERE uuid = ?;";
pub(crate) const MARK_STALE_WITH_PARENT: &str =
	include_str!("../../sql/mark_stale_with_parent.sql");
pub(crate) const DELETE_STALE_WITH_PARENT: &str =
	include_str!("../../sql/delete_stale_with_parent.sql");
pub(crate) const MARK_STALE_TRASHED: &str = include_str!("../../sql/mark_stale_trashed.sql");
pub(crate) const DELETE_STALE_TRASHED: &str = include_str!("../../sql/delete_stale_trashed.sql");
pub(crate) const SELECT_POS_NOT_IN_UUIDS: &str =
	include_str!("../../sql/select_pos_not_in_uuids.sql");

// Item/Recents
pub(crate) const UPDATE_ITEM_SET_RECENT: &str =
	include_str!("../../sql/update_item_set_recent.sql");
pub(crate) const CLEAR_RECENTS: &str = include_str!("../../sql/clear_recents.sql");
const SELECT_RECENTS: &str = include_str!("../../sql/select_recents.sql");
pub(crate) fn select_recents(order_by: Option<&str>) -> String {
	format!("{} {}", SELECT_RECENTS, convert_order_by(order_by))
}

// Item
pub(crate) const SELECT_ITEM_BY_UUID: &str = include_str!("../../sql/select_item.sql");

// File
pub(crate) const SELECT_FILE: &str = include_str!("../../sql/select_file.sql");
pub(crate) const UPSERT_FILE: &str = include_str!("../../sql/upsert_file.sql");
pub(crate) const UPSERT_FILE_META: &str = include_str!("../../sql/upsert_file_meta.sql");
pub(crate) const DELETE_FILE_META: &str = include_str!("../../sql/delete_file_meta.sql");
pub(crate) const UPDATE_FILE_FAVORITE_RANK: &str =
	include_str!("../../sql/update_file_favorite_rank.sql");

// Dir
pub(crate) const SELECT_DIR: &str = include_str!("../../sql/select_dir.sql");
pub(crate) const UPSERT_DIR: &str = include_str!("../../sql/upsert_dir.sql");
pub(crate) const UPSERT_DIR_META: &str = include_str!("../../sql/upsert_dir_meta.sql");
pub(crate) const DELETE_DIR_META: &str = include_str!("../../sql/delete_dir_meta.sql");
pub(crate) const UPDATE_DIR_FAVORITE_RANK: &str =
	include_str!("../../sql/update_dir_favorite_rank.sql");
pub(crate) const UPDATE_DIR_LAST_LISTED: &str =
	include_str!("../../sql/update_dir_last_listed.sql");
/// uuids of every dir we've listed (`last_listed > 0`) — the dirs whose child listing we mirror
/// locally. Drives the socket reconnect catch-up so remote changes to deep folders surface.
/// `items.type = 1` = Dir only: excludes the drive root (re-listed separately on reconnect, so this
/// avoids a double root re-list) and the synthetic `'trash'` sentinel row.
pub(crate) const SELECT_MATERIALIZED_DIR_UUIDS: &str = "SELECT items.uuid FROM dirs JOIN items ON items.id = dirs.id WHERE dirs.last_listed > 0 AND items.type = 1;";

const SELECT_DIR_CHILDREN: &str = include_str!("../../sql/select_dir_children.sql");
pub(crate) fn select_dir_children(order_by: Option<&str>) -> String {
	format!("{} {}", SELECT_DIR_CHILDREN, convert_order_by(order_by))
}
/// Paginated variant: appends `LIMIT ?2 OFFSET ?3` (parent is `?1`). For the File Provider
/// enumeration, which hands the system one page at a time via an offset cursor.
pub(crate) fn select_dir_children_page(order_by: Option<&str>) -> String {
	format!("{} LIMIT ? OFFSET ?", select_dir_children(order_by))
}

const SELECT_TRASH_CHILDREN: &str = include_str!("../../sql/select_trash_children.sql");
pub(crate) fn select_trash_children(order_by: Option<&str>) -> String {
	format!("{} {}", SELECT_TRASH_CHILDREN, convert_order_by(order_by))
}

// Root
pub(crate) const SELECT_ROOT: &str = include_str!("../../sql/select_root.sql");
pub(crate) const UPSERT_ROOT_EMPTY: &str = include_str!("../../sql/upsert_root_empty.sql");
pub(crate) const INSERT_ROOT_INTO_ITEMS: &str =
	"INSERT INTO items (uuid, stable_uuid, parent, type) VALUES (?1, ?1, NULL, ?2) RETURNING id;";
pub(crate) const INSERT_ROOT_INTO_ROOTS: &str = "INSERT INTO roots (id) VALUES (?);";
pub(crate) const INSERT_ROOT_INTO_DIRS: &str =
	"INSERT INTO dirs (id, metadata_state, timestamp, raw_metadata) VALUES (?, 1, 0, '');";
pub(crate) const UPDATE_ROOT: &str =
	"UPDATE roots SET storage_used = ?, max_storage = ?, last_updated = ? WHERE id = ?;";

// Object
pub(crate) const SELECT_OBJECT_BY_UUID: &str = include_str!("../../sql/select_object.sql");

// Change tracking (workstream A / Phase 4)
/// `(epoch, seq)` of the single sync_state row. Read to build the current anchor and to detect a
/// stale anchor epoch (a rebuilt DB re-randomizes `epoch`).
pub(crate) const SELECT_SYNC_STATE: &str = "SELECT epoch, seq FROM sync_state WHERE id = 1;";
/// Wide-join live (non-trashed) children of one container changed since a `seq` (?1 = parent BLOB,
/// ?2 = from_seq).
pub(crate) const SELECT_CHANGED_CHILDREN: &str =
	include_str!("../../sql/select_changed_children.sql");
/// Wide-join trashed items changed since a `seq` (?1 = from_seq) — the trash container's delta feed.
/// Trash is addressed by `trashed = TRUE`, not a parent uuid (trashed rows keep their original
/// `parent`).
pub(crate) const SELECT_CHANGED_TRASH: &str = include_str!("../../sql/select_changed_trash.sql");
/// Wide-join non-root items changed anywhere since a `seq` (?1 = from_seq) — the working-set feed.
pub(crate) const SELECT_CHANGED_WORKINGSET: &str =
	include_str!("../../sql/select_changed_workingset.sql");
/// Wide-join current working set: favorited (file or dir), recent, or trashed non-root items.
pub(crate) const SELECT_WORKING_SET: &str = include_str!("../../sql/select_working_set.sql");
/// Tombstoned stable_uuids (rendered as text) for one container since a `seq` (?1 = parent BLOB,
/// ?2 = from_seq). `stable_uuid` is stored as a BLOB; `uuid_text` renders it as the canonical string
/// the File Provider expects.
pub(crate) const SELECT_DELETIONS_BY_PARENT: &str =
	"SELECT uuid_text(stable_uuid) FROM deletions WHERE parent = ?1 AND seq > ?2;";
/// Tombstoned stable_uuids (rendered as text) anywhere since a `seq` (?1 = from_seq) — the working-set
/// and trash delta.
pub(crate) const SELECT_DELETIONS_ALL: &str =
	"SELECT uuid_text(stable_uuid) FROM deletions WHERE seq > ?1;";

// Helpers
// todo improve significantly
fn convert_order_by(order_by: Option<&str>) -> &'static str {
	if let Some(order_by) = order_by {
		if order_by.contains("display_name") {
			if order_by.contains("ASC") {
				return "ORDER BY coalesce(files_meta.name, dirs_meta.name, uuid_text(items.uuid)) ASC";
			} else if order_by.contains("DESC") {
				return "ORDER BY coalesce(files_meta.name, dirs_meta.name, uuid_text(items.uuid)) DESC";
			}
		} else if order_by.contains("last_modified") {
			if order_by.contains("ASC") {
				return "ORDER BY files_meta.modified + 0 ASC";
			} else if order_by.contains("DESC") {
				return "ORDER BY files_meta.modified + 0 DESC";
			}
		} else if order_by.contains("size") {
			if order_by.contains("ASC") {
				return "ORDER BY files.size + 0 ASC";
			} else if order_by.contains("DESC") {
				return "ORDER BY files.size + 0 DESC";
			}
		}
	}
	"ORDER BY coalesce(files_meta.name, dirs_meta.name, uuid_text(items.uuid)) ASC"
}

// Constants
/// Number of leading `items` columns selected by the wide-join queries
/// (`id, uuid, stable_uuid, parent, trashed, local_data, type`). Does not include is_stale and
/// is_recent.
pub(crate) const ITEM_COLUMN_COUNT_NO_EXTRA: usize = 7;
// does not include the `id` column for the below
pub(crate) const DIRS_COLUMN_COUNT: usize = 6;
pub(crate) const DIRS_META_COLUMN_COUNT: usize = 2;
pub(crate) const FILES_COLUMN_COUNT: usize = 8;
pub(crate) const FILES_META_COLUMN_COUNT: usize = 7;
