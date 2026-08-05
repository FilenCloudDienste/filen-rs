use std::{borrow::Cow, collections::HashMap, str::FromStr, time::Instant};

use filen_sdk_rs::fs::HasUUID;
use filen_types::fs::{ParentUuid, Uuid};
use rusqlite::{Connection, OptionalExtension};
use tracing::debug;

use crate::{
	CacheError,
	auth::{AuthCacheState, FilenMobileCacheState},
	ffi::{
		FfiChanges, FfiId, FfiObject, FfiRoot, QueryChildrenResponse, QueryNonDirChildrenResponse,
	},
	sql::{
		self, DBDirExt, DBDirObject, DBItemTrait, DBRoot,
		error::OptionalExtensionSQL,
		item::RawDBItem,
		json_object::JsonObject,
		object::{DBNonRootObject, DBObject},
	},
};

// yes this should be done with macros
// no I didn't have time
#[uniffi::export]
impl FilenMobileCacheState {
	pub fn query_roots_info(&self, root_uuid_str: String) -> Result<Option<FfiRoot>, CacheError> {
		self.sync_execute_authed(|auth_state| auth_state.query_roots_info(root_uuid_str))
	}

	pub fn query_dir_children(
		&self,
		path: &FfiId,
		order_by: Option<String>,
	) -> Result<Option<QueryChildrenResponse>, CacheError> {
		self.sync_execute_authed(|auth_state| auth_state.query_dir_children(path, order_by))
	}

	pub fn query_recents(
		&self,
		order_by: Option<String>,
	) -> Result<QueryNonDirChildrenResponse, CacheError> {
		self.sync_execute_authed(|auth_state| auth_state.query_recents(order_by))
	}

	pub fn query_trash(
		&self,
		order_by: Option<String>,
	) -> Result<QueryNonDirChildrenResponse, CacheError> {
		self.sync_execute_authed(|auth_state| auth_state.query_trash(order_by))
	}

	pub fn query_item(&self, path: &FfiId) -> Result<Option<FfiObject>, CacheError> {
		self.sync_execute_authed(|auth_state| auth_state.query_item(path))
	}

	pub fn query_item_by_uuid(&self, uuid: &str) -> Result<Option<FfiObject>, CacheError> {
		self.sync_execute_authed(|auth_state| auth_state.query_item_by_uuid(uuid))
	}

	pub fn query_path_for_uuid(&self, uuid: String) -> Result<Option<FfiId>, CacheError> {
		self.sync_execute_authed(|auth_state| auth_state.query_path_for_uuid(uuid))
	}

	pub fn get_all_descendant_paths(&self, path: &FfiId) -> Result<Vec<FfiId>, CacheError> {
		self.sync_execute_authed(|auth_state| auth_state.get_all_descendant_paths(path))
	}

	/// Replaces the item's local data with `local_data`.
	///
	/// The column is the app's alone: the cache stores nothing of its own in it, so a write here
	/// can only ever lose keys the app itself put there. An outstanding local edit is tracked on
	/// [`crate::ffi::FfiFile::pending_upload_at`], out of reach of this.
	pub fn update_local_data(
		&self,
		uuid: &str,
		local_data: HashMap<String, String>,
	) -> Result<(), CacheError> {
		self.sync_execute_authed(|auth_state| auth_state.update_local_data(uuid, local_data))
	}

	pub fn insert_into_local_data_for_path(
		&self,
		path: FfiId,
		key: String,
		value: Option<String>,
	) -> Result<FfiObject, CacheError> {
		self.sync_execute_authed(|auth_state| {
			auth_state.insert_into_local_data_for_path(path, key, value)
		})
	}

	/// How many local edits have not reached the server yet.
	///
	/// Non-zero means at least one file was edited locally and its upload has not succeeded; those
	/// are retried by [`FilenMobileCacheState::retry_pending_uploads`].
	pub fn pending_upload_count(&self) -> Result<u32, CacheError> {
		self.sync_execute_authed(|auth_state| auth_state.pending_upload_count())
	}

