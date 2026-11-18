//! The registry values File Explorer reads to show a sync root by name, with its status icons, its
//! "Always keep on this device" and "Free up space" commands, and its navigation-pane entry,
//! measured against what `StorageProviderSyncRootManager.Register` writes for an unpackaged
//! process. Pure, so the key names and values are tested on every host; `os::win` writes them.

use crate::{
	branding::{Branding, IconPath},
	helper::{ExistingRoot, UserId},
};

/// The machine-wide sync root list, under `HKEY_LOCAL_MACHINE`. Its ACL lets every user create
/// and delete a subkey of their own.
pub(crate) const SYNC_ROOT_MANAGER: &str =
	r"SOFTWARE\Microsoft\Windows\CurrentVersion\Explorer\SyncRootManager";

/// Where a user's own COM classes are, under `HKEY_CURRENT_USER`.
pub(crate) const USER_CLSIDS: &str = r"Software\Classes\CLSID";

/// The navigation pane's entries, under `HKEY_CURRENT_USER`.
pub(crate) const DESKTOP_NAMESPACE: &str =
	r"Software\Microsoft\Windows\CurrentVersion\Explorer\Desktop\NameSpace";

/// The desktop icons hidden, under `HKEY_CURRENT_USER`: a navigation-pane entry is not on the
/// desktop.
pub(crate) const HIDE_DESKTOP_ICONS: &str =
	r"Software\Microsoft\Windows\CurrentVersion\Explorer\HideDesktopIcons\NewStartPanel";

/// The prefix of every sync root list entry this provider writes; nothing else is ever removed.
pub(crate) const KEY_PREFIX: &str = "Filen!";

/// The value marking a navigation-pane class as this provider's, so one left behind is found.
pub(crate) const MARKER_VALUE: &str = "FilenCloudFiles";

/// The system's cloud icon, which Windows ships: the icon of a root whose parent names none.
pub(crate) const DEFAULT_ICON: &str = r"%SystemRoot%\system32\imageres.dll,-1043";

/// The sync root list entry's `Flags`: without both bits Explorer offers neither command
/// (measured: 0x2, 0x20 and 0 give none).
pub(crate) const SYNC_ROOT_FLAGS: u32 = 0x22;

/// The shell's delegate folder class a navigation-pane entry instantiates.
pub(crate) const DELEGATE_FOLDER_CLSID: &str = "{0E5AAE11-A475-4c5b-AB00-C66DE400274E}";

/// The server the entry's class is served by.
pub(crate) const SHELL32: &str = r"%SystemRoot%\system32\shell32.dll";

/// `Instance\InitPropertyBag`'s `Attributes`: a folder, read-only to the shell's rename.
pub(crate) const PROPERTY_BAG_ATTRIBUTES: u32 = 0x11;

/// `ShellFolder`'s `FolderValueFlags`.
pub(crate) const FOLDER_VALUE_FLAGS: u32 = 0x228;

/// `ShellFolder`'s `Attributes`: `SFGAO_*` of a file-system folder with subfolders.
pub(crate) const SHELL_FOLDER_ATTRIBUTES: u32 = 0xF080_004D;

/// The navigation pane's order of the entry: where OneDrive's sits.
pub(crate) const SORT_ORDER_INDEX: u32 = 0x42;

/// The sync root list entry of `user`'s root for the Windows user `sid`: `Filen!<SID>!<userId>`.
pub(crate) fn sync_root_key_name(sid: &str, user: UserId) -> String {
	format!("{KEY_PREFIX}{sid}!{user}")
}

/// Whether `name` is a sync root list entry this provider wrote for the Windows user `sid`.
pub(crate) fn is_own_key(name: &str, sid: &str) -> bool {
	name.strip_prefix(KEY_PREFIX)
		.and_then(|rest| rest.strip_prefix(sid))
		.and_then(|rest| rest.strip_prefix('!'))
		.is_some_and(|user| UserId::parse(user).is_some())
}

