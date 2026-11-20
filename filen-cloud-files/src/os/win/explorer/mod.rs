//! Showing a sync root in File Explorer, unpackaged: the registry keys
//! `StorageProviderSyncRootManager.Register` writes, written by the helper itself as the user (the
//! machine-wide sync root list lets every user create and delete a subkey of their own), and
//! removed once the OS let the root go; the context menu commands the parent names, per user,
//! removed with the user's last root; and, while the process serves thumbnails (`com`), the
//! thumbnail class per user, removed with the user's last root, and the root's entry naming it.
//! The values are `os::cfapi::explorer`'s.

#[cfg(test)]
mod tests;

use std::{io, path::Path};

use tracing::{debug, warn};

use super::{
	LOG_TARGET, com,
	ffi::{self, Hive, RegKey},
};
use crate::{
	branding::{Branding, ExplorerCommands},
	helper::{BrandingScope, UserId},
	os::cfapi::{
		WideNul,
		explorer::{
			COMMAND_KEY, CommandData, CommandRemoval, DELEGATE_FOLDER_CLSID, DESKTOP_NAMESPACE,
			FOLDER_VALUE_FLAGS, HIDE_DESKTOP_ICONS, MARKER_VALUE, PROPERTY_BAG_ATTRIBUTES,
			SHELL_FOLDER_ATTRIBUTES, SHELL32, SORT_ORDER_INDEX, SYNC_ROOT_FLAGS, SYNC_ROOT_MANAGER,
			THUMBNAIL_CLASS_NAME, THUMBNAIL_PROVIDER_VALUE, USER_CLSIDS, UnbrandablePath,
			brandable_root, command_removal, command_trees, command_values, icon_resource,
			is_own_key, namespace_clsid, registry_string, same_root, sweeps_navigation_class,
			sync_root_key_name, thumbnail_class_key, thumbnail_clsid_text,
			thumbnail_server_command, thumbnail_server_exe, thumbnail_value_is_stale,
		},
	},
};

/// Why File Explorer could not be told about a root.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ExplorerError {
	/// A registry or token call failed.
	#[error("{context}: {source}")]
	Io {
		/// What was done.
		context: &'static str,
		#[source]
		source: io::Error,
	},
	/// A string holds a NUL, which no registry string can.
	#[error("a registry string would hold a NUL")]
	Nul,
	/// The root's path is not UTF-8.
	#[error("the root's path is not UTF-8")]
	NotUtf8,
	/// The root's path holds a `%`.
	#[error(transparent)]
	Unbrandable(#[from] UnbrandablePath),
	/// The user's sync root list entry is owned by another account, which may rewrite it: one
	/// created ahead of the user, which is left as it is.
	#[error("the root's sync root list entry is owned by another account")]
	NotOwned,
}

fn io(context: &'static str) -> impl FnOnce(io::Error) -> ExplorerError {
	move |source| ExplorerError::Io { context, source }
}

fn wide(text: &str) -> Result<WideNul, ExplorerError> {
	registry_string(text).ok_or(ExplorerError::Nul)
}

/// The key `path` below `hive`, created when missing.
fn create(hive: Hive, path: &str) -> Result<RegKey, ExplorerError> {
	ffi::reg_create(hive, &wide(path)?).map_err(io("creating a registry key"))
}

fn set_string(
	key: &RegKey,
	name: Option<&str>,
	value: &str,
	expand: bool,
) -> Result<(), ExplorerError> {
	let name = name.map(wide).transpose()?;
	ffi::reg_set_string(key, name.as_ref(), &wide(value)?, expand)
		.map_err(io("writing a registry string"))
}

fn set_dword(key: &RegKey, name: &str, value: u32) -> Result<(), ExplorerError> {
	ffi::reg_set_dword(key, &wide(name)?, value).map_err(io("writing a registry number"))
}

fn delete_tree(hive: Hive, path: &str) -> Result<(), ExplorerError> {
	ffi::reg_delete_tree(hive, &wide(path)?).map_err(io("deleting a registry key"))
}

