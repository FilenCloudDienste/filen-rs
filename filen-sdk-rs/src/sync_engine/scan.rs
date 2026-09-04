//! The local-side scan: walk a pair's local root into a `rel_path -> LocalNode` map, applying the
//! mtime+size fast-path so an unchanged file is never re-hashed.
//!
//! Paths are NFC-normalized (macOS hands back NFD) and `/`-joined so they key 1:1 against the
//! NFC-normalized remote snapshot and the baseline. Two entries that normalize to the same key are
//! a collision (the engine refuses to reconcile a pair with one — a 1:1 local mapping is
//! required). The scan reports whether it completed: a partial scan (an unreadable subtree, a
//! missing root) must never let the mass-delete guard propagate deletions.
//!
//! Symlinks are followed and their targets read as regular files (Filen has no symlink concept and
//! the engine never writes one); `walkdir`'s loop detection guards against cycles.

use std::{
	collections::{BTreeMap, HashMap},
	ffi::OsStr,
	path::{Component, Path},
};

use filen_types::crypto::Blake3Hash;
use unicode_normalization::UnicodeNormalization;

use super::baseline::{BaselineEntry, NodeKind};
use crate::{
	fs::name::ValidatedName,
	io::{DOWNLOAD_TMP_EXT, FilenMetaExt},
};

/// The per-pair local quarantine directory (where remote-propagated deletions are moved instead of
/// being destroyed). Always excluded from the scan so it is never itself synced back up.
pub(crate) const QUARANTINE_DIR: &str = ".filen-sync-trash";

/// One item observed under the local root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LocalNode {
	/// NFC-normalized, `/`-joined path relative to the local root.
	pub(crate) rel_path: String,
	pub(crate) kind: NodeKind,
	/// Size in bytes (0 for directories).
	pub(crate) size: u64,
	/// Modification time in epoch millis.
	pub(crate) mtime_millis: i64,
	/// BLAKE3 of the content (files only): reused from the baseline when `(size, mtime)` matched
	/// (the fast-path), otherwise freshly computed.
	pub(crate) content_hash: Option<Blake3Hash>,
}

/// What went wrong for one entry during a scan. Non-fatal individually (collected), but any error
/// marks the whole scan [`incomplete`](LocalScan::complete).
// Variant fields are diagnostic context surfaced through `Debug` (the errors are collected and
// logged), not read directly — kept for observability rather than deleted.
#[allow(dead_code)]
#[derive(Debug)]
pub(crate) enum ScanError {
	/// An entry could not be read (permission, vanished mid-walk, hash failure, a symlink loop).
	Io {
		rel_path: String,
		source: std::io::Error,
	},
	/// An entry whose name is not valid UTF-8 — Filen names are UTF-8, so it cannot be synced.
	NonUtf8Name { lossy_path: String },
	/// Two entries normalize to the same key — the pair cannot be reconciled until the user
	/// resolves it (a single local path cannot hold both).
	DuplicateName { rel_path: String },
}

/// The result of scanning a local root.
#[derive(Debug)]
pub(crate) struct LocalScan {
	pub(crate) nodes: HashMap<String, LocalNode>,
	/// `false` if the node set may be INCOMPLETE — a missing/unreadable root, an unreadable
	/// subtree, a non-UTF-8 name, or a normalized-name collision. The mass-delete guard refuses to
	/// propagate deletions from an incomplete scan (an empty/half-read source must not nuke the
	/// destination).
	pub(crate) complete: bool,
	pub(crate) errors: Vec<ScanError>,
	/// Paths whose NAME the remote would reject, mapped to the validator's own message. Neither
	/// the path nor its subtree is in `nodes`, so nothing is planned for them.
	///
	/// Deliberately NOT an entry in `errors`: those mean the scan may have MISSED something, which
	/// makes every apparent deletion untrustworthy. An unsyncable name is the opposite — a fully
	/// observed item that simply cannot be pushed — so the guard must not be tripped by it.
	pub(crate) invalid_names: BTreeMap<String, String>,
}

