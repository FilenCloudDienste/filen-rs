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
pub(crate) const SELECT_DIR_LAST_LISTED: &str =
	include_str!("../../sql/select_dir_last_listed.sql");
pub(crate) const UPDATE_LOCAL_DATA_BY_UUID: &str = include_str!("../../sql/update_local_data.sql");
pub(crate) const MARK_STALE_WITH_PARENT: &str =
	include_str!("../../sql/mark_stale_with_parent.sql");
pub(crate) const DELETE_STALE_WITH_PARENT: &str =
	include_str!("../../sql/delete_stale_with_parent.sql");
pub(crate) const UNMARK_STALE_PENDING_WITH_PARENT: &str =
	include_str!("../../sql/unmark_stale_pending_with_parent.sql");
pub(crate) const UNMARK_STALE_UUID: &str = include_str!("../../sql/unmark_stale_uuid.sql");
pub(crate) const MARK_STALE_TRASHED: &str = include_str!("../../sql/mark_stale_trashed.sql");
pub(crate) const DELETE_STALE_TRASHED: &str = include_str!("../../sql/delete_stale_trashed.sql");
pub(crate) const UNMARK_STALE_PENDING_TRASHED: &str =
	include_str!("../../sql/unmark_stale_pending_trashed.sql");
pub(crate) const SELECT_POS_NOT_IN_UUIDS: &str =
	include_str!("../../sql/select_pos_not_in_uuids.sql");

// Slot-remint log: which cache slots hold bytes whose row has moved to a new
// uuid. A remote content edit re-mints a file's `uuid` in place (the stable
// tier of `upsert_item`), but the local slot — cache/<uuid>/<name> — keeps the
// OLD uuid, so a row holding an unsent edit loses track of its own bytes and
// the identity sweep sees the slot as an orphan. The durable table records the
// (old, new) pair; the per-connection trigger writes it exactly when a row
// carrying a pending-upload marker changes uuid. Lives OUTSIDE init.sql on
// purpose: an init.sql edit flips DB_INIT_HASH and wipes the account's cache,
// which is precisely the data this log protects. Installed by
// `install_slot_remint_log` on every connection once `items` exists (the
// trigger is TEMP — per-connection — so each process guards its own writes).
/// The table half, created by `configure_conn` (it depends on nothing, so it can go in before
/// `init.sql` has run — which matters, because the sweep query above joins against it on every
/// connection).
pub(crate) const CREATE_SLOT_REMINTS_TABLE: &str = "CREATE TABLE IF NOT EXISTS slot_remints (
	id INTEGER PRIMARY KEY,
	old_uuid BLOB NOT NULL,
	new_uuid BLOB NOT NULL
);";
/// The trigger half, installed by `install_slot_remint_log` once `items` exists.
pub(crate) const INSTALL_SLOT_REMINT_LOG: &str = "
CREATE TEMP TRIGGER IF NOT EXISTS slot_remint_log
AFTER UPDATE OF uuid ON items
FOR EACH ROW
WHEN old.uuid IS NOT new.uuid AND old.pending_upload_at IS NOT NULL
BEGIN
	INSERT INTO slot_remints (old_uuid, new_uuid) VALUES (old.uuid, new.uuid);
END;";
pub(crate) const SELECT_SLOT_REMINTS: &str =
	"SELECT id, old_uuid, new_uuid FROM slot_remints ORDER BY id ASC;";
pub(crate) const DELETE_SLOT_REMINT: &str = "DELETE FROM slot_remints WHERE id = ?;";
/// Whether any logged remint still points at this row's CURRENT uuid — i.e.
/// the row's bytes may be sitting in a slot the rescue has not moved yet.
pub(crate) const SLOT_REMINT_TARGETS_UUID: &str =
	"SELECT EXISTS (SELECT 1 FROM slot_remints WHERE new_uuid = ?);";
