//! Remote names as placeholder names: only a name Windows can hold under its exact spelling
//! becomes one.

use std::{fmt, iter};

use filen_sdk_rs::io::{DeviceRejection, windows_rejection};
use filen_types::fs::Uuid;

/// The prefix of the name a new file is uploaded under before it is renamed to its own: a remote
/// item so named is such an upload, never placed. A user's own file so named is not placed
/// either, a cost accepted for a name nobody picks.
const UPLOAD_NAME_PREFIX: &str = ".filen-upload-";

/// The name the local file `file_id` (its id on its volume) is uploaded under as a new file: no
/// other file holds it, so the upload never becomes the new version of one, and only this file's
/// uploads do, so an upload of it that never got its name is found by it again.
pub(crate) fn upload_name(file_id: u128) -> String {
	format!("{UPLOAD_NAME_PREFIX}{file_id:032x}")
}

/// The name the file `stable` is under on the drive while it moves to another directory: only it
/// holds it, so the move never lands over another file, and it starts as an upload's does, so it is
/// never placed either.
pub(crate) fn transit_name(stable: Uuid) -> String {
	format!("{UPLOAD_NAME_PREFIX}move-{stable}")
}

/// A name of its own for a copy written into the sync root before it is renamed to its own: one a
/// stop in the middle of the copy leaves stays an ordinary file nothing uploads.
pub(crate) fn copy_name() -> String {
	format!("{UPLOAD_NAME_PREFIX}copy-{}", Uuid::new_v4())
}

/// Whether `name` is one a new file is uploaded under before it is renamed to its own, a file is
/// under while it moves ([`transit_name`]), or a copy is written under ([`copy_name`]): never a
/// placeholder, and never uploaded from the sync root.
pub(crate) fn is_upload_name(name: &str) -> bool {
	name.starts_with(UPLOAD_NAME_PREFIX)
}

/// A remote name Windows can hold under its exact spelling, with its NUL-terminated UTF-16 form.
///
/// A name Windows cannot hold is skipped and reported, never sanitised into another name: the
/// default for the open decision on such names ("skip and report"), recorded here.
#[derive(Clone, PartialEq, Eq)]
pub struct PlaceholderName {
	name: Box<str>,
	/// `name` in UTF-16, followed by one NUL, as cfapi takes it.
	wide_nul: Box<[u16]>,
}

/// Why a remote name cannot be a placeholder name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum NameRejection {
	/// The name is empty.
	#[error("the name is empty")]
	Empty,
	/// Windows cannot hold the name under its exact spelling.
	#[error(transparent)]
	Windows(#[from] DeviceRejection),
}

impl PlaceholderName {
	/// The only constructor: `name` when Windows can hold it as spelled. `.` and `..` fail as
	/// [`DeviceRejection::TrailingDotOrSpace`], and a NUL as a forbidden character, so the
	/// wide form holds exactly one NUL, its last unit.
	pub fn parse(name: &str) -> Result<Self, NameRejection> {
		if name.is_empty() {
			return Err(NameRejection::Empty);
		}
		if let Some(rejection) = windows_rejection(name) {
			return Err(rejection.into());
		}
		Ok(Self {
			name: Box::from(name),
			wide_nul: name.encode_utf16().chain(iter::once(0)).collect(),
		})
	}

	/// The name as the remote spells it.
	pub fn as_str(&self) -> &str {
		&self.name
	}

	/// The name in UTF-16 followed by one NUL: `CF_PLACEHOLDER_CREATE_INFO.RelativeFileName`.
	pub fn as_wide_nul(&self) -> &[u16] {
		&self.wide_nul
	}

	/// The name folded as NTFS compares names, so two names with equal keys cannot sit in one
	/// directory: each character by its uppercase form where that is one character, else as it
	/// is. An approximation of NTFS's upcase table, which maps one UTF-16 unit to one unit (so `ß`
	/// stays `ß` rather than becoming `SS`), and normalizes nothing; the OS's per-entry result of a
	/// population backstops it.
	pub(crate) fn case_key(&self) -> String {
		self.name
			.chars()
			.map(|ch| {
				let mut upper = ch.to_uppercase();
				match (upper.next(), upper.next()) {
					(Some(single), None) => single,
					_ => ch,
				}
			})
			.collect()
	}
}

impl fmt::Debug for PlaceholderName {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		fmt::Debug::fmt(&*self.name, f)
	}
}

#[cfg(test)]
mod tests;