/// The reason the remote would reject `rel_path`'s own name, or `None` if it would accept it.
///
/// Uses the SDK's own validator — the very rule the upload and create-dir paths enforce — rather
/// than a second copy of the rules, so the scan can never disagree with what an upload does. Only
/// the LAST component is checked: every ancestor is itself a scanned entry that was checked when
/// the walk reached it.
fn name_rejection(rel_path: &str) -> Option<String> {
	let name = rel_path.rsplit('/').next()?;
	ValidatedName::try_from(name).err().map(|e| e.to_string())
}

/// Whether some ANCESTOR of `rel_path` was already rejected, in which case this entry is part of a
/// subtree that is already reported and must be skipped silently.
fn under_invalid_name(invalid: &BTreeMap<String, String>, rel_path: &str) -> bool {
	let mut path = rel_path;
	while let Some(cut) = path.rfind('/') {
		path = &path[..cut];
		if invalid.contains_key(path) {
			return true;
		}
	}
	false
}

/// NFC-normalize a relative path's components (case preserved) and `/`-join them. `None` if any
/// component is non-UTF-8 or not a plain name (a walked subtree only yields `Normal` components).
fn normalize_rel_path(rel: &Path) -> Option<String> {
	let mut parts = Vec::new();
	for component in rel.components() {
		match component {
			Component::Normal(os) => parts.push(os.to_str()?.nfc().collect::<String>()),
			_ => return None,
		}
	}
	Some(parts.join("/"))
}

/// The case-insensitive collision key for a (already NFC-normalized) relative path. Filen treats
/// names case-insensitively, so two paths differing only in case collide. Shared with the remote
/// view, which has to fold case the same way for its own duplicate check to mean the same thing.
pub(super) fn collision_key(rel_path: &str) -> String {
	rel_path.chars().flat_map(char::to_lowercase).collect()
}

/// BLAKE3 of the file at `path`, streamed (no full read into memory).
fn hash_file(path: &Path) -> std::io::Result<Blake3Hash> {
	let file = std::fs::File::open(path)?;
	let mut hasher = blake3::Hasher::new();
	hasher.update_reader(&file)?;
	Ok(hasher.finalize().into())
}

/// The fast-path: reuse the baseline's content hash when the file's `(size, mtime)` are unchanged,
/// so an untouched file is never re-hashed. `None` means "diverged or unknown — must hash".
fn fast_path_hash(baseline: Option<&BaselineEntry>, size: u64, mtime: i64) -> Option<Blake3Hash> {
	let entry = baseline?;
	if entry.kind == NodeKind::File && entry.size == Some(size) && entry.local_mtime == Some(mtime)
	{
		entry.content_hash
	} else {
		None
	}
}

