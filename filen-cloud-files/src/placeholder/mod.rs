//! The values a placeholder is created with, each validated into the range cfapi takes.

use filen_types::fs::Uuid;

use crate::{identity::PlaceholderIdentity, names::PlaceholderName};

/// Milliseconds from the FILETIME epoch (1601-01-01 UTC) to the Unix epoch.
const UNIX_EPOCH_FILETIME_MILLIS: i64 = 11_644_473_600_000;
/// FILETIME ticks (100 ns) per millisecond.
const TICKS_PER_MILLI: i64 = 10_000;

/// A file size in bytes, as cfapi takes it (an `i64`): 0 to `i64::MAX` by construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileSize(i64);

/// A size below zero, which no file has.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("a size of {size} bytes is negative")]
pub struct NegativeSize {
	/// The size given, in bytes.
	pub size: i64,
}

impl FileSize {
	/// `size` bytes, refused below zero.
	pub fn from_i64(size: i64) -> Result<Self, NegativeSize> {
		if size < 0 {
			return Err(NegativeSize { size });
		}
		Ok(Self(size))
	}

	/// The size in bytes.
	pub fn get(self) -> u64 {
		// Never negative, so the absolute value is the value.
		self.0.unsigned_abs()
	}

	/// The size in bytes, as cfapi takes it.
	pub fn as_i64(self) -> i64 {
		self.0
	}
}

/// A Windows FILETIME: 100 ns ticks since 1601-01-01 UTC, never negative.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileTime(i64);

/// A Unix time a FILETIME cannot hold: before 1601, or past what its ticks count to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("{millis} ms since the Unix epoch is outside what a FILETIME holds")]
pub struct UnrepresentableTime {
	/// The time given, in milliseconds since the Unix epoch.
	pub millis: i64,
}

impl FileTime {
	/// The FILETIME of `millis` milliseconds since the Unix epoch:
	/// `(millis + 11_644_473_600_000) * 10_000`, each step checked, negative ticks refused.
	pub fn from_unix_millis(millis: i64) -> Result<Self, UnrepresentableTime> {
		millis
			.checked_add(UNIX_EPOCH_FILETIME_MILLIS)
			.and_then(|since_1601| since_1601.checked_mul(TICKS_PER_MILLI))
			.filter(|ticks| *ticks >= 0)
			.map(Self)
			.ok_or(UnrepresentableTime { millis })
	}

	/// 100 ns ticks since 1601-01-01 UTC.
	pub fn ticks(self) -> i64 {
		self.0
	}
}

/// Whether a placeholder is a file or a directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpecKind {
	/// A file of `size` bytes.
	File { size: FileSize },
	/// A directory.
	Dir,
}

/// What a placeholder stands for, by kind, so a spec cannot pair a file identity with a directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SpecEntry {
	File {
		stable: Uuid,
		version: Uuid,
		size: FileSize,
	},
	Dir {
		uuid: Uuid,
	},
}

/// One placeholder to create under a directory, every value already validated. Only population
/// builds one. The OS layer sets the access and change times to [`Self::modified`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaceholderSpec {
	name: PlaceholderName,
	entry: SpecEntry,
	created: FileTime,
	modified: FileTime,
}

impl PlaceholderSpec {
	/// A file placeholder: the content version `version` of the file `stable`, `size` bytes long.
	pub(crate) fn file(
		name: PlaceholderName,
		stable: Uuid,
		version: Uuid,
		size: FileSize,
		created: FileTime,
		modified: FileTime,
	) -> Self {
		Self {
			name,
			entry: SpecEntry::File {
				stable,
				version,
				size,
			},
			created,
			modified,
		}
	}

	/// A directory placeholder for the directory `uuid`.
	pub(crate) fn dir(
		name: PlaceholderName,
		uuid: Uuid,
		created: FileTime,
		modified: FileTime,
	) -> Self {
		Self {
			name,
			entry: SpecEntry::Dir { uuid },
			created,
			modified,
		}
	}
}

impl PlaceholderSpec {
	/// The placeholder's name.
	pub fn name(&self) -> &PlaceholderName {
		&self.name
	}

	/// What the placeholder stands for; its encoding is the identity cfapi stores.
	pub fn identity(&self) -> PlaceholderIdentity {
		match self.entry {
			SpecEntry::File {
				stable, version, ..
			} => PlaceholderIdentity::File { stable, version },
			SpecEntry::Dir { uuid } => PlaceholderIdentity::Dir { uuid },
		}
	}

	/// Whether the placeholder is a file, and of what size, or a directory.
	pub fn kind(&self) -> SpecKind {
		match self.entry {
			SpecEntry::File { size, .. } => SpecKind::File { size },
			SpecEntry::Dir { .. } => SpecKind::Dir,
		}
	}

	/// When the item was created.
	pub fn created(&self) -> FileTime {
		self.created
	}

	/// When the item was last modified.
	pub fn modified(&self) -> FileTime {
		self.modified
	}
}

#[cfg(test)]
mod tests;
