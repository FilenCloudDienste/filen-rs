//! Showing a sync root in File Explorer, unpackaged: the registry keys
//! `StorageProviderSyncRootManager.Register` writes, written by the helper itself as the user (the
//! machine-wide sync root list lets every user create and delete a subkey of their own), and
//! removed once the OS let the root go. The values are `os::cfapi::explorer`'s.

#[cfg(test)]
mod tests;

use std::{io, path::Path};

use tracing::debug;

use super::{
	LOG_TARGET,
	ffi::{self, Hive, RegKey},
};
use crate::{
	branding::Branding,
	helper::{BrandingScope, UserId},
	os::cfapi::{
		WideNul,
		explorer::{
			DELEGATE_FOLDER_CLSID, DESKTOP_NAMESPACE, FOLDER_VALUE_FLAGS, HIDE_DESKTOP_ICONS,
			MARKER_VALUE, PROPERTY_BAG_ATTRIBUTES, SHELL_FOLDER_ATTRIBUTES, SHELL32,
			SORT_ORDER_INDEX, SYNC_ROOT_FLAGS, SYNC_ROOT_MANAGER, USER_CLSIDS, UnbrandablePath,
			brandable_root, icon_resource, namespace_clsid, registry_string, same_root,
			sync_root_key_name,
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
/// the user owns (every user may create keys in that list). A repeat writes the same.
pub(crate) fn brand(root: &Path, user: UserId, branding: &Branding) -> Result<(), ExplorerError> {
	brand_as(&own_sid()?, root, user, branding)
}

/// [`brand`] for the Windows user `sid`.
fn brand_as(
	sid: &str,
	root: &Path,
	user: UserId,
	branding: &Branding,
) -> Result<(), ExplorerError> {
	let root_text = brandable_root(root.to_str().ok_or(ExplorerError::NotUtf8)?)?;
	let clsid = namespace_clsid(sid, user);
	let name = branding.display_name.as_str();
	let icon = icon_resource(branding, |icon| Path::new(icon.as_str()).is_file());
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
	let entry = format!(r"{SYNC_ROOT_MANAGER}\{}", sync_root_key_name(sid, user));
	{
		let key = create(Hive::LocalMachine, &entry)?;
		owned_by(&key, sid)?;
		set_string(&key, Some("DisplayNameResource"), name, true)?;
		set_string(&key, Some("IconResource"), &icon, true)?;
		set_dword(&key, "Flags", SYNC_ROOT_FLAGS)?;
		set_string(&key, Some("NamespaceCLSID"), &clsid, false)?;
	}
	{
		let key = create(Hive::LocalMachine, &format!(r"{entry}\UserSyncRoots"))?;
		owned_by(&key, sid)?;
		set_string(&key, Some(sid), root_text, false)?;
	}
	debug!(target: LOG_TARGET, ?root, %clsid, "showed the root in File Explorer");
	notify(root);
	Ok(())
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

/// Removes what [`brand`] wrote for `user`'s root at `root`, within `scope`, for the Windows
/// user this process runs as: as the user, the sync root list entry, then the navigation-pane
/// entry; elevated, only the sync root list entry. Each only when it names `root`, as another root
/// of the same user has the same names. Every step is tried; the first failure is returned.
pub(crate) fn unbrand(
	root: &Path,
	user: UserId,
	scope: BrandingScope,
) -> Result<(), ExplorerError> {
	unbrand_as(&own_sid()?, root, user, scope)
}

/// [`unbrand`] for the Windows user `sid`.
fn unbrand_as(
	sid: &str,
	root: &Path,
	user: UserId,
	scope: BrandingScope,
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
			[machine, navigation].into_iter().collect()
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
