//! What the engine keeps across a restart, in the root's data directory: the file placeholder each
//! path held ([`SavedSeen`]), so a save made while the provider was not running, or that it was
//! stopped in the middle of, is still the new version of the file it replaced; and every rename the
//! drive was changed for that is not settled yet ([`RenameRecord`]), so one the user was refused
//! is put back on the drive even when the provider was killed before it could.
//!
//! Each file is written whole and atomically (a temporary file renamed over it), and names the
//! sync root it belongs to: the account's root and the local root directory's file id, so a
//! record is never applied to another root, or to a root folder made anew at the same path. A file
//! that is missing, unreadable, of another format or of another root is ignored: the engine then
//! works as on a first start, which keeps both sides of what it cannot tell apart.

use std::{
	io,
	path::PathBuf,
	sync::{Arc, Mutex, PoisonError},
};

use filen_mobile_native_cache::ffi::FfiId;
use filen_sdk_rs::io::write_private_file_atomically;
use filen_types::fs::Uuid;
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::{
	identity::{IdentityBlob, PlaceholderIdentity},
	os::LocalFileId,
	root_path::RootPath,
};

const LINEAGE_FILE_NAME: &str = "cloud-files-lineage.json";
const LINEAGE_TEMP_FILE_NAME: &str = "cloud-files-lineage.json.tmp";
const RENAMES_FILE_NAME: &str = "cloud-files-renames.json";
const RENAMES_TEMP_FILE_NAME: &str = "cloud-files-renames.json.tmp";
/// Every file the records are kept in, temporary files included: what wiping a data directory
/// removes beside the session's and the cache's.
pub(crate) const RECORD_FILE_NAMES: [&str; 4] = [
	LINEAGE_FILE_NAME,
	LINEAGE_TEMP_FILE_NAME,
	RENAMES_FILE_NAME,
	RENAMES_TEMP_FILE_NAME,
];
/// The format of both files; a file of another is ignored.
const FORMAT: u32 = 1;

/// The sync root a record belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct RootKey {
	/// The account's root directory.
	pub(super) root: Uuid,
	/// The local root directory's file id.
	pub(super) dir: LocalFileId,
}

/// A file placeholder the engine saw at a path, as saved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SavedSeen {
	pub(super) path: RootPath,
	pub(super) stable: Uuid,
	pub(super) version: Uuid,
	/// Where the user renamed it to, when the path claims it for a save through a backup.
	pub(super) moved_to: Option<RootPath>,
}

/// A rename the drive was changed for, or is about to be, as saved: where the item was on the
/// drive, so it can be put back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RenameRecord {
	/// Tells the records of one run apart; not saved, given anew on loading.
	pub(super) id: u64,
	pub(super) identity: PlaceholderIdentity,
	pub(super) from: RootPath,
	pub(super) to: RootPath,
	/// The item's cache id.
	pub(super) item: FfiId,
	/// The cache id of the parent the item had on the drive, and its uuid.
	pub(super) old_parent: FfiId,
	pub(super) old_parent_uuid: Uuid,
	/// The name the item had on the drive.
	pub(super) old_name: Option<String>,
	/// The rename happened locally as the drive has it: the record stays only until the lineage
	/// saved after it, which then holds where the item went.
	pub(super) confirmed: bool,
	/// Another file the drive held at the rename's target name, which the rename replaces locally:
	/// moved to its transit name for the rename, trashed once the rename happened locally, and
	/// given its name back when the rename is put back.
	pub(super) displaced: Option<Displaced>,
	/// The file the rename saves, when it puts a save's temporary file, uploaded as a file of its
	/// own moments before, where that file was: the drive is not changed for it, and once it
	/// happened the temporary file becomes the ordinary file it was before its upload, which the
	/// path's job uploads as the saved file's new version, and its upload goes to the trash.
	pub(super) save_over: Option<SaveOver>,
}

/// The file a rename of a save's temporary file saves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct SaveOver {
	/// Its stable id.
	pub(super) stable: Uuid,
	/// The version the path the rename puts the temporary file at held.
	pub(super) version: Uuid,
}