/// Whether the open key `key` is owned by the Windows user `sid`: [`ExplorerError::NotOwned`]
/// otherwise.
fn owned_by(key: &RegKey, sid: &str) -> Result<(), ExplorerError> {
	let owner = ffi::reg_owner_sid(key).map_err(io("reading a registry key's owner"))?;
	if String::from_utf16_lossy(&owner) == sid {
		Ok(())
	} else {
		Err(ExplorerError::NotOwned)
	}
}

/// Whether the key `path` below `HKEY_LOCAL_MACHINE` is owned by the Windows user `sid`, or is not
/// there: [`ExplorerError::NotOwned`] otherwise. Nothing is created.
fn owned_or_absent(path: &str, sid: &str) -> Result<(), ExplorerError> {
	match ffi::reg_open(Hive::LocalMachine, &wide(path)?).map_err(io("opening a registry key"))? {
		Some(key) => owned_by(&key, sid),
		None => Ok(()),
	}
}

/// The SID of the Windows user this process runs as.
fn own_sid() -> Result<String, ExplorerError> {
	let units = ffi::current_user_sid().map_err(io("reading the process's user"))?;
	String::from_utf16(&units).map_err(|_| ExplorerError::Io {
		context: "reading the process's user",
		source: io::Error::from(io::ErrorKind::InvalidData),
	})
}

/// Tells the windows showing `path` to read it again.
fn notify(path: &Path) {
	if let Some(path) = path.to_str().and_then(registry_string) {
		ffi::notify_dir_updated(&path);
	}
}

/// Writes every key that shows `user`'s root at `root` in File Explorer as `branding` says, for
/// the Windows user this process runs as: the navigation-pane class first, marked as this
/// provider's so one a stopped run left is found, then its navigation-pane entry, then the sync
/// root list entry, which gives the root its status icons and commands, written only into keys
/// the user owns (every user may create keys in that list), and nothing at all while one is there
/// that another account owns; last, when `branding` names them and
/// the app is there, the context menu commands, which the entry's provider id scopes. A repeat
/// writes the same. A failure to write the commands is logged and fails nothing: what shows the
/// root is written.
///
/// Then, while this process serves thumbnails, the thumbnail class and the entry's value naming
/// it; otherwise (its server did not start) the value is removed, so Explorer goes back to its own
/// thumbnails. A failure there is logged and fails nothing either.
pub(crate) fn brand(root: &Path, user: UserId, branding: &Branding) -> Result<(), ExplorerError> {
	let sid = own_sid()?;
	brand_as(&sid, root, user, branding, COMMAND_KEY)?;
	let server = if com::is_serving() {
		match std::env::current_exe() {
			Ok(exe) => match exe.to_str().and_then(thumbnail_server_command) {
				Some(command) => Some(command),
				None => {
					warn!(target: LOG_TARGET, ?exe, "the helper's path cannot start the thumbnail class");
					None
				}
			},
			Err(error) => {
				warn!(target: LOG_TARGET, %error, "the helper's path is not known; no thumbnail class");
				None
			}
		}
	} else {
		None
	};
	let (key, clsid) = (thumbnail_class_key(), thumbnail_clsid_text());
	let class = ThumbnailClass {
		key: &key,
		clsid: &clsid,
	};
	if let Err(error) = set_thumbnails_as(&sid, user, class, server.as_deref()) {
		warn!(target: LOG_TARGET, %error, "registering File Explorer's thumbnail class failed");
	}
	Ok(())
}

/// Removes the value naming the thumbnail class from every sync root list entry of this provider
/// for the Windows user this process runs as: what a process whose server did not start does, so
/// File Explorer goes back to its own thumbnails on the roots it does not brand too. Every entry is
/// tried; the first failure is returned.
pub(crate) fn remove_thumbnail_values() -> Result<(), ExplorerError> {
	remove_thumbnail_values_as(&own_sid()?)
}

