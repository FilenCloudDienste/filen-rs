//! Extracting an archive into the drive.

pub(crate) mod codec;

/// Why an archive entry was not extracted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExtractSkipReason {
	/// A symbolic link, which the drive cannot hold; `target` is the stored target, cut to at most
	/// 4096 bytes.
	Symlink {
		target: String,
	},
	Hardlink,
	/// A device node or FIFO.
	Device,
	/// A sparse file, stored with its holes left out.
	Sparse,
	/// A kind of entry the SDK does not extract, such as a tar multivolume continuation.
	UnsupportedType,
	/// A path longer than 4096 bytes.
	PathTooLong,
	/// A path more than 256 directories deep.
	PathTooDeep,
	/// A path that climbs out of the folder it is extracted into, or cannot be made into drive
	/// names.
	UnsafePath,
}
