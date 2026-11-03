use anyhow::{Context, Result, anyhow};
use std::fmt::Display;

/// Represents a path to a remote file or directory.
#[derive(Clone, Eq, PartialEq, Hash, Debug)]
pub(crate) struct RemotePath(pub(crate) String);

impl RemotePath {
	/// The normalized path of `path`, read from the root whether or not it starts with '/'.
	pub(crate) fn new(path: &str) -> Self {
		RemotePath(String::from("/")).navigate(path)
	}

	/// Navigate to a new path.
	/// If the path starts with '/', it replaces the current path, otherwise it interprets it as a relative path.
	/// Supports ".." and ".", and ignores empty segments, so a trailing or doubled '/' changes
	/// nothing. Paths will always start with "/" and never end with one, except the root.
	pub(crate) fn navigate(&self, navigation: &str) -> Self {
		let mut segments: Vec<&str> = if navigation.starts_with('/') {
			Vec::new()
		} else {
			self.0.split('/').filter(|part| !part.is_empty()).collect()
		};
		for part in navigation.split('/') {
			match part {
				"" | "." => {}
				// the root is its own parent
				".." => {
					segments.pop();
				}
				name => segments.push(name),
			}
		}
		RemotePath(format!("/{}", segments.join("/")))
	}

	/// Returns the base name (last segment) of the path.
	/// If the path is root ("/") or otherwise ends in "/", returns None.
	pub(crate) fn basename(&self) -> Option<&str> {
		let basename = self.0.rsplit_once('/').map(|(_, name)| name).unwrap(); // always starts with "/"
		if !basename.is_empty() {
			Some(basename)
		} else {
			None
		}
	}

	/// Returns the parent path, or itself if this is the root path
	pub(crate) fn parent(&self) -> RemotePath {
		if self.0 == "/" {
			self.clone()
		} else {
			let (parent, _) = self.0.rsplit_once('/').unwrap_or(("/", ""));
			RemotePath::new(parent)
		}
	}

	pub(crate) fn is_root(&self) -> bool {
		self.0 == "/"
	}
}

impl Display for RemotePath {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "{}", self.0)
	}
}

/// The most bytes one keyring entry holds of a [`LongKeyringEntry`]'s value.
const KEYRING_CHUNK_LEN: usize = 1000;

/// A store of named string values, which [`LongKeyringEntry`] splits a long value across.
pub(crate) trait ChunkStore {
	/// The value stored under `key`, or `None` if there is none.
	fn get(&self, key: &str) -> Result<Option<String>>;
	/// Store `value` under `key`, replacing any value stored there.
	fn set(&self, key: &str, value: &str) -> Result<()>;
	/// Delete the value stored under `key`. Returns false if there was none.
	fn delete(&self, key: &str) -> Result<bool>;
}

/// The system keyring, with each value an entry of one service.
pub(crate) struct KeyringChunkStore {
	service: &'static str,
}

impl KeyringChunkStore {
	pub(crate) fn new(service: &'static str) -> Self {
		KeyringChunkStore { service }
	}

	fn entry(&self, key: &str) -> Result<keyring::Entry> {
		keyring::Entry::new(self.service, key).context("Failed to create keyring entry")
	}
}

impl ChunkStore for KeyringChunkStore {
	fn get(&self, key: &str) -> Result<Option<String>> {
		match self.entry(key)?.get_password() {
			Ok(value) => Ok(Some(value)),
			Err(keyring::Error::NoEntry) => Ok(None),
			Err(e) => Err(anyhow!(e)).context("Failed to read keyring entry"),
		}
	}

	fn set(&self, key: &str, value: &str) -> Result<()> {
		self.entry(key)?
			.set_password(value)
			.context("Failed to save chunk to keyring")
	}

	fn delete(&self, key: &str) -> Result<bool> {
		match self.entry(key)?.delete_credential() {
			Ok(()) => Ok(true),
			Err(keyring::Error::NoEntry) => Ok(false),
			Err(e) => Err(anyhow!(e)).context("Failed to delete keyring entry"),
		}
	}
}

/// Provides an interface for reading and writing keyring entries that exceed the character limit.
/// It splits the entry up into multiple entries with a numerical suffix.
pub(crate) struct LongKeyringEntry<'a> {
	store: &'a dyn ChunkStore,
	name: &'a str,
}

