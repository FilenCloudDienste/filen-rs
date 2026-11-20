//! The registry values File Explorer reads to show a sync root by name, with its status icons, its
//! "Always keep on this device" and "Free up space" commands, and its navigation-pane entry,
//! measured against what `StorageProviderSyncRootManager.Register` writes for an unpackaged
//! process. Pure, so the key names and values are tested on every host; `os::win` writes them.

use crate::{
	branding::{AppPath, Branding, CommandLabel, ExplorerCommands, IconPath},
	helper::{ExistingRoot, THUMBNAIL_SERVER_ARG, UserId},
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

/// This provider's thumbnail class, which the helper exe serves as a COM local server
/// (`os::win::com`): `{CE43ED2E-6C0A-4846-A973-E43FFFACF389}`, minted for it.
pub(crate) const THUMBNAIL_CLSID: u128 = 0xce43_ed2e_6c0a_4846_a973_e43f_ffac_f389;

/// The sync root list entry's value naming the COM class File Explorer asks for the thumbnails of
/// the root's files. Measured: once it is set, Explorer asks only that class, out of process, for
/// every file of the root, hydrated ones included, so it is written only by a build that serves
/// the class.
pub(crate) const THUMBNAIL_PROVIDER_VALUE: &str = "ThumbnailProvider";

/// The thumbnail class's name: its key's default value.
pub(crate) const THUMBNAIL_CLASS_NAME: &str = "Filen thumbnails";

/// [`THUMBNAIL_CLSID`] as the registry spells a class: braced, upper-case hex.
pub(crate) fn thumbnail_clsid_text() -> String {
	let clsid = THUMBNAIL_CLSID;
	format!(
		"{{{:08X}-{:04X}-{:04X}-{:04X}-{:012X}}}",
		clsid >> 96,
		(clsid >> 80) & 0xffff,
		(clsid >> 64) & 0xffff,
		(clsid >> 48) & 0xffff,
		clsid & 0xffff_ffff_ffff
	)
}

/// The thumbnail class's key, under `HKEY_CURRENT_USER`: a class of the user's own.
pub(crate) fn thumbnail_class_key() -> String {
	format!(r"{USER_CLSIDS}\{}", thumbnail_clsid_text())
}

/// The thumbnail class's `LocalServer32` command line, which starts the helper exe at `exe`:
/// quoted, as the path may hold spaces; `None` for a path holding a `"`, which no quoting carries.
pub(crate) fn thumbnail_server_command(exe: &str) -> Option<String> {
	(!exe.contains('"')).then(|| format!("\"{exe}\" {THUMBNAIL_SERVER_ARG}"))
}

/// The exe the `LocalServer32` command line `command` starts, as [`thumbnail_server_command`]
/// writes it: the quoted path at its start; `None` for a command not written that way.
pub(crate) fn thumbnail_server_exe(command: &str) -> Option<&str> {
	let (exe, _) = command.strip_prefix('"')?.split_once('"')?;
	(!exe.is_empty()).then_some(exe)
}

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

/// The provider id File Explorer reports for an item in a root of this provider
/// (`System.StorageProviderId`): the first `!`-separated part of the root's sync root list entry.
/// Measured to exist only while that entry is there; without it Explorer reports "computer".
pub(crate) const PROVIDER_ID: &str = "Filen";

/// The context menu commands' key, below [`FILE_VERBS`] and [`DIRECTORY_VERBS`]: this provider's
/// marker, which it also carries as a value, so only a key this provider wrote is removed.
pub(crate) const COMMAND_KEY: &str = MARKER_VALUE;

/// Where File Explorer reads a user's context menu commands on every file, under
/// `HKEY_CURRENT_USER`.
pub(crate) const FILE_VERBS: &str = r"Software\Classes\*\shell";

/// Where File Explorer reads a user's context menu commands on every folder (a root and the
/// navigation pane's entry included), under `HKEY_CURRENT_USER`. A folder's background takes none:
/// it cannot be limited to a provider's items.
pub(crate) const DIRECTORY_VERBS: &str = r"Software\Classes\Directory\shell";

/// The switch the app's command line names the action with.
const COMMAND_SWITCH: &str = "--filen-explorer-command";

/// The English labels the commands show when the parent names none.
const DEFAULT_SHARE_LABEL: &str = "Share / create public link";
const DEFAULT_WEB_LABEL: &str = "View on filen.io";
const DEFAULT_VERSIONS_LABEL: &str = "Version history";

/// A context menu command the app carries out on an item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CommandAction {
	/// Opens the item's public link dialog.
	Share,
	/// Opens the item in the web app.
	Web,
	/// Opens a file's version history.
	Versions,
}

