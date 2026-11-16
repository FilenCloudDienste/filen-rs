//! What a placeholder stands for, as the bytes cfapi stores with it (its file identity) and hands
//! back with every callback for it.

use std::fmt;

use filen_mobile_native_cache::ffi::{FfiId, FfiNonRootObject};
use filen_types::fs::Uuid;

/// The most bytes cfapi stores as a placeholder's file identity
/// (`CF_PLACEHOLDER_CREATE_INFO.FileIdentityLength`).
pub const MAX_IDENTITY_LEN: usize = 4096;

/// The layout this build writes, and the only one it reads.
const FORMAT_V1: u8 = 1;

const KIND_ROOT: u8 = 0;
const KIND_DIR: u8 = 1;
const KIND_FILE: u8 = 2;
const KIND_DETACHED: u8 = 3;

/// Format, kind and one uuid: a root, a directory or a detached placeholder.
const ROOT_OR_DIR_LEN: usize = 2 + 16;
/// Format, kind and two uuids.
const FILE_LEN: usize = 2 + 16 + 16;

// The longest encoding fits what cfapi stores, which is why encoding cannot fail.
const _: () = assert!(FILE_LEN <= MAX_IDENTITY_LEN);

/// What a placeholder stands for.
///
/// Encoded as `[FORMAT_V1][kind][16-byte uuid][16-byte version, files only]`. Binary rather than
/// the cache's `stable/<uuid>` text id the design proposed: shorter, and parsed once (a recorded
/// default).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PlaceholderIdentity {
	/// The sync root, carrying the account's root directory uuid, so that a registration another
	/// account left behind is refused.
	Root { root: Uuid },
	/// A directory, by its uuid, which the server never re-mints.
	Dir { uuid: Uuid },
	/// A file, by its whole-life stable id (a plain [`Uuid`]: `StableUuid` is mint-only) and the
	/// uuid of the content version whose size and bytes the placeholder holds.
	File { stable: Uuid, version: Uuid },
	/// A placeholder that stands for no remote item: a local file the provider set apart from the
	/// item it was (the losing side of a conflict, or an edit whose remote file is gone) until it
	/// is uploaded as a new file. `nonce` tells two of them apart. Deleting or renaming one touches
	/// nothing remote.
	Detached { nonce: Uuid },
}

/// A placeholder's file identity as cfapi stores it: 1 to [`MAX_IDENTITY_LEN`] bytes, of any
/// content. [`PlaceholderIdentity::decode`] says what it stands for. `Debug` prints hex.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct IdentityBlob(Box<[u8]>);

/// Why bytes are not an identity this build understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum IdentityError {
	/// The OS handed over no bytes.
	#[error("the placeholder carries no identity")]
	Empty,
	/// `len` bytes, more than [`MAX_IDENTITY_LEN`].
	#[error("an identity of {len} bytes is longer than the {MAX_IDENTITY_LEN} the OS stores")]
	TooLong { len: usize },
	/// A layout this build does not write.
	#[error("identity format {format} is not one this build writes")]
	UnknownFormat { format: u8 },
	/// The identity ends after its format byte.
	#[error("the identity ends before its kind")]
	NoKind,
	/// A kind byte this build does not write.
	#[error("identity kind {kind} is unknown")]
	UnknownKind { kind: u8 },
	/// The identity is `actual` bytes long, where its kind takes exactly `expected`.
	#[error("a kind-{kind} identity is {expected} bytes, not {actual}")]
	Length {
		kind: u8,
		expected: usize,
		actual: usize,
	},
}

/// The id the cache resolves the directory or file `uuid` (a file's stable id) by: `stable/<uuid>`.
pub(crate) fn stable_id(uuid: Uuid) -> FfiId {
	FfiId(format!("stable/{uuid}"))
}

/// The uuid of a [`stable_id`] text; `None` for any other text.
pub(crate) fn uuid_of_stable_id(id: &str) -> Option<Uuid> {
	Uuid::parse_str(id.strip_prefix("stable/")?).ok()
}

/// A listed child whose id is not a uuid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the item's id is not a uuid")]
pub(crate) struct MalformedId;

impl TryFrom<&[u8]> for IdentityBlob {
	type Error = IdentityError;

	/// The only way bytes from the OS become a blob: refuses none and more than
	/// [`MAX_IDENTITY_LEN`].
	fn try_from(bytes: &[u8]) -> Result<Self, Self::Error> {
		if bytes.is_empty() {
			return Err(IdentityError::Empty);
		}
		if bytes.len() > MAX_IDENTITY_LEN {
			return Err(IdentityError::TooLong { len: bytes.len() });
		}
		Ok(Self(Box::from(bytes)))
	}
}

