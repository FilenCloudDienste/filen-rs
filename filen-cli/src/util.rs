use anyhow::{Context, Result, anyhow};
use std::fmt::Display;

/// Represents a path to a remote file or directory.
#[derive(Clone)]
pub struct RemotePath(pub String);

impl RemotePath {
	pub fn new(path: &str) -> Self {
		RemotePath(String::from("/") + path.trim_start_matches("/"))
		//TODO check for correct syntax?
	}

	/// Navigate to a new path.
	/// If the path starts with '/', it replaces the current path, otherwise it interprets it as a relative path.
	/// Supports ".." and ".". Paths will always start with "/".
	pub fn navigate(&self, navigation: &str) -> Self {
		if navigation.starts_with("/") {
			RemotePath::new(navigation)
		} else {
			let mut new_path = self.0.clone();
			navigation.split("/").for_each(|part| {
				if part == ".." {
					let (parent, _) = new_path.rsplit_once('/').unwrap_or(("/", ""));
					new_path = parent.to_string();
				} else if part != "." && !part.is_empty() {
					if !new_path.ends_with("/") {
						new_path.push('/');
					}
					new_path.push_str(part);
				}
			});
			RemotePath::new(&new_path)
		}
	}
}

impl Display for RemotePath {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "{}", self.0)
	}
}

const KEYRING_SERVICE_NAME: &str = "filen-cli-rs";

/// Provides an interface for reading and writing keyring entries that exceed the character limit.
/// It splits the entry up into multiple entries with a numerical suffix.
pub struct LongKeyringEntry {
	pub name: String,
}

impl LongKeyringEntry {
	pub fn new(name: &str) -> Self {
		LongKeyringEntry {
			name: name.to_string(),
		}
	}

	pub fn read(&self) -> Result<Option<String>> {
		let mut result = String::new();
		let mut i = 0;
		loop {
			let entry = keyring::Entry::new(KEYRING_SERVICE_NAME, &format!("{}_{}", self.name, i))
				.context("Failed to create keyring entry for reading")?;
			match entry.get_password() {
				Ok(chunk) => {
					result.push_str(&chunk);
				}
				Err(keyring::Error::NoEntry) => break,
				Err(e) => return Err(anyhow!("Failed to read keyring entry: {}", e)),
			}
			i += 1;
		}
		if i > 0 { Ok(Some(result)) } else { Ok(None) }
	}

	pub fn write(&self, str: &str) -> Result<()> {
		let chunks = str
			.as_bytes()
			.chunks(1000) // todo: larger limit possible?
			.map(str::from_utf8)
			.collect::<Result<Vec<&str>, _>>()
			.unwrap();
		for (i, chunk) in chunks.into_iter().enumerate() {
			keyring::Entry::new(KEYRING_SERVICE_NAME, &format!("{}_{}", self.name, i))
				.context("Failed to create keyring entry for writing")?
				.set_password(chunk)
				.context("Failed to save chunk to keyring")?;
		}
		Ok(())
	}

	/// Returns true if the entry was deleted, false if it didn't exist.
	pub fn delete(&self) -> Result<bool> {
		let mut i = 0;
		loop {
			let entry = keyring::Entry::new(KEYRING_SERVICE_NAME, &format!("{}_{}", self.name, i))
				.context("Failed to create keyring entry for deletion")?;
			match entry.delete_credential() {
				Ok(_) => {}
				Err(keyring::Error::NoEntry) => {
					if i == 0 {
						return Ok(false);
					}
					break;
				}
				Err(e) => return Err(anyhow!("Failed to delete keyring entry: {}", e)),
			}
			i += 1;
		}
		Ok(true)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn test_remote_path_navigate() {
		let path = RemotePath::new("/root/dir");
		assert_eq!(path.navigate("subdir").0, "/root/dir/subdir");
		assert_eq!(path.navigate("..").0, "/root");
		assert_eq!(path.navigate("../..").0, "/");
		assert_eq!(path.navigate("./file.txt").0, "/root/dir/file.txt");
		assert_eq!(path.navigate("./../notthedir/.././adir").0, "/root/adir"); // complex
		assert_eq!(path.navigate("../../..").0, "/"); // root has no parent
	}
}