impl CommandAction {
	/// The action as the app's command line spells it.
	pub(crate) fn as_str(self) -> &'static str {
		match self {
			Self::Share => "share",
			Self::Web => "web",
			Self::Versions => "versions",
		}
	}

	/// The command's key below the submenu's `shell`: numbered, as Explorer orders a submenu by key
	/// name.
	fn key(self) -> &'static str {
		match self {
			Self::Share => "01share",
			Self::Web => "02web",
			Self::Versions => "03versions",
		}
	}
}

/// The command line File Explorer runs for `action` on an item: the app, quoted, the action, and
/// the item's full path quoted as Explorer substitutes it for `%1`.
pub(crate) fn command_line(app: &AppPath, action: CommandAction) -> String {
	format!(
		"\"{}\" {COMMAND_SWITCH}={} \"%1\"",
		app.as_str(),
		action.as_str()
	)
}

/// A registry value the commands write: never expandable, so no `%` is read as a variable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CommandData {
	/// A `REG_SZ`, the key's default value when `name` is `None`.
	Text {
		/// The value's name.
		name: Option<&'static str>,
		/// What it holds.
		text: String,
	},
	/// A `REG_DWORD`.
	Number {
		/// The value's name.
		name: &'static str,
		/// What it holds.
		number: u32,
	},
}

/// One value the commands write, in the key `key` below `HKEY_CURRENT_USER`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommandValue {
	/// The key, below `HKEY_CURRENT_USER`.
	pub(crate) key: String,
	/// The value.
	pub(crate) data: CommandData,
}

/// `label` as a menu shows it unchanged: a `&` would mark the next letter as the item's
/// accelerator, so each is doubled.
fn menu_text(label: &str) -> String {
	label.replace('&', "&&")
}

/// The two submenus' keys below `HKEY_CURRENT_USER`, named `key`: on files, then on folders.
pub(crate) fn command_trees(key: &str) -> [String; 2] {
	[
		format!(r"{FILE_VERBS}\{key}"),
		format!(r"{DIRECTORY_VERBS}\{key}"),
	]
}

/// The value that limits a submenu to the items of this provider's roots.
const APPLIES_TO: &str = "AppliesTo";

/// The values that show a submenu, as [`command_values`] writes them.
const SUBMENU_VALUES: [&str; 4] = ["MUIVerb", "Icon", "MultiSelectModel", "SubCommands"];