impl IdentityBlob {
	/// The bytes, as `CF_PLACEHOLDER_CREATE_INFO.FileIdentity` takes them.
	pub fn as_bytes(&self) -> &[u8] {
		&self.0
	}
}

impl fmt::Debug for IdentityBlob {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("IdentityBlob(")?;
		for byte in &self.0 {
			write!(f, "{byte:02x}")?;
		}
		f.write_str(")")
	}
}

impl PlaceholderIdentity {
	/// The bytes cfapi stores for this placeholder. Infallible: at most 34 bytes.
	pub fn encode(self) -> IdentityBlob {
		let mut bytes = Vec::with_capacity(FILE_LEN);
		bytes.push(FORMAT_V1);
		match self {
			Self::Root { root } => {
				bytes.push(KIND_ROOT);
				bytes.extend_from_slice(root.as_bytes());
			}
			Self::Dir { uuid } => {
				bytes.push(KIND_DIR);
				bytes.extend_from_slice(uuid.as_bytes());
			}
			Self::File { stable, version } => {
				bytes.push(KIND_FILE);
				bytes.extend_from_slice(stable.as_bytes());
				bytes.extend_from_slice(version.as_bytes());
			}
			Self::Detached { nonce } => {
				bytes.push(KIND_DETACHED);
				bytes.extend_from_slice(nonce.as_bytes());
			}
		}
		IdentityBlob(bytes.into_boxed_slice())
	}

	/// What `blob` stands for: the one parser of identity bytes. Each kind has an exact length, so
	/// a trailing byte is refused too.
	pub fn decode(blob: &IdentityBlob) -> Result<Self, IdentityError> {
		let bytes = blob.as_bytes();
		let (&format, rest) = bytes.split_first().ok_or(IdentityError::Empty)?;
		if format != FORMAT_V1 {
			return Err(IdentityError::UnknownFormat { format });
		}
		let (&kind, rest) = rest.split_first().ok_or(IdentityError::NoKind)?;
		let length = |expected| IdentityError::Length {
			kind,
			expected,
			actual: bytes.len(),
		};
		match kind {
			KIND_ROOT | KIND_DIR | KIND_DETACHED => {
				let uuid = <[u8; 16]>::try_from(rest)
					.map(Uuid::from_bytes)
					.map_err(|_| length(ROOT_OR_DIR_LEN))?;
				Ok(match kind {
					KIND_ROOT => Self::Root { root: uuid },
					KIND_DIR => Self::Dir { uuid },
					_ => Self::Detached { nonce: uuid },
				})
			}
			KIND_FILE => {
				let (stable, version) = rest
					.split_first_chunk::<16>()
					.and_then(|(stable, version)| {
						Some((*stable, <[u8; 16]>::try_from(version).ok()?))
					})
					.ok_or_else(|| length(FILE_LEN))?;
				Ok(Self::File {
					stable: Uuid::from_bytes(stable),
					version: Uuid::from_bytes(version),
				})
			}
			kind => Err(IdentityError::UnknownKind { kind }),
		}
	}
}

impl PlaceholderIdentity {
	/// The id the cache resolves this placeholder by: the root by its uuid, a directory or a file
	/// under `stable/<uuid>` (a directory's uuid is its stable id), lowercase and hyphenated; `None`
	/// for a detached placeholder, which stands for no remote item. The only place an [`FfiId`] is
	/// built from an identity.
	pub(crate) fn cache_id(self) -> Option<FfiId> {
		match self {
			Self::Root { root } => Some(FfiId(root.to_string())),
			Self::Dir { uuid } => Some(stable_id(uuid)),
			Self::File { stable, .. } => Some(stable_id(stable)),
			Self::Detached { .. } => None,
		}
	}

	/// The identity of a child the cache listed: a file by its stable id and the version it holds,
	/// a directory by its uuid.
	pub(crate) fn from_child(child: &FfiNonRootObject) -> Result<Self, MalformedId> {
		let parse = |id: &str| Uuid::parse_str(id).map_err(|_| MalformedId);
		Ok(match child {
			FfiNonRootObject::File(file) => Self::File {
				stable: parse(&file.stable_uuid)?,
				version: parse(&file.uuid)?,
			},
			FfiNonRootObject::Dir(dir) => Self::Dir {
				uuid: parse(&dir.uuid)?,
			},
		})
	}
}

#[cfg(test)]
mod tests;