	pub fn root_uuid(&self) -> Result<String, CacheError> {
		self.sync_execute_authed(|auth_state| Ok(auth_state.root_uuid()))
	}

	/// The sequence the cache has reached, as the opaque anchor to hand back to
	/// [`FilenMobileCacheState::enumerate_changes`] to be told what happened after it.
	pub fn current_sync_anchor(&self) -> Result<Vec<u8>, CacheError> {
		self.sync_execute_authed(|auth_state| current_sync_anchor(&auth_state.conn()))
	}

	/// Everything that happened after `anchor`: the items to (re)render and the ids to drop.
	///
	/// `None` asks for the whole drive — every item the cache holds and nothing to delete, which
	/// is what a replica starting from nothing needs. An anchor this database did not issue is
	/// [`CacheError::SyncAnchorExpired`]; the caller answers that by asking again with `None`.
	///
	/// Purely local: it reports what the cache knows, which is what makes it work offline. Keeping
	/// the cache fresh is the refreshing calls' job, not this one's.
	///
	/// The objects in the feed carry `pending_upload_at` and `local_data` as of the read that
	/// produced them, and NOTHING re-stamps an item when either of those changes — they are
	/// deliberately outside the bump guards, because they are device-local state no replica is
	/// meant to see, and because restamping on them would put every directory refresh into every
	/// replica's diff. So a feed object may name an outstanding edit that has since been delivered,
	/// or miss one made after it was read. Consumers must treat those two fields as a hint and ask
	/// for the item itself when the answer matters.
	pub fn enumerate_changes(&self, anchor: Option<Vec<u8>>) -> Result<FfiChanges, CacheError> {
		self.sync_execute_authed(|auth_state| changes_since(&auth_state.conn(), anchor.as_deref()))
	}

	/// The items this device has a stake in: bytes in the local cache, an edit that has not
	/// reached the server, or a favourite. This is the set worth keeping up to date
	/// incrementally — everything else is reconciled when it is presented.
	pub fn query_working_set(&self) -> Result<Vec<FfiObject>, CacheError> {
		self.sync_execute_authed(|auth_state| working_set(&auth_state.conn()))
	}
}

/// The namespace an id uses to name a row outright, where every other id form describes a
/// location.
pub(crate) const STABLE_PREFIX: &str = "stable/";

/// The stable id an [`FfiId`] addresses, if it is in the stable namespace.
///
/// Callers that can CREATE something at a path need to know the difference: an id that named a
/// row is a promise the row exists, and [`AuthCacheState::canonicalize_id`] hands back a
/// display-name path built from that row — a snapshot which goes stale the moment the item is
/// renamed or moved remotely. Creating something for such an id would put a new item at the old
/// name and call it the one the caller meant.
pub(crate) fn addressed_stable_uuid(id: &FfiId) -> Option<Uuid> {
	Uuid::from_str(id.0.strip_prefix(STABLE_PREFIX)?).ok()
}

/// Resolves an FFI-provided identifier to the row's CURRENT uuid.
///
/// Takes the `stable/<id>` namespace as well as a bare uuid, because the methods that hand us a
/// raw uuid string take their identifiers from the same place the [`FfiId`] ones do — a provider
/// that persists `stable/<id>` has no second, plainer form of it to offer. Which tier is tried
/// first follows from which form was used, and the two orders are not interchangeable:
/// - `stable/<id>` asks the stable tier first, exactly as
///   [`AuthCacheState::canonicalize_id`] resolves the same namespace. A row whose `uuid` is
///   also some file's stable id is that file's retired predecessor — the trashed ghost a
///   versioning-disabled edit leaves behind — and answering with it would name a dead version.
/// - A bare uuid asks the uuid tier first: it is whatever the caller last saw, most often a
///   current uuid, and this is the order that form has always resolved in.
///
/// Dirs and roots have no stable id at all (the column is files-only), so `stable/<dir-uuid>`
/// misses the stable tier and falls through — a dir's `uuid` already is its whole-life id.
/// Falls through to the parsed value when no row matches either, letting the caller produce its
/// own not-found handling.
pub(crate) fn resolve_uuid_or_stable(conn: &Connection, s: &str) -> Result<Uuid, CacheError> {
	let (addressed_stable, s) = match s.strip_prefix(STABLE_PREFIX) {
		Some(rest) => (true, rest),
		None => (false, s),
	};
	let uuid = Uuid::from_str(s)?;
	if !addressed_stable && RawDBItem::select(conn, uuid)?.is_some() {
		return Ok(uuid);
	}
	if let Some(item) = RawDBItem::select_by_stable(conn, uuid)? {
		return Ok(item.uuid);
	}
	Ok(uuid)
}