/// Walk `root` into a `rel_path -> LocalNode` map. `baseline` (keyed by rel_path) drives the
/// fast-path. Blocking work — the engine calls this on a blocking thread.
pub(crate) fn scan_local(root: &Path, baseline: &HashMap<String, BaselineEntry>) -> LocalScan {
	let mut nodes = HashMap::new();
	let mut errors = Vec::new();
	let mut invalid_names = BTreeMap::new();
	let mut complete = true;
	// collision key -> the rel_path that claimed it, to detect a second entry normalizing the same.
	let mut claimed: HashMap<String, String> = HashMap::new();

	let walker = walkdir::WalkDir::new(root)
		.follow_links(true)
		.into_iter()
		// Never descend into our own quarantine dir (it holds locally-deleted items).
		.filter_entry(|e| e.depth() != 1 || e.file_name() != OsStr::new(QUARANTINE_DIR));

	for entry in walker {
		let entry = match entry {
			Ok(entry) => entry,
			Err(err) => {
				// A walk error (unreadable dir, symlink loop) — the tree is partial.
				complete = false;
				let rel_path = err
					.path()
					.and_then(|p| p.strip_prefix(root).ok())
					.and_then(normalize_rel_path)
					.unwrap_or_default();
				errors.push(ScanError::Io {
					rel_path,
					source: err
						.into_io_error()
						.unwrap_or_else(|| std::io::Error::other("directory walk error")),
				});
				continue;
			}
		};

		// The root itself is the pair's anchor, not a synced item.
		if entry.depth() == 0 {
			continue;
		}

		let Ok(rel) = entry.path().strip_prefix(root) else {
			continue;
		};
		let Some(rel_path) = normalize_rel_path(rel) else {
			complete = false;
			errors.push(ScanError::NonUtf8Name {
				lossy_path: rel.to_string_lossy().into_owned(),
			});
			continue;
		};

		// `metadata()` follows symlinks (the walker has follow_links set), so a symlinked dir/file
		// is classified by its target.
		let metadata = match entry.metadata() {
			Ok(metadata) => metadata,
			Err(err) => {
				complete = false;
				errors.push(ScanError::Io {
					rel_path,
					source: err
						.into_io_error()
						.unwrap_or_else(|| std::io::Error::other("metadata error")),
				});
				continue;
			}
		};

		let kind = if metadata.is_dir() {
			NodeKind::Dir
		} else if metadata.is_file() {
			NodeKind::File
		} else {
			// Sockets, FIFOs, devices, broken symlinks: not syncable, skip silently.
			continue;
		};

		// A `<uuid>.filendl` FILE is the temp file a download in flight is writing into this very
		// tree (see `Client::download_file_to_path`): partial bytes that must never be read as a
		// local item to upload. A directory with that suffix is a legitimate user item.
		if kind == NodeKind::File && entry.path().extension() == Some(OsStr::new(DOWNLOAD_TMP_EXT))
		{
			continue;
		}

		// A name the remote would reject can never be pushed, so the whole subtree is left out of
		// the plan and reported instead of failing an upload every pass. The scan still COMPLETED:
		// this is a fully observed item that cannot be synced, not evidence the walk missed
		// anything, so `complete` stays true and the delete guard is unaffected.
		if under_invalid_name(&invalid_names, &rel_path) {
			continue;
		}
		if let Some(reason) = name_rejection(&rel_path) {
			invalid_names.insert(rel_path, reason);
			continue;
		}

		if let Some(previous) = claimed.insert(collision_key(&rel_path), rel_path.clone()) {
			complete = false;
			errors.push(ScanError::DuplicateName {
				rel_path: format!("{previous} / {rel_path}"),
			});
			continue;
		}

		let node = match kind {
			NodeKind::Dir => LocalNode {
				rel_path: rel_path.clone(),
				kind,
				size: 0,
				mtime_millis: FilenMetaExt::modified(&metadata).timestamp_millis(),
				content_hash: None,
			},
			NodeKind::File => {
				let size = metadata.len();
				let mtime = FilenMetaExt::modified(&metadata).timestamp_millis();
				let content_hash = match fast_path_hash(baseline.get(&rel_path), size, mtime) {
					Some(hash) => Some(hash),
					None => match hash_file(entry.path()) {
						Ok(hash) => Some(hash),
						Err(source) => {
							complete = false;
							errors.push(ScanError::Io { rel_path, source });
							continue;
						}
					},
				};
				LocalNode {
					rel_path: rel_path.clone(),
					kind,
					size,
					mtime_millis: mtime,
					content_hash,
				}
			}
		};
		nodes.insert(rel_path, node);
	}

	LocalScan {
		nodes,
		complete,
		errors,
		invalid_names,
	}
}

#[cfg(test)]
mod tests {
	use std::fs;

	use uuid::Uuid;

	use super::*;
	use crate::sync_engine::baseline::BaselineState;

