//! The roots index, `roots.json` in the state directory: which user's root is where, with the
//! user's data directory and the account root the root was registered for. It lets `clearAuth`,
//! `unregister`, `listRoots` and the uninstaller's `--unregister-all` find a root with no
//! `register` first. It holds no secret and no email.
//!
//! It also holds what each unregister kept until a reply the parent answered has reported it: the
//! record is written before the OS lets the root go, with where the root is to be renamed aside,
//! and settled once it is, so a helper killed anywhere in between finishes the job and reports it
//! on its next run.

use std::path::{Path, PathBuf};

use filen_sdk_rs::io::write_private_file_atomically;
use filen_types::fs::Uuid;
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::{
	branding::Branding,
	helper::protocol::{ErrorCode, KeptEarlier, KeptId, KeptItem, ReplyError, RootDir, UserId},
};

/// The index's format version.
const INDEX_VERSION: u32 = 1;
/// The index, in the state directory.
const INDEX_FILE_NAME: &str = "roots.json";
/// What the index is written to before it is renamed onto `roots.json`.
const INDEX_TEMP_FILE_NAME: &str = "roots.json.tmp";
/// Held for the helper's life, so only one helper uses the state directory.
pub(crate) const LOCK_FILE_NAME: &str = "helper.lock";

/// The helper's state directory, which holds the roots index and the single-instance lock: in
/// local app data, the same for the app and the uninstaller. Absolute and UTF-8, as the index
/// spells its paths as strings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateDir(PathBuf);

/// Why a path cannot be the state directory.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StateDirError {
	/// The path is relative.
	#[error("{0:?} is not absolute")]
	NotAbsolute(PathBuf),
	/// The path is not valid UTF-8.
	#[error("{0:?} is not UTF-8")]
	NotUtf8(PathBuf),
}

impl StateDir {
	/// `path` as the state directory, which need not exist yet.
	pub fn new(path: PathBuf) -> Result<Self, StateDirError> {
		if !path.is_absolute() {
			return Err(StateDirError::NotAbsolute(path));
		}
		if path.to_str().is_none() {
			return Err(StateDirError::NotUtf8(path));
		}
		Ok(Self(path))
	}

	/// The directory.
	pub fn path(&self) -> &Path {
		&self.0
	}

	/// The single-instance lock file in it.
	pub fn lock_file(&self) -> PathBuf {
		self.0.join(LOCK_FILE_NAME)
	}
}

/// One root the index records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RootEntry {
	/// Whose root it is.
	pub(crate) user: UserId,
	/// Where it is.
	pub(crate) root: RootDir,
	/// The user's data directory.
	pub(crate) data_dir: PathBuf,
	/// The account root its registration carries, once registered with a session.
	pub(crate) account_root: Option<Uuid>,
	/// What unregisters of the root kept, until a reply the parent answered reported it.
	pub(crate) kept: Vec<KeptRecord>,
	/// The root is unregistered and the user's data wiped: the entry stays only for `kept`.
	pub(crate) unregistered: bool,
	/// What File Explorer shows for the root, as the last `register` named it; `None` for the
	/// default. Kept so a root served again without a `register` (before an unregister) shows the
	/// same.
	pub(crate) branding: Option<Branding>,
	/// The root is held (an `unregister` asked it, and its changes are not all on the drive yet):
	/// served, it refuses other processes' downloads, until a sign-in of the user lifts it or the
	/// root is unregistered. Never set on an unregistered entry.
	pub(crate) held: bool,
}

impl RootEntry {
	/// The record of an unregister whose items may still be in the root.
	pub(crate) fn unsettled(&self) -> Option<&KeptRecord> {
		self.kept.iter().find(|record| !record.settled())
	}

	/// What the settled records report, but `except`'s.
	pub(crate) fn earlier(&self, except: Option<KeptId>) -> Vec<KeptEarlier> {
		self.kept
			.iter()
			.filter(|record| record.settled() && Some(record.id) != except)
			.map(KeptRecord::earlier)
			.collect()
	}
}