/// Length of the `change_meta.db_instance` blob an anchor carries.
const DB_INSTANCE_LEN: usize = 16;

/// An anchor: the id of the database incarnation that issued it, then the sequence it names,
/// little-endian. Opaque to the caller, which only ever hands it back.
///
/// The instance id is what makes an anchor expire. A wipe re-runs `init.sql`, which seeds
/// `change_meta` with a fresh `randomblob`, so an anchor from the previous incarnation names a
/// sequence in a history that no longer exists — honouring it would silently under-report
/// everything the wipe destroyed.
fn encode_anchor(db_instance: &[u8; DB_INSTANCE_LEN], seq: i64) -> Vec<u8> {
	let mut anchor = Vec::with_capacity(DB_INSTANCE_LEN + size_of::<i64>());
	anchor.extend_from_slice(db_instance);
	anchor.extend_from_slice(&seq.to_le_bytes());
	anchor
}

/// The sequence an anchor names, or [`CacheError::SyncAnchorExpired`] when it names none in this
/// database — a wrong length is treated as a foreign anchor rather than as a caller bug, because
/// the answer is the same either way: enumerate from scratch.
fn decode_anchor(anchor: &[u8], db_instance: &[u8; DB_INSTANCE_LEN]) -> Result<i64, CacheError> {
	let expired =
		|| CacheError::SyncAnchorExpired("the sync anchor was not issued by this database".into());
	let (instance, seq) = anchor
		.split_at_checked(DB_INSTANCE_LEN)
		.ok_or_else(expired)?;
	let seq: [u8; size_of::<i64>()] = seq.try_into().map_err(|_| expired())?;
	if instance != db_instance {
		return Err(expired());
	}
	Ok(i64::from_le_bytes(seq))
}

/// The anchor naming the sequence the cache has reached.
pub(crate) fn current_sync_anchor(conn: &Connection) -> Result<Vec<u8>, CacheError> {
	let (db_instance, seq) = sql::select_change_meta(conn)?;
	Ok(encode_anchor(&db_instance, seq))
}

/// The diff above `anchor`, or the whole drive when there is none.
///
/// **The order of the three reads below is load-bearing and must not be rearranged.** The anchor
/// is read FIRST, and everything else after it. There is no snapshot here to lean on: in
/// deployment the app and the file provider extension are separate processes holding separate
/// connections to the same WAL database, so the connection guard this call holds bounds nothing
/// the other process is doing, and the counter can move while the rows and the retired ids are
/// being read.
///
/// Reading the anchor first makes that harmless. Whatever lands mid-call is stamped ABOVE the
/// anchor being handed back, so the next diff serves it again: the worst case is delivering the
/// same change twice, which a replica applying it idempotently absorbs. Read in the other order
/// the anchor would sit above rows nobody was shown, and those changes would be lost for good.
pub(crate) fn changes_since(
	conn: &Connection,
	anchor: Option<&[u8]>,
) -> Result<FfiChanges, CacheError> {
	let (db_instance, seq) = sql::select_change_meta(conn)?;
	let seq_floor = match anchor {
		Some(anchor) => decode_anchor(anchor, &db_instance)?,
		// A replica with no anchor holds nothing, so it has nothing to be told to delete: every
		// live item is new to it, and every retired id names something it never heard of.
		None => 0,
	};
	debug!("Enumerating changes above sequence {seq_floor} (at {seq})");

	let updated = sql::select_changed_items(conn, seq_floor)?
		.into_iter()
		.map(|obj| FfiObject::from(DBObject::from(obj)))
		.collect();
	let deleted_ids = match anchor {
		Some(_) => sql::select_retired_ids(conn, seq_floor)?
			.into_iter()
			.map(|id| format!("{STABLE_PREFIX}{id}"))
			.collect(),
		None => Vec::new(),
	};

	Ok(FfiChanges {
		updated,
		deleted_ids,
		anchor: encode_anchor(&db_instance, seq),
		more: false,
	})
}