/// The class of `user`'s navigation-pane entry for the Windows user `sid`, as the registry spells
/// it: `{XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX}`. Derived, so it is the same on every run and
/// needs storing nowhere: the first 16 bytes of a BLAKE3 hash, with the variant and version bits
/// of an RFC 9562 version 8 UUID.
pub(crate) fn namespace_clsid(sid: &str, user: UserId) -> String {
	let mut hasher = blake3::Hasher::new();
	hasher.update(b"filen-cloud-files navigation pane\0");
	hasher.update(sid.as_bytes());
	hasher.update(b"\0");
	hasher.update(user.to_string().as_bytes());
	let hash = hasher.finalize();
	let mut bytes: [u8; 16] = *hash
		.as_bytes()
		.first_chunk()
		.expect("a BLAKE3 hash is 32 bytes (should be impossible)");
	bytes[6] = (bytes[6] & 0x0F) | 0x80;
	bytes[8] = (bytes[8] & 0x3F) | 0x80;
	let hex: String = bytes.iter().map(|byte| format!("{byte:02X}")).collect();
	// Every slice is of the 32 ASCII hex digits.
	format!(
		"{{{}-{}-{}-{}-{}}}",
		hex.get(0..8).unwrap_or_default(),
		hex.get(8..12).unwrap_or_default(),
		hex.get(12..16).unwrap_or_default(),
		hex.get(16..20).unwrap_or_default(),
		hex.get(20..32).unwrap_or_default()
	)
}

/// The icon resource string Explorer reads (`<path>,<index>`): the branding's icon when its file
/// `exists`, else the system's cloud icon.
pub(crate) fn icon_resource(branding: &Branding, exists: impl Fn(&IconPath) -> bool) -> String {
	match &branding.icon {
		Some(icon) if exists(icon) => format!("{},{}", icon.as_str(), branding.icon_index),
		Some(_) | None => DEFAULT_ICON.to_owned(),
	}
}

/// Why a root's path cannot go in the registry's expandable strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the root's path holds a %, which the registry would read as a variable")]
pub(crate) struct UnbrandablePath;

/// `root`, checked to go unchanged through `REG_EXPAND_SZ`, as `TargetFolderPath` is written.
pub(crate) fn brandable_root(root: &str) -> Result<&str, UnbrandablePath> {
	if root.contains('%') {
		Err(UnbrandablePath)
	} else {
		Ok(root)
	}
}

/// `text` as a registry string takes it, NUL-terminated; `None` when it holds a NUL.
pub(crate) fn registry_string(text: &str) -> Option<super::WideNul> {
	let units: Vec<u16> = text.encode_utf16().collect();
	super::WideNul::new(&units)
}

/// Whether two paths the registry holds name the same root: Windows compares paths without case.
pub(crate) fn same_root(a: &str, b: &str) -> bool {
	let trim = |path: &str| path.trim_end_matches('\\').to_lowercase();
	trim(a) == trim(b)
}

