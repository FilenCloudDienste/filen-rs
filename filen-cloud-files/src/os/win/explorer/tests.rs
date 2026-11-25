//! What `explorer` writes and removes in the real registry, as the user the tests run as. Each test
//! uses Filen user ids of its own, far above any account's, and roots that do not exist, and
//! removes everything it wrote; a test of another Windows user's keys writes them under a made-up
//! SID. Windows only.

use std::{
	path::PathBuf,
	sync::{Mutex, MutexGuard, OnceLock, PoisonError},
};

use super::*;
use crate::os::cfapi::explorer::DEFAULT_ICON;

/// A SID no account has, for keys written as if by another Windows user.
const OTHER_SID: &str = "S-1-5-21-1000-1000-1000-4242";

/// SIDs no account has, each used by one test alone, so no other test's entries are listed for
/// them while it runs.
const LONE_SID: &str = "S-1-5-21-1000-1000-1000-4243";
const SWEPT_SID: &str = "S-1-5-21-1000-1000-1000-4244";
const UNBRAND_SID: &str = "S-1-5-21-1000-1000-1000-4245";
const UNNAMED_SID: &str = "S-1-5-21-1000-1000-1000-4246";
const NAMED_SID: &str = "S-1-5-21-1000-1000-1000-4247";
const STALE_SID: &str = "S-1-5-21-1000-1000-1000-4248";
const SQUATTED_SID: &str = "S-1-5-21-1000-1000-1000-4249";

/// The context menu commands' key name of the tests that write none: no test writes one there.
const NO_COMMANDS: &str = "FilenCloudFilesTestNone";

/// The thumbnail class key of the tests that write none: no test writes one there.
const NO_THUMBNAILS: &str = r"Software\Classes\CLSID\{00000000-F11E-4E00-8000-000000000000}";

/// [`NO_THUMBNAILS`] with its class id, for the sweep.
const NO_THUMBNAIL_CLASS: ThumbnailClass<'static> = ThumbnailClass {
	key: NO_THUMBNAILS,
	clsid: "{00000000-F11E-4E00-8000-000000000000}",
};

/// Held by every test that sweeps the user's classes or writes a marked class, a thumbnail class
/// or a navigation-pane class (`brand_as` marks one before it writes its property bag): a sweep
/// removes every marked class with no property bag but the thumbnail class it is handed and this
/// build's, so another test's would vanish mid-test.
static MARKED_CLASSES: Mutex<()> = Mutex::new(());

/// [`MARKED_CLASSES`], held until the test ends; a test that failed holding it does not fail the
/// others.
fn marked_classes() -> MutexGuard<'static, ()> {
	MARKED_CLASSES
		.lock()
		.unwrap_or_else(PoisonError::into_inner)
}

/// The thumbnail class the thumbnail tests write, each its own.
fn test_thumbnail_class(n: u8) -> (String, String) {
	let clsid = format!("{{00000000-F11E-4E00-8000-0000000000{n:02X}}}");
	(format!(r"{USER_CLSIDS}\{clsid}"), clsid)
}

const SERVER: &str = r#""C:\filen-explorer-tests\filen-cloud-files.exe" --thumbnail-server"#;

/// The tests' Filen user `n`: no account's id is this high.
fn test_user(n: u64) -> UserId {
	UserId::from(9_000_000_000 + n)
}

/// The root `name` of this test run, below a directory that does not exist.
fn test_root(name: &str) -> PathBuf {
	PathBuf::from(format!(
		r"C:\filen-explorer-tests-{}\{name}",
		std::process::id()
	))
}

fn branding(name: &str) -> Branding {
	Branding::parse(
		Some(name),
		Some(r"C:\filen-explorer-tests\missing.exe"),
		Some(3),
		None,
	)
	.unwrap()
	.unwrap()
}

fn entry(sid: &str, user: UserId) -> String {
	format!(r"{SYNC_ROOT_MANAGER}\{}", sync_root_key_name(sid, user))
}

fn class(sid: &str, user: UserId) -> String {
	format!(r"{USER_CLSIDS}\{}", namespace_clsid(sid, user))
}

fn present(hive: Hive, path: &str) -> bool {
	ffi::reg_open(hive, &wide(path).unwrap()).unwrap().is_some()
}

fn string_value(hive: Hive, path: &str, name: Option<&str>) -> Option<String> {
	let key = ffi::reg_open(hive, &wide(path).unwrap()).unwrap()?;
	let name = name.map(|name| wide(name).unwrap());
	ffi::reg_query_string(&key, name.as_ref())
		.unwrap()
		.map(|units| String::from_utf16_lossy(&units))
}

fn dword_value(hive: Hive, path: &str, name: &str) -> Option<u32> {
	let key = ffi::reg_open(hive, &wide(path).unwrap()).unwrap()?;
	ffi::reg_query_dword(&key, &wide(name).unwrap()).unwrap()
}