/// [`remove_thumbnail_values`] for the Windows user `sid`.
fn remove_thumbnail_values_as(sid: &str) -> Result<(), ExplorerError> {
	let Some((_, names)) = subkeys(Hive::LocalMachine, SYNC_ROOT_MANAGER)? else {
		return Ok(());
	};
	let value = wide(THUMBNAIL_PROVIDER_VALUE)?;
	let mut first = Ok(());
	for name in names.iter().filter(|name| is_own_key(name, sid)) {
		let removed = wide(&format!(r"{SYNC_ROOT_MANAGER}\{name}")).and_then(|entry| {
			ffi::reg_delete_value(Hive::LocalMachine, &entry, &value)
				.map_err(io("deleting a registry value"))
		});
		if let Err(error) = removed
			&& first.is_ok()
		{
			first = Err(error);
		}
	}
	first
}

/// Removes the value naming the thumbnail class from `user`'s sync root list entry, for the Windows
/// user this process runs as: what the helper does once it stops serving the root because the user
/// signed out, so File Explorer goes back to its own thumbnails there. The class stays while the
/// entry does ([`remove_thumbnail_class_unless_rooted`]). A value not there is gone already.
pub(crate) fn remove_thumbnail_value(user: UserId) -> Result<(), ExplorerError> {
	remove_thumbnail_value_as(&own_sid()?, user)
}

/// [`remove_thumbnail_value`] for the Windows user `sid`.
fn remove_thumbnail_value_as(sid: &str, user: UserId) -> Result<(), ExplorerError> {
	let entry = format!(r"{SYNC_ROOT_MANAGER}\{}", sync_root_key_name(sid, user));
	ffi::reg_delete_value(
		Hive::LocalMachine,
		&wide(&entry)?,
		&wide(THUMBNAIL_PROVIDER_VALUE)?,
	)
	.map_err(io("deleting a registry value"))
}

/// Where a thumbnail class is registered: its key under `HKEY_CURRENT_USER`, and the class id the
/// root's entry names.
#[derive(Debug, Clone, Copy)]
struct ThumbnailClass<'a> {
	key: &'a str,
	clsid: &'a str,
}

/// With `server`, the class's local server command line: writes `class`, marked as this
/// provider's, with its `LocalServer32`, then the value of `user`'s sync root list entry naming it,
/// in that order, so Explorer never reads a value naming a class not registered. Without, removes
/// the entry's value and leaves the class to [`remove_thumbnail_class_unless_rooted`]. The entry is
/// [`brand_as`]'s, owned by the Windows user `sid`.
fn set_thumbnails_as(
	sid: &str,
	user: UserId,
	class: ThumbnailClass<'_>,
	server: Option<&str>,
) -> Result<(), ExplorerError> {
	let Some(server) = server else {
		return remove_thumbnail_value_as(sid, user);
	};
	let entry = format!(r"{SYNC_ROOT_MANAGER}\{}", sync_root_key_name(sid, user));
	{
		let key = create(Hive::CurrentUser, class.key)?;
		set_string(&key, None, THUMBNAIL_CLASS_NAME, false)?;
		set_dword(&key, MARKER_VALUE, 1)?;
	}
	set_string(
		&create(Hive::CurrentUser, &format!(r"{}\LocalServer32", class.key))?,
		None,
		server,
		false,
	)?;
	let key = create(Hive::LocalMachine, &entry)?;
	owned_by(&key, sid)?;
	set_string(&key, Some(THUMBNAIL_PROVIDER_VALUE), class.clsid, false)?;
	debug!(target: LOG_TARGET, clsid = class.clsid, "registered File Explorer's thumbnail class");
	Ok(())
}

/// Removes the thumbnail class at `class` unless the Windows user `sid` still has a sync root list
/// entry of this provider, and only when it carries this provider's marker: whether it was
/// removed. A list that cannot be read removes nothing.
fn remove_thumbnail_class_unless_rooted(sid: &str, class: &str) -> Result<bool, ExplorerError> {
	if let Some((_, names)) = subkeys(Hive::LocalMachine, SYNC_ROOT_MANAGER)?
		&& names.iter().any(|name| is_own_key(name, sid))
	{
		return Ok(false);
	}
	let Some(key) =
		ffi::reg_open(Hive::CurrentUser, &wide(class)?).map_err(io("opening a registry key"))?
	else {
		return Ok(false);
	};
	let marked = ffi::reg_query_dword(&key, &wide(MARKER_VALUE)?)
		.map_err(io("reading a registry value"))?
		== Some(1);
	drop(key);
	if !marked {
		return Ok(false);
	}
	debug!(target: LOG_TARGET, %class, "removing File Explorer's thumbnail class");
	delete_tree(Hive::CurrentUser, class)?;
	Ok(true)
}