/// Whether the sweep leaves what shows a root in File Explorer, the OS having reported
/// `registration` of its path: unless the OS positively holds no registration of this provider's
/// there. One that cannot be read counts as live, and so does a root on a volume not present
/// (`volume_present`), whose registration the OS cannot report: nothing is removed on a doubt.
pub(crate) fn sweep_keeps<E>(
	registration: &Result<Option<ExistingRoot>, E>,
	volume_present: impl FnOnce() -> bool,
) -> bool {
	match registration {
		Ok(Some(ExistingRoot { ours: true, .. })) | Err(_) => true,
		Ok(Some(ExistingRoot { ours: false, .. })) => false,
		Ok(None) => !volume_present(),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	const SID: &str = "S-1-5-21-3291749577-2867003985-2111167235-1000";

	#[test]
	fn the_sync_root_key_names_the_provider_the_windows_user_and_the_filen_user() {
		assert_eq!(
			sync_root_key_name(SID, UserId::from(42)),
			"Filen!S-1-5-21-3291749577-2867003985-2111167235-1000!42"
		);
	}

	#[test]
	fn only_keys_of_this_provider_and_this_windows_user_are_its_own() {
		let own = sync_root_key_name(SID, UserId::from(42));
		assert!(is_own_key(&own, SID));
		assert!(!is_own_key(&own, "S-1-5-21-1-2-3-1001"));
		assert!(!is_own_key(&format!("{SID}!42"), SID));
		assert!(!is_own_key(&format!("FilenProbe!{SID}!42"), SID));
		assert!(!is_own_key(&format!("Filen!{SID}!"), SID));
		assert!(!is_own_key(&format!("Filen!{SID}!x"), SID));
		assert!(!is_own_key(&format!("Filen!{SID}0!42"), SID));
	}

	#[test]
	fn the_navigation_pane_class_is_derived_the_same_on_every_run() {
		let clsid = namespace_clsid(SID, UserId::from(42));
		assert_eq!(clsid, namespace_clsid(SID, UserId::from(42)));
		assert_eq!(clsid.len(), 38);
		assert!(clsid.starts_with('{') && clsid.ends_with('}'));
		let parts: Vec<&str> = clsid.trim_matches(['{', '}']).split('-').collect();
		assert_eq!(
			parts.iter().map(|part| part.len()).collect::<Vec<_>>(),
			[8, 4, 4, 4, 12]
		);
		assert!(parts.iter().all(|part| {
			part.bytes()
				.all(|b| b.is_ascii_digit() || (b'A'..=b'F').contains(&b))
		}));
		assert!(parts[2].starts_with('8'));
		assert!(matches!(parts[3].as_bytes()[0], b'8' | b'9' | b'A' | b'B'));
	}

	#[test]
	fn each_user_has_a_navigation_pane_class_of_its_own() {
		let a = namespace_clsid(SID, UserId::from(42));
		assert_ne!(a, namespace_clsid(SID, UserId::from(43)));
		assert_ne!(a, namespace_clsid("S-1-5-21-1-2-3-1001", UserId::from(42)));
	}

	#[test]
	fn the_icon_resource_names_the_icon_when_its_file_is_there() {
		let branding = Branding::parse(None, Some(r"C:\Filen\Filen.exe"), Some(-3), None)
			.unwrap()
			.unwrap();
		assert_eq!(icon_resource(&branding, |_| true), r"C:\Filen\Filen.exe,-3");
		assert_eq!(icon_resource(&branding, |_| false), DEFAULT_ICON);
		assert_eq!(icon_resource(&Branding::default(), |_| true), DEFAULT_ICON);
	}

	#[test]
	fn a_root_with_a_percent_sign_is_not_branded() {
		assert_eq!(brandable_root(r"C:\Users\u\Filen"), Ok(r"C:\Users\u\Filen"));
		assert_eq!(brandable_root(r"C:\Users\%u%\Filen"), Err(UnbrandablePath));
	}

	#[test]
	fn the_sweep_removes_only_what_the_os_positively_holds_no_root_of_this_providers_for() {
		let ours = ExistingRoot {
			ours: true,
			account_root: None,
		};
		let other = ExistingRoot {
			ours: false,
			account_root: None,
		};
		let cases: [(Result<Option<ExistingRoot>, ()>, bool, bool); 7] = [
			(Ok(Some(ours)), true, true),
			(Ok(Some(other)), true, false),
			(Ok(None), true, false),
			// A root on a volume not mounted has no registration the OS can read.
			(Ok(None), false, true),
			(Err(()), true, true),
			(Err(()), false, true),
			(Ok(Some(ours)), false, true),
		];
		for (registration, volume_present, keeps) in cases {
			assert_eq!(
				sweep_keeps(&registration, || volume_present),
				keeps,
				"{registration:?} volume present {volume_present}"
			);
		}
	}

	#[test]
	fn roots_compare_without_case_and_trailing_separator() {
		assert!(same_root(r"C:\Users\u\Filen", r"c:\users\U\filen\"));
		assert!(!same_root(r"C:\Users\u\Filen", r"C:\Users\u\Filen2"));
	}
}