fn set_value(hive: Hive, path: &str, name: Option<&str>, value: &str) {
	set_string(&create(hive, path).unwrap(), name, value, false).unwrap();
}

/// The Windows user the tests write keys as: the owner of the keys this process creates, which
/// `brand_as` and the sweep check every entry against. Not always the process's user: an elevated
/// administrator's process creates keys owned by the Administrators group where the "default owner
/// for objects created by members of the Administrators group" policy names the group, as it does
/// by default on Windows Server, where CI runs the tests elevated. The helper refuses to start
/// elevated, so for it the two are always the same SID.
fn own() -> String {
	static OWNER: OnceLock<String> = OnceLock::new();
	OWNER
		.get_or_init(|| {
			let probe = format!(r"Software\FilenCloudFilesTestOwner{}", std::process::id());
			let owner = ffi::reg_owner_sid(&create(Hive::CurrentUser, &probe).unwrap()).unwrap();
			delete_tree(Hive::CurrentUser, &probe).unwrap();
			String::from_utf16(&owner).unwrap()
		})
		// Every test holds its own copy, as it did when this read the token each time.
		.clone()
}

/// Removes, when dropped, what a test wrote for each of `users` as each of `sids`, and each of
/// `extra`, unlocking each of `locked` first.
struct Cleanup {
	sids: Vec<String>,
	users: Vec<UserId>,
	locked: Vec<(Hive, String)>,
	extra: Vec<(Hive, String)>,
}

impl Cleanup {
	fn new(sids: &[&str], users: &[UserId]) -> Self {
		Self {
			sids: sids.iter().map(|sid| (*sid).to_owned()).collect(),
			users: users.to_vec(),
			locked: Vec::new(),
			extra: Vec::new(),
		}
	}

	/// Makes the key `path` below `hive` one no one may open, until the test ends.
	fn lock(&mut self, hive: Hive, path: &str) {
		drop(create(hive, path).unwrap());
		ffi::reg_set_dacl_for_test(hive, &wide(path).unwrap(), true).unwrap();
		self.locked.push((hive, path.to_owned()));
	}
}

impl Drop for Cleanup {
	fn drop(&mut self) {
		for (hive, path) in &self.locked {
			drop(ffi::reg_set_dacl_for_test(
				*hive,
				&wide(path).unwrap(),
				false,
			));
		}
		for sid in &self.sids {
			for user in &self.users {
				drop(delete_tree(Hive::LocalMachine, &entry(sid, *user)));
				drop(remove_navigation_entry(&namespace_clsid(sid, *user)));
			}
		}
		for (hive, path) in &self.extra {
			drop(delete_tree(*hive, path));
		}
	}
}

#[test]
fn a_root_is_shown_with_every_key_and_removed_again() {
	let _marked = marked_classes();
	let sid = own();
	let user = test_user(1);
	let _cleanup = Cleanup::new(&[&sid], &[user]);
	let root = test_root("shown");
	let root_text = root.to_str().unwrap();
	brand_as(&sid, &root, user, &branding("Filen (2)"), NO_COMMANDS).unwrap();
	let entry = entry(&sid, user);
	let class = class(&sid, user);
	let clsid = namespace_clsid(&sid, user);
	let hklm = Hive::LocalMachine;
	let hkcu = Hive::CurrentUser;
	assert_eq!(
		string_value(hklm, &entry, Some("DisplayNameResource")).as_deref(),
		Some("Filen (2)")
	);
	// The icon's file is not there, so the system's cloud icon shows.
	assert_eq!(
		string_value(hklm, &entry, Some("IconResource")).as_deref(),
		Some(DEFAULT_ICON)
	);
	assert_eq!(dword_value(hklm, &entry, "Flags"), Some(SYNC_ROOT_FLAGS));
	assert_eq!(
		string_value(hklm, &entry, Some("NamespaceCLSID")).as_deref(),
		Some(clsid.as_str())
	);
	assert_eq!(
		string_value(hklm, &format!(r"{entry}\UserSyncRoots"), Some(&sid)).as_deref(),
		Some(root_text)
	);
	assert_eq!(
		string_value(hkcu, &class, None).as_deref(),
		Some("Filen (2)")
	);
	assert_eq!(dword_value(hkcu, &class, MARKER_VALUE), Some(1));
	assert_eq!(
		string_value(
			hkcu,
			&format!(r"{class}\Instance\InitPropertyBag"),
			Some("TargetFolderPath")
		)
		.as_deref(),
		Some(root_text)
	);
	assert!(present(hkcu, &format!(r"{DESKTOP_NAMESPACE}\{clsid}")));
	assert_eq!(dword_value(hkcu, HIDE_DESKTOP_ICONS, &clsid), Some(1));

	unbrand_as(
		&sid,
		&root,
		user,
		BrandingScope::User,
		NO_COMMANDS,
		NO_THUMBNAILS,
	)
	.unwrap();
	assert!(!present(hklm, &entry));
	assert!(!present(hkcu, &class));
	assert!(!present(hkcu, &format!(r"{DESKTOP_NAMESPACE}\{clsid}")));
	assert_eq!(dword_value(hkcu, HIDE_DESKTOP_ICONS, &clsid), None);
}