/// [`brand`] for the Windows user `sid`, the commands under the key name `commands`.
fn brand_as(
	sid: &str,
	root: &Path,
	user: UserId,
	branding: &Branding,
	commands: &str,
) -> Result<(), ExplorerError> {
	let root_text = brandable_root(root.to_str().ok_or(ExplorerError::NotUtf8)?)?;
	let clsid = namespace_clsid(sid, user);
	let name = branding.display_name.as_str();
	let icon = icon_resource(branding, |icon| Path::new(icon.as_str()).is_file());
	let entry = format!(r"{SYNC_ROOT_MANAGER}\{}", sync_root_key_name(sid, user));
	let roots = format!(r"{entry}\UserSyncRoots");
	// Before anything is written: an entry another account created ahead of the user leaves no
	// navigation-pane entry either. Checked again as each key is created.
	owned_or_absent(&entry, sid)?;
	owned_or_absent(&roots, sid)?;
	let class = format!(r"{USER_CLSIDS}\{clsid}");
	{
		let key = create(Hive::CurrentUser, &class)?;
		set_string(&key, None, name, false)?;
		set_dword(&key, "System.IsPinnedToNameSpaceTree", 1)?;
		set_dword(&key, "SortOrderIndex", SORT_ORDER_INDEX)?;
		set_dword(&key, MARKER_VALUE, 1)?;
	}
	set_string(
		&create(Hive::CurrentUser, &format!(r"{class}\DefaultIcon"))?,
		None,
		&icon,
		true,
	)?;
	set_string(
		&create(Hive::CurrentUser, &format!(r"{class}\InProcServer32"))?,
		None,
		SHELL32,
		true,
	)?;
	set_string(
		&create(Hive::CurrentUser, &format!(r"{class}\Instance"))?,
		Some("CLSID"),
		DELEGATE_FOLDER_CLSID,
		false,
	)?;
	{
		let key = create(
			Hive::CurrentUser,
			&format!(r"{class}\Instance\InitPropertyBag"),
		)?;
		set_dword(&key, "Attributes", PROPERTY_BAG_ATTRIBUTES)?;
		set_string(&key, Some("TargetFolderPath"), root_text, true)?;
	}
	{
		let key = create(Hive::CurrentUser, &format!(r"{class}\ShellFolder"))?;
		set_dword(&key, "FolderValueFlags", FOLDER_VALUE_FLAGS)?;
		set_dword(&key, "Attributes", SHELL_FOLDER_ATTRIBUTES)?;
	}
	set_string(
		&create(Hive::CurrentUser, &format!(r"{DESKTOP_NAMESPACE}\{clsid}"))?,
		None,
		name,
		false,
	)?;
	set_dword(&create(Hive::CurrentUser, HIDE_DESKTOP_ICONS)?, &clsid, 1)?;
	{
		let key = create(Hive::LocalMachine, &entry)?;
		owned_by(&key, sid)?;
		set_string(&key, Some("DisplayNameResource"), name, true)?;
		set_string(&key, Some("IconResource"), &icon, true)?;
		set_dword(&key, "Flags", SYNC_ROOT_FLAGS)?;
		set_string(&key, Some("NamespaceCLSID"), &clsid, false)?;
	}
	{
		let key = create(Hive::LocalMachine, &roots)?;
		owned_by(&key, sid)?;
		set_string(&key, Some(sid), root_text, false)?;
	}
	if let Some(offered) = &branding.commands {
		if Path::new(offered.app.as_str()).is_file() {
			if let Err(error) = write_commands(commands, branding, offered) {
				warn!(target: LOG_TARGET, %error, "writing File Explorer's context menu commands failed");
			}
		} else {
			warn!(
				target: LOG_TARGET,
				app = offered.app.as_str(),
				"the app the context menu commands start is not there; they are not written"
			);
		}
	}
	debug!(target: LOG_TARGET, ?root, %clsid, "showed the root in File Explorer");
	notify(root);
	Ok(())
}