/// What one unregister kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct KeptRecord {
	/// Names it on the wire.
	pub(crate) id: KeptId,
	/// The first items kept.
	pub(crate) kept: Vec<KeptItem>,
	/// How many items were kept.
	pub(crate) kept_count: u64,
	/// Where they are.
	pub(crate) place: KeptPlace,
}

/// Where what an unregister kept is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum KeptPlace {
	/// Still in the root, which the OS may hold registered yet; the root is renamed to `aside` once
	/// the OS let it go and anything stays in it (`None`: no free name, so it stays).
	InRoot { aside: Option<PathBuf> },
	/// Where they ended: the root renamed aside, the root itself, or `None` when they are nowhere the
	/// helper knows of.
	Settled { kept_in: Option<PathBuf> },
}

impl KeptRecord {
	/// Whether the items are where they end.
	pub(crate) fn settled(&self) -> bool {
		matches!(self.place, KeptPlace::Settled { .. })
	}

	/// Where the root is to be renamed aside, while the items are still in it.
	pub(crate) fn aside(&self) -> Option<&Path> {
		match &self.place {
			KeptPlace::InRoot { aside } => aside.as_deref(),
			KeptPlace::Settled { .. } => None,
		}
	}

	/// Where the items ended, once settled.
	pub(crate) fn kept_in(&self) -> Option<&Path> {
		match &self.place {
			KeptPlace::InRoot { .. } => None,
			KeptPlace::Settled { kept_in } => kept_in.as_deref(),
		}
	}

	/// The record as a reply reports it.
	pub(crate) fn earlier(&self) -> KeptEarlier {
		KeptEarlier {
			id: self.id,
			kept: self.kept.clone(),
			kept_count: self.kept_count,
			kept_in: self.kept_in().map(Path::to_path_buf),
		}
	}
}