#[test]
fn a_root_whose_path_holds_a_percent_sign_is_not_shown() {
	let sid = own();
	let user = test_user(2);
	let _cleanup = Cleanup::new(&[&sid], &[user]);
	let root = test_root("%USERNAME%");
	assert!(matches!(
		brand_as(&sid, &root, user, &Branding::default(), NO_COMMANDS),
		Err(ExplorerError::Unbrandable(_))
	));
	assert!(!present(Hive::LocalMachine, &entry(&sid, user)));
	assert!(!present(Hive::CurrentUser, &class(&sid, user)));
}

#[test]
fn removing_one_root_leaves_another_root_of_the_same_user_shown() {
	let _marked = marked_classes();
	let sid = own();
	let user = test_user(3);
	let _cleanup = Cleanup::new(&[&sid], &[user]);
	let first = test_root("first");
	let second = test_root("second");
	brand_as(&sid, &first, user, &Branding::default(), NO_COMMANDS).unwrap();
	// A second state directory registered a root of the same user since: the same key names.
	brand_as(&sid, &second, user, &Branding::default(), NO_COMMANDS).unwrap();
	unbrand_as(
		&sid,
		&first,
		user,
		BrandingScope::User,
		NO_COMMANDS,
		NO_THUMBNAILS,
	)
	.unwrap();
	let entry = entry(&sid, user);
	assert_eq!(
		string_value(
			Hive::LocalMachine,
			&format!(r"{entry}\UserSyncRoots"),
			Some(&sid)
		)
		.as_deref(),
		second.to_str()
	);
	assert_eq!(
		string_value(
			Hive::CurrentUser,
			&format!(r"{}\Instance\InitPropertyBag", class(&sid, user)),
			Some("TargetFolderPath")
		)
		.as_deref(),
		second.to_str()
	);
	unbrand_as(
		&sid,
		&second,
		user,
		BrandingScope::User,
		NO_COMMANDS,
		NO_THUMBNAILS,
	)
	.unwrap();
	assert!(!present(Hive::LocalMachine, &entry));
	assert!(!present(Hive::CurrentUser, &class(&sid, user)));
}

#[test]
fn a_sync_root_list_entry_another_account_owns_is_not_written() {
	let _marked = marked_classes();
	let user = test_user(4);
	let _cleanup = Cleanup::new(&[OTHER_SID], &[user]);
	// The entry is created by this process, so it is not owned by OTHER_SID: as an entry another
	// account created ahead of the user is not the user's.
	let shown = brand_as(
		OTHER_SID,
		&test_root("squatted"),
		user,
		&Branding::default(),
		NO_COMMANDS,
	);
	assert!(matches!(shown, Err(ExplorerError::NotOwned)), "{shown:?}");
	assert_eq!(
		string_value(
			Hive::LocalMachine,
			&entry(OTHER_SID, user),
			Some("DisplayNameResource")
		),
		None
	);
}

#[test]
fn a_root_whose_entry_another_account_created_first_gets_no_navigation_entry() {
	let _marked = marked_classes();
	let user = test_user(13);
	let _cleanup = Cleanup::new(&[OTHER_SID], &[user]);
	// Created ahead of the call, by this process, so not owned by OTHER_SID.
	drop(create(Hive::LocalMachine, &entry(OTHER_SID, user)).unwrap());
	let shown = brand_as(
		OTHER_SID,
		&test_root("squatted-first"),
		user,
		&Branding::default(),
		NO_COMMANDS,
	);
	assert!(matches!(shown, Err(ExplorerError::NotOwned)), "{shown:?}");
	let clsid = namespace_clsid(OTHER_SID, user);
	assert!(!present(Hive::CurrentUser, &class(OTHER_SID, user)));
	assert!(!present(
		Hive::CurrentUser,
		&format!(r"{DESKTOP_NAMESPACE}\{clsid}")
	));
	assert_eq!(
		dword_value(Hive::CurrentUser, HIDE_DESKTOP_ICONS, &clsid),
		None
	);
}