/// The working set (see [`FilenMobileCacheState::query_working_set`]).
pub(crate) fn working_set(conn: &Connection) -> Result<Vec<FfiObject>, CacheError> {
	Ok(sql::select_working_set(conn)?
		.into_iter()
		.map(|obj| FfiObject::from(DBObject::from(obj)))
		.collect())
}

impl AuthCacheState {
	pub(crate) fn query_roots_info(
		&self,
		root_uuid_str: String,
	) -> Result<Option<FfiRoot>, CacheError> {
		debug!("Querying root info for UUID: {root_uuid_str}");
		let conn = self.conn();
		Ok(DBRoot::select(&conn, Uuid::from_str(&root_uuid_str)?)
			.optional()?
			.map(Into::into))
	}

	pub(crate) fn add_root(&self, root: &str) -> Result<(), CacheError> {
		debug!("Adding root with UUID: {root}");
		let root_uuid = Uuid::from_str(root)?;
		let mut conn = self.conn();
		sql::insert_root(&mut conn, root_uuid)?;
		Ok(())
	}

	pub(crate) fn query_dir_children(
		&self,
		path: &FfiId,
		order_by: Option<String>,
	) -> Result<Option<QueryChildrenResponse>, CacheError> {
		let path = self.canonicalize_id(path)?;
		let path_id = path.as_path()?;
		debug!("Querying directory children at path: {}", path.0);

		let dir: DBDirObject = match sql::select_object_at_path(&self.conn(), &path_id)? {
			Some(obj) => obj.try_into()?,
			None => return Ok(None),
		};

		let conn = self.conn();
		let children = dir.select_children(&conn, order_by.as_deref())?;
		Ok(Some(QueryChildrenResponse {
			parent: dir.into(),
			objects: children.into_iter().map(Into::into).collect(),
		}))
	}

	pub(crate) fn query_recents(
		&self,
		order_by: Option<String>,
	) -> Result<QueryNonDirChildrenResponse, CacheError> {
		debug!("Querying recents with order by: {order_by:?}");
		let children = sql::select_recents(&self.conn(), order_by.as_deref())?;
		let last_update = *self.last_recents_update.read().unwrap();
		let now = Instant::now();
		Ok(QueryNonDirChildrenResponse {
			objects: children.into_iter().map(Into::into).collect(),
			millis_since_updated: last_update
				.map(|t| now.duration_since(t).as_millis().try_into().unwrap()),
		})
	}

	pub(crate) fn query_trash(
		&self,
		order_by: Option<String>,
	) -> Result<QueryNonDirChildrenResponse, CacheError> {
		debug!("Querying trash with order by: {order_by:?}");
		let children = sql::select_trash(&self.conn(), order_by.as_deref())?;
		let last_update = *self.last_trash_update.read().unwrap();
		let now = Instant::now();
		Ok(QueryNonDirChildrenResponse {
			objects: children.into_iter().map(Into::into).collect(),
			millis_since_updated: last_update
				.map(|t| now.duration_since(t).as_millis().try_into().unwrap()),
		})
	}

