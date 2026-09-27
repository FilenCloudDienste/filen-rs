//! The password of an encrypted archive: never printed, never serialized, wiped from memory
//! when dropped.

use std::fmt;

use zeroize::Zeroizing;

use crate::{Error, ErrorKind};

/// Most characters a password may have; archive tools take far fewer.
const MAX_CHARS: usize = 1024;

#[derive(Clone)]
pub struct ArchivePassword(Zeroizing<String>);

impl ArchivePassword {
	/// A password exactly as given: no trimming or normalization, since the archive was
	/// encrypted with its bytes.
	pub fn new(password: String) -> Result<Self, Error> {
		let password = Zeroizing::new(password);
		if password.is_empty() || password.chars().count() > MAX_CHARS {
			return Err(Error::custom(
				ErrorKind::InvalidState,
				format!("an archive password has 1 to {MAX_CHARS} characters"),
			));
		}
		Ok(Self(password))
	}

	pub(crate) fn as_bytes(&self) -> &[u8] {
		self.0.as_bytes()
	}

	/// The password as UTF-16LE, which 7z derives its keys from.
	pub(crate) fn utf16le(&self) -> Zeroizing<Vec<u8>> {
		Zeroizing::new(self.0.encode_utf16().flat_map(u16::to_le_bytes).collect())
	}
}

impl fmt::Debug for ArchivePassword {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("ArchivePassword(<redacted>)")
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn passwords_are_checked_and_never_printed() {
		assert!(ArchivePassword::new(String::new()).is_err());
		assert!(ArchivePassword::new("x".repeat(MAX_CHARS + 1)).is_err());
		let password = ArchivePassword::new(" secret ".into()).unwrap();
		assert_eq!(password.as_bytes(), b" secret ", "kept exactly as given");
		assert_eq!(format!("{password:?}"), "ArchivePassword(<redacted>)");
		let password = ArchivePassword::new("aé𝄞".into()).unwrap();
		assert_eq!(
			*password.utf16le(),
			[0x61, 0, 0xE9, 0, 0x34, 0xD8, 0x1E, 0xDD],
			"UTF-16LE, with a surrogate pair"
		);
	}
}