/// Writes every value of [`command_values`] for `offered`, under the key name `key`, in order, in
/// place: a repeat writes the same.
fn write_commands(
	key: &str,
	branding: &Branding,
	offered: &ExplorerCommands,
) -> Result<(), ExplorerError> {
	for value in command_values(key, branding, offered, |icon| {
		Path::new(icon.as_str()).is_file()
	}) {
		let at = create(Hive::CurrentUser, &value.key)?;
		match &value.data {
			CommandData::Text { name, text } => set_string(&at, *name, text, false)?,
			CommandData::Number { name, number } => set_dword(&at, name, *number)?,
		}
	}
	Ok(())
}

/// Removes the context menu commands under the key name `key` unless the Windows user `sid` still
/// has a sync root list entry of this provider: each submenu only when it carries this provider's
/// marker, or is a key with no marker, subkey or string value, which a removal cut short at its
/// very end leaves; step by step as
/// [`command_removal`] says, so a removal cut short earlier leaves the marker on what is left. How
/// many were removed; a list that cannot be read removes nothing.
fn remove_commands_unless_rooted(sid: &str, key: &str) -> Result<u64, ExplorerError> {
	if let Some((_, names)) = subkeys(Hive::LocalMachine, SYNC_ROOT_MANAGER)?
		&& names.iter().any(|name| is_own_key(name, sid))
	{
		return Ok(0);
	}
	let mut removed = 0_u64;
	for tree in command_trees(key) {
		let Some(submenu) = ffi::reg_open(Hive::CurrentUser, &wide(&tree)?)
			.map_err(io("opening a registry key"))?
		else {
			continue;
		};
		let marker = ffi::reg_query_dword(&submenu, &wide(MARKER_VALUE)?)
			.map_err(io("reading a registry value"))?;
		let marked = marker == Some(1);
		let empty = marker.is_none()
			&& ffi::reg_subkeys(&submenu)
				.map_err(io("listing registry keys"))?
				.is_empty()
			&& ffi::reg_string_values(&submenu)
				.map_err(io("reading registry values"))?
				.is_empty();
		drop(submenu);
		if marked || empty {
			debug!(target: LOG_TARGET, %tree, "removing File Explorer's context menu commands");
			for step in command_removal(&tree) {
				match step {
					CommandRemoval::Tree(path) => delete_tree(Hive::CurrentUser, &path)?,
					CommandRemoval::Value { key, name } => {
						ffi::reg_delete_value(Hive::CurrentUser, &wide(&key)?, &wide(name)?)
							.map_err(io("deleting a registry value"))?;
					}
				}
			}
			removed = removed.saturating_add(1);
		}
	}
	Ok(removed)
}

/// Removes the navigation-pane entry `clsid` of the Windows user this process runs as: its entry
/// and its hidden desktop icon, then its class, marker last, so a stopped run leaves the marker.
/// Every step is tried; the first failure is returned.
pub(crate) fn remove_navigation_entry(clsid: &str) -> Result<(), ExplorerError> {
	let steps = [
		delete_tree(Hive::CurrentUser, &format!(r"{DESKTOP_NAMESPACE}\{clsid}")),
		wide(HIDE_DESKTOP_ICONS).and_then(|path| {
			ffi::reg_delete_value(Hive::CurrentUser, &path, &wide(clsid)?)
				.map_err(io("deleting a registry value"))
		}),
		delete_tree(Hive::CurrentUser, &format!(r"{USER_CLSIDS}\{clsid}")),
	];
	steps.into_iter().collect()
}

/// Removes what shows in File Explorer roots the OS no longer holds as this provider's, as `live`
/// says of each root's path, for the Windows user this process runs as: its sync root list entries
/// of this provider (`Filen!<SID>!*`) that it owns and its navigation-pane classes carrying this provider's
/// marker, each with its entry and hidden icon, never the thumbnail class, which is no
/// navigation-pane entry; then, once no entry of the user's is left, the thumbnail class and the
/// context menu commands carrying the marker; last, the value of every entry left that names the
/// thumbnail class while the class is not registered (an older build's), so Explorer is not
/// pointed at a class COM cannot start: a helper that serves writes both again when it brands.
/// Nothing else is touched. How many were removed; every one is tried, and the first failure is
/// returned.
pub(crate) fn sweep(live: impl Fn(&Path) -> bool) -> Result<u64, ExplorerError> {
	let (key, clsid) = (thumbnail_class_key(), thumbnail_clsid_text());
	let thumbnails = ThumbnailClass {
		key: &key,
		clsid: &clsid,
	};
	let sid = own_sid()?;
	sweep_as(&sid, &sid, &live, COMMAND_KEY, thumbnails)
}