	/// Resolves the `stable/<id>` namespace to the canonical id form the rest
	/// of the cache understands (a display-name path, `trash/<uuid>`, or the
	/// bare root uuid); every other id form passes through unchanged. The
	/// value after `stable/` is matched against `stable_uuid` first — only a
	/// file can match, that column being files-only — and falls back to a
	/// `uuid` match, which is what addresses dirs and roots (their uuid is
	/// their whole-life id) as well as file uuids persisted before the
	/// stable-id migration, while the cache still knows the row.
	pub(crate) fn canonicalize_id<'a>(&self, id: &'a FfiId) -> Result<Cow<'a, FfiId>, CacheError> {
		let Some(rest) = id.0.strip_prefix(STABLE_PREFIX) else {
			return Ok(Cow::Borrowed(id));
		};
		let stable_uuid = Uuid::from_str(rest)
			.map_err(|e| CacheError::conversion(format!("Invalid stable id {rest}: {e}")))?;
		let conn = self.conn();
		let item = match RawDBItem::select_by_stable(&conn, stable_uuid)? {
			Some(item) => item,
			None => RawDBItem::select(&conn, stable_uuid)?.ok_or_else(|| {
				CacheError::DoesNotExist(format!("No item for stable id: {rest}").into())
			})?,
		};
		match item.parent {
			None => Ok(Cow::Owned(FfiId(item.uuid.to_string()))),
			Some(ParentUuid::Trash(_)) => Ok(Cow::Owned(FfiId(format!("trash/{}", item.uuid)))),
			Some(_) => {
				let path =
					sql::recursive_select_path_from_uuid(&conn, item.uuid)?.ok_or_else(|| {
						CacheError::DoesNotExist(format!("No path for stable id: {rest}").into())
					})?;
				Ok(Cow::Owned(FfiId(path)))
			}
		}
	}

	/// See [`resolve_uuid_or_stable`].
	pub(crate) fn resolve_uuid_or_stable(&self, s: &str) -> Result<Uuid, CacheError> {
		resolve_uuid_or_stable(&self.conn(), s)
	}

	pub(crate) fn query_item(&self, path: &FfiId) -> Result<Option<FfiObject>, CacheError> {
		debug!("Querying item at path: {}", path.0);
		let path = self.canonicalize_id(path)?;
		let path_values = path.as_parsed()?;
		let obj = sql::select_object_at_parsed_id(&self.conn(), &path_values)?;

		let dir_obj = match obj {
			Some(DBObject::Dir(dbdir)) => DBDirObject::Dir(dbdir),
			Some(DBObject::Root(dbroot)) => DBDirObject::Root(dbroot),
			other => return Ok(other.map(Into::into)),
		};
		// stop error for ios complaining that folder doesn't exist
		#[cfg(target_os = "ios")]
		{
			use crate::sql::DBDirTrait;
			let name = match &dir_obj {
				DBDirObject::Dir(dbdir) => sql::dir::DBDirTrait::name(dbdir),
				DBDirObject::Root(_) => Some("root"),
			};
			let path = self.get_cached_file_path_from_name(&dir_obj.uuid().to_string(), name);
			if let Err(e) = std::fs::create_dir_all(path)
				&& e.kind() != std::io::ErrorKind::AlreadyExists
			{
				return Err(CacheError::io(format!(
					"Failed to create directory for {}: {e}",
					dir_obj.uuid()
				)));
			}
		}
		Ok(Some(FfiObject::from(DBObject::from(dir_obj))))
	}

	pub(crate) fn query_item_by_uuid(&self, uuid: &str) -> Result<Option<FfiObject>, CacheError> {
		debug!("Querying item by UUID: {uuid}");
		let uuid = self.resolve_uuid_or_stable(uuid)?;
		Ok(DBObject::select(&self.conn(), uuid)
			.optional()?
			.map(Into::into))
	}

	pub(crate) fn query_path_for_uuid(&self, uuid: String) -> Result<Option<FfiId>, CacheError> {
		debug!("Querying path for UUID: {uuid}");
		if uuid == self.client.root().uuid().to_string() {
			return Ok(Some(uuid.into()));
		}
		let uuid = self.resolve_uuid_or_stable(&uuid)?;
		let conn = self.conn();
		let path = sql::recursive_select_path_from_uuid(&conn, uuid)?;

		Ok(path.map(Into::into))
	}

	pub(crate) fn get_all_descendant_paths(&self, path: &FfiId) -> Result<Vec<FfiId>, CacheError> {
		debug!("Getting all descendant paths for: {}", path.0);
		let path = self.canonicalize_id(path)?;
		let path_values = path.as_path()?;
		let obj = sql::select_object_at_path(&self.conn(), &path_values)?;
		Ok(match obj {
			Some(obj) => sql::get_all_descendant_paths(&self.conn(), obj.uuid(), &path.0)?
				.into_iter()
				.map(FfiId)
				.collect(),
			None => vec![],
		})
	}

	pub(crate) fn update_local_data(
		&self,
		uuid: &str,
		local_data: HashMap<String, String>,
	) -> Result<(), CacheError> {
		debug!("Setting local data for UUID: {uuid} to {local_data:?}");
		let uuid = self.resolve_uuid_or_stable(uuid)?;
		let mut conn = self.conn();
		sql::update_local_data(&mut conn, uuid, Some(&JsonObject::new(local_data)))?;
		Ok(())
	}

	pub(crate) fn insert_into_local_data_for_path(
		&self,
		path: FfiId,
		key: String,
		value: Option<String>,
	) -> Result<FfiObject, CacheError> {
		debug!(
			"Setting {key} to {value:?} for local data for path: {}",
			path.0
		);

		let path = self.canonicalize_id(&path)?;
		let path_values = path.as_path()?;
		let mut obj = match sql::select_object_at_path(&self.conn(), &path_values)? {
			Some(DBObject::Dir(dir)) => DBNonRootObject::Dir(dir),
			Some(DBObject::File(file)) => DBNonRootObject::File(file),
			Some(DBObject::Root(_)) => {
				return Err(CacheError::conversion(
					"Cannot insert into local data for root",
				));
			}
			None => {
				return Err(CacheError::remote(format!(
					"Path {} does not point to an item",
					path_values.full_path
				)));
			}
		};

		let mut local_data = obj.local_data().map(|o| o.to_map()).unwrap_or_default();
		match value {
			Some(v) => local_data.insert(key, v),
			None => local_data.remove(&key),
		};
		let local_data = JsonObject::new(local_data);

		let stored = if local_data.is_empty() {
			None
		} else {
			Some(local_data)
		};
		sql::update_local_data(&mut self.conn(), obj.uuid(), stored.as_ref())?;
		obj.set_local_data(stored);

		Ok(FfiObject::from(DBObject::from(obj)))
	}

	pub(crate) fn root_uuid(&self) -> String {
		self.client.root().uuid().to_string()
	}
}