#[test]
fn the_sweep_leaves_an_entry_another_account_owns_and_fails_nothing() {
	let _marked = marked_classes();
	let gone = PathBuf::from(format!(
		r"C:\filen-explorer-squat-test-{}\Gone",
		std::process::id()
	));
	let user = test_user(71);
	let _cleanup = Cleanup::new(&[SQUATTED_SID], &[user]);
	set_value(
		Hive::LocalMachine,
		&format!(r"{}\UserSyncRoots", entry(SQUATTED_SID, user)),
		Some(SQUATTED_SID),
		gone.to_str().unwrap(),
	);
	let prefix = gone.parent().unwrap().to_path_buf();
	// The entry is this process's, so not owned by OTHER_SID: as an entry another account created.
	let swept = sweep_as(
		SQUATTED_SID,
		OTHER_SID,
		&|root| !root.starts_with(&prefix),
		NO_COMMANDS,
		NO_THUMBNAIL_CLASS,
	);
	assert_eq!(swept.unwrap(), 0);
	assert!(present(Hive::LocalMachine, &entry(SQUATTED_SID, user)));
}

#[test]
fn an_elevated_removal_leaves_the_entry_of_another_windows_user() {
	let _marked = marked_classes();
	let sid = own();
	let user = test_user(5);
	let mut cleanup = Cleanup::new(&[&sid, OTHER_SID], &[user]);
	let root = test_root("elevated");
	let root_text = root.to_str().unwrap();
	brand_as(&sid, &root, user, &Branding::default(), NO_COMMANDS).unwrap();
	let other = entry(OTHER_SID, user);
	cleanup.extra.push((Hive::LocalMachine, other.clone()));
	set_value(
		Hive::LocalMachine,
		&format!(r"{other}\UserSyncRoots"),
		Some(OTHER_SID),
		root_text,
	);
	unbrand_as(
		&sid,
		&root,
		user,
		BrandingScope::MachineOnly,
		NO_COMMANDS,
		NO_THUMBNAILS,
	)
	.unwrap();
	assert!(!present(Hive::LocalMachine, &entry(&sid, user)));
	assert!(present(Hive::LocalMachine, &other));
	assert!(present(Hive::CurrentUser, &class(&sid, user)));
}

#[test]
fn removing_a_root_removes_its_navigation_entry_past_a_list_entry_it_cannot_read() {
	let _marked = marked_classes();
	let sid = own();
	let user = test_user(6);
	let mut cleanup = Cleanup::new(&[&sid], &[user]);
	let root = test_root("unreadable");
	brand_as(&sid, &root, user, &Branding::default(), NO_COMMANDS).unwrap();
	cleanup.lock(
		Hive::LocalMachine,
		&format!(r"{}\UserSyncRoots", entry(&sid, user)),
	);
	assert!(
		unbrand_as(
			&sid,
			&root,
			user,
			BrandingScope::User,
			NO_COMMANDS,
			NO_THUMBNAILS
		)
		.is_err()
	);
	assert!(!present(Hive::CurrentUser, &class(&sid, user)));
}

#[test]
fn the_sweep_tries_every_entry_past_one_it_cannot_read() {
	let _marked = marked_classes();
	// Roots of this test alone, so the sweep removes nothing another test wrote.
	let gone = PathBuf::from(format!(
		r"C:\filen-explorer-sweep-test-{}\Gone",
		std::process::id()
	));
	let gone_text = gone.to_str().unwrap();
	let (unreadable, removable) = (test_user(61), test_user(62));
	let mut cleanup = Cleanup::new(&[OTHER_SID], &[unreadable, removable]);
	for user in [unreadable, removable] {
		set_value(
			Hive::LocalMachine,
			&format!(r"{}\UserSyncRoots", entry(OTHER_SID, user)),
			Some(OTHER_SID),
			gone_text,
		);
	}
	cleanup.lock(
		Hive::LocalMachine,
		&format!(r"{}\UserSyncRoots", entry(OTHER_SID, unreadable)),
	);
	// Two marked classes, in the order the registry lists them, the first one unreadable.
	let classes = [
		format!(r"{USER_CLSIDS}\{{0F11E7E5-0000-8000-8000-{:012}}}", 1),
		format!(r"{USER_CLSIDS}\{{0F11E7E5-0000-8000-8000-{:012}}}", 2),
	];
	for class in &classes {
		cleanup.extra.push((Hive::CurrentUser, class.clone()));
		set_dword(&create(Hive::CurrentUser, class).unwrap(), MARKER_VALUE, 1).unwrap();
		set_value(
			Hive::CurrentUser,
			&format!(r"{class}\Instance\InitPropertyBag"),
			Some("TargetFolderPath"),
			gone_text,
		);
	}
	cleanup.lock(Hive::CurrentUser, &classes[0]);
	// The thumbnail class among them, marked too and with no property bag, as `set_thumbnails_as`
	// writes it.
	let (thumbnails, clsid) = test_thumbnail_class(6);
	cleanup.extra.push((Hive::CurrentUser, thumbnails.clone()));
	{
		let key = create(Hive::CurrentUser, &thumbnails).unwrap();
		set_string(&key, None, THUMBNAIL_CLASS_NAME, false).unwrap();
		set_dword(&key, MARKER_VALUE, 1).unwrap();
	}
	set_value(
		Hive::CurrentUser,
		&format!(r"{thumbnails}\LocalServer32"),
		None,
		SERVER,
	);
	let prefix = gone.parent().unwrap().to_path_buf();
	let swept = sweep_as(
		OTHER_SID,
		&own(),
		&|root| !root.starts_with(&prefix),
		NO_COMMANDS,
		ThumbnailClass {
			key: &thumbnails,
			clsid: &clsid,
		},
	);
	assert!(swept.is_err(), "{swept:?}");
	assert!(present(Hive::LocalMachine, &entry(OTHER_SID, unreadable)));
	assert!(!present(Hive::LocalMachine, &entry(OTHER_SID, removable)));
	assert!(!present(Hive::CurrentUser, &classes[1]));
	// Not a navigation-pane entry: kept, with its server, while an entry of the user is left.
	assert_eq!(
		string_value(
			Hive::CurrentUser,
			&format!(r"{thumbnails}\LocalServer32"),
			None
		)
		.as_deref(),
		Some(SERVER)
	);
}