impl<'a> LongKeyringEntry<'a> {
	pub(crate) fn new(store: &'a dyn ChunkStore, name: &'a str) -> Self {
		LongKeyringEntry { store, name }
	}

	fn chunk_key(&self, i: usize) -> String {
		format!("{}_{}", self.name, i)
	}

	pub(crate) fn read(&self) -> Result<Option<String>> {
		let mut result = None::<String>;
		for i in 0.. {
			let Some(chunk) = self.store.get(&self.chunk_key(i))? else {
				break;
			};
			result.get_or_insert_default().push_str(&chunk);
		}
		Ok(result)
	}

	/// Store `value`, and delete the chunks a longer value written before left behind, which
	/// [`LongKeyringEntry::read`] would otherwise append to it.
	pub(crate) fn write(&self, value: &str) -> Result<()> {
		let chunks = split_on_char_boundaries(value, KEYRING_CHUNK_LEN);
		for (i, chunk) in chunks.iter().enumerate() {
			self.store.set(&self.chunk_key(i), chunk)?;
		}
		self.delete_from(chunks.len())?;
		Ok(())
	}

	/// Returns true if the entry was deleted, false if it didn't exist.
	pub(crate) fn delete(&self) -> Result<bool> {
		self.delete_from(0)
	}

	/// Delete the chunks from the `first`th on, up to the first one missing. Returns whether there
	/// was one to delete.
	fn delete_from(&self, first: usize) -> Result<bool> {
		let mut deleted_any = false;
		for i in first.. {
			if !self.store.delete(&self.chunk_key(i))? {
				break;
			}
			deleted_any = true;
		}
		Ok(deleted_any)
	}
}

/// `value` split into consecutive parts of at most `max_len` bytes each, or of one character
/// where a character is longer, so that no part splits a character.
fn split_on_char_boundaries(value: &str, max_len: usize) -> Vec<&str> {
	let mut parts = Vec::new();
	let mut rest = value;
	while !rest.is_empty() {
		let end = match rest.floor_char_boundary(max_len) {
			// a character longer than max_len: take it whole, so that the loop makes progress
			0 => rest.ceil_char_boundary(1),
			end => end,
		};
		let (part, tail) = rest.split_at(end);
		parts.push(part);
		rest = tail;
	}
	parts
}

#[cfg(test)]
pub(crate) mod test_support {
	use std::{cell::RefCell, collections::HashMap};

	use super::*;

	/// A [`ChunkStore`] in memory, in place of the system keyring.
	#[derive(Default)]
	pub(crate) struct MemoryChunkStore(RefCell<HashMap<String, String>>);

	impl MemoryChunkStore {
		/// The keys stored, sorted.
		pub(crate) fn keys(&self) -> Vec<String> {
			let mut keys = self.0.borrow().keys().cloned().collect::<Vec<_>>();
			keys.sort();
			keys
		}
	}

	impl ChunkStore for MemoryChunkStore {
		fn get(&self, key: &str) -> Result<Option<String>> {
			Ok(self.0.borrow().get(key).cloned())
		}

		fn set(&self, key: &str, value: &str) -> Result<()> {
			self.0.borrow_mut().insert(key.to_owned(), value.to_owned());
			Ok(())
		}

		fn delete(&self, key: &str) -> Result<bool> {
			Ok(self.0.borrow_mut().remove(key).is_some())
		}
	}
}

#[cfg(test)]
mod tests {
	use super::{test_support::MemoryChunkStore, *};

	#[test]
	fn a_shorter_value_replaces_a_longer_one_and_its_extra_chunks() {
		let store = MemoryChunkStore::default();
		let entry = LongKeyringEntry::new(&store, "sdk-config");
		let long = "a".repeat(2 * KEYRING_CHUNK_LEN + 1);
		entry.write(&long).unwrap();
		assert_eq!(
			store.keys(),
			["sdk-config_0", "sdk-config_1", "sdk-config_2"]
		);

		entry.write("short").unwrap();

		assert_eq!(entry.read().unwrap().as_deref(), Some("short"));
		assert_eq!(store.keys(), ["sdk-config_0"]);
	}

