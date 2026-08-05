use filen_types::crypto::Blake3Hash;
use lazy_static::lazy_static;

// Generic
pub(crate) const INIT: &str = include_str!("../../sql/init.sql");
lazy_static! {
	pub static ref DB_INIT_HASH: Blake3Hash = blake3::hash(INIT.as_bytes()).into();
}
pub(crate) const SELECT_ID_BY_UUID: &str = "SELECT id FROM items WHERE uuid = ?;";
pub(crate) const SELECT_STABLE_BY_UUID: &str =
	"SELECT stable_uuid FROM items WHERE uuid = ?1 AND type = ?2;";
pub(crate) const DELETE_BY_UUID: &str = "DELETE FROM items WHERE uuid = ?;";
pub(crate) const RECURSIVE_SELECT_PATH_FROM_UUID: &str =
	include_str!("../../sql/recursive_select_path_from_uuid.sql");

// Item
pub(crate) const UPSERT_ITEM: &str = include_str!("../../sql/upsert_item.sql");
pub(crate) const SELECT_ITEM_BY_PARENT_NAME: &str =
	include_str!("../../sql/select_item_by_parent_name.sql");
pub(crate) const SELECT_UUID_TYPE_NAME_BY_PARENT: &str =
	include_str!("../../sql/select_uuid_type_name_by_parent.sql");
pub(crate) const UPDATE_LOCAL_DATA_BY_UUID: &str = include_str!("../../sql/update_local_data.sql");
pub(crate) const MARK_STALE_WITH_PARENT: &str =
	include_str!("../../sql/mark_stale_with_parent.sql");
pub(crate) const DELETE_STALE_WITH_PARENT: &str =
	include_str!("../../sql/delete_stale_with_parent.sql");
pub(crate) const MARK_STALE_TRASHED: &str = include_str!("../../sql/mark_stale_trashed.sql");
pub(crate) const DELETE_STALE_TRASHED: &str = include_str!("../../sql/delete_stale_trashed.sql");
pub(crate) const SELECT_POS_NOT_IN_UUIDS: &str =
	include_str!("../../sql/select_pos_not_in_uuids.sql");
pub(crate) const MARK_PENDING_UPLOAD: &str = include_str!("../../sql/mark_pending_upload.sql");
pub(crate) const CLEAR_PENDING_UPLOAD: &str = include_str!("../../sql/clear_pending_upload.sql");
pub(crate) const SELECT_PENDING_UPLOADS: &str =
	include_str!("../../sql/select_pending_uploads.sql");
pub(crate) const SELECT_PENDING_UPLOAD_AT: &str =
	include_str!("../../sql/select_pending_upload_at.sql");
pub(crate) const SELECT_DESCENDANT_PENDING_UPLOAD: &str =
	include_str!("../../sql/select_descendant_pending_upload.sql");
pub(crate) const MARK_MATERIALISED: &str = include_str!("../../sql/mark_materialised.sql");
pub(crate) const CLEAR_MATERIALISED: &str = include_str!("../../sql/clear_materialised.sql");
pub(crate) const CLEAR_MATERIALISED_NOT_IN_CACHE: &str =
	include_str!("../../sql/clear_materialised_not_in_cache.sql");

// Item/Change feed
pub(crate) const SELECT_CHANGE_META: &str = "SELECT db_instance, counter FROM change_meta;";
/// The stamp a row carries now. Read back rather than RETURNED by the upserts: a RETURNING clause
/// is evaluated before the AFTER triggers that do the stamping, and the per-type tables bump the
/// item again after the `items` row itself is written.
pub(crate) const SELECT_CHANGE_SEQ: &str = "SELECT change_seq FROM items WHERE id = ?;";
/// The provider ids retired above a sequence. `kind` is not selected: it is what keeps the
/// tombstone lookups off a full scan (see `init.sql`), but both kinds render into the same
/// `stable/<id>` namespace, so the id alone is the whole answer.
pub(crate) const SELECT_RETIRED_IDS: &str =
	"SELECT item_id FROM tombstones WHERE seq > ?1 ORDER BY seq ASC;";
pub(crate) const SELECT_CHANGED_ITEMS: &str = include_str!("../../sql/select_changed_items.sql");
pub(crate) const SELECT_WORKING_SET: &str = include_str!("../../sql/select_working_set.sql");

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
pub(crate) const SELECT_ITEM_BY_STABLE_UUID: &str =
	include_str!("../../sql/select_item_by_stable_uuid.sql");

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

const SELECT_DIR_CHILDREN: &str = include_str!("../../sql/select_dir_children.sql");
pub(crate) fn select_dir_children(order_by: Option<&str>) -> String {
	format!("{} {}", SELECT_DIR_CHILDREN, convert_order_by(order_by))
}

pub(crate) fn select_dir_children_page(order_by: Option<&str>) -> String {
	format!(
		"{} {} LIMIT ? OFFSET ?",
		SELECT_DIR_CHILDREN,
		convert_order_by(order_by)
	)
}

const SELECT_TRASH_CHILDREN: &str = include_str!("../../sql/select_trash_children.sql");
pub(crate) fn select_trash_children(order_by: Option<&str>) -> String {
	format!("{} {}", SELECT_TRASH_CHILDREN, convert_order_by(order_by))
}

// Root
pub(crate) const SELECT_ROOT: &str = include_str!("../../sql/select_root.sql");
pub(crate) const UPSERT_ROOT_EMPTY: &str = include_str!("../../sql/upsert_root_empty.sql");
pub(crate) const INSERT_ROOT_INTO_ITEMS: &str =
	"INSERT INTO items (uuid, parent, type) VALUES (?1, NULL, ?2) RETURNING id;";
pub(crate) const INSERT_ROOT_INTO_ROOTS: &str = "INSERT INTO roots (id) VALUES (?);";
pub(crate) const INSERT_ROOT_INTO_DIRS: &str =
	"INSERT INTO dirs (id, metadata_state, timestamp, raw_metadata) VALUES (?, 1, 0, '');";
pub(crate) const UPDATE_ROOT: &str =
	"UPDATE roots SET storage_used = ?, max_storage = ?, last_updated = ? WHERE id = ?;";

// Object
pub(crate) const SELECT_OBJECT_BY_UUID: &str = include_str!("../../sql/select_object.sql");

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
