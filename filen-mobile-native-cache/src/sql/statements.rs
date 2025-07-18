use filen_types::crypto::Sha256Hash;
use lazy_static::lazy_static;
use sha2::Digest;

// Generic
pub(crate) const INIT: &str = include_str!("../../sql/init.sql");
lazy_static! {
	pub static ref DB_INIT_HASH: Sha256Hash = sha2::Sha256::digest(INIT.as_bytes()).into();
}
pub(crate) const SELECT_ID_BY_UUID: &str = "SELECT id FROM items WHERE uuid = ?;";
pub(crate) const DELETE_BY_UUID: &str = "DELETE FROM items WHERE uuid = ?;";
pub(crate) const RECURSIVE_SELECT_PATH_FROM_UUID: &str =
	include_str!("../../sql/recursive_select_path_from_uuid.sql");

// Item
pub(crate) const UPSERT_ITEM: &str = include_str!("../../sql/upsert_item.sql");
pub(crate) const SELECT_ITEM_BY_PARENT_NAME: &str =
	include_str!("../../sql/select_item_by_parent_name.sql");
pub(crate) const SELECT_UUID_NAME_TYPE_BY_PARENT: &str =
	"SELECT uuid, name, type FROM items WHERE parent = ?;";
pub(crate) const UPDATE_LOCAL_DATA_BY_UUID: &str =
	"UPDATE items SET local_data = ? WHERE uuid = ?;";
pub(crate) const MARK_STALE_WITH_PARENT: &str =
	include_str!("../../sql/mark_stale_with_parent.sql");
pub(crate) const DELETE_STALE_WITH_PARENT: &str =
	include_str!("../../sql/delete_stale_with_parent.sql");
pub(crate) const SELECT_POS_NOT_IN_UUIDS: &str =
	include_str!("../../sql/select_pos_not_in_uuids.sql");

// Item/Recents
pub(crate) const UPDATE_ITEM_SET_RECENT: &str =
	include_str!("../../sql/update_item_set_recent.sql");
pub(crate) const CLEAR_RECENTS: &str = include_str!("../../sql/clear_recents.sql");
const SELECT_RECENTS: &str = include_str!("../../sql/select_recents.sql");
pub(crate) fn select_recents(order_by: Option<&str>) -> String {
	format!(
		"{} {}",
		SELECT_RECENTS,
		match order_by {
			Some(order_by) => convert_order_by(order_by),
			_ => "ORDER BY items.name ASC",
		}
	)
}

// Item/Search
pub(crate) const CLEAR_ORPHANED_SEARCH_ITEMS: &str =
	include_str!("../../sql/clear_orphaned_search_items.sql");
pub(crate) const CLEAR_SEARCH_FROM_ITEMS: &str =
	include_str!("../../sql/clear_search_from_items.sql");
pub(crate) const SELECT_SEARCH: &str = include_str!("../../sql/select_search.sql");
pub(crate) const SELECT_ITEM_BY_UUID: &str = include_str!("../../sql/select_item.sql");
pub(crate) const UPDATE_SEARCH_PATH: &str = include_str!("../../sql/update_search_path.sql");

// File
pub(crate) const SELECT_FILE: &str = include_str!("../../sql/select_file.sql");
pub(crate) const UPSERT_FILE: &str = include_str!("../../sql/upsert_file.sql");
pub(crate) const UPDATE_FILE_FAVORITE_RANK: &str =
	include_str!("../../sql/update_file_favorite_rank.sql");

// Dir
pub(crate) const SELECT_DIR: &str = include_str!("../../sql/select_dir.sql");
pub(crate) const UPSERT_DIR: &str = include_str!("../../sql/upsert_dir.sql");
pub(crate) const UPDATE_DIR_FAVORITE_RANK: &str =
	include_str!("../../sql/update_dir_favorite_rank.sql");
pub(crate) const UPDATE_DIR_LAST_LISTED: &str =
	include_str!("../../sql/update_dir_last_listed.sql");

const SELECT_DIR_CHILDREN: &str = include_str!("../../sql/select_dir_children.sql");
pub(crate) fn select_dir_children(order_by: Option<&str>) -> String {
	format!(
		"{} {}",
		SELECT_DIR_CHILDREN,
		match order_by {
			Some(order_by) => convert_order_by(order_by),
			_ => "ORDER BY items.name ASC",
		}
	)
}

// Root
pub(crate) const SELECT_ROOT: &str = include_str!("../../sql/select_root.sql");
pub(crate) const UPSERT_ROOT_EMPTY: &str = include_str!("../../sql/upsert_root_empty.sql");
pub(crate) const INSERT_ROOT_INTO_ITEMS: &str =
	"INSERT INTO items (uuid, parent, name, type) VALUES (?, NULL, ?, ?) RETURNING id;";
pub(crate) const INSERT_ROOT_INTO_ROOTS: &str = "INSERT INTO roots (id) VALUES (?);";
pub(crate) const INSERT_ROOT_INTO_DIRS: &str = "INSERT INTO dirs (id) VALUES (?);";
pub(crate) const UPDATE_ROOT: &str =
	"UPDATE roots SET storage_used = ?, max_storage = ?, last_updated = ? WHERE id = ?;";

// Object
pub(crate) const SELECT_OBJECT_BY_UUID: &str = include_str!("../../sql/select_object.sql");

// Helpers
// todo improve significantly
fn convert_order_by(order_by: &str) -> &'static str {
	if order_by.contains("display_name") {
		if order_by.contains("ASC") {
			return "ORDER BY items.name ASC";
		} else if order_by.contains("DESC") {
			return "ORDER BY items.name DESC";
		}
	} else if order_by.contains("last_modified") {
		if order_by.contains("ASC") {
			return "ORDER BY files.modified + 0 ASC";
		} else if order_by.contains("DESC") {
			return "ORDER BY files.modified + 0 DESC";
		}
	} else if order_by.contains("size") {
		if order_by.contains("ASC") {
			return "ORDER BY files.size + 0 ASC";
		} else if order_by.contains("DESC") {
			return "ORDER BY files.size + 0 DESC";
		}
	}
	"ORDER BY items.name ASC"
}
