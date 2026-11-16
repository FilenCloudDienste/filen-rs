//! What `explorer` writes and removes in the real registry, as the user the tests run as. Each test
//! uses Filen user ids of its own, far above any account's, and roots that do not exist, and
//! removes everything it wrote; a test of another Windows user's keys writes them under a made-up
//! SID. Windows only.

use std::path::PathBuf;

use super::*;
use crate::os::cfapi::explorer::DEFAULT_ICON;

/// A SID no account has, for keys written as if by another Windows user.
const OTHER_SID: &str = "S-1-5-21-1000-1000-1000-4242";

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

fn own() -> String {
	own_sid().unwrap()
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
	let sid = own();
	let user = test_user(1);
	let _cleanup = Cleanup::new(&[&sid], &[user]);
	let root = test_root("shown");
	let root_text = root.to_str().unwrap();
	brand_as(&sid, &root, user, &branding("Filen (2)")).unwrap();
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

	unbrand_as(&sid, &root, user, BrandingScope::User).unwrap();
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
		brand_as(&sid, &root, user, &Branding::default()),
		Err(ExplorerError::Unbrandable(_))
	));
	assert!(!present(Hive::LocalMachine, &entry(&sid, user)));
	assert!(!present(Hive::CurrentUser, &class(&sid, user)));
}

#[test]
fn removing_one_root_leaves_another_root_of_the_same_user_shown() {
	let sid = own();
	let user = test_user(3);
	let _cleanup = Cleanup::new(&[&sid], &[user]);
	let first = test_root("first");
	let second = test_root("second");
	brand_as(&sid, &first, user, &Branding::default()).unwrap();
	// A second state directory registered a root of the same user since: the same key names.
	brand_as(&sid, &second, user, &Branding::default()).unwrap();
	unbrand_as(&sid, &first, user, BrandingScope::User).unwrap();
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
	unbrand_as(&sid, &second, user, BrandingScope::User).unwrap();
	assert!(!present(Hive::LocalMachine, &entry));
	assert!(!present(Hive::CurrentUser, &class(&sid, user)));
}

#[test]
fn a_sync_root_list_entry_another_account_owns_is_not_written() {
	let user = test_user(4);
	let _cleanup = Cleanup::new(&[OTHER_SID], &[user]);
	// The entry is created by this process, so it is not owned by OTHER_SID: as an entry another
	// account created ahead of the user is not the user's.
	let shown = brand_as(
		OTHER_SID,
		&test_root("squatted"),
		user,
		&Branding::default(),
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
fn an_elevated_removal_leaves_the_entry_of_another_windows_user() {
	let sid = own();
	let user = test_user(5);
	let mut cleanup = Cleanup::new(&[&sid, OTHER_SID], &[user]);
	let root = test_root("elevated");
	let root_text = root.to_str().unwrap();
	brand_as(&sid, &root, user, &Branding::default()).unwrap();
	let other = entry(OTHER_SID, user);
	cleanup.extra.push((Hive::LocalMachine, other.clone()));
	set_value(
		Hive::LocalMachine,
		&format!(r"{other}\UserSyncRoots"),
		Some(OTHER_SID),
		root_text,
	);
	unbrand_as(&sid, &root, user, BrandingScope::MachineOnly).unwrap();
	assert!(!present(Hive::LocalMachine, &entry(&sid, user)));
	assert!(present(Hive::LocalMachine, &other));
	assert!(present(Hive::CurrentUser, &class(&sid, user)));
}

#[test]
fn removing_a_root_removes_its_navigation_entry_past_a_list_entry_it_cannot_read() {
	let sid = own();
	let user = test_user(6);
	let mut cleanup = Cleanup::new(&[&sid], &[user]);
	let root = test_root("unreadable");
	brand_as(&sid, &root, user, &Branding::default()).unwrap();
	cleanup.lock(
		Hive::LocalMachine,
		&format!(r"{}\UserSyncRoots", entry(&sid, user)),
	);
	assert!(unbrand_as(&sid, &root, user, BrandingScope::User).is_err());
	assert!(!present(Hive::CurrentUser, &class(&sid, user)));
}
