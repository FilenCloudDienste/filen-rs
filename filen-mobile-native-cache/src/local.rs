use std::{collections::HashMap, str::FromStr, time::Instant};

use filen_sdk_rs::fs::HasUUID;
use filen_types::fs::{ParentUuid, UuidStr};
use log::debug;

use crate::{
	CacheError,
	auth::{AuthCacheState, FilenMobileCacheState},
	ffi::{
		FfiId, FfiNonRootObject, FfiObject, FfiRoot, QueryChildrenResponse,
		QueryNonDirChildrenResponse, SearchQueryArgs, SearchQueryResponseEntry,
	},
	sql::{
		self, DBDirExt, DBDirObject, DBDirTrait, DBItemTrait, DBNonRootObject, DBObject, DBRoot,
		error::OptionalExtensionSQL, json_object::JsonObject,
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

	pub fn query_search(
		&self,
		args: SearchQueryArgs,
	) -> Result<Vec<SearchQueryResponseEntry>, CacheError> {
		self.sync_execute_authed(|auth_state| auth_state.query_search(args))
	}
}

impl AuthCacheState {
	pub(crate) fn query_roots_info(
		&self,
		root_uuid_str: String,
	) -> Result<Option<FfiRoot>, CacheError> {
		debug!("Querying root info for UUID: {root_uuid_str}");
		let conn = self.conn();
		Ok(DBRoot::select(&conn, UuidStr::from_str(&root_uuid_str)?)
			.optional()?
			.map(Into::into))
	}

	pub(crate) fn add_root(&self, root: &str) -> Result<(), CacheError> {
		debug!("Adding root with UUID: {root}");
		let root_uuid = UuidStr::from_str(root)?;
		let mut conn = self.conn();
		sql::insert_root(&mut conn, root_uuid)?;
		Ok(())
	}

	pub(crate) fn query_dir_children(
		&self,
		path: &FfiId,
		order_by: Option<String>,
	) -> Result<Option<QueryChildrenResponse>, CacheError> {
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
		let children = sql::select_children(&self.conn(), order_by.as_deref(), ParentUuid::Trash)?;
		let last_update = *self.last_trash_update.read().unwrap();
		let now = Instant::now();
		Ok(QueryNonDirChildrenResponse {
			objects: children.into_iter().map(Into::into).collect(),
			millis_since_updated: last_update
				.map(|t| now.duration_since(t).as_millis().try_into().unwrap()),
		})
	}

	pub(crate) fn query_item(&self, path: &FfiId) -> Result<Option<FfiObject>, CacheError> {
		debug!("Querying item at path: {}", path.0);
		let path_values = path.as_parsed()?;
		let obj = sql::select_object_at_parsed_id(&self.conn(), &path_values)?;

		let dir_obj = match obj {
			Some(DBObject::Dir(dbdir)) => DBDirObject::Dir(dbdir),
			Some(DBObject::Root(dbroot)) => DBDirObject::Root(dbroot),
			other => return Ok(other.map(Into::into)),
		};
		// stop error for ios complaining that folder doesn't exist
		match std::fs::create_dir_all(self.cache_dir.join(dir_obj.uuid().as_ref())) {
			Ok(_) => {}
			Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
			Err(e) => {
				return Err(CacheError::io(format!(
					"Failed to create directory for {}: {e}",
					dir_obj.uuid()
				)));
			}
		}
		Ok(Some(FfiObject::from(DBObject::from(dir_obj))))
	}

	pub(crate) fn query_path_for_uuid(&self, uuid: String) -> Result<Option<FfiId>, CacheError> {
		debug!("Querying path for UUID: {uuid}");
		if uuid == self.client.root().uuid().as_ref() {
			return Ok(Some(uuid.into()));
		}
		let uuid = UuidStr::from_str(&uuid)?;
		let conn = self.conn();
		let path = sql::recursive_select_path_from_uuid(&conn, uuid)?;

		Ok(path.map(|s| FfiId(format!("{}{}", self.client.root().uuid(), s))))
	}

	pub(crate) fn get_all_descendant_paths(&self, path: &FfiId) -> Result<Vec<FfiId>, CacheError> {
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
		let uuid = UuidStr::from_str(uuid)?;
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

	pub(crate) fn query_search(
		&self,
		args: SearchQueryArgs,
	) -> Result<Vec<SearchQueryResponseEntry>, CacheError> {
		Ok(
			sql::select_search(&self.conn(), &args, self.client.root().uuid())?
				.into_iter()
				.map(|(o, path)| SearchQueryResponseEntry {
					object: FfiNonRootObject::from(o),
					path,
				})
				.collect(),
		)
	}
}