/// [`sweep`] for the Windows user `sid`, removing only the sync root list entries the Windows user
/// `owner` owns, the commands under the key name `commands` and the thumbnail class `thumbnails`.
fn sweep_as(
	sid: &str,
	owner: &str,
	live: &dyn Fn(&Path) -> bool,
	commands: &str,
	thumbnails: ThumbnailClass<'_>,
) -> Result<u64, ExplorerError> {
	let mut removed = 0_u64;
	let mut first = Ok(());
	let mut note = |result: Result<bool, ExplorerError>| match result {
		Ok(true) => removed = removed.saturating_add(1),
		Ok(false) => {}
		Err(error) => {
			if first.is_ok() {
				first = Err(error);
			}
		}
	};
	match subkeys(Hive::LocalMachine, SYNC_ROOT_MANAGER) {
		Ok(Some((list, names))) => {
			for name in names.iter().filter(|name| is_own_key(name, sid)) {
				note(sweep_entry(&list, name, sid, owner, live));
			}
		}
		Ok(None) => {}
		Err(error) => note(Err(error)),
	}
	match subkeys(Hive::CurrentUser, USER_CLSIDS) {
		Ok(Some((classes, names))) => {
			// This build's thumbnail class is no navigation-pane entry whichever class this sweep
			// was handed: the tests hand it their own.
			let installed = thumbnail_class_key();
			for name in &names {
				if format!(r"{USER_CLSIDS}\{name}").eq_ignore_ascii_case(&installed) {
					continue;
				}
				note(sweep_class(&classes, name, thumbnails.key, live));
			}
		}
		Ok(None) => {}
		Err(error) => note(Err(error)),
	}
	note(remove_thumbnail_class_unless_rooted(sid, thumbnails.key));
	for counted in [
		remove_commands_unless_rooted(sid, commands),
		remove_stale_thumbnail_values_as(sid, thumbnails),
	] {
		match counted {
			Ok(count) => removed = removed.saturating_add(count),
			Err(error) => {
				if first.is_ok() {
					first = Err(error);
				}
			}
		}
	}
	first.map(|()| removed)
}

/// Removes the value naming `class` from each sync root list entry of this provider for the
/// Windows user `sid` while the class has no `LocalServer32` COM could start: none, or one whose
/// command names no exe that is there (see [`thumbnail_value_is_stale`]). How many were removed. Every entry is tried; the first failure
/// is returned.
fn remove_stale_thumbnail_values_as(
	sid: &str,
	class: ThumbnailClass<'_>,
) -> Result<u64, ExplorerError> {
	let registered = match ffi::reg_open(
		Hive::CurrentUser,
		&wide(&format!(r"{}\LocalServer32", class.key))?,
	)
	.map_err(io("opening a registry key"))?
	{
		Some(server) => ffi::reg_query_string(&server, None)
			.map_err(io("reading a registry value"))?
			.is_some_and(|command| {
				thumbnail_server_exe(&String::from_utf16_lossy(&command))
					.is_some_and(|exe| Path::new(exe).is_file())
			}),
		None => false,
	};
	let Some((list, names)) = subkeys(Hive::LocalMachine, SYNC_ROOT_MANAGER)? else {
		return Ok(0);
	};
	let value = wide(THUMBNAIL_PROVIDER_VALUE)?;
	let mut removed = 0_u64;
	let mut first = Ok(());
	for name in names.iter().filter(|name| is_own_key(name, sid)) {
		let stale = wide(name).and_then(|key| {
			let named =
				match ffi::reg_open_subkey(&list, &key).map_err(io("opening a registry key"))? {
					Some(entry) => ffi::reg_query_string(&entry, Some(&value))
						.map_err(io("reading a registry value"))?,
					None => None,
				};
			let named = named.map(|named| String::from_utf16_lossy(&named));
			if !thumbnail_value_is_stale(named.as_deref(), class.clsid, registered) {
				return Ok(false);
			}
			debug!(target: LOG_TARGET, key = %name, "removing a thumbnail value naming no registered class");
			ffi::reg_delete_value(
				Hive::LocalMachine,
				&wide(&format!(r"{SYNC_ROOT_MANAGER}\{name}"))?,
				&value,
			)
			.map_err(io("deleting a registry value"))?;
			Ok(true)
		});
		match stale {
			Ok(true) => removed = removed.saturating_add(1),
			Ok(false) => {}
			Err(error) => {
				if first.is_ok() {
					first = Err(error);
				}
			}
		}
	}
	first.map(|()| removed)
}