	#[test]
	fn a_value_over_several_chunks_reads_back_whole() {
		let store = MemoryChunkStore::default();
		let entry = LongKeyringEntry::new(&store, "sdk-config");
		let value = "abc".repeat(KEYRING_CHUNK_LEN);
		entry.write(&value).unwrap();
		assert_eq!(entry.read().unwrap(), Some(value));
	}

	#[test]
	fn an_entry_never_written_reads_as_none() {
		let store = MemoryChunkStore::default();
		assert_eq!(
			LongKeyringEntry::new(&store, "sdk-config").read().unwrap(),
			None
		);
	}

	#[test]
	fn deleting_an_entry_deletes_all_its_chunks() {
		let store = MemoryChunkStore::default();
		let entry = LongKeyringEntry::new(&store, "sdk-config");
		entry.write(&"a".repeat(2 * KEYRING_CHUNK_LEN)).unwrap();
		assert!(entry.delete().unwrap());
		assert!(store.keys().is_empty());
		assert!(!entry.delete().unwrap());
	}

	#[test]
	fn values_are_split_without_splitting_a_character() {
		assert_eq!(split_on_char_boundaries("aééb", 3), ["aé", "éb"]);
		assert_eq!(split_on_char_boundaries("€a", 2), ["€", "a"]);
		assert!(split_on_char_boundaries("", 3).is_empty());
	}

	#[test]
	fn remote_path() {
		let path = RemotePath::new("/root/dir");
		assert_eq!(path.navigate("subdir").0, "/root/dir/subdir");
		assert_eq!(path.navigate(".").0, "/root/dir");
		assert_eq!(path.navigate("..").0, "/root");
		assert_eq!(path.navigate("../..").0, "/");
		assert_eq!(path.navigate("./file.txt").0, "/root/dir/file.txt");
		assert_eq!(path.navigate("./../notthedir/.././adir").0, "/root/adir"); // complex
		assert_eq!(path.navigate("../../..").0, "/"); // root has no parent
		assert_eq!(path.basename(), Some("dir"));
		assert_eq!(RemotePath::new("/").basename(), None);
		assert_eq!(RemotePath::new("/").navigate(".").0, "/");
		assert_eq!(RemotePath::new("/").parent(), RemotePath::new("/"));
		assert_eq!(
			RemotePath::new("/dir/subdir").parent(),
			RemotePath::new("/dir")
		);
		assert_eq!(
			RemotePath::new("/dir/file.txt").parent(),
			RemotePath::new("/dir")
		);
	}

	#[test]
	fn an_absolute_path_resolves_parent_and_current_directory_segments() {
		let path = RemotePath::new("/root/dir");
		assert_eq!(path.navigate("/a/../b").0, "/b");
		assert_eq!(path.navigate("/a/./b").0, "/a/b");
		assert_eq!(path.navigate("/a/b/c/../../d/./e").0, "/a/d/e");
	}

	#[test]
	fn an_absolute_path_drops_a_trailing_slash() {
		let path = RemotePath::new("/root/dir").navigate("/a/b/");
		assert_eq!(path.0, "/a/b");
		assert_eq!(path.basename(), Some("b"));
		assert_eq!(path.parent(), RemotePath::new("/a"));
	}

	#[test]
	fn an_absolute_path_collapses_repeated_slashes() {
		let path = RemotePath::new("/root/dir");
		assert_eq!(path.navigate("//a//b").0, "/a/b");
		assert_eq!(path.navigate("/").0, "/");
		assert_eq!(path.navigate("//").0, "/");
	}

	#[test]
	fn an_absolute_path_never_goes_above_the_root() {
		let path = RemotePath::new("/root/dir");
		assert_eq!(path.navigate("/..").0, "/");
		assert_eq!(path.navigate("/../../a").0, "/a");
		assert_eq!(path.navigate("/..").basename(), None);
	}

	#[test]
	fn a_relative_path_drops_a_trailing_slash() {
		let path = RemotePath::new("/root").navigate("dir/sub/");
		assert_eq!(path.0, "/root/dir/sub");
		assert_eq!(path.basename(), Some("sub"));
		assert_eq!(path.parent(), RemotePath::new("/root/dir"));
	}

	#[test]
	fn a_new_path_is_normalized() {
		assert_eq!(RemotePath::new("a/../b/").0, "/b");
		assert_eq!(RemotePath::new("").0, "/");
		assert_eq!(RemotePath::new("/x//y/.").0, "/x/y");
	}
}
