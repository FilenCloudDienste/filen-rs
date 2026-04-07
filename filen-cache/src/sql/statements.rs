macro_rules! def_sql_user_version {
	($ver:literal) => {
		pub(crate) const SQL_USER_VERSION: i64 = $ver;
		pub(crate) const SET_USER_VERSION: &str = concat!("PRAGMA user_version = ", $ver, ";");
	};
}

def_sql_user_version!(1);

pub(crate) const VACUUM: &str = "VACUUM;";
pub(crate) const GET_USER_VERSION: &str = "PRAGMA user_version;";

pub(crate) const DIR_UPSERT: &str = include_str!("raw/dir_upsert.sql");

pub(crate) const FILE_UPSERT: &str = include_str!("raw/file_upsert.sql");

pub(crate) const INIT: &str = include_str!("raw/init.sql");

pub(crate) const ITEM_DELETE: &str = include_str!("raw/item_delete.sql");
pub(crate) const ITEM_UPDATE_OWN_ROOT_ID: &str = include_str!("raw/item_update_own_root_id.sql");
pub(crate) const ITEM_UPSERT: &str = include_str!("raw/item_upsert.sql");

pub(crate) const ROOT_INSERT: &str = include_str!("raw/root_insert.sql");
pub(crate) const ROOT_ITEM_INSERT: &str = include_str!("raw/root_item_insert.sql");