impl RenameRecord {
	/// Whether the rename is of the item `uuid` (a file's stable id, a directory's uuid), or saves
	/// it.
	pub(super) fn concerns(&self, uuid: Uuid) -> bool {
		super::remote::populated_uuid(self.identity) == Some(uuid)
			|| self.save_over.is_some_and(|save| save.stable == uuid)
	}
}

/// A file a rename replaces, moved out of its way on the drive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Displaced {
	/// Its stable id.
	pub(super) stable: Uuid,
	/// The cache id of its directory.
	pub(super) dir: FfiId,
	/// Its name there before it was moved out of the way.
	pub(super) name: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct RootWire {
	root: Uuid,
	/// The local root directory's file id, in hex.
	dir: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct LineageWire {
	format: u32,
	root: RootWire,
	entries: Vec<SeenWire>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct SeenWire {
	path: Vec<String>,
	stable: Uuid,
	version: Uuid,
	moved_to: Option<Vec<String>>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct RenamesWire {
	format: u32,
	root: RootWire,
	renames: Vec<RenameWire>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct RenameWire {
	/// The placeholder identity's bytes.
	identity: Vec<u8>,
	from: Vec<String>,
	to: Vec<String>,
	item: String,
	old_parent: String,
	old_parent_uuid: String,
	old_name: Option<String>,
	confirmed: bool,
	/// Absent from the records an earlier build wrote.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	displaced: Option<DisplacedWire>,
	/// Absent from the records an earlier build wrote.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	save_over: Option<SaveOverWire>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct SaveOverWire {
	stable: Uuid,
	version: Uuid,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct DisplacedWire {
	stable: Uuid,
	dir: String,
	name: String,
}

fn root_wire(key: RootKey) -> RootWire {
	RootWire {
		root: key.root,
		dir: format!("{:x}", key.dir.get()),
	}
}

fn same_root(wire: &RootWire, key: RootKey) -> bool {
	wire.root == key.root && u128::from_str_radix(&wire.dir, 16).ok() == Some(key.dir.get())
}

fn path_wire(path: &RootPath) -> Vec<String> {
	path.components()
		.iter()
		.map(|name| name.as_str().to_owned())
		.collect()
}

fn path_of(wire: &[String]) -> Option<RootPath> {
	RootPath::parse(wire.iter().map(String::as_str)).ok()
}

/// The engine's records in one data directory. Clones share one write lock.
#[derive(Debug, Clone)]
pub(super) struct Records {
	dir: PathBuf,
	/// Held while a file is written.
	// Reason for the Arc: the engine's blocking writes each take a clone.
	writes: Arc<Mutex<Writes>>,
}

/// What was written, under the write lock.
#[derive(Debug, Default)]
struct Writes {
	/// Set once closed, after which nothing is written, so a write left running by a stopped engine
	/// never lands in a directory being wiped.
	closed: bool,
	/// The newest snapshot written of each file: an older one that reaches the lock later (its
	/// task stopped while its write ran on) is dropped.
	lineage: Option<Snapshot>,
	renames: Option<Snapshot>,
}

/// The number of a snapshot of what the engine keeps, taken in the order of the state it holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Snapshot(pub(super) u64);

/// Why the records were not written.
#[derive(Debug, thiserror::Error)]
pub(super) enum WriteError {
	/// The records are closed: the engine stopped.
	#[error("the records are closed")]
	Closed,
	#[error(transparent)]
	Io(#[from] io::Error),
}

/// Which file a write goes to.
#[derive(Debug, Clone, Copy)]
enum Kept {
	Lineage,
	Renames,
}

impl Records {
	/// The records kept in the directory `dir`, which must exist.
	pub(super) fn new(dir: PathBuf) -> Self {
		Self {
			dir,
			writes: Arc::new(Mutex::new(Writes::default())),
		}
	}

	/// Waits for a write under way to end, then refuses every later one. Blocks.
	pub(super) fn close(&self) {
		self.writes
			.lock()
			.unwrap_or_else(PoisonError::into_inner)
			.closed = true;
	}

	/// The bytes of the file `name`, `None` when it is missing or unreadable (logged).
	fn read(&self, name: &str) -> Option<Vec<u8>> {
		let path = self.dir.join(name);
		match std::fs::read(&path) {
			Ok(bytes) => Some(bytes),
			Err(error) if error.kind() == io::ErrorKind::NotFound => None,
			Err(error) => {
				warn!(?path, %error, "reading the engine's records failed; starting without them");
				None
			}
		}
	}

	/// Writes `bytes`, the snapshot `snapshot`, to the file `kept` names through its temporary
	/// file: nothing once closed, and nothing older than the snapshot written last, which holds
	/// newer state.
	fn write(&self, kept: Kept, snapshot: Snapshot, bytes: &[u8]) -> Result<(), WriteError> {
		let mut writes = self.writes.lock().unwrap_or_else(PoisonError::into_inner);
		if writes.closed {
			return Err(WriteError::Closed);
		}
		let (temp, name, last) = match kept {
			Kept::Lineage => (
				LINEAGE_TEMP_FILE_NAME,
				LINEAGE_FILE_NAME,
				&mut writes.lineage,
			),
			Kept::Renames => (
				RENAMES_TEMP_FILE_NAME,
				RENAMES_FILE_NAME,
				&mut writes.renames,
			),
		};
		if last.is_some_and(|last| last >= snapshot) {
			return Ok(());
		}
		write_private_file_atomically(&self.dir.join(temp), &self.dir.join(name), bytes)?;
		*last = Some(snapshot);
		Ok(())
	}

	/// The lineage saved for the root `key`; empty when there is none for it.
	pub(super) fn load_lineage(&self, key: RootKey) -> Vec<SavedSeen> {
		let Some(bytes) = self.read(LINEAGE_FILE_NAME) else {
			return Vec::new();
		};
		let wire: LineageWire = match serde_json::from_slice(&bytes) {
			Ok(wire) => wire,
			Err(error) => {
				warn!(%error, "the saved lineage does not parse; starting without it");
				return Vec::new();
			}
		};
		if wire.format != FORMAT || !same_root(&wire.root, key) {
			warn!("the saved lineage is of another format or root; starting without it");
			return Vec::new();
		}
		let saved = wire.entries.len();
		let loaded: Vec<SavedSeen> = wire
			.entries
			.into_iter()
			.filter_map(|entry| {
				Some(SavedSeen {
					path: path_of(&entry.path)?,
					stable: entry.stable,
					version: entry.version,
					moved_to: match entry.moved_to {
						Some(moved_to) => Some(path_of(&moved_to)?),
						None => None,
					},
				})
			})
			.collect();
		if loaded.len() < saved {
			warn!(
				left_out = saved - loaded.len(),
				"saved lineage entries do not parse; they are left out"
			);
		}
		loaded
	}

	/// Saves `entries`, the snapshot `snapshot`, as the lineage of the root `key`.
	pub(super) fn save_lineage(
		&self,
		key: RootKey,
		snapshot: Snapshot,
		entries: &[SavedSeen],
	) -> Result<(), WriteError> {
		let wire = LineageWire {
			format: FORMAT,
			root: root_wire(key),
			entries: entries
				.iter()
				.map(|seen| SeenWire {
					path: path_wire(&seen.path),
					stable: seen.stable,
					version: seen.version,
					moved_to: seen.moved_to.as_ref().map(path_wire),
				})
				.collect(),
		};
		let bytes = serde_json::to_vec(&wire)
			.expect("the lineage holds only strings, numbers and uuids (should be impossible)");
		self.write(Kept::Lineage, snapshot, &bytes)
	}

	/// The renames saved for the root `key`, numbered from `first_id`; empty when there are none
	/// for it.
	pub(super) fn load_renames(&self, key: RootKey, first_id: u64) -> Vec<RenameRecord> {
		let Some(bytes) = self.read(RENAMES_FILE_NAME) else {
			return Vec::new();
		};
		let wire: RenamesWire = match serde_json::from_slice(&bytes) {
			Ok(wire) => wire,
			Err(error) => {
				warn!(%error, "the saved renames do not parse; starting without them");
				return Vec::new();
			}
		};
		if wire.format != FORMAT || !same_root(&wire.root, key) {
			warn!("the saved renames are of another format or root; starting without them");
			return Vec::new();
		}
		let mut loaded = Vec::with_capacity(wire.renames.len());
		for (rename, id) in wire.renames.into_iter().zip(first_id..) {
			let identity = IdentityBlob::try_from(rename.identity.as_slice())
				.and_then(|blob| PlaceholderIdentity::decode(&blob));
			let (Ok(identity), Some(from), Some(to), Ok(old_parent_uuid)) = (
				identity,
				path_of(&rename.from),
				path_of(&rename.to),
				Uuid::parse_str(&rename.old_parent_uuid),
			) else {
				warn!("a saved rename does not parse; it is left out");
				continue;
			};
			loaded.push(RenameRecord {
				id,
				identity,
				from,
				to,
				item: FfiId(rename.item),
				old_parent: FfiId(rename.old_parent),
				old_parent_uuid,
				old_name: rename.old_name,
				confirmed: rename.confirmed,
				displaced: rename.displaced.map(|wire| Displaced {
					stable: wire.stable,
					dir: FfiId(wire.dir),
					name: wire.name,
				}),
				save_over: rename.save_over.map(|wire| SaveOver {
					stable: wire.stable,
					version: wire.version,
				}),
			});
		}
		loaded
	}

	/// Saves `renames`, the snapshot `snapshot`, as the renames of the root `key`.
	pub(super) fn save_renames(
		&self,
		key: RootKey,
		snapshot: Snapshot,
		renames: &[RenameRecord],
	) -> Result<(), WriteError> {
		let wire = RenamesWire {
			format: FORMAT,
			root: root_wire(key),
			renames: renames
				.iter()
				.map(|rename| RenameWire {
					identity: rename.identity.encode().as_bytes().to_vec(),
					from: path_wire(&rename.from),
					to: path_wire(&rename.to),
					item: rename.item.0.clone(),
					old_parent: rename.old_parent.0.clone(),
					old_parent_uuid: rename.old_parent_uuid.to_string(),
					old_name: rename.old_name.clone(),
					confirmed: rename.confirmed,
					displaced: rename.displaced.as_ref().map(|displaced| DisplacedWire {
						stable: displaced.stable,
						dir: displaced.dir.0.clone(),
						name: displaced.name.clone(),
					}),
					save_over: rename.save_over.map(|save| SaveOverWire {
						stable: save.stable,
						version: save.version,
					}),
				})
				.collect(),
		};
		let bytes = serde_json::to_vec(&wire)
			.expect("the renames hold only strings, numbers and uuids (should be impossible)");
		self.write(Kept::Renames, snapshot, &bytes)
	}
}

#[cfg(test)]
mod tests {
	use tempfile::TempDir;

	use super::*;
	use crate::test_support::root_path;

	const KEY: RootKey = RootKey {
		root: Uuid::from_u128(1),
		dir: LocalFileId::new(0xfeed),
	};

	fn seen(path: &str, moved_to: Option<&str>) -> SavedSeen {
		SavedSeen {
			path: root_path(path),
			stable: Uuid::from_u128(2),
			version: Uuid::from_u128(3),
			moved_to: moved_to.map(root_path),
		}
	}

	fn rename() -> RenameRecord {
		RenameRecord {
			id: 7,
			identity: PlaceholderIdentity::File {
				stable: Uuid::from_u128(2),
				version: Uuid::from_u128(3),
			},
			from: root_path("a.txt"),
			to: root_path(r"d\b.txt"),
			item: FfiId("stable/x".to_owned()),
			old_parent: FfiId("root".to_owned()),
			old_parent_uuid: Uuid::from_u128(1),
			old_name: Some("a.txt".to_owned()),
			confirmed: false,
			displaced: None,
			save_over: None,
		}
	}

	#[test]
	fn nothing_is_written_once_closed() {
		let temp = TempDir::new().unwrap();
		let records = Records::new(temp.path().to_path_buf());
		records
			.save_lineage(KEY, Snapshot(1), &[seen("a.txt", None)])
			.unwrap();
		records.close();
		assert!(matches!(
			records.save_lineage(KEY, Snapshot(2), &[seen("b.txt", None)]),
			Err(WriteError::Closed)
		));
		assert!(matches!(
			records.save_renames(KEY, Snapshot(3), &[rename()]),
			Err(WriteError::Closed)
		));
		assert_eq!(records.load_lineage(KEY), [seen("a.txt", None)]);
		assert_eq!(records.load_renames(KEY, 0), []);
	}

	#[test]
	fn an_older_snapshot_never_lands_after_a_newer_one() {
		let temp = TempDir::new().unwrap();
		let records = Records::new(temp.path().to_path_buf());
		records
			.save_lineage(KEY, Snapshot(5), &[seen("new.txt", None)])
			.unwrap();
		records
			.save_lineage(KEY, Snapshot(4), &[seen("old.txt", None)])
			.unwrap();
		assert_eq!(records.load_lineage(KEY), [seen("new.txt", None)]);
		records.save_renames(KEY, Snapshot(7), &[]).unwrap();
		records.save_renames(KEY, Snapshot(6), &[rename()]).unwrap();
		assert_eq!(records.load_renames(KEY, 0), []);
	}

	#[test]
	fn a_saved_lineage_loads_as_it_was_saved() {
		let temp = TempDir::new().unwrap();
		let records = Records::new(temp.path().to_path_buf());
		let saved = vec![seen(r"d\a.txt", None), seen("b.txt", Some("b.bak"))];
		records.save_lineage(KEY, Snapshot(1), &saved).unwrap();
		assert_eq!(records.load_lineage(KEY), saved);
	}

	#[test]
	fn saved_renames_load_numbered_from_the_first_id() {
		let temp = TempDir::new().unwrap();
		let records = Records::new(temp.path().to_path_buf());
		records.save_renames(KEY, Snapshot(2), &[rename()]).unwrap();
		assert_eq!(
			records.load_renames(KEY, 40),
			[RenameRecord { id: 40, ..rename() }]
		);
	}

	#[test]
	fn a_saved_rename_keeps_the_file_it_moved_out_of_the_way() {
		let temp = TempDir::new().unwrap();
		let records = Records::new(temp.path().to_path_buf());
		let replacing = RenameRecord {
			displaced: Some(Displaced {
				stable: Uuid::from_u128(9),
				dir: FfiId("stable/d".to_owned()),
				name: "b.txt".to_owned(),
			}),
			..rename()
		};
		records
			.save_renames(KEY, Snapshot(2), std::slice::from_ref(&replacing))
			.unwrap();
		assert_eq!(records.load_renames(KEY, 7), [replacing]);
	}

	#[test]
	fn a_saved_rename_keeps_the_file_it_saves() {
		let temp = TempDir::new().unwrap();
		let records = Records::new(temp.path().to_path_buf());
		let saving = RenameRecord {
			save_over: Some(SaveOver {
				stable: Uuid::from_u128(9),
				version: Uuid::from_u128(10),
			}),
			..rename()
		};
		records
			.save_renames(KEY, Snapshot(2), std::slice::from_ref(&saving))
			.unwrap();
		assert_eq!(records.load_renames(KEY, 7), [saving]);
	}

	#[test]
	fn records_of_another_root_folder_are_ignored() {
		let temp = TempDir::new().unwrap();
		let records = Records::new(temp.path().to_path_buf());
		records
			.save_lineage(KEY, Snapshot(1), &[seen("a.txt", None)])
			.unwrap();
		records.save_renames(KEY, Snapshot(2), &[rename()]).unwrap();
		let other = RootKey {
			dir: LocalFileId::new(0xbeef),
			..KEY
		};
		assert_eq!(records.load_lineage(other), []);
		assert_eq!(records.load_renames(other, 0), []);
	}

	#[test]
	fn missing_or_damaged_records_load_as_none() {
		let temp = TempDir::new().unwrap();
		let records = Records::new(temp.path().to_path_buf());
		assert_eq!(records.load_lineage(KEY), []);
		std::fs::write(temp.path().join(LINEAGE_FILE_NAME), b"{not json").unwrap();
		std::fs::write(temp.path().join(RENAMES_FILE_NAME), b"[]").unwrap();
		assert_eq!(records.load_lineage(KEY), []);
		assert_eq!(records.load_renames(KEY, 0), []);
	}
}