	fn temp_root() -> std::path::PathBuf {
		let dir = std::env::temp_dir().join(format!("filen_scan_test_{}", Uuid::new_v4()));
		fs::create_dir_all(&dir).unwrap();
		dir
	}

	#[test]
	fn collision_key_folds_case() {
		assert_eq!(collision_key("A/B.txt"), "a/b.txt");
		assert_eq!(collision_key("already/low.txt"), "already/low.txt");
	}

	#[test]
	fn scans_a_tree_into_normalized_nodes() {
		let root = temp_root();
		fs::write(root.join("a.txt"), b"hello").unwrap();
		fs::create_dir(root.join("sub")).unwrap();
		fs::write(root.join("sub").join("b.txt"), b"world").unwrap();

		let scan = scan_local(&root, &HashMap::new());
		assert!(
			scan.complete,
			"a clean tree scans completely: {:?}",
			scan.errors
		);

		let mut paths: Vec<_> = scan.nodes.keys().cloned().collect();
		paths.sort();
		assert_eq!(paths, vec!["a.txt", "sub", "sub/b.txt"]);

		let a = &scan.nodes["a.txt"];
		assert_eq!(a.kind, NodeKind::File);
		assert_eq!(a.size, 5);
		assert!(
			a.content_hash.is_some(),
			"files are hashed without a baseline"
		);
		assert_eq!(scan.nodes["sub"].kind, NodeKind::Dir);
		assert!(
			scan.nodes["sub"].content_hash.is_none(),
			"dirs have no hash"
		);

		fs::remove_dir_all(&root).ok();
	}

	#[test]
	fn fast_path_reuses_baseline_hash_when_size_and_mtime_match() {
		let root = temp_root();
		let file = root.join("a.txt");
		fs::write(&file, b"hello").unwrap();
		let meta = fs::metadata(&file).unwrap();
		let mtime = FilenMetaExt::modified(&meta).timestamp_millis();

		// A baseline whose (size, mtime) match the file but carries a SENTINEL hash that is NOT the
		// real content hash. The fast-path must hand back the sentinel without re-hashing.
		let sentinel = Blake3Hash::from([0xAB; 32]);
		let baseline = HashMap::from([(
			"a.txt".to_string(),
			BaselineEntry {
				rel_path: "a.txt".to_string(),
				kind: NodeKind::File,
				remote_uuid: None,
				content_hash: Some(sentinel),
				size: Some(5),
				local_mtime: Some(mtime),
				remote_modified: None,
				state: BaselineState::Synced,
				local_kind: None,
				remote_kind: None,
				remote_hash: None,
				remote_size: None,
			},
		)]);

		let scan = scan_local(&root, &baseline);
		assert_eq!(
			scan.nodes["a.txt"].content_hash,
			Some(sentinel),
			"unchanged (size, mtime) reuses the baseline hash — no re-hash"
		);

		// A baseline with a stale size forces a real re-hash (sentinel must NOT survive).
		let stale = HashMap::from([(
			"a.txt".to_string(),
			BaselineEntry {
				size: Some(999),
				..baseline["a.txt"].clone()
			},
		)]);
		let rescan = scan_local(&root, &stale);
		assert_ne!(
			rescan.nodes["a.txt"].content_hash,
			Some(sentinel),
			"a diverged size triggers a fresh hash"
		);

		fs::remove_dir_all(&root).ok();
	}

	#[test]
	fn missing_root_is_reported_incomplete() {
		let root = std::env::temp_dir().join(format!("filen_scan_absent_{}", Uuid::new_v4()));
		let scan = scan_local(&root, &HashMap::new());
		assert!(
			!scan.complete,
			"a missing root scans incomplete (guards mass-delete)"
		);
		assert!(scan.nodes.is_empty());
		assert!(!scan.errors.is_empty());
	}

