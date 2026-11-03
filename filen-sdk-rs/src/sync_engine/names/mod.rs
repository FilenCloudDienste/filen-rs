//! Names between the pair's keys and the file system: which remote names this device's file system
//! can hold under their exact spelling ([`device_rejection`]), and where the local entry a key names
//! sits on disk ([`on_disk`]).
//!
//! Keys are NFC-normalized (see `scan`), and the remote view keys remote items the same way, so a
//! key is a name's identity, not its spelling. macOS resolves either Unicode form to the entry, so
//! there the key IS the path; Linux and Windows file systems compare names byte for byte, so an
//! entry another program created under its NFD spelling (what macOS hands out, kept by a copy or an
//! archive) is only found under that spelling.

use std::{
	borrow::Cow,
	collections::HashMap,
	fmt,
	fs::Metadata,
	io::{self, ErrorKind},
	path::{Path, PathBuf},
};

use unicode_normalization::{UnicodeNormalization, is_nfc, is_nfd};

/// The longest name, in bytes, every file system the engine writes to on Linux and macOS holds
/// (`NAME_MAX`). Windows counts UTF-16 code units instead (see [`WINDOWS_MAX_UNITS`]).
const MAX_NAME_BYTES: usize = 255;

/// The longest name, in UTF-16 code units, NTFS and the Win32 API hold.
const WINDOWS_MAX_UNITS: usize = 255;

/// The characters Win32 refuses in a name, besides the control characters below U+0020.
const WINDOWS_FORBIDDEN: [char; 9] = ['<', '>', ':', '"', '/', '\\', '|', '?', '*'];

/// Why this device's file system cannot hold a remote name under its exact spelling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DeviceRejection {
	/// Longer than the file system holds: `len` bytes, or UTF-16 code units on Windows, of at most
	/// `max`.
	TooLong { len: usize, max: usize },
	/// A character Windows refuses in a name.
	ForbiddenChar(char),
	/// A trailing dot or space, which Win32 strips, so the item would land under another name, and
	/// on top of the item already holding that one.
	TrailingDotOrSpace,
	/// A name Windows reserves for a device (`CON`, `NUL`, `COM1`, ... with or without an
	/// extension), which opens the device instead of a file.
	ReservedName,
}

impl fmt::Display for DeviceRejection {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::TooLong { len, max } => write!(
				f,
				"the name is {len} long, more than the {max} this device's file system holds"
			),
			Self::ForbiddenChar(ch) => {
				write!(f, "Windows does not allow the character {ch:?} in a name")
			}
			Self::TrailingDotOrSpace => f.write_str(
				"Windows drops a trailing dot or space, so the item would be stored under another name",
			),
			Self::ReservedName => f.write_str("Windows reserves the name for a device"),
		}
	}
}

/// Why this device's file system cannot hold `name` (an NFC remote name, as the view keys it) under
/// that exact spelling, or `None` when it can.
///
/// This asks what the file system holds, not what the SDK's name validator accepts: a name the
/// remote already holds is pulled wherever the file system can hold it, even one the validator
/// would refuse for an upload (a `:` or a trailing dot on Linux or macOS). A name with a path
/// separator or NUL is not asked about here: no file system holds that, and the view refuses it
/// first as an unsafe name.
pub(super) fn device_rejection(name: &str) -> Option<DeviceRejection> {
	if cfg!(windows) {
		windows_rejection(name)
	} else {
		posix_rejection(name)
	}
}

/// [`device_rejection`] on Linux and macOS: only the length.
fn posix_rejection(name: &str) -> Option<DeviceRejection> {
	(name.len() > MAX_NAME_BYTES).then_some(DeviceRejection::TooLong {
		len: name.len(),
		max: MAX_NAME_BYTES,
	})
}

/// [`device_rejection`] on Windows: the length, the forbidden characters, a trailing dot or space,
/// and the device names. Compiled on every host, so its rules are tested everywhere. Allocates
/// nothing: the view asks it of every name it places, and of every directory above one.
fn windows_rejection(name: &str) -> Option<DeviceRejection> {
	// A UTF-16 unit never takes fewer than one UTF-8 byte, so only a name past the limit in bytes
	// is counted.
	if name.len() > WINDOWS_MAX_UNITS {
		let units = name.encode_utf16().count();
		if units > WINDOWS_MAX_UNITS {
			return Some(DeviceRejection::TooLong {
				len: units,
				max: WINDOWS_MAX_UNITS,
			});
		}
	}
	if let Some(ch) = name
		.chars()
		.find(|ch| *ch < ' ' || WINDOWS_FORBIDDEN.contains(ch))
	{
		return Some(DeviceRejection::ForbiddenChar(ch));
	}
	if name.ends_with(['.', ' ']) {
		return Some(DeviceRejection::TrailingDotOrSpace);
	}
	is_windows_device(name).then_some(DeviceRejection::ReservedName)
}