/// The key `path` below `hive` and the names of its subkeys; `None` when it is not there.
fn subkeys(hive: Hive, path: &str) -> Result<Option<(RegKey, Vec<String>)>, ExplorerError> {
	let Some(key) = ffi::reg_open(hive, &wide(path)?).map_err(io("opening a registry key"))? else {
		return Ok(None);
	};
	let names = ffi::reg_subkeys(&key)
		.map_err(io("listing registry keys"))?
		.iter()
		.map(|name| String::from_utf16_lossy(name))
		.collect();
	Ok(Some((key, names)))
}

/// Removes this provider's sync root list entry `name` below `list` unless the root it names for
/// the Windows user `sid` is `live`, or the entry is not owned by the Windows user `owner`: one
/// another account created ahead of the user, which is theirs and which the user may not delete.
/// Whether it was removed.
fn sweep_entry(
	list: &RegKey,
	name: &str,
	sid: &str,
	owner: &str,
	live: &dyn Fn(&Path) -> bool,
) -> Result<bool, ExplorerError> {
	let Some(entry) =
		ffi::reg_open_subkey(list, &wide(name)?).map_err(io("opening a registry key"))?
	else {
		return Ok(false);
	};
	match owned_by(&entry, owner) {
		Ok(()) => {}
		Err(ExplorerError::NotOwned) => {
			debug!(target: LOG_TARGET, key = %name, "leaving a sync root list entry another account owns");
			return Ok(false);
		}
		Err(error) => return Err(error),
	}
	drop(entry);
	let root = match ffi::reg_open_subkey(list, &wide(&format!(r"{name}\UserSyncRoots"))?)
		.map_err(io("opening a registry key"))?
	{
		Some(roots) => ffi::reg_query_string(&roots, Some(&wide(sid)?))
			.map_err(io("reading a registry value"))?,
		None => None,
	};
	if root.is_some_and(|root| live(Path::new(&String::from_utf16_lossy(&root)))) {
		return Ok(false);
	}
	debug!(target: LOG_TARGET, key = %name, "removing a sync root list entry left behind");
	delete_tree(Hive::LocalMachine, &format!(r"{SYNC_ROOT_MANAGER}\{name}"))?;
	Ok(true)
}

/// Removes the navigation-pane class `name` below `classes`, with its entry and hidden icon, as
/// [`sweeps_navigation_class`] says: when it carries this provider's marker, is not the thumbnail
/// class at `thumbnails`, and the root it names is not `live`. Whether it was removed.
fn sweep_class(
	classes: &RegKey,
	name: &str,
	thumbnails: &str,
	live: &dyn Fn(&Path) -> bool,
) -> Result<bool, ExplorerError> {
	let Some(class) =
		ffi::reg_open_subkey(classes, &wide(name)?).map_err(io("opening a registry key"))?
	else {
		return Ok(false);
	};
	let marked = ffi::reg_query_dword(&class, &wide(MARKER_VALUE)?)
		.map_err(io("reading a registry value"))?
		== Some(1);
	let root = || -> Result<Option<String>, ExplorerError> {
		let bag = ffi::reg_open_subkey(&class, &wide(r"Instance\InitPropertyBag")?)
			.map_err(io("opening a registry key"))?;
		let Some(bag) = bag else {
			return Ok(None);
		};
		Ok(
			ffi::reg_query_string(&bag, Some(&wide("TargetFolderPath")?))
				.map_err(io("reading a registry value"))?
				.map(|root| String::from_utf16_lossy(&root)),
		)
	};
	let swept = sweeps_navigation_class(
		&format!(r"{USER_CLSIDS}\{name}"),
		thumbnails,
		marked,
		root,
		|root| live(Path::new(root)),
	)?;
	drop(class);
	if !swept {
		return Ok(false);
	}
	// Any key text, not checked as a class name: quoted.
	debug!(target: LOG_TARGET, ?name, "removing a navigation-pane entry left behind");
	remove_navigation_entry(name)?;
	Ok(true)
}

