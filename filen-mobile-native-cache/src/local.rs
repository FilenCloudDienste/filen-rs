use std::{collections::HashMap, str::FromStr, time::Instant};

use filen_sdk_rs::fs::HasUUID;
use filen_types::fs::{Uuid, UuidStr};
use rusqlite::{Connection, OptionalExtension};
use tracing::debug;

use crate::{
	CacheError,
	auth::{AuthCacheState, FilenMobileCacheState},
	ffi::{
		FfiId, FfiNonRootObject, FfiObject, FfiRoot, ParsedFfiId, QueryChildrenResponse,
		QueryNonDirChildrenResponse,
	},
	sql::{
		self, DBDirExt, DBDirObject, DBItemTrait, DBRoot,
		error::OptionalExtensionSQL,
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

	pub fn root_uuid(&self) -> Result<String, CacheError> {
		self.sync_execute_authed(|auth_state| Ok(auth_state.root_uuid()))
	}

	/// The current opaque sync anchor (epoch‖seq). The extension stores it and passes it back to
	/// [`Self::enumerate_changes`](crate::auth::FilenMobileCacheState::enumerate_changes).
	pub fn current_sync_anchor(&self) -> Result<Vec<u8>, CacheError> {
		self.sync_execute_authed(|auth_state| auth_state.current_sync_anchor())
	}

	/// The FULL working-set enumeration: favorited (file or dir), recent, or trashed items (see
	/// `select_working_set.sql`). Local-only (no server refresh); pair with
	/// `update_recents`/`update_trash` before calling for freshness.
	///
	/// Materialized items are deliberately NOT enumerated here: the system tracks its own materialized
	/// set and merges it into the working set, and any materialized item we pull a change into surfaces
	/// through the working-set CHANGE delta (`select_changed_workingset` returns every row with
	/// `seq > anchor`, regardless of favorite/recent status — see `enumerate_changes`). So
	/// favorited + recent (+ trash) is the correct proactive-refresh surface; enumerating materialized
	/// items would additionally require tracking materialization in the cache, which we don't do.
	pub fn query_working_set_items(&self) -> Result<Vec<FfiNonRootObject>, CacheError> {
		self.sync_execute_authed(|auth_state| auth_state.query_working_set_items())
	}

	/// uuids of every materialized (already-listed) dir. The reconnect handler re-lists these so a
	/// remote change to a deep folder made during a socket-down window surfaces, not just root-level
	/// ones. See [`crate::socket`] / the Swift `SocketNotifier.onReconnect`.
	pub fn query_materialized_dir_uuids(&self) -> Result<Vec<String>, CacheError> {
		self.sync_execute_authed(|auth_state| auth_state.query_materialized_dir_uuids())
	}
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
		let path = self.canonicalize_ffi_id(path)?;
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

	/// One page of a directory's children, rows `[offset, offset + limit)` in `order_by` order.
	/// Local only (no server refresh) — the File Provider enumerator refreshes on the first page via
	/// [`AuthCacheState::update_and_query_dir_children_page`] and pages the rest from cache.
	pub(crate) fn query_dir_children_page(
		&self,
		path: &FfiId,
		order_by: Option<String>,
		offset: u32,
		limit: u32,
	) -> Result<Option<QueryChildrenResponse>, CacheError> {
		let path = self.canonicalize_ffi_id(path)?;
		let path_id = path.as_path()?;
		let dir: DBDirObject = match sql::select_object_at_path(&self.conn(), &path_id)? {
			Some(obj) => obj.try_into()?,
			None => return Ok(None),
		};
		let conn = self.conn();
		let children = dir.select_children_page(&conn, order_by.as_deref(), limit, offset)?;
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

	pub(crate) fn query_item(&self, path: &FfiId) -> Result<Option<FfiObject>, CacheError> {
		let path = self.canonicalize_ffi_id(path)?;
		debug!("Querying item at path: {}", path.0);
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
		let uuid = UuidStr::from_str(uuid)?;
		let conn = self.conn();
		let uuid = self.resolve_uuid(&conn, uuid.into())?;
		Ok(DBObject::select(&conn, uuid).optional()?.map(Into::into))
	}

	pub(crate) fn query_path_for_uuid(&self, uuid: String) -> Result<Option<FfiId>, CacheError> {
		debug!("Querying path for UUID: {uuid}");
		if uuid == self.client.root().uuid().to_string() {
			return Ok(Some(uuid.into()));
		}
		let uuid = Uuid::from_str(&uuid)?;
		let conn = self.conn();
		let uuid = self.resolve_uuid(&conn, uuid)?;
		let path = sql::recursive_select_path_from_uuid(&conn, uuid)?;

		Ok(path.map(Into::into))
	}

	pub(crate) fn get_all_descendant_paths(&self, path: &FfiId) -> Result<Vec<FfiId>, CacheError> {
		let path = self.canonicalize_ffi_id(path)?;
		debug!("Getting all descendant paths for: {}", path.0);
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
		let uuid = Uuid::from_str(uuid)?;
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
		let path = self.canonicalize_ffi_id(&path)?;
		debug!(
			"Setting {key} to {value:?} for local data for path: {}",
			path.0
		);

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

		sql::update_local_data(
			&mut self.conn(),
			obj.uuid(),
			if local_data.is_empty() {
				None
			} else {
				Some(&local_data)
			},
		)?;
		obj.set_local_data(Some(local_data));

		Ok(FfiObject::from(DBObject::from(obj)))
	}

	pub(crate) fn root_uuid(&self) -> String {
		self.client.root().uuid().to_string()
	}

	pub(crate) fn current_sync_anchor(&self) -> Result<Vec<u8>, CacheError> {
		debug!("Reading current sync anchor");
		let conn = self.conn();
		Ok(sql::changes::current_anchor(&conn)?.to_bytes())
	}

	pub(crate) fn query_working_set_items(&self) -> Result<Vec<FfiNonRootObject>, CacheError> {
		debug!("Querying working set items");
		let conn = self.conn();
		Ok(sql::changes::select_working_set(&conn)?
			.into_iter()
			.map(Into::into)
			.collect())
	}

	pub(crate) fn query_materialized_dir_uuids(&self) -> Result<Vec<String>, CacheError> {
		debug!("Querying materialized dir uuids");
		let conn = self.conn();
		Ok(sql::select_materialized_dir_uuids(&conn)?
			.into_iter()
			.map(|uuid| uuid.to_string())
			.collect())
	}

	/// Resolves a stable-or-real uuid to the real uuid of the row it identifies. Returns the input
	/// unchanged when it is not any row's `stable_uuid` (i.e. it is already a real uuid, or unknown).
	pub(crate) fn resolve_uuid(&self, conn: &Connection, uuid: Uuid) -> Result<Uuid, CacheError> {
		Ok(sql::select_uuid_by_stable_uuid(conn, uuid)?.unwrap_or(uuid))
	}

	/// Canonicalizes an [`FfiId`] for the exported methods that accept path-like ids.
	///
	/// Non-`uuid/<uuid>` ids pass through untouched. A `uuid/<uuid>` id (where `<uuid>` may be a
	/// stable or real uuid) is resolved to its canonical form: the drive root uuid becomes a bare
	/// root id, a trash-parented item becomes `trash/<real-uuid>`, and any other item becomes its
	/// full path id. An unknown uuid yields [`CacheError::DoesNotExist`].
	pub(crate) fn canonicalize_ffi_id(&self, id: &FfiId) -> Result<FfiId, CacheError> {
		let stable_or_real = match id.as_parsed()? {
			ParsedFfiId::Uuid(uuid_id) => uuid_id.uuid.ok_or_else(|| {
				CacheError::DoesNotExist(format!("uuid id is missing a uuid: {}", id.0).into())
			})?,
			ParsedFfiId::Trash(_) | ParsedFfiId::Recents(_) | ParsedFfiId::Path(_) => {
				return Ok(id.clone());
			}
		};

		let conn = self.conn();
		let real_uuid = self.resolve_uuid(&conn, stable_or_real)?;
		let root_uuid = self.client.root().uuid();
		if real_uuid == root_uuid {
			return Ok(FfiId(root_uuid.to_string()));
		}

		// Determine the container by climbing `items.parent`, not by string-matching the first segment
		// of a recursively-built path: an item with a broken/uncached ancestor chain (normal for
		// `update_recents` inserts) is a genuine error, never a trash-parented item.
		match sql::classify_item_container(&conn, real_uuid, root_uuid)? {
			// A trash-parented item addresses by `trash/<real-uuid>` regardless of nesting depth.
			sql::ItemContainer::Trash => Ok(FfiId(format!("trash/{real_uuid}"))),
			// A root-reachable item addresses by its full path INCLUDING the root uuid
			// (e.g. "<root>/dir/file").
			sql::ItemContainer::Root => {
				let path =
					sql::recursive_select_path_from_uuid(&conn, real_uuid)?.ok_or_else(|| {
						CacheError::DoesNotExist(
							format!("no item found for uuid id: {}", id.0).into(),
						)
					})?;
				Ok(FfiId(path))
			}
		}
	}
}