#[test]
fn a_value_naming_a_thumbnail_class_not_registered_is_removed_and_one_registered_kept() {
	let _marked = marked_classes();
	let users = [test_user(44), test_user(45)];
	let mut cleanup = Cleanup::new(&[STALE_SID], &users);
	let (key, clsid) = test_thumbnail_class(7);
	cleanup.extra.push((Hive::CurrentUser, key.clone()));
	let class = ThumbnailClass {
		key: &key,
		clsid: &clsid,
	};
	let [named, other] = users;
	set_value(
		Hive::LocalMachine,
		&entry(STALE_SID, named),
		Some(THUMBNAIL_PROVIDER_VALUE),
		&clsid,
	);
	set_value(
		Hive::LocalMachine,
		&entry(STALE_SID, other),
		Some(THUMBNAIL_PROVIDER_VALUE),
		"{00000000-F11E-4E00-8000-000000000005}",
	);
	let value = |user| {
		string_value(
			Hive::LocalMachine,
			&entry(STALE_SID, user),
			Some(THUMBNAIL_PROVIDER_VALUE),
		)
	};
	// The class is not there: the value naming it goes, the one naming another class stays.
	assert_eq!(
		remove_stale_thumbnail_values_as(STALE_SID, class).unwrap(),
		1
	);
	assert_eq!(value(named), None);
	assert!(present(Hive::LocalMachine, &entry(STALE_SID, named)));
	assert!(value(other).is_some());
	// A class key with no server is not registered either.
	set_dword(&create(Hive::CurrentUser, &key).unwrap(), MARKER_VALUE, 1).unwrap();
	set_value(
		Hive::LocalMachine,
		&entry(STALE_SID, named),
		Some(THUMBNAIL_PROVIDER_VALUE),
		&clsid,
	);
	assert_eq!(
		remove_stale_thumbnail_values_as(STALE_SID, class).unwrap(),
		1
	);
	assert_eq!(value(named), None);
	// A server whose exe is not there, as a helper moved by an update leaves it: not registered.
	set_value(
		Hive::CurrentUser,
		&format!(r"{key}\LocalServer32"),
		None,
		SERVER,
	);
	set_value(
		Hive::LocalMachine,
		&entry(STALE_SID, named),
		Some(THUMBNAIL_PROVIDER_VALUE),
		&clsid,
	);
	assert_eq!(
		remove_stale_thumbnail_values_as(STALE_SID, class).unwrap(),
		1
	);
	assert_eq!(value(named), None);
	// Registered, naming an exe that is there: kept.
	let exe = std::env::current_exe().unwrap();
	set_value(
		Hive::CurrentUser,
		&format!(r"{key}\LocalServer32"),
		None,
		&thumbnail_server_command(exe.to_str().unwrap()).unwrap(),
	);
	set_value(
		Hive::LocalMachine,
		&entry(STALE_SID, named),
		Some(THUMBNAIL_PROVIDER_VALUE),
		&clsid,
	);
	assert_eq!(
		remove_stale_thumbnail_values_as(STALE_SID, class).unwrap(),
		0
	);
	assert_eq!(value(named).as_deref(), Some(clsid.as_str()));
}

/// The context menu commands' key name of test `n`, of this test run alone.
fn command_key(n: u32) -> String {
	format!("FilenCloudFilesTest{}x{n}", std::process::id())
}

/// Removes, when dropped, both submenus under a test's command key name.
struct CommandCleanup(String);

impl Drop for CommandCleanup {
	fn drop(&mut self) {
		for tree in command_trees(&self.0) {
			drop(delete_tree(Hive::CurrentUser, &tree));
		}
	}
}

