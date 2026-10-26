//! The import of a filen-sync pair's last synced state: what the engine this one replaces recorded
//! as synced, kept as evidence for the paths this engine has no row for yet.
//!
//! filen-sync keeps a pair's state in `<dbPath>/state/v2/<pairUuid>/`, a file per record, each line
//! `JSON.stringify({prop, data})`: the path, and the item that engine last saw there. Two of them
//! are read, `previousLocalTree` and `previousRemoteTree`, both saved at the end of a cycle in which
//! that engine had brought the two sides together; nothing else in the directory is opened.
//!
//! The reader is the old one's, made strict where it was lenient. That one read the lines with
//! `\r\n` taken as one break, skipped blank and unparsable ones, needed both `prop` and `data`, and
//! let the last line for a path win; so does this one. A path is then keyed as the scan keys it
//! (NFC, through [`normalize_rel_path`]), and one that names no plain relative path — an empty
//! component, `.`, `..` — is skipped. What a path imports is decided by
//! [`BaselineStore::replace_legacy_synced`].
//!
//! A remote item also carries the file's SHA-512 `hash` and its encryption `key`. Neither has a
//! field here: serde skips both while reading the line, so neither reaches a struct, a log line or
//! an error. A line that cannot be read is named in the log by its file and number alone, never by
//! its text or by what serde said about it, since either could quote the key.

use std::{
	borrow::Cow,
	fs::File,
	io::{self, BufRead, BufReader},
	path::Path,
};

use filen_macros::js_type;
use serde::Deserialize;
use uuid::Uuid;

use super::{
	baseline::{BaselineStore, LegacyItem, LegacySide, PairId, PutLegacy},
	engine::db_error,
	scan::normalize_rel_path,
};
use crate::{Error, ErrorKind};

/// The two files of a filen-sync state directory that are read.
const LOCAL_TREE: &str = "previousLocalTree";
const REMOTE_TREE: &str = "previousRemoteTree";

/// How many unreadable lines an import names in the log, one warning each; the rest are only
/// counted, in [`LegacyImport::skipped`] and the summary line. A state file that is garbage from
/// end to end would otherwise log a line per line of it.
const WARNED_LINES: u64 = 16;

/// What [`SyncEngine::import_legacy_state`](super::SyncEngine::import_legacy_state) imported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[js_type(export, no_deser, no_default, napi)]
pub struct LegacyImport {
	/// How many paths the pair now holds filen-sync state for: files both trees list, at the same
	/// size, with what each needs to be compared to this engine's view of them.
	pub imported: u64,
	/// How many lines of the two trees could not be read: not UTF-8, not JSON, missing `prop` or
	/// `data`, a value of the wrong type, or a path that names no plain relative path. Blank lines
	/// are not counted.
	pub skipped: u64,
}

/// One line of a tree, its `data` read as `D`, the item of that tree. `data` is not optional: a
/// line without it is skipped, as the old reader skipped it.
#[derive(Deserialize)]
struct Line<'a, D> {
	#[serde(borrow)]
	prop: Cow<'a, str>,
	data: D,
}

/// The fields of a `previousLocalTree` item this engine reads. Unknown fields are ignored. A value
/// of another type than the field's — a float `lastModified` among them — fails the whole line.
#[derive(Deserialize)]
struct LocalItem<'a> {
	#[serde(rename = "type", borrow)]
	kind: Option<Cow<'a, str>>,
	size: Option<u64>,
	#[serde(rename = "lastModified")]
	last_modified: Option<i64>,
}

/// The fields of a `previousRemoteTree` item this engine reads. Unknown fields are ignored, which is
/// what keeps the item's `key` and `hash` out of everything here (see the module docs), and also
/// its `lastModified`: that one comes from metadata any client may have written, so a value this
/// engine could not read would otherwise lose the line for a field the import never uses.
#[derive(Deserialize)]
struct RemoteItem<'a> {
	#[serde(rename = "type", borrow)]
	kind: Option<Cow<'a, str>>,
	uuid: Option<Uuid>,
	size: Option<u64>,
}

/// What a line of either tree comes to: the fields both trees' items have, and what only its own
/// tree records.
struct Entry<'a> {
	prop: Cow<'a, str>,
	kind: Option<Cow<'a, str>>,
	size: Option<u64>,
	side: LegacySide,
}

impl<'a> From<Line<'a, LocalItem<'a>>> for Entry<'a> {
	fn from(Line { prop, data }: Line<'a, LocalItem<'a>>) -> Self {
		Self {
			prop,
			kind: data.kind,
			size: data.size,
			side: LegacySide::Local {
				mtime: data.last_modified,
			},
		}
	}
}

impl<'a> From<Line<'a, RemoteItem<'a>>> for Entry<'a> {
	fn from(Line { prop, data }: Line<'a, RemoteItem<'a>>) -> Self {
		Self {
			prop,
			kind: data.kind,
			size: data.size,
			side: LegacySide::Remote { uuid: data.uuid },
		}
	}
}

/// Which tree a file is, and so which item its lines hold.
#[derive(Debug, Clone, Copy)]
enum Tree {
	Local,
	Remote,
}

impl Tree {
	fn file_name(self) -> &'static str {
		match self {
			Self::Local => LOCAL_TREE,
			Self::Remote => REMOTE_TREE,
		}
	}

	/// `text`, a line of this tree, read; `None` when it is not one.
	fn read(self, text: &str) -> Option<Entry<'_>> {
		match self {
			Self::Local => serde_json::from_str::<Line<'_, LocalItem<'_>>>(text)
				.ok()
				.map(Entry::from),
			Self::Remote => serde_json::from_str::<Line<'_, RemoteItem<'_>>>(text)
				.ok()
				.map(Entry::from),
		}
	}
}