	#[test]
	fn in_flight_download_temp_files_are_excluded() {
		let root = temp_root();
		fs::write(root.join("keep.txt"), b"x").unwrap();
		// What `download_file_to_path` writes into the tree while a transfer is in flight.
		fs::write(
			root.join("dee76e0e-0000-0000-0000-000000000000.filendl"),
			b"half",
		)
		.unwrap();
		// A DIRECTORY with that suffix is a legitimate user item and stays.
		fs::create_dir(root.join("notes.filendl")).unwrap();

		let scan = scan_local(&root, &HashMap::new());
		let mut paths: Vec<_> = scan.nodes.keys().cloned().collect();
		paths.sort();
		assert_eq!(
			paths,
			vec!["keep.txt", "notes.filendl"],
			"a partial download must never be seen as a local file to upload"
		);
		assert!(
			scan.complete,
			"skipping our own staging file is not an error"
		);

		fs::remove_dir_all(&root).ok();
	}

	#[test]
	fn a_name_the_remote_would_reject_is_excluded_with_its_whole_subtree() {
		let root = temp_root();
		fs::write(root.join("keep.txt"), b"x").unwrap();
		// `CON` is a reserved device name and `bad.` ends in a dot: both are creatable on unix and
		// both are refused by the SDK's own name validator, so no upload of them can ever succeed.
		fs::write(root.join("CON"), b"reserved").unwrap();
		fs::create_dir(root.join("bad.")).unwrap();
		fs::write(root.join("bad.").join("inner.txt"), b"child").unwrap();
		fs::create_dir_all(root.join("bad.").join("deeper")).unwrap();
		fs::write(root.join("bad.").join("deeper").join("x.txt"), b"deep").unwrap();

		let scan = scan_local(&root, &HashMap::new());
		let mut paths: Vec<_> = scan.nodes.keys().cloned().collect();
		paths.sort();
		assert_eq!(
			paths,
			vec!["keep.txt"],
			"a rejected name takes its whole subtree out of the plan"
		);

		let reported: Vec<_> = scan.invalid_names.keys().cloned().collect();
		assert_eq!(
			reported,
			vec!["CON", "bad."],
			"each rejected name is reported ONCE, not once per descendant"
		);
		assert!(
			scan.invalid_names["bad."].contains("dot or space"),
			"the report carries the validator's own reason: {:?}",
			scan.invalid_names["bad."]
		);

		// Crucially the scan still COMPLETED: an unsyncable name is not missing evidence, so the
		// delete guard must not start holding every deletion because of it.
		assert!(scan.complete, "{:?}", scan.errors);
		assert!(scan.errors.is_empty(), "{:?}", scan.errors);

		fs::remove_dir_all(&root).ok();
	}

	#[test]
	fn a_valid_name_that_merely_contains_a_rejected_one_is_kept() {
		let root = temp_root();
		for name in ["CONSOLE", "console.txt", "CON.txt", "a.b", ".hidden"] {
			fs::write(root.join(name), b"x").unwrap();
		}
		let scan = scan_local(&root, &HashMap::new());
		assert!(
			scan.invalid_names.is_empty(),
			"none of these are rejected by the SDK validator: {:?}",
			scan.invalid_names
		);
		assert_eq!(scan.nodes.len(), 5);
		fs::remove_dir_all(&root).ok();
	}

	#[test]
	fn quarantine_dir_is_excluded() {
		let root = temp_root();
		fs::write(root.join("keep.txt"), b"x").unwrap();
		let quarantine = root.join(QUARANTINE_DIR);
		fs::create_dir(&quarantine).unwrap();
		fs::write(quarantine.join("trashed.txt"), b"old").unwrap();

		let scan = scan_local(&root, &HashMap::new());
		let paths: Vec<_> = scan.nodes.keys().cloned().collect();
		assert_eq!(
			paths,
			vec!["keep.txt"],
			"the quarantine subtree is not scanned"
		);

		fs::remove_dir_all(&root).ok();
	}
}