/// The names Win32 opens a device for, besides `COM` and `LPT` with a digit.
const WINDOWS_DEVICES: [&str; 6] = ["CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$"];

/// The digits that make `COM` and `LPT` a device name: 0 to 9 and the superscripts 1 to 3.
const WINDOWS_PORT_DIGITS: [&str; 13] = [
	"0", "1", "2", "3", "4", "5", "6", "7", "8", "9", "\u{B9}", "\u{B2}", "\u{B3}",
];

/// Whether Win32 opens a device for `name`: its part before the first dot, trailing spaces dropped,
/// is a device name in any case. `CON.txt` and `nul.tar.gz` are devices too, unlike what the name
/// validator checks; Windows 11 stopped reserving some of these with an extension, and refusing them
/// costs nothing but a pull that could not happen on older Windows.
fn is_windows_device(name: &str) -> bool {
	let stem = name
		.split_once('.')
		.map_or(name, |(stem, _)| stem)
		.trim_end_matches(' ');
	// Three to seven bytes: `CON` to `CONOUT$`, and `COM` with a two-byte superscript digit.
	if !(3..=7).contains(&stem.len()) {
		return false;
	}
	if WINDOWS_DEVICES
		.iter()
		.any(|device| stem.eq_ignore_ascii_case(device))
	{
		return true;
	}
	// `split_at_checked` refuses a cut inside a character, which no ASCII prefix ends in.
	stem.split_at_checked(3).is_some_and(|(prefix, digit)| {
		(prefix.eq_ignore_ascii_case("COM") || prefix.eq_ignore_ascii_case("LPT"))
			&& WINDOWS_PORT_DIGITS.contains(&digit)
	})
}

/// Whether the NFC `name` reads the same in another Unicode form: it holds a character with a
/// canonical decomposition, so its NFD spelling is other bytes. Most names in a script without
/// accents (CJK, Cyrillic) have no such character, and their NFC and NFD forms are one.
pub(super) fn has_other_form(name: &str) -> bool {
	!name.is_ascii() && !is_nfd(name)
}

/// What [`on_disk`] may do to find a path's last component.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Leaf {
	/// The entry exists (the scan saw it): its spelling is looked for as far as listing its
	/// directory.
	Existing,
	/// The entry is about to be written, and is usually not there yet: its NFC and NFD spellings are
	/// tried, its directory is never listed. Listing a directory per download would read it once for
	/// each file a pass pulls into it.
	Target,
}

/// `rel_path` joined onto `root` as it is spelled, `/` by `/`, with nothing looked up.
pub(super) fn joined(root: &Path, rel_path: &str) -> PathBuf {
	rel_path
		.split('/')
		.fold(root.to_path_buf(), |p, c| p.join(c))
}

/// Where the entry the key `rel_path` names sits under `root`: each component under the spelling
/// its directory holds it by, which on Linux and Windows need not be the NFC key (see the module
/// docs). A component nothing on disk spells, and everything under it, is joined as the key spells
/// it, which is how a new entry is written.
///
/// Costs nothing on macOS and iOS, whose file systems resolve either Unicode form, nor for an ASCII
/// path, which has one spelling. Elsewhere a path found as spelled costs one `lstat`; only a miss
/// looks for another spelling: under the parent as spelled where that is there, else component by
/// component, the NFC and NFD forms first, then (see [`Leaf`]) the directory listing, for a
/// spelling neither form is. Two entries of one directory that normalize alike are a collision the
/// engine never acts on, so a listing that finds more than one keeps the key's own spelling.
pub(super) fn on_disk(root: &Path, rel_path: &str, leaf: Leaf) -> PathBuf {
	if cfg!(target_vendor = "apple") || rel_path.is_ascii() {
		return joined(root, rel_path);
	}
	respelled(root, rel_path, leaf)
}

/// [`on_disk`] for an entry an observation reads, which may well be gone (a deletion, the old end
/// of a rename), with what `stat` (following symlinks) says of it.
///
/// A path found as spelled costs that one `stat`. A miss is looked up as [`on_disk`] looks up an
/// existing entry, but a directory is listed at most once per `listings`, however many gone paths
/// under it are asked about: K deletions from a directory of N entries cost one listing, not K.
pub(super) fn stat_on_disk(
	root: &Path,
	rel_path: &str,
	listings: &mut Listings,
) -> (PathBuf, io::Result<Metadata>) {
	let plain = joined(root, rel_path);
	let stat = std::fs::metadata(&plain);
	if cfg!(target_vendor = "apple")
		|| rel_path.is_ascii()
		|| !stat
			.as_ref()
			.is_err_and(|error| error.kind() == ErrorKind::NotFound)
	{
		return (plain, stat);
	}
	let found = respelled_missing(root, rel_path, Leaf::Existing, Some(listings));
	if found == plain {
		return (plain, stat);
	}
	let stat = std::fs::metadata(&found);
	(found, stat)
}