/// Every value that offers `commands` in File Explorer's context menu on the items of this
/// provider's roots, under the submenus named `key`, in the order they are written: in each
/// submenu, first the marker, so a key a write cut short leaves is still found and removed, then
/// the scope (`AppliesTo`), so it never leaves a command offered on every item, then the
/// commands, then what shows the submenu. The submenu is
/// labelled with `commands`' menu label or `branding`'s display name, and shows `branding`'s icon
/// when its file `exists`, else the app's. Folders get no version history.
pub(crate) fn command_values(
	key: &str,
	branding: &Branding,
	commands: &ExplorerCommands,
	exists: impl Fn(&IconPath) -> bool,
) -> Vec<CommandValue> {
	let label = |label: Option<&CommandLabel>, default: &str| {
		menu_text(label.map_or(default, CommandLabel::as_str))
	};
	let menu = label(commands.menu.as_ref(), branding.display_name.as_str());
	let icon = match &branding.icon {
		Some(icon) if exists(icon) => format!("{},{}", icon.as_str(), branding.icon_index),
		Some(_) | None => format!("{},0", commands.app.as_str()),
	};
	let actions = [
		(
			CommandAction::Share,
			label(commands.share.as_ref(), DEFAULT_SHARE_LABEL),
		),
		(
			CommandAction::Web,
			label(commands.web.as_ref(), DEFAULT_WEB_LABEL),
		),
		(
			CommandAction::Versions,
			label(commands.versions.as_ref(), DEFAULT_VERSIONS_LABEL),
		),
	];
	let text = |key: &str, name: Option<&'static str>, text: &str| CommandValue {
		key: key.to_owned(),
		data: CommandData::Text {
			name,
			text: text.to_owned(),
		},
	};
	let [files, folders] = command_trees(key);
	let mut values = Vec::new();
	for (tree, on_files) in [(files, true), (folders, false)] {
		values.push(CommandValue {
			key: tree.clone(),
			data: CommandData::Number {
				name: MARKER_VALUE,
				number: 1,
			},
		});
		values.push(text(
			&tree,
			Some(APPLIES_TO),
			&format!("System.StorageProviderId:=\"{PROVIDER_ID}\""),
		));
		for (action, label) in &actions {
			if *action == CommandAction::Versions && !on_files {
				continue;
			}
			let verb = format!(r"{tree}\shell\{}", action.key());
			values.push(text(&verb, Some("MUIVerb"), label));
			values.push(text(&verb, Some("MultiSelectModel"), "Single"));
			values.push(text(
				&format!(r"{verb}\command"),
				None,
				&command_line(&commands.app, *action),
			));
		}
		let [verb, icon_value, select, sub_commands] = SUBMENU_VALUES;
		values.push(text(&tree, Some(verb), &menu));
		values.push(text(&tree, Some(icon_value), &icon));
		values.push(text(&tree, Some(select), "Single"));
		values.push(text(&tree, Some(sub_commands), ""));
	}
	values
}

/// One step of removing a submenu, below `HKEY_CURRENT_USER`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CommandRemoval {
	/// Deletes the key with every key and value below it.
	Tree(String),
	/// Deletes the value `name` of the key `key`.
	Value {
		/// The key.
		key: String,
		/// The value's name.
		name: &'static str,
	},
}