// Permanent-deletion log: the sequence ranges whose tombstones a server-confirmed
// deletion wrote, so every id it retired (the whole subtree, through the delete
// cascade) answers as gone without asking the server, whose answer for a
// descendant of a permanently deleted directory is `trash: true`, naming a
// trashed item no trash listing contains. A tombstone counts only while it still
// carries the sequence that deletion gave it: an id that resolves again loses
// its tombstone, and a later retirement re-stamps it outside the range. Lives
// OUTSIDE init.sql: a database without it just has no ranges, which is no reason
// for the full wipe and resync an init.sql hash change forces.
/// Created by `configure_conn`, like the slot-remint table.
pub(crate) const CREATE_PERMANENT_DELETIONS_TABLE: &str =
	"CREATE TABLE IF NOT EXISTS permanent_deletions (
	through_seq INTEGER PRIMARY KEY,
	after_seq INTEGER NOT NULL
);";
/// Moves the end of the range ending at `?1` to `?2`: back-to-back deletions share one row.
pub(crate) const EXTEND_PERMANENT_DELETION: &str =
	"UPDATE permanent_deletions SET through_seq = ?2 WHERE through_seq = ?1;";
pub(crate) const INSERT_PERMANENT_DELETION: &str =
	"INSERT INTO permanent_deletions (after_seq, through_seq) VALUES (?1, ?2);";
/// Whether the provider id `?2` of kind `?1` (as in `tombstones`) was retired by a permanent
/// deletion and still is. Ranges are disjoint, so the first one ending at or above the
/// tombstone's sequence is the only one that can hold it: one step down the primary key.
///
/// A non-trashed row filed under the id lifts it: a listing, search or recents answer that put
/// something live under that directory shows it is not gone (a cascade can reach a child that
/// moved away remotely without this cache knowing), and the server is asked as before. Trashed
/// rows stay filed under their original parent, so they do not count. A file has no children,
/// so for one the guard always holds.
pub(crate) const SELECT_IS_PERMANENTLY_DELETED: &str = "SELECT EXISTS (
	SELECT 1 FROM tombstones
	WHERE tombstones.kind = ?1 AND tombstones.item_id = ?2
	AND tombstones.seq > (
		SELECT permanent_deletions.after_seq FROM permanent_deletions
		WHERE permanent_deletions.through_seq >= tombstones.seq
		ORDER BY permanent_deletions.through_seq ASC LIMIT 1
	)
	AND NOT EXISTS (
		SELECT 1 FROM items WHERE items.parent = ?2 AND items.trashed = FALSE
	)
);";
pub(crate) const MARK_PENDING_UPLOAD: &str = include_str!("../../sql/mark_pending_upload.sql");
pub(crate) const CLEAR_PENDING_UPLOAD: &str = include_str!("../../sql/clear_pending_upload.sql");
pub(crate) const CLEAR_PENDING_UPLOAD_BY_UUID: &str =
	include_str!("../../sql/clear_pending_upload_by_uuid.sql");
pub(crate) const CLEAR_PENDING_UPLOAD_SUBTREE: &str =
	include_str!("../../sql/clear_pending_upload_subtree.sql");
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
/// The provider ids retired above a sequence, up to the page cutoff (`?2`, i64::MAX for an
/// unpaged read). `kind` is not selected: it is what keeps the tombstone lookups off a full scan
/// (see `init.sql`), but both kinds render into the same `stable/<id>` namespace, so the id
/// alone is the whole answer.
pub(crate) const SELECT_RETIRED_IDS: &str =
	"SELECT item_id FROM tombstones WHERE seq > ?1 AND seq <= ?2 ORDER BY seq ASC;";
/// The sequences live above `?1`, items and tombstones pooled, oldest first, at most `?2` of
/// them — how a paged diff picks its cutoff: the last sequence of a full page bounds what that
/// page serves, so items and retirements advance through ONE ordered history and neither can
/// starve the other. The items half deliberately skips the per-type joins: a half-written row's
/// sequence may shorten a page, never lose a change (landing the other half re-stamps it).
pub(crate) const SELECT_CHANGE_SEQS_PAGE: &str = "SELECT seq FROM (
	SELECT items.change_seq AS seq FROM items
	WHERE items.change_seq > ?1 AND items.type != 0
	UNION ALL
	SELECT tombstones.seq AS seq FROM tombstones WHERE tombstones.seq > ?1
)
ORDER BY seq ASC LIMIT ?2;";
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