/// The directory listings [`stat_on_disk`] took, kept for the observation that asked: by directory,
/// each entry whose name is not NFC by its NFC key, `None` where two of them share one. An NFC
/// entry is found by its key's own `lstat`, so a directory of those keeps nothing.
#[derive(Debug, Default)]
pub(super) struct Listings {
	dirs: HashMap<PathBuf, HashMap<String, Option<String>>>,
}

impl Listings {
	/// How `dir`'s listing spells `component`, listing it on the first question.
	fn spelling(&mut self, dir: &Path, component: &str) -> Option<String> {
		if !self.dirs.contains_key(dir) {
			self.dirs.insert(dir.to_path_buf(), list_respelled(dir));
		}
		self.dirs.get(dir)?.get(component)?.clone()
	}
}

/// `dir`'s entries whose name is not NFC, by their NFC key (see [`Listings`]). A directory that
/// cannot be listed holds none: the same answer an uncached lookup gives it.
fn list_respelled(dir: &Path) -> HashMap<String, Option<String>> {
	let mut spelled = HashMap::new();
	let Ok(entries) = std::fs::read_dir(dir) else {
		return spelled;
	};
	for entry in entries.flatten() {
		let name = entry.file_name();
		let Some(name) = name.to_str() else {
			continue;
		};
		if name.is_ascii() || is_nfc(name) {
			continue;
		}
		spelled
			.entry(name.nfc().collect::<String>())
			.and_modify(|found: &mut Option<String>| *found = None)
			.or_insert_with(|| Some(name.to_owned()));
	}
	spelled
}

/// [`on_disk`] past its shortcuts, on every host: the tests run it on macOS too.
fn respelled(root: &Path, rel_path: &str, leaf: Leaf) -> PathBuf {
	let plain = joined(root, rel_path);
	if exists(&plain) {
		return plain;
	}
	respelled_missing(root, rel_path, leaf, None)
}

/// [`respelled`] once `rel_path` is known not to be there as spelled. The parent as spelled is
/// tried first, which is where nearly every entry sits: only the last component is then looked
/// for. `listings`, where given, keeps the listings taken (see [`Listings`]).
fn respelled_missing(
	root: &Path,
	rel_path: &str,
	leaf: Leaf,
	mut listings: Option<&mut Listings>,
) -> PathBuf {
	let list_leaf = leaf == Leaf::Existing;
	if let Some((parent, name)) = rel_path.rsplit_once('/') {
		let dir = joined(root, parent);
		if exists(&dir) {
			let spelling = other_spelling_in(&dir, name, list_leaf, listings);
			return dir.join(spelling.as_deref().unwrap_or(name));
		}
	}
	let mut path = root.to_path_buf();
	let mut components = rel_path.split('/').peekable();
	while let Some(component) = components.next() {
		let list = list_leaf || components.peek().is_some();
		let spelling = if exists(&path.join(component)) {
			Some(Cow::Borrowed(component))
		} else {
			other_spelling_in(&path, component, list, listings.as_deref_mut()).map(Cow::Owned)
		};
		match spelling {
			Some(spelling) => path.push(&*spelling),
			None => {
				path.push(component);
				for rest in components {
					path.push(rest);
				}
				break;
			}
		}
	}
	path
}

/// How `dir` spells the entry whose key is `component`, when it holds it under another spelling
/// than the key's own (which the caller found missing): the NFD form, then, where `list` allows,
/// the directory listing, through `listings` where given.
fn other_spelling_in(
	dir: &Path,
	component: &str,
	list: bool,
	listings: Option<&mut Listings>,
) -> Option<String> {
	// An ASCII key is NFC and NFD alike, and [`on_disk`] never asks about an ASCII path: a component
	// of one under a non-ASCII directory has no other spelling worth a listing.
	if component.is_ascii() {
		return None;
	}
	let nfd: String = component.nfd().collect();
	if nfd != component && exists(&dir.join(&nfd)) {
		return Some(nfd);
	}
	if !list {
		return None;
	}
	if let Some(listings) = listings {
		return listings.spelling(dir, component);
	}
	let entries = std::fs::read_dir(dir).ok()?;
	let mut found = None;
	for entry in entries.flatten() {
		let name = entry.file_name();
		let Some(name) = name.to_str() else {
			continue;
		};
		if name.nfc().eq(component.chars()) {
			if found.is_some() {
				return None;
			}
			found = Some(name.to_owned());
		}
	}
	found
}

fn exists(path: &Path) -> bool {
	path.symlink_metadata().is_ok()
}

#[cfg(test)]
mod tests;