/// Commands starting this test binary, which is there, so they are written.
fn present_commands() -> (String, Branding) {
	let app = std::env::current_exe().unwrap();
	let app = app.to_str().unwrap().to_owned();
	let commands = ExplorerCommands::parse(&app, None, Some("Teilen & Link"), None, None).unwrap();
	let branding = Branding::parse(Some("Filen (2)"), None, None, Some(commands))
		.unwrap()
		.unwrap();
	(app, branding)
}

/// Writes `branding`'s commands under `key` as `brand_as` does, without a root.
fn plant_commands(key: &str, branding: &Branding) {
	write_commands(key, branding, branding.commands.as_deref().unwrap()).unwrap();
}

#[test]
fn a_root_whose_branding_names_commands_offers_them_with_every_value() {
	let _marked = marked_classes();
	let sid = own();
	let user = test_user(7);
	let _cleanup = Cleanup::new(&[&sid], &[user]);
	let key = command_key(7);
	let _commands = CommandCleanup(key.clone());
	let (app, branding) = present_commands();
	let root = test_root("commands");
	brand_as(&sid, &root, user, &branding, &key).unwrap();
	let [files, folders] = command_trees(&key);
	let hkcu = Hive::CurrentUser;
	for tree in [&files, &folders] {
		assert_eq!(
			string_value(hkcu, tree, Some("AppliesTo")).as_deref(),
			Some(r#"System.StorageProviderId:="Filen""#)
		);
		assert_eq!(dword_value(hkcu, tree, MARKER_VALUE), Some(1));
		assert_eq!(
			string_value(hkcu, tree, Some("MUIVerb")).as_deref(),
			Some("Filen (2)")
		);
		assert_eq!(
			string_value(hkcu, tree, Some("Icon")),
			Some(format!("{app},0"))
		);
		assert_eq!(
			string_value(hkcu, tree, Some("SubCommands")).as_deref(),
			Some("")
		);
		assert_eq!(
			string_value(hkcu, tree, Some("MultiSelectModel")).as_deref(),
			Some("Single")
		);
		assert_eq!(
			string_value(hkcu, &format!(r"{tree}\shell\01share"), Some("MUIVerb")).as_deref(),
			Some("Teilen && Link")
		);
		assert_eq!(
			string_value(hkcu, &format!(r"{tree}\shell\02web\command"), None),
			Some(format!("\"{app}\" --filen-explorer-command=web \"%1\""))
		);
	}
	assert_eq!(
		string_value(hkcu, &format!(r"{files}\shell\03versions"), Some("MUIVerb")).as_deref(),
		Some("Version history")
	);
	assert!(!present(hkcu, &format!(r"{folders}\shell\03versions")));
	// A repeat writes the same, in place.
	brand_as(&sid, &root, user, &branding, &key).unwrap();
	assert_eq!(
		string_value(hkcu, &format!(r"{files}\shell\01share\command"), None),
		Some(format!("\"{app}\" --filen-explorer-command=share \"%1\""))
	);
}

#[test]
fn commands_naming_an_app_that_is_not_there_are_not_written_and_the_root_is_shown() {
	let _marked = marked_classes();
	let sid = own();
	let user = test_user(8);
	let _cleanup = Cleanup::new(&[&sid], &[user]);
	let key = command_key(8);
	let _commands = CommandCleanup(key.clone());
	let commands = ExplorerCommands::parse(
		r"C:\filen-explorer-tests\missing.exe",
		None,
		None,
		None,
		None,
	)
	.unwrap();
	let branding = Branding::parse(None, None, None, Some(commands))
		.unwrap()
		.unwrap();
	brand_as(&sid, &test_root("no-app"), user, &branding, &key).unwrap();
	assert!(present(Hive::LocalMachine, &entry(&sid, user)));
	for tree in command_trees(&key) {
		assert!(!present(Hive::CurrentUser, &tree), "{tree}");
	}
}

#[test]
fn the_commands_stay_while_the_user_has_another_root() {
	let _marked = marked_classes();
	let sid = own();
	let (first_user, second_user) = (test_user(9), test_user(10));
	let _cleanup = Cleanup::new(&[&sid], &[first_user, second_user]);
	let key = command_key(9);
	let _commands = CommandCleanup(key.clone());
	let (_, branding) = present_commands();
	let (first, second) = (test_root("first-commands"), test_root("second-commands"));
	brand_as(&sid, &first, first_user, &branding, &key).unwrap();
	brand_as(&sid, &second, second_user, &branding, &key).unwrap();
	unbrand_as(
		&sid,
		&first,
		first_user,
		BrandingScope::User,
		&key,
		NO_THUMBNAILS,
	)
	.unwrap();
	for tree in command_trees(&key) {
		assert!(present(Hive::CurrentUser, &tree), "{tree}");
	}
}

#[test]
fn removing_the_last_root_of_a_windows_user_removes_the_commands() {
	let user = test_user(15);
	let mut cleanup = Cleanup::new(&[UNBRAND_SID], &[user]);
	let key = command_key(15);
	let _commands = CommandCleanup(key.clone());
	let (_, branding) = present_commands();
	plant_commands(&key, &branding);
	// The root's sync root list entry, as `brand_as` writes it for another Windows user.
	let root = test_root("last");
	let listed = entry(UNBRAND_SID, user);
	cleanup.extra.push((Hive::LocalMachine, listed.clone()));
	set_value(
		Hive::LocalMachine,
		&format!(r"{listed}\UserSyncRoots"),
		Some(UNBRAND_SID),
		root.to_str().unwrap(),
	);
	unbrand_as(
		UNBRAND_SID,
		&root,
		user,
		BrandingScope::User,
		&key,
		NO_THUMBNAILS,
	)
	.unwrap();
	assert!(!present(Hive::LocalMachine, &listed));
	for tree in command_trees(&key) {
		assert!(!present(Hive::CurrentUser, &tree), "{tree}");
	}
}

#[test]
fn an_elevated_removal_leaves_the_commands() {
	let _marked = marked_classes();
	let sid = own();
	let user = test_user(11);
	let _cleanup = Cleanup::new(&[&sid], &[user]);
	let key = command_key(11);
	let _commands = CommandCleanup(key.clone());
	let (_, branding) = present_commands();
	let root = test_root("elevated-commands");
	brand_as(&sid, &root, user, &branding, &key).unwrap();
	unbrand_as(
		&sid,
		&root,
		user,
		BrandingScope::MachineOnly,
		&key,
		NO_THUMBNAILS,
	)
	.unwrap();
	assert!(!present(Hive::LocalMachine, &entry(&sid, user)));
	for tree in command_trees(&key) {
		assert!(present(Hive::CurrentUser, &tree), "{tree}");
	}
}

#[test]
fn the_commands_of_a_windows_user_with_no_root_left_are_removed_unless_another_entry_is_there() {
	let user = test_user(12);
	let mut cleanup = Cleanup::new(&[LONE_SID], &[user]);
	let key = command_key(12);
	let _commands = CommandCleanup(key.clone());
	let (_, branding) = present_commands();
	plant_commands(&key, &branding);
	let other = entry(LONE_SID, user);
	cleanup.extra.push((Hive::LocalMachine, other.clone()));
	set_value(
		Hive::LocalMachine,
		&format!(r"{other}\UserSyncRoots"),
		Some(LONE_SID),
		r"C:\filen-explorer-tests\kept",
	);
	assert_eq!(remove_commands_unless_rooted(LONE_SID, &key).unwrap(), 0);
	for tree in command_trees(&key) {
		assert!(present(Hive::CurrentUser, &tree), "{tree}");
	}
	delete_tree(Hive::LocalMachine, &other).unwrap();
	assert_eq!(remove_commands_unless_rooted(LONE_SID, &key).unwrap(), 2);
	for tree in command_trees(&key) {
		assert!(!present(Hive::CurrentUser, &tree), "{tree}");
	}
}

#[test]
fn a_command_key_without_the_marker_is_left_alone() {
	let key = command_key(13);
	let _commands = CommandCleanup(key.clone());
	let [files, folders] = command_trees(&key);
	set_value(Hive::CurrentUser, &files, Some("MUIVerb"), "someone else's");
	set_dword(
		&create(Hive::CurrentUser, &folders).unwrap(),
		MARKER_VALUE,
		0,
	)
	.unwrap();
	assert_eq!(remove_commands_unless_rooted(SWEPT_SID, &key).unwrap(), 0);
	assert!(present(Hive::CurrentUser, &files));
	assert!(present(Hive::CurrentUser, &folders));
}

#[test]
fn an_empty_command_key_a_removal_cut_short_left_is_removed() {
	let key = command_key(16);
	let _commands = CommandCleanup(key.clone());
	let [files, _] = command_trees(&key);
	drop(create(Hive::CurrentUser, &files).unwrap());
	assert_eq!(remove_commands_unless_rooted(SWEPT_SID, &key).unwrap(), 1);
	assert!(!present(Hive::CurrentUser, &files));
}

#[test]
fn the_sweep_removes_the_commands_once_no_entry_of_the_user_is_left() {
	let _marked = marked_classes();
	let key = command_key(14);
	let _commands = CommandCleanup(key.clone());
	let (_, branding) = present_commands();
	plant_commands(&key, &branding);
	// Of a SID no other test writes an entry of.
	let swept = sweep_as(SWEPT_SID, &own(), &|_| true, &key, NO_THUMBNAIL_CLASS).unwrap();
	assert_eq!(swept, 2);
	for tree in command_trees(&key) {
		assert!(!present(Hive::CurrentUser, &tree), "{tree}");
	}
}

#[test]
fn a_build_serving_thumbnails_registers_its_class_then_names_it_on_the_entry() {
	let _marked = marked_classes();
	let sid = own();
	let user = test_user(40);
	let root = test_root("thumbnails");
	let (key, clsid) = test_thumbnail_class(1);
	let class = ThumbnailClass {
		key: &key,
		clsid: &clsid,
	};
	brand_as(&sid, &root, user, &Branding::default(), NO_COMMANDS).unwrap();
	set_thumbnails_as(&sid, user, class, Some(SERVER)).unwrap();
	assert_eq!(
		string_value(Hive::CurrentUser, &key, None).as_deref(),
		Some(THUMBNAIL_CLASS_NAME)
	);
	assert_eq!(
		string_value(Hive::CurrentUser, &format!(r"{key}\LocalServer32"), None).as_deref(),
		Some(SERVER)
	);
	assert_eq!(
		string_value(
			Hive::LocalMachine,
			&entry(&sid, user),
			Some(THUMBNAIL_PROVIDER_VALUE)
		)
		.as_deref(),
		Some(clsid.as_str())
	);
	unbrand_as(&sid, &root, user, BrandingScope::User, NO_COMMANDS, &key).unwrap();
	assert!(!present(Hive::LocalMachine, &entry(&sid, user)));
	// Another test's entry of this user may still be listed; then the class stays for it.
	if !present(Hive::CurrentUser, &key) {
		return;
	}
	delete_tree(Hive::CurrentUser, &key).unwrap();
}

#[test]
fn a_build_not_serving_thumbnails_removes_the_entrys_value_and_writes_no_class() {
	let _marked = marked_classes();
	let sid = own();
	let user = test_user(41);
	let root = test_root("no-thumbnails");
	let (key, clsid) = test_thumbnail_class(2);
	let class = ThumbnailClass {
		key: &key,
		clsid: &clsid,
	};
	brand_as(&sid, &root, user, &Branding::default(), NO_COMMANDS).unwrap();
	set_thumbnails_as(&sid, user, class, Some(SERVER)).unwrap();
	set_thumbnails_as(&sid, user, class, None).unwrap();
	assert_eq!(
		string_value(
			Hive::LocalMachine,
			&entry(&sid, user),
			Some(THUMBNAIL_PROVIDER_VALUE)
		),
		None
	);
	delete_tree(Hive::CurrentUser, &key).unwrap();
	set_thumbnails_as(&sid, user, class, None).unwrap();
	assert!(!present(Hive::CurrentUser, &key));
	unbrand_as(
		&sid,
		&root,
		user,
		BrandingScope::User,
		NO_COMMANDS,
		NO_THUMBNAILS,
	)
	.unwrap();
}

#[test]
fn a_thumbnail_class_left_with_no_entry_is_swept_only_when_marked() {
	let _marked = marked_classes();
	let (marked, _) = test_thumbnail_class(3);
	let (foreign, _) = test_thumbnail_class(4);
	set_dword(
		&create(Hive::CurrentUser, &marked).unwrap(),
		MARKER_VALUE,
		1,
	)
	.unwrap();
	set_string(
		&create(Hive::CurrentUser, &foreign).unwrap(),
		None,
		"not ours",
		false,
	)
	.unwrap();
	assert!(remove_thumbnail_class_unless_rooted(LONE_SID, &marked).unwrap());
	assert!(!remove_thumbnail_class_unless_rooted(LONE_SID, &foreign).unwrap());
	assert!(!present(Hive::CurrentUser, &marked));
	assert!(present(Hive::CurrentUser, &foreign));
	delete_tree(Hive::CurrentUser, &foreign).unwrap();
}

#[test]
fn a_server_that_did_not_start_unnames_the_class_on_every_entry_of_the_user_only() {
	let users = [test_user(42), test_user(43)];
	let _cleanup = Cleanup::new(&[UNNAMED_SID, NAMED_SID], &users);
	for sid in [UNNAMED_SID, NAMED_SID] {
		for user in users {
			set_value(
				Hive::LocalMachine,
				&entry(sid, user),
				Some(THUMBNAIL_PROVIDER_VALUE),
				"{00000000-F11E-4E00-8000-000000000005}",
			);
		}
	}
	remove_thumbnail_values_as(UNNAMED_SID).unwrap();
	for user in users {
		let value = |sid| {
			string_value(
				Hive::LocalMachine,
				&entry(sid, user),
				Some(THUMBNAIL_PROVIDER_VALUE),
			)
		};
		assert_eq!(value(UNNAMED_SID), None);
		assert!(present(Hive::LocalMachine, &entry(UNNAMED_SID, user)));
		assert!(value(NAMED_SID).is_some());
	}
	remove_thumbnail_values_as(UNNAMED_SID).unwrap();
}