/// The steps that remove the submenu `tree`, one of [`command_trees`], in order, so that a removal
/// cut short anywhere, within a step too, leaves neither a command offered outside the roots nor a
/// key without the marker: one registry call deletes a key's values in no order it states, so no
/// step deletes the scope or the marker with anything that outlives it. First the commands, then
/// what shows the submenu, then the scope, last the key, which by then holds only the marker: cut
/// short, an empty key, which the sweep removes too.
pub(crate) fn command_removal(tree: &str) -> Vec<CommandRemoval> {
	let value = |name| CommandRemoval::Value {
		key: tree.to_owned(),
		name,
	};
	let mut steps = vec![CommandRemoval::Tree(format!(r"{tree}\shell"))];
	steps.extend(SUBMENU_VALUES.into_iter().map(value));
	steps.push(value(APPLIES_TO));
	steps.push(CommandRemoval::Tree(tree.to_owned()));
	steps
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

/// Whether the sweep removes the class at `class` below `HKEY_CURRENT_USER` as a navigation-pane
/// entry left behind, from what it read of the key: whether it carries this provider's `marked`,
/// and the root its `Instance\InitPropertyBag` names, read by `root` only for a marked class that
/// is not the thumbnail class at `thumbnails`, and kept when `live`. The thumbnail class carries
/// the marker too but no property bag, and is never a navigation-pane entry: it goes with the
/// user's last sync root list entry instead. A class without the marker is never touched. Key
/// names compare without case, as the registry's do.
pub(crate) fn sweeps_navigation_class<E>(
	class: &str,
	thumbnails: &str,
	marked: bool,
	root: impl FnOnce() -> Result<Option<String>, E>,
	live: impl FnOnce(&str) -> bool,
) -> Result<bool, E> {
	if !marked || class.eq_ignore_ascii_case(thumbnails) {
		return Ok(false);
	}
	Ok(!root()?.is_some_and(|root| live(&root)))
}

/// Whether the value `named` of a sync root list entry of this provider (its
/// [`THUMBNAIL_PROVIDER_VALUE`]) points File Explorer at the thumbnail class `clsid` while that
/// class is not `registered` (no `LocalServer32` COM could start, or one naming an exe no longer
/// there, as a helper moved by an update leaves it): stale, as an older build may have left it, so
/// it is removed and Explorer goes back to its own thumbnails until a serving
/// helper writes both again. A value naming another class is not this provider's to judge.
pub(crate) fn thumbnail_value_is_stale(named: Option<&str>, clsid: &str, registered: bool) -> bool {
	!registered && named.is_some_and(|named| named.trim().eq_ignore_ascii_case(clsid))
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
	use std::collections::{BTreeMap, BTreeSet};

	use super::*;

	const SID: &str = "S-1-5-21-3291749577-2867003985-2111167235-1000";

	#[test]
	fn the_thumbnail_class_is_spelled_as_the_registry_spells_a_class() {
		assert_eq!(
			thumbnail_clsid_text(),
			"{CE43ED2E-6C0A-4846-A973-E43FFFACF389}"
		);
		assert_eq!(
			thumbnail_class_key(),
			r"Software\Classes\CLSID\{CE43ED2E-6C0A-4846-A973-E43FFFACF389}"
		);
	}

	#[test]
	fn the_thumbnail_server_command_quotes_the_exe_and_names_the_mode() {
		assert_eq!(
			thumbnail_server_command(r"C:\Program Files\Filen\resources\filen-cloud-files.exe")
				.as_deref(),
			Some(r#""C:\Program Files\Filen\resources\filen-cloud-files.exe" --thumbnail-server"#)
		);
		assert_eq!(thumbnail_server_command(r#"C:\a"b.exe"#), None);
	}

	#[test]
	fn the_thumbnail_server_command_names_the_exe_it_starts() {
		let exe = r"C:\Program Files\Filen\resources\filen-cloud-files.exe";
		let command = thumbnail_server_command(exe).unwrap();
		assert_eq!(thumbnail_server_exe(&command), Some(exe));
		// Not as `thumbnail_server_command` writes it: no exe this provider knows.
		assert_eq!(thumbnail_server_exe(exe), None);
		assert_eq!(
			thumbnail_server_exe(r#""C:\a.exe --thumbnail-server"#),
			None
		);
		assert_eq!(thumbnail_server_exe(r#""" --thumbnail-server"#), None);
	}

	#[test]
	fn the_thumbnail_class_is_its_own_and_not_the_probes() {
		assert_eq!(
			format!("{THUMBNAIL_CLSID:032x}"),
			"ce43ed2e6c0a4846a973e43fffacf389"
		);
		assert_ne!(THUMBNAIL_CLSID, 0x1111_1111_2222_3333_4444_5555_5555_5555);
	}

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

	/// What [`sweeps_navigation_class`] decides for the class `class` read as `marked` with the
	/// property bag `bag`, its root `live` or not, and whether it read the bag.
	fn swept(class: &str, marked: bool, bag: Option<&str>, live: bool) -> (bool, bool) {
		let mut read = false;
		let removed = sweeps_navigation_class::<()>(
			class,
			&thumbnail_class_key(),
			marked,
			|| {
				read = true;
				Ok(bag.map(str::to_owned))
			},
			|_| live,
		)
		.unwrap();
		(removed, read)
	}

	#[test]
	fn the_navigation_sweep_keeps_the_marked_thumbnail_class_without_a_property_bag() {
		let thumbnails = thumbnail_class_key();
		assert_eq!(swept(&thumbnails, true, None, false), (false, false));
		// Key names compare without case.
		assert_eq!(
			swept(&thumbnails.to_lowercase(), true, None, false),
			(false, false)
		);
	}

	#[test]
	fn the_navigation_sweep_removes_a_marked_class_whose_root_is_not_live() {
		let class = format!(r"{USER_CLSIDS}\{}", namespace_clsid(SID, UserId::from(42)));
		let root = Some(r"C:\Users\u\Filen");
		assert_eq!(swept(&class, true, root, false), (true, true));
		assert_eq!(swept(&class, true, root, true), (false, true));
		// A class a write cut short left before its property bag.
		assert_eq!(swept(&class, true, None, true), (true, true));
	}

	#[test]
	fn the_navigation_sweep_never_touches_an_unmarked_class() {
		let class = format!(r"{USER_CLSIDS}\{{0E5AAE11-A475-4C5B-AB00-C66DE400274E}}");
		assert_eq!(swept(&class, false, None, false), (false, false));
		assert_eq!(
			swept(&thumbnail_class_key(), false, None, false),
			(false, false)
		);
	}

	#[test]
	fn the_navigation_sweep_passes_a_failed_read_of_the_property_bag_on() {
		let class = format!(r"{USER_CLSIDS}\{}", namespace_clsid(SID, UserId::from(42)));
		assert_eq!(
			sweeps_navigation_class(&class, &thumbnail_class_key(), true, || Err("x"), |_| true),
			Err("x")
		);
	}

	#[test]
	fn a_value_naming_the_thumbnail_class_is_stale_only_while_the_class_is_not_registered() {
		let clsid = thumbnail_clsid_text();
		assert!(thumbnail_value_is_stale(Some(&clsid), &clsid, false));
		assert!(thumbnail_value_is_stale(
			Some(&clsid.to_lowercase()),
			&clsid,
			false
		));
		assert!(!thumbnail_value_is_stale(Some(&clsid), &clsid, true));
		assert!(!thumbnail_value_is_stale(None, &clsid, false));
		assert!(!thumbnail_value_is_stale(
			Some("{00000000-F11E-4E00-8000-000000000005}"),
			&clsid,
			false
		));
	}

	#[test]
	fn the_provider_id_is_what_the_sync_root_list_entries_start_with() {
		assert_eq!(KEY_PREFIX, format!("{PROVIDER_ID}!"));
	}

	const APP: &str = r"C:\Program Files\Filen\Filen.exe";

	fn commands(labels: [Option<&str>; 4]) -> ExplorerCommands {
		let [menu, share, web, versions] = labels;
		ExplorerCommands::parse(APP, menu, share, web, versions).unwrap()
	}

	/// A text value `name` of `key` holding `text`.
	fn text(key: &str, name: Option<&'static str>, text: &str) -> CommandValue {
		CommandValue {
			key: key.to_owned(),
			data: CommandData::Text {
				name,
				text: text.to_owned(),
			},
		}
	}

	/// The text values named `name`, by key.
	fn named<'a>(values: &'a [CommandValue], wanted: &str) -> Vec<(&'a str, &'a str)> {
		values
			.iter()
			.filter_map(|value| match &value.data {
				CommandData::Text {
					name: Some(name),
					text,
				} if *name == wanted => Some((value.key.as_str(), text.as_str())),
				CommandData::Text { .. } | CommandData::Number { .. } => None,
			})
			.collect()
	}

	#[test]
	fn the_commands_are_written_marker_and_scope_first_with_versions_on_files_only() {
		let values = command_values("K", &Branding::default(), &commands([None; 4]), |_| true);
		let file = r"Software\Classes\*\shell\K";
		let dir = r"Software\Classes\Directory\shell\K";
		let verb = |tree: &str, key: &str, label: &str, action: &str| {
			let at = format!(r"{tree}\shell\{key}");
			[
				text(&at, Some("MUIVerb"), label),
				text(&at, Some("MultiSelectModel"), "Single"),
				text(
					&format!(r"{at}\command"),
					None,
					&format!(
						r#""C:\Program Files\Filen\Filen.exe" --filen-explorer-command={action} "%1""#
					),
				),
			]
		};
		let head = |tree: &str| {
			[
				CommandValue {
					key: tree.to_owned(),
					data: CommandData::Number {
						name: "FilenCloudFiles",
						number: 1,
					},
				},
				text(
					tree,
					Some("AppliesTo"),
					r#"System.StorageProviderId:="Filen""#,
				),
			]
		};
		let tail = |tree: &str| {
			[
				text(tree, Some("MUIVerb"), "Filen"),
				text(tree, Some("Icon"), r"C:\Program Files\Filen\Filen.exe,0"),
				text(tree, Some("MultiSelectModel"), "Single"),
				text(tree, Some("SubCommands"), ""),
			]
		};
		let mut expected = Vec::new();
		expected.extend(head(file));
		expected.extend(verb(file, "01share", "Share / create public link", "share"));
		expected.extend(verb(file, "02web", "View on filen.io", "web"));
		expected.extend(verb(file, "03versions", "Version history", "versions"));
		expected.extend(tail(file));
		expected.extend(head(dir));
		expected.extend(verb(dir, "01share", "Share / create public link", "share"));
		expected.extend(verb(dir, "02web", "View on filen.io", "web"));
		expected.extend(tail(dir));
		assert_eq!(values, expected);
		assert_eq!(command_trees("K"), [file, dir]);
	}

	/// The registry as far as the submenus go: each key below `HKEY_CURRENT_USER`, with the names
	/// of its values (`""` for the default one).
	type Model = BTreeMap<String, BTreeSet<&'static str>>;

	/// What is wrong with `model` for the submenu `tree`, if anything: a command offered while the
	/// submenu has no scope, or a key left without the marker that is not an empty submenu key, the
	/// two a sweep removes.
	fn unsafe_state(model: &Model, tree: &str) -> Option<&'static str> {
		let below = format!(r"{tree}\");
		let top = model.get(tree);
		let has = |name| top.is_some_and(|values| values.contains(name));
		let offered = model.iter().any(|(key, values)| {
			key.starts_with(&format!(r"{tree}\shell\"))
				&& key.ends_with(r"\command")
				&& values.contains("")
		});
		if offered && !has(APPLIES_TO) {
			return Some("a command is offered without the scope");
		}
		let left = model
			.iter()
			.any(|(key, values)| key.starts_with(&below) || (key == tree && !values.is_empty()));
		(left && !has(MARKER_VALUE)).then_some("a key is left without the marker")
	}

	/// Writes `value` into `model`, creating its key and the keys above it below `tree`.
	fn write(model: &mut Model, tree: &str, value: &CommandValue) {
		let mut key = value.key.as_str();
		let name = match &value.data {
			CommandData::Text { name, .. } => name.unwrap_or(""),
			CommandData::Number { name, .. } => name,
		};
		model.entry(key.to_owned()).or_default().insert(name);
		while key != tree {
			key = key.rsplit_once('\\').map_or(tree, |(parent, _)| parent);
			model.entry(key.to_owned()).or_default();
		}
	}

	/// Checks every state removing `tree` by `plan` passes through, a step cut short included: a
	/// step that deletes a whole key may delete any one of the values below it first. The first
	/// unsafe state, if any.
	fn remove_by(mut model: Model, tree: &str, plan: &[CommandRemoval]) -> Result<(), String> {
		for step in plan {
			match step {
				CommandRemoval::Value { key, name } => {
					if let Some(values) = model.get_mut(key) {
						values.remove(name);
					}
				}
				CommandRemoval::Tree(at) => {
					let covered: Vec<(String, &'static str)> = model
						.iter()
						.filter(|(key, _)| *key == at || key.starts_with(&format!(r"{at}\")))
						.flat_map(|(key, values)| values.iter().map(|name| (key.clone(), *name)))
						.collect();
					for (key, name) in covered {
						let mut cut = model.clone();
						if let Some(values) = cut.get_mut(&key) {
							values.remove(name);
						}
						if let Some(problem) = unsafe_state(&cut, tree) {
							return Err(format!(
								"{step:?} cut short after {key}:{name:?}: {problem}"
							));
						}
					}
					model.retain(|key, _| key != at && !key.starts_with(&format!(r"{at}\")));
				}
			}
			if let Some(problem) = unsafe_state(&model, tree) {
				return Err(format!("after {step:?}: {problem}"));
			}
		}
		if model
			.keys()
			.any(|key| key == tree || key.starts_with(&format!(r"{tree}\")))
		{
			return Err("the submenu is left".to_owned());
		}
		Ok(())
	}

	#[test]
	fn a_write_of_the_commands_cut_short_anywhere_leaves_nothing_unscoped_or_unmarked() {
		let values = command_values("K", &Branding::default(), &commands([None; 4]), |_| true);
		for tree in command_trees("K") {
			let mut model = Model::new();
			for value in values.iter().filter(|value| value.key.starts_with(&tree)) {
				write(&mut model, &tree, value);
				assert_eq!(unsafe_state(&model, &tree), None, "after {value:?}");
			}
		}
	}

	#[test]
	fn a_removal_of_the_commands_cut_short_anywhere_leaves_nothing_unscoped_or_unmarked() {
		let values = command_values("K", &Branding::default(), &commands([None; 4]), |_| true);
		for tree in command_trees("K") {
			let mut model = Model::new();
			for value in values.iter().filter(|value| value.key.starts_with(&tree)) {
				write(&mut model, &tree, value);
			}
			assert_eq!(
				remove_by(model.clone(), &tree, &command_removal(&tree)),
				Ok(())
			);
			// One delete of the whole tree may drop the scope while a command is left.
			assert!(remove_by(model, &tree, &[CommandRemoval::Tree(tree.clone())]).is_err());
		}
	}

	#[test]
	fn a_command_line_quotes_an_app_path_holding_spaces_and_the_items_path() {
		let app = AppPath::parse(r"C:\Program Files (x86)\My Filen\Filen.exe").unwrap();
		assert_eq!(
			command_line(&app, CommandAction::Versions),
			r#""C:\Program Files (x86)\My Filen\Filen.exe" --filen-explorer-command=versions "%1""#
		);
	}

	#[test]
	fn the_labels_given_are_shown_with_each_ampersand_doubled() {
		let branding = Branding::parse(Some("Filen & Co"), None, None, None)
			.unwrap()
			.unwrap();
		let values = command_values(
			"K",
			&branding,
			&commands([None, Some("Teilen & Link"), Some("Web"), Some("A&&B")]),
			|_| true,
		);
		assert_eq!(
			named(&values, "MUIVerb"),
			[
				(
					r"Software\Classes\*\shell\K\shell\01share",
					"Teilen && Link"
				),
				(r"Software\Classes\*\shell\K\shell\02web", "Web"),
				(r"Software\Classes\*\shell\K\shell\03versions", "A&&&&B"),
				(r"Software\Classes\*\shell\K", "Filen && Co"),
				(
					r"Software\Classes\Directory\shell\K\shell\01share",
					"Teilen && Link"
				),
				(r"Software\Classes\Directory\shell\K\shell\02web", "Web"),
				(r"Software\Classes\Directory\shell\K", "Filen && Co"),
			]
		);
		let menu = command_values(
			"K",
			&branding,
			&commands([Some("Mein Filen"), None, None, None]),
			|_| true,
		);
		assert!(menu.contains(&text(
			r"Software\Classes\*\shell\K",
			Some("MUIVerb"),
			"Mein Filen"
		)));
	}

	#[test]
	fn the_submenu_shows_the_brandings_icon_when_its_file_is_there_and_the_apps_otherwise() {
		let branding = Branding::parse(None, Some(r"C:\Filen\filen.ico"), Some(-3), None)
			.unwrap()
			.unwrap();
		let icons = |branding: &Branding, exists: bool| {
			let values = command_values("K", branding, &commands([None; 4]), |_| exists);
			named(&values, "Icon")
				.into_iter()
				.map(|(_, icon)| icon.to_owned())
				.collect::<Vec<_>>()
		};
		assert_eq!(
			icons(&branding, true),
			[r"C:\Filen\filen.ico,-3", r"C:\Filen\filen.ico,-3"]
		);
		let app = r"C:\Program Files\Filen\Filen.exe,0";
		assert_eq!(icons(&branding, false), [app, app]);
		assert_eq!(icons(&Branding::default(), true), [app, app]);
	}

	#[test]
	fn no_command_value_names_a_variable_but_the_items_path() {
		let values = command_values("K", &Branding::default(), &commands([None; 4]), |_| true);
		for value in &values {
			if let CommandData::Text { text, .. } = &value.data {
				assert!(!text.replace("\"%1\"", "").contains('%'), "{value:?}");
			}
		}
	}

	#[test]
	fn roots_compare_without_case_and_trailing_separator() {
		assert!(same_root(r"C:\Users\u\Filen", r"c:\users\U\filen\"));
		assert!(!same_root(r"C:\Users\u\Filen", r"C:\Users\u\Filen2"));
	}
}