/// Why the index could not be read or written.
#[derive(Debug, thiserror::Error)]
pub(crate) enum IndexError {
	/// A file operation failed.
	#[error("{context} {path:?}: {source}")]
	Io {
		context: &'static str,
		path: PathBuf,
		#[source]
		source: std::io::Error,
	},
	/// The file is not an index this build reads: refused rather than overwritten, so roots it
	/// names are not forgotten.
	#[error("{path:?} is not a roots index of version {INDEX_VERSION}")]
	Malformed { path: PathBuf },
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct IndexWire {
	version: u32,
	roots: Vec<EntryWire>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct EntryWire {
	user_id: String,
	root: String,
	data_dir: String,
	account_root: Option<Uuid>,
	// Left out when empty or false, so an index written before them reads the same.
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	kept: Vec<KeptWire>,
	#[serde(default, skip_serializing_if = "is_false")]
	unregistered: bool,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	display_name: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	icon_path: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	icon_index: Option<i32>,
	#[serde(default, skip_serializing_if = "is_false")]
	held: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct KeptWire {
	id: String,
	items: Vec<KeptItem>,
	count: u64,
	settled: bool,
	/// The planned aside while not settled, where they ended once settled.
	path: Option<String>,
}

fn is_false(value: &bool) -> bool {
	!*value
}

/// `error` as a reply's error.
pub(crate) fn index_error(error: &IndexError) -> ReplyError {
	ReplyError::new(ErrorCode::Io, error.to_string())
}

/// The roots index, as loaded from the state directory; every change is written through.
#[derive(Debug)]
pub(crate) struct RootsIndex {
	path: PathBuf,
	temp: PathBuf,
	entries: Vec<RootEntry>,
}

impl RootsIndex {
	/// The index in `dir`, empty when it has none yet. Creates the directory.
	pub(crate) fn load(dir: &StateDir) -> Result<Self, IndexError> {
		let path = dir.0.join(INDEX_FILE_NAME);
		std::fs::create_dir_all(&dir.0).map_err(|source| IndexError::Io {
			context: "creating",
			path: dir.0.clone(),
			source,
		})?;
		let entries = match std::fs::read(&path) {
			Ok(bytes) => {
				parse(&bytes).ok_or_else(|| IndexError::Malformed { path: path.clone() })?
			}
			Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
			Err(source) => {
				return Err(IndexError::Io {
					context: "reading",
					path,
					source,
				});
			}
		};
		Ok(Self {
			temp: dir.0.join(INDEX_TEMP_FILE_NAME),
			path,
			entries,
		})
	}

	/// Every root, in the order they were added.
	pub(crate) fn entries(&self) -> &[RootEntry] {
		&self.entries
	}

	/// `user`'s root.
	pub(crate) fn entry(&self, user: UserId) -> Option<&RootEntry> {
		self.entries.iter().find(|entry| entry.user == user)
	}

	/// Records `entry`, replacing what the index held for its user but the kept records, which stay
	/// until reported, and the hold of a root still registered, which only a sign-in or the
	/// unregister lifts.
	pub(crate) fn upsert(&mut self, mut entry: RootEntry) -> Result<(), IndexError> {
		let mut entries = self.entries.clone();
		match entries.iter_mut().find(|held| held.user == entry.user) {
			Some(held) => {
				entry.kept = std::mem::take(&mut held.kept);
				entry.held = held.held && !held.unregistered && !entry.unregistered;
				*held = entry;
			}
			None => entries.push(entry),
		}
		self.commit(entries)
	}

	/// Changes `user`'s entry with `change`, if the index holds one.
	fn change(
		&mut self,
		user: UserId,
		change: impl FnOnce(&mut RootEntry),
	) -> Result<(), IndexError> {
		let mut entries = self.entries.clone();
		let Some(entry) = entries.iter_mut().find(|entry| entry.user == user) else {
			return Ok(());
		};
		change(entry);
		self.commit(entries)
	}

	/// Records what File Explorer shows for `user`'s root, if the index holds it.
	pub(crate) fn set_branding(
		&mut self,
		user: UserId,
		branding: Option<Branding>,
	) -> Result<(), IndexError> {
		self.change(user, |entry| entry.branding = branding)
	}

	/// Records whether `user`'s root is held, if the index holds it; an unregistered root is never
	/// held.
	pub(crate) fn set_held(&mut self, user: UserId, held: bool) -> Result<(), IndexError> {
		self.change(user, |entry| entry.held = held && !entry.unregistered)
	}

	/// Records what an unregister of `user`'s root kept, replacing the record with its id, or the
	/// record of an unregister whose items are still in the root (one stopped part way, whose
	/// items a new walk found again). A settled record that kept nothing is dropped instead.
	pub(crate) fn put_kept(&mut self, user: UserId, record: KeptRecord) -> Result<(), IndexError> {
		self.change(user, |entry| {
			entry
				.kept
				.retain(|held| held.id != record.id && held.settled());
			if !(record.settled() && record.kept_count == 0) {
				entry.kept.push(record);
			}
		})
	}

	/// Drops the record of an unregister of `user`'s root whose items are still in it: the OS
	/// holds the root registered again, so they are served, not kept aside.
	pub(crate) fn drop_unsettled(&mut self, user: UserId) -> Result<(), IndexError> {
		self.change(user, |entry| entry.kept.retain(KeptRecord::settled))
	}

	/// Forgets `user`'s root, unregistered and its data wiped, its hold with it; while a record of
	/// what was kept waits to be reported, the entry stays, marked unregistered, for it.
	pub(crate) fn forget(&mut self, user: UserId) -> Result<(), IndexError> {
		if self.entry(user).is_some_and(|entry| !entry.kept.is_empty()) {
			return self.change(user, |entry| {
				entry.unregistered = true;
				entry.held = false;
			});
		}
		self.remove(user)
	}

	/// The parent read the reports `ids` of `user`'s root: their records go, and an unregistered
	/// entry with none left.
	pub(crate) fn acknowledge(&mut self, user: UserId, ids: &[KeptId]) -> Result<(), IndexError> {
		let Some(entry) = self.entry(user) else {
			return Ok(());
		};
		if !entry.kept.iter().any(|record| ids.contains(&record.id)) {
			return Ok(());
		}
		let mut entries = self.entries.clone();
		entries.retain_mut(|entry| {
			if entry.user != user {
				return true;
			}
			entry.kept.retain(|record| !ids.contains(&record.id));
			!(entry.unregistered && entry.kept.is_empty())
		});
		self.commit(entries)
	}

	/// Records that `user`'s root is registered for `account_root`.
	pub(crate) fn set_account_root(
		&mut self,
		user: UserId,
		account_root: Uuid,
	) -> Result<(), IndexError> {
		let mut entries = self.entries.clone();
		if let Some(entry) = entries.iter_mut().find(|entry| entry.user == user) {
			entry.account_root = Some(account_root);
		}
		self.commit(entries)
	}

	/// Forgets `user`'s root.
	pub(crate) fn remove(&mut self, user: UserId) -> Result<(), IndexError> {
		let mut entries = self.entries.clone();
		entries.retain(|entry| entry.user != user);
		self.commit(entries)
	}

	/// Writes `entries`, then holds them: an index that fails to save stays as it was on disk.
	fn commit(&mut self, entries: Vec<RootEntry>) -> Result<(), IndexError> {
		let wire = IndexWire {
			version: INDEX_VERSION,
			roots: entries
				.iter()
				.map(|entry| EntryWire {
					user_id: entry.user.to_string(),
					root: entry.root.as_str().to_owned(),
					data_dir: entry.data_dir.to_string_lossy().into_owned(),
					account_root: entry.account_root,
					kept: entry.kept.iter().map(kept_wire).collect(),
					unregistered: entry.unregistered,
					display_name: entry
						.branding
						.as_ref()
						.map(|branding| branding.display_name.as_str().to_owned()),
					icon_path: entry
						.branding
						.as_ref()
						.and_then(|branding| branding.icon.as_ref())
						.map(|icon| icon.as_str().to_owned()),
					icon_index: entry
						.branding
						.as_ref()
						.filter(|branding| branding.icon.is_some())
						.map(|branding| branding.icon_index),
					held: entry.held,
				})
				.collect(),
		};
		let bytes = serde_json::to_vec(&wire)
			.expect("the index holds only strings, numbers and uuids (should be impossible)");
		write_private_file_atomically(&self.temp, &self.path, &bytes).map_err(|source| {
			IndexError::Io {
				context: "writing",
				path: self.path.clone(),
				source,
			}
		})?;
		self.entries = entries;
		Ok(())
	}
}

/// The entries `bytes` hold, or `None` when they are not an index of this version.
fn parse(bytes: &[u8]) -> Option<Vec<RootEntry>> {
	let wire: IndexWire = serde_json::from_slice(bytes).ok()?;
	if wire.version != INDEX_VERSION {
		return None;
	}
	wire.roots
		.into_iter()
		.map(|entry| {
			Some(RootEntry {
				user: UserId::parse(&entry.user_id)?,
				root: RootDir::parse(entry.root).ok()?,
				data_dir: PathBuf::from(entry.data_dir),
				account_root: entry.account_root,
				kept: entry
					.kept
					.into_iter()
					.map(kept_record)
					.collect::<Option<_>>()?,
				unregistered: entry.unregistered,
				branding: stored_branding(
					entry.display_name.as_deref(),
					entry.icon_path.as_deref(),
					entry.icon_index,
				),
				// An unregistered root holds nothing: a hold an edited index names is dropped.
				held: entry.held && !entry.unregistered,
			})
		})
		.collect()
}

/// The branding an entry stored, or the default when it holds none, or one this build refuses:
/// the index is a file any process of the user can write, and the default is always safe to show,
/// so a bad value does not make the whole index unreadable.
fn stored_branding(
	display_name: Option<&str>,
	icon: Option<&str>,
	icon_index: Option<i32>,
) -> Option<Branding> {
	Branding::parse(display_name, icon, icon_index, None)
		.inspect_err(|error| warn!(%error, "the index's branding is refused; the default is shown"))
		.ok()
		.flatten()
}

fn kept_wire(record: &KeptRecord) -> KeptWire {
	let (settled, path) = match &record.place {
		KeptPlace::InRoot { aside } => (false, aside.as_deref()),
		KeptPlace::Settled { kept_in } => (true, kept_in.as_deref()),
	};
	KeptWire {
		id: record.id.to_string(),
		items: record.kept.clone(),
		count: record.kept_count,
		settled,
		path: path.map(|path| path.to_string_lossy().into_owned()),
	}
}

fn kept_record(wire: KeptWire) -> Option<KeptRecord> {
	let path = wire.path.map(PathBuf::from);
	Some(KeptRecord {
		id: KeptId::parse(&wire.id)?,
		kept: wire.items,
		kept_count: wire.count,
		place: if wire.settled {
			KeptPlace::Settled { kept_in: path }
		} else {
			KeptPlace::InRoot { aside: path }
		},
	})
}

#[cfg(test)]
mod tests {
	#[cfg(unix)]
	use std::os::unix::fs::PermissionsExt;

	use super::*;
	use crate::{branding::ExplorerCommands, helper::protocol::KeptItemReason};

	fn state_dir() -> (tempfile::TempDir, StateDir) {
		let temp = tempfile::tempdir().unwrap();
		let dir = StateDir::new(temp.path().join("state")).unwrap();
		(temp, dir)
	}

	fn entry(user: u64, root: &str) -> RootEntry {
		RootEntry {
			user: UserId::from(user),
			root: RootDir::parse(root.to_owned()).unwrap(),
			data_dir: PathBuf::from(format!("{root}-data")),
			account_root: None,
			kept: Vec::new(),
			unregistered: false,
			branding: None,
			held: false,
		}
	}

	#[test]
	fn a_hold_is_written_through_read_back_and_kept_by_a_register() {
		let (_temp, dir) = state_dir();
		let mut index = RootsIndex::load(&dir).unwrap();
		let user = UserId::from(42);
		index.upsert(entry(42, "/r")).unwrap();
		index.set_held(user, true).unwrap();
		assert_eq!(
			std::fs::read_to_string(dir.path().join(INDEX_FILE_NAME)).unwrap(),
			r#"{"version":1,"roots":[{"userId":"42","root":"/r","dataDir":"/r-data","accountRoot":null,"held":true}]}"#
		);
		assert!(RootsIndex::load(&dir).unwrap().entries()[0].held);
		index.upsert(entry(42, "/r")).unwrap();
		assert!(index.entry(user).unwrap().held, "a register keeps the hold");
		index.set_held(user, false).unwrap();
		assert_eq!(
			std::fs::read_to_string(dir.path().join(INDEX_FILE_NAME)).unwrap(),
			r#"{"version":1,"roots":[{"userId":"42","root":"/r","dataDir":"/r-data","accountRoot":null}]}"#
		);
	}

	#[test]
	fn forgetting_a_held_root_drops_its_hold_with_it() {
		let (_temp, dir) = state_dir();
		let mut index = RootsIndex::load(&dir).unwrap();
		let user = UserId::from(42);
		index.upsert(entry(42, "/r")).unwrap();
		index.set_held(user, true).unwrap();
		index
			.put_kept(user, record(1, KeptPlace::Settled { kept_in: None }))
			.unwrap();
		index.forget(user).unwrap();
		let kept_only = index.entry(user).unwrap();
		assert!(kept_only.unregistered);
		assert!(!kept_only.held);
		index.set_held(user, true).unwrap();
		assert!(
			!index.entry(user).unwrap().held,
			"an unregistered root is never held"
		);
		// A register of the root again, once let go, starts unheld.
		index.upsert(entry(42, "/r")).unwrap();
		assert!(!index.entry(user).unwrap().held);
		index.set_held(user, true).unwrap();
		index.acknowledge(user, &[KeptId::from(1)]).unwrap();
		index.forget(user).unwrap();
		assert!(index.entry(user).is_none());
	}

	#[test]
	fn a_roots_branding_is_written_through_and_read_back() {
		let (_temp, dir) = state_dir();
		let mut index = RootsIndex::load(&dir).unwrap();
		let mut named = entry(1, "C:\\a");
		named.branding = Branding::parse(
			Some("Filen (2)"),
			Some("C:\\Filen\\Filen.exe"),
			Some(2),
			None,
		)
		.unwrap();
		index.upsert(named.clone()).unwrap();
		index.upsert(entry(2, "C:\\b")).unwrap();
		let bytes = std::fs::read(dir.path().join(INDEX_FILE_NAME)).unwrap();
		let text = String::from_utf8(bytes).unwrap();
		assert!(
			text.contains(
				r#""displayName":"Filen (2)","iconPath":"C:\\Filen\\Filen.exe","iconIndex":2"#
			),
			"{text}"
		);
		let loaded = RootsIndex::load(&dir).unwrap();
		assert_eq!(loaded.entries(), [named, entry(2, "C:\\b")]);
	}

	#[test]
	fn a_roots_explorer_commands_are_not_written_to_the_index() {
		let (_temp, dir) = state_dir();
		let mut index = RootsIndex::load(&dir).unwrap();
		let commands =
			ExplorerCommands::parse("C:\\Filen\\Filen.exe", Some("Filen"), None, None, None)
				.unwrap();
		let mut named = entry(1, "C:\\a");
		named.branding = Branding::parse(Some("Filen (2)"), None, None, Some(commands)).unwrap();
		index.upsert(named).unwrap();
		let text = std::fs::read_to_string(dir.path().join(INDEX_FILE_NAME)).unwrap();
		assert!(text.contains(r#""displayName":"Filen (2)""#), "{text}");
		assert!(!text.contains("Filen.exe"), "{text}");
		let loaded = RootsIndex::load(&dir).unwrap();
		assert_eq!(
			loaded.entries()[0].branding,
			Branding::parse(Some("Filen (2)"), None, None, None).unwrap()
		);
	}

	#[test]
	fn a_branding_the_index_holds_that_this_build_refuses_reads_as_the_default() {
		let (_temp, dir) = state_dir();
		std::fs::create_dir_all(dir.path()).unwrap();
		std::fs::write(
			dir.path().join(INDEX_FILE_NAME),
			r#"{"version":1,"roots":[{"userId":"1","root":"C:\\a","dataDir":"C:\\d","accountRoot":null,"displayName":"%x%","iconPath":"rel.ico"}]}"#,
		)
		.unwrap();
		let loaded = RootsIndex::load(&dir).unwrap();
		assert_eq!(loaded.entries()[0].branding, None);
	}

	#[test]
	fn a_missing_index_is_empty() {
		let (_temp, dir) = state_dir();
		assert!(RootsIndex::load(&dir).unwrap().entries().is_empty());
	}

	#[test]
	fn entries_are_written_through_and_read_back() {
		let (_temp, dir) = state_dir();
		let mut index = RootsIndex::load(&dir).unwrap();
		index.upsert(entry(42, "/r42")).unwrap();
		index.upsert(entry(43, "/r43")).unwrap();
		index
			.set_account_root(UserId::from(42), Uuid::from_u128(0x100))
			.unwrap();
		index.upsert(entry(43, "/r43b")).unwrap();
		let loaded = RootsIndex::load(&dir).unwrap();
		assert_eq!(
			loaded.entries(),
			[
				RootEntry {
					account_root: Some(Uuid::from_u128(0x100)),
					..entry(42, "/r42")
				},
				entry(43, "/r43b"),
			]
		);
	}

	#[test]
	fn the_index_file_is_versioned_json_with_no_secret() {
		let (_temp, dir) = state_dir();
		let mut index = RootsIndex::load(&dir).unwrap();
		index.upsert(entry(42, "/r")).unwrap();
		let text = std::fs::read_to_string(dir.path().join("roots.json")).unwrap();
		assert_eq!(
			text,
			r#"{"version":1,"roots":[{"userId":"42","root":"/r","dataDir":"/r-data","accountRoot":null}]}"#
		);
	}

	fn record(id: u64, place: KeptPlace) -> KeptRecord {
		KeptRecord {
			id: KeptId::from(id),
			kept: vec![KeptItem {
				path: "a\\b.txt".to_owned(),
				reason: KeptItemReason::Modified,
			}],
			kept_count: 2,
			place,
		}
	}

	#[test]
	fn an_index_written_before_kept_records_reads_and_is_written_back_the_same() {
		let (_temp, dir) = state_dir();
		std::fs::create_dir_all(dir.path()).unwrap();
		let text = r#"{"version":1,"roots":[{"userId":"42","root":"/r","dataDir":"/r-data","accountRoot":null}]}"#;
		std::fs::write(dir.path().join("roots.json"), text).unwrap();
		let mut index = RootsIndex::load(&dir).unwrap();
		assert_eq!(index.entries(), [entry(42, "/r")]);
		index.upsert(entry(42, "/r")).unwrap();
		assert_eq!(
			std::fs::read_to_string(dir.path().join("roots.json")).unwrap(),
			text
		);
	}

	#[test]
	fn kept_records_are_written_through_and_read_back() {
		let (_temp, dir) = state_dir();
		let mut index = RootsIndex::load(&dir).unwrap();
		index.upsert(entry(42, "/r")).unwrap();
		let planned = record(
			0xab,
			KeptPlace::InRoot {
				aside: Some(PathBuf::from("/r (not synced d)")),
			},
		);
		index.put_kept(UserId::from(42), planned.clone()).unwrap();
		let text = std::fs::read_to_string(dir.path().join("roots.json")).unwrap();
		assert_eq!(
			text,
			r#"{"version":1,"roots":[{"userId":"42","root":"/r","dataDir":"/r-data","accountRoot":null,"kept":[{"id":"00000000000000ab","items":[{"path":"a\\b.txt","reason":"modified"}],"count":2,"settled":false,"path":"/r (not synced d)"}]}]}"#
		);
		assert_eq!(RootsIndex::load(&dir).unwrap().entries()[0].kept, [planned]);
		let settled = record(0xab, KeptPlace::Settled { kept_in: None });
		index.put_kept(UserId::from(42), settled.clone()).unwrap();
		assert_eq!(RootsIndex::load(&dir).unwrap().entries()[0].kept, [settled]);
	}

	#[test]
	fn a_record_stays_through_a_register_until_the_parent_read_it() {
		let (_temp, dir) = state_dir();
		let mut index = RootsIndex::load(&dir).unwrap();
		let user = UserId::from(42);
		index.upsert(entry(42, "/r")).unwrap();
		let earlier = record(1, KeptPlace::Settled { kept_in: None });
		index.put_kept(user, earlier.clone()).unwrap();
		// A new unregister's record replaces one still in the root, never a settled one.
		index
			.put_kept(user, record(2, KeptPlace::InRoot { aside: None }))
			.unwrap();
		let newer = record(3, KeptPlace::InRoot { aside: None });
		index.put_kept(user, newer.clone()).unwrap();
		assert_eq!(index.entry(user).unwrap().kept, [earlier.clone(), newer]);
		index.drop_unsettled(user).unwrap();
		index.upsert(entry(42, "/r")).unwrap();
		assert_eq!(index.entry(user).unwrap().kept, [earlier]);
		// Settled having kept nothing: nothing to report.
		index
			.put_kept(
				user,
				KeptRecord {
					kept_count: 0,
					..record(4, KeptPlace::Settled { kept_in: None })
				},
			)
			.unwrap();
		assert_eq!(index.entry(user).unwrap().kept.len(), 1);
		index.forget(user).unwrap();
		assert!(index.entry(user).unwrap().unregistered);
		index.acknowledge(user, &[KeptId::from(9)]).unwrap();
		assert!(index.entry(user).is_some());
		index.acknowledge(user, &[KeptId::from(1)]).unwrap();
		assert!(index.entry(user).is_none());
		assert!(RootsIndex::load(&dir).unwrap().entries().is_empty());
	}

	#[test]
	fn a_root_with_nothing_to_report_is_forgotten_at_once() {
		let (_temp, dir) = state_dir();
		let mut index = RootsIndex::load(&dir).unwrap();
		index.upsert(entry(42, "/r")).unwrap();
		index.forget(UserId::from(42)).unwrap();
		assert!(index.entries().is_empty());
	}

	#[test]
	fn an_index_with_a_malformed_kept_record_is_refused() {
		let (_temp, dir) = state_dir();
		std::fs::create_dir_all(dir.path()).unwrap();
		for id in ["AB", "00000000000000AB", "0000000000000g00"] {
			let text = format!(
				r#"{{"version":1,"roots":[{{"userId":"42","root":"/r","dataDir":"/d","accountRoot":null,"kept":[{{"id":"{id}","items":[],"count":1,"settled":true,"path":null}}]}}]}}"#
			);
			std::fs::write(dir.path().join("roots.json"), &text).unwrap();
			assert!(
				matches!(RootsIndex::load(&dir), Err(IndexError::Malformed { .. })),
				"{id}"
			);
		}
	}

	#[test]
	fn an_index_of_another_version_or_shape_is_refused_not_overwritten() {
		let (_temp, dir) = state_dir();
		std::fs::create_dir_all(dir.path()).unwrap();
		for text in [
			r#"{"version":2,"roots":[]}"#,
			r#"{"version":1}"#,
			"nonsense",
		] {
			std::fs::write(dir.path().join("roots.json"), text).unwrap();
			assert!(matches!(
				RootsIndex::load(&dir),
				Err(IndexError::Malformed { .. })
			));
			assert_eq!(
				std::fs::read_to_string(dir.path().join("roots.json")).unwrap(),
				text
			);
		}
	}

	#[cfg(unix)]
	#[test]
	fn a_change_that_cannot_be_saved_leaves_the_index_as_it_is_on_disk() {
		let (_temp, dir) = state_dir();
		let mut index = RootsIndex::load(&dir).unwrap();
		index.upsert(entry(42, "/r42")).unwrap();
		let saved = index.entries().to_vec();
		std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
		let upserted = index.upsert(entry(43, "/r43"));
		let replaced = index.upsert(entry(42, "/elsewhere"));
		let recorded = index.set_account_root(UserId::from(42), Uuid::from_u128(0x100));
		let removed = index.remove(UserId::from(42));
		std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
		for result in [upserted, replaced, recorded, removed] {
			assert!(matches!(result, Err(IndexError::Io { .. })));
		}
		assert_eq!(index.entries(), saved);
		assert_eq!(RootsIndex::load(&dir).unwrap().entries(), saved);
	}

	#[test]
	fn a_relative_state_dir_is_refused() {
		assert_eq!(
			StateDir::new(PathBuf::from("state")).unwrap_err(),
			StateDirError::NotAbsolute(PathBuf::from("state"))
		);
	}
}