/// Why an import stopped: a state file it could not read to the end, or the store.
enum Failure {
	Read {
		file: &'static str,
		error: io::Error,
	},
	Store(rusqlite::Error),
}

impl From<rusqlite::Error> for Failure {
	fn from(error: rusqlite::Error) -> Self {
		Self::Store(error)
	}
}

impl From<Failure> for Error {
	fn from(failure: Failure) -> Self {
		match failure {
			Failure::Read { file, error } => Error::custom_with_source(
				ErrorKind::IO,
				error,
				Some(format!("reading the filen-sync state file {file}")),
			),
			Failure::Store(error) => db_error(error, "importing the filen-sync state"),
		}
	}
}

/// What became of one line.
enum Read {
	Blank,
	Skipped,
	Staged,
}

/// The path a tree's `prop` names, keyed as the scan keys a local path: one leading `/` dropped,
/// then NFC. `None` for a path that names no plain relative path — empty, or with an empty, `.` or
/// `..` component — which is refused here rather than resolved, since the walk never yields one.
fn rel_key(prop: &str) -> Option<String> {
	let key = prop.strip_prefix('/').unwrap_or(prop);
	if key.is_empty()
		|| key
			.split('/')
			.any(|component| component.is_empty() || component == "." || component == "..")
	{
		return None;
	}
	normalize_rel_path(Path::new(key))
}

/// Stage one line of `tree`, without its line break.
fn put_line(line: &[u8], tree: Tree, put: &mut PutLegacy<'_>) -> rusqlite::Result<Read> {
	let Ok(text) = std::str::from_utf8(line) else {
		return Ok(Read::Skipped);
	};
	if text.trim().is_empty() {
		return Ok(Read::Blank);
	}
	let Some(Entry {
		prop,
		kind,
		size,
		side,
	}) = tree.read(text)
	else {
		return Ok(Read::Skipped);
	};
	let Some(rel_path) = rel_key(&prop) else {
		return Ok(Read::Skipped);
	};
	// SQLite holds an `i64`; a size past it is no size a file has.
	let Ok(size) = size.map(i64::try_from).transpose() else {
		return Ok(Read::Skipped);
	};
	put(&LegacyItem {
		rel_path: &rel_path,
		raw: &prop,
		is_file: kind.as_deref() == Some("file"),
		size,
		side,
	})?;
	Ok(Read::Staged)
}

/// Stream `file`, which is `tree`, through `put` a line at a time, counting into `skipped` the lines
/// that could not be read.
fn put_tree(
	file: File,
	tree: Tree,
	put: &mut PutLegacy<'_>,
	skipped: &mut u64,
) -> Result<(), Failure> {
	let mut reader = BufReader::new(file);
	let mut buf = Vec::new();
	let mut number = 0_u64;
	loop {
		buf.clear();
		let read = reader
			.read_until(b'\n', &mut buf)
			.map_err(|error| Failure::Read {
				file: tree.file_name(),
				error,
			})?;
		if read == 0 {
			return Ok(());
		}
		number = number.saturating_add(1);
		let line = buf.strip_suffix(b"\n").unwrap_or(&buf);
		let line = line.strip_suffix(b"\r").unwrap_or(line);
		if let Read::Skipped = put_line(line, tree, put)? {
			if *skipped < WARNED_LINES {
				tracing::warn!(
					"filen-sync import: skipped line {number} of {}: not an entry this engine can read",
					tree.file_name()
				);
			}
			*skipped = skipped.saturating_add(1);
		}
	}
}

/// `dir`'s file `tree`, or `None` when it does not exist.
fn open_tree(dir: &Path, tree: Tree) -> Result<Option<File>, Failure> {
	match File::open(dir.join(tree.file_name())) {
		Ok(file) => Ok(Some(file)),
		Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
		Err(error) => Err(Failure::Read {
			file: tree.file_name(),
			error,
		}),
	}
}

/// Import the filen-sync state in `dir` for `pair`, replacing whatever it imported before.
///
/// `dir` has to be a directory that can be read. A tree file missing from it is the old engine's
/// own "no saved state", and imports nothing — and still replaces an earlier import, which the
/// state no longer backs.
pub(super) fn import(
	store: &BaselineStore,
	pair: PairId,
	dir: &Path,
) -> Result<LegacyImport, Error> {
	let metadata = std::fs::metadata(dir).map_err(|error| {
		Error::custom_with_source(
			ErrorKind::IO,
			error,
			Some(format!(
				"the filen-sync state directory {dir:?} cannot be read"
			)),
		)
	})?;
	if !metadata.is_dir() {
		return Err(Error::custom(
			ErrorKind::IO,
			format!("the filen-sync state directory {dir:?} is not a directory"),
		));
	}
	let local = open_tree(dir, Tree::Local)?;
	let remote = open_tree(dir, Tree::Remote)?;
	let mut skipped = 0;
	let imported = store.replace_legacy_synced(pair, |put| match (local, remote) {
		(Some(local), Some(remote)) => {
			put_tree(local, Tree::Local, put, &mut skipped)?;
			put_tree(remote, Tree::Remote, put, &mut skipped)
		}
		_ => {
			tracing::info!(
				"filen-sync import[pair {pair}]: {dir:?} holds no saved state (no {LOCAL_TREE} \
				 and {REMOTE_TREE} pair), so nothing is imported"
			);
			Ok(())
		}
	})?;
	tracing::info!(
		"filen-sync import[pair {pair}]: {imported} synced path(s) imported, {skipped} line(s) \
		 skipped"
	);
	Ok(LegacyImport { imported, skipped })
}

#[cfg(test)]
mod tests;