#[cfg(test)]
mod id_resolution_tests {
	use super::*;

	fn db() -> Connection {
		let conn = Connection::open_in_memory().unwrap();
		crate::auth::configure_conn(&conn).unwrap();
		conn.execute_batch(sql::statements::INIT).unwrap();
		// The parent every row below hangs off; nothing resolves through it, it just makes the
		// rows the shape a listing leaves behind.
		add_dir(&conn, uuid(9));
		conn
	}

	fn uuid(byte: u8) -> Uuid {
		Uuid::from_bytes([byte; 16])
	}

	/// A file row as a listing leaves it: `uuid` names the current version, `stable_uuid` the
	/// identity the providers persist.
	fn add_file(conn: &Connection, uuid_: Uuid, stable: Uuid, trashed: bool) {
		conn.execute(
			"INSERT INTO items (uuid, stable_uuid, parent, trashed, type)
			VALUES (?1, ?2, ?3, ?4, 2);",
			rusqlite::params![uuid_, stable, uuid(9), trashed],
		)
		.unwrap();
	}

	fn add_dir(conn: &Connection, uuid_: Uuid) {
		conn.execute(
			"INSERT INTO items (uuid, parent, type) VALUES (?1, ?2, 1);",
			rusqlite::params![uuid_, uuid(9)],
		)
		.unwrap();
	}