/// Removes what [`brand`] wrote for `user`'s root at `root`, within `scope`, for the Windows
/// user this process runs as: as the user, the sync root list entry, then the navigation-pane
/// entry, and the context menu commands once no sync root list entry of the user's is left;
/// elevated, only the sync root list entry. Each only when it names `root`, as another root of the
/// same user has the same names. The entry's thumbnail value goes with the entry, and, as the user,
/// the thumbnail class with the user's last entry. Every step is tried; the first failure is
/// returned.
pub(crate) fn unbrand(
	root: &Path,
	user: UserId,
	scope: BrandingScope,
) -> Result<(), ExplorerError> {
	unbrand_as(
		&own_sid()?,
		root,
		user,
		scope,
		COMMAND_KEY,
		&thumbnail_class_key(),
	)
}

/// [`unbrand`] for the Windows user `sid`, the commands under the key name `commands` and the
/// thumbnail class at `thumbnails`.
fn unbrand_as(
	sid: &str,
	root: &Path,
	user: UserId,
	scope: BrandingScope,
	commands: &str,
	thumbnails: &str,
) -> Result<(), ExplorerError> {
	let root_text = root.to_str().ok_or(ExplorerError::NotUtf8)?;
	let entry = format!(r"{SYNC_ROOT_MANAGER}\{}", sync_root_key_name(sid, user));
	let machine = holds_root(
		Hive::LocalMachine,
		&format!(r"{entry}\UserSyncRoots"),
		root_text,
	)
	.and_then(|holds| {
		if holds {
			delete_tree(Hive::LocalMachine, &entry)
		} else {
			Ok(())
		}
	});
	let result = match scope {
		BrandingScope::User => {
			let clsid = namespace_clsid(sid, user);
			let bag = format!(r"{USER_CLSIDS}\{clsid}\Instance\InitPropertyBag");
			let navigation = holds_root(Hive::CurrentUser, &bag, root_text).and_then(|holds| {
				if holds {
					remove_navigation_entry(&clsid)
				} else {
					Ok(())
				}
			});
			// After the entry's removal, which the commands and the thumbnail class outlive only
			// while another entry is left.
			let offered = remove_commands_unless_rooted(sid, commands).map(|_| ());
			let thumbnail_class = remove_thumbnail_class_unless_rooted(sid, thumbnails).map(|_| ());
			[machine, navigation, offered, thumbnail_class]
				.into_iter()
				.collect()
		}
		BrandingScope::MachineOnly => machine,
	};
	notify(root);
	if let Some(parent) = root.parent() {
		notify(parent);
	}
	result
}

/// Whether the key `path` below `hive` names `root` in one of its string values, or is not there:
/// what [`brand`] wrote there is `root`'s, or nothing is left to remove.
fn holds_root(hive: Hive, path: &str, root: &str) -> Result<bool, ExplorerError> {
	let Some(key) = ffi::reg_open(hive, &wide(path)?).map_err(io("opening a registry key"))? else {
		return Ok(true);
	};
	has_root_value(&key, root)
}

/// Whether one of `key`'s string values is `root`.
fn has_root_value(key: &RegKey, root: &str) -> Result<bool, ExplorerError> {
	let values = ffi::reg_string_values(key).map_err(io("reading registry values"))?;
	Ok(values
		.iter()
		.any(|(_, path)| same_root(&String::from_utf16_lossy(path), root)))
}