	/// The form the providers hand back for a file. Its whole point is surviving the uuid re-mint
	/// an edit causes, so resolving it must produce the row's CURRENT uuid.
	#[test]
	fn a_stable_namespace_id_resolves_to_the_files_current_uuid() {
		let conn = db();
		add_file(&conn, uuid(1), uuid(2), false);

		assert_eq!(
			resolve_uuid_or_stable(&conn, &format!("stable/{}", uuid(2))).unwrap(),
			uuid(1)
		);
	}

	/// A dir has no stable id — the column is files-only — because its own uuid never gets
	/// re-minted. The providers still address it through the one namespace, so the miss on the
	/// stable tier has to fall through rather than fail.
	#[test]
	fn a_stable_namespace_id_for_a_dir_resolves_by_uuid() {
		let conn = db();
		add_dir(&conn, uuid(3));

		assert_eq!(
			resolve_uuid_or_stable(&conn, &format!("stable/{}", uuid(3))).unwrap(),
			uuid(3)
		);
	}

	/// The form every one of these methods took before the namespace existed, for both types.
	#[test]
	fn a_bare_uuid_resolves_to_itself_for_either_type() {
		let conn = db();
		add_file(&conn, uuid(1), uuid(2), false);
		add_dir(&conn, uuid(3));

		assert_eq!(
			resolve_uuid_or_stable(&conn, &uuid(1).to_string()).unwrap(),
			uuid(1)
		);
		assert_eq!(
			resolve_uuid_or_stable(&conn, &uuid(3).to_string()).unwrap(),
			uuid(3)
		);
	}

	/// A versioning-disabled edit leaves the retired version readable for a while under the uuid
	/// that is now the live file's stable id. The two forms must not answer the same way: asked
	/// for the identity, the answer is the live row; asked for that literal uuid, it is the row
	/// that carries it.
	#[test]
	fn the_stable_namespace_prefers_the_live_row_over_its_retired_predecessor() {
		let conn = db();
		// The live head: edited, so its uuid was re-minted while its stable id stayed.
		add_file(&conn, uuid(1), uuid(2), false);
		// The ghost the edit left behind: the old uuid, trashed, re-stamped with a stable id of
		// its own that belongs to no lineage.
		add_file(&conn, uuid(2), uuid(7), true);

		assert_eq!(
			resolve_uuid_or_stable(&conn, &format!("stable/{}", uuid(2))).unwrap(),
			uuid(1),
			"the stable namespace names the identity, which is the live row"
		);
		assert_eq!(
			resolve_uuid_or_stable(&conn, &uuid(2).to_string()).unwrap(),
			uuid(2),
			"a bare uuid names whatever row carries that uuid"
		);
	}

	/// Not-found is the caller's to report — several of them answer it with `Ok(None)` rather
	/// than an error — so an id that matches no row comes back as itself.
	#[test]
	fn an_id_matching_no_row_falls_through_to_its_own_value() {
		let conn = db();

		assert_eq!(
			resolve_uuid_or_stable(&conn, &format!("stable/{}", uuid(8))).unwrap(),
			uuid(8)
		);
		assert_eq!(
			resolve_uuid_or_stable(&conn, &uuid(8).to_string()).unwrap(),
			uuid(8)
		);
	}

	/// Learning the namespace must not make the resolver accept anything that is not an id.
	#[test]
	fn garbage_is_still_rejected() {
		let conn = db();

		assert!(resolve_uuid_or_stable(&conn, "not-a-uuid").is_err());
		assert!(resolve_uuid_or_stable(&conn, "stable/not-a-uuid").is_err());
		assert!(resolve_uuid_or_stable(&conn, "stable/").is_err());
		assert!(
			resolve_uuid_or_stable(&conn, &format!("{}/child.txt", uuid(9))).is_err(),
			"a path names a place, and this is not the resolver that walks one"
		);
	}
}
