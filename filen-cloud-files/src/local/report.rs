//! What the local side reports to the app: each change it carried to the drive or brought from it,
//! and each item it left alone and why.

use filen_sdk_rs::fs::name::EntryNameErrorKind;

use crate::{
	failure::Failure, identity::PlaceholderIdentity, os::UnsupportedKind, root_path::RootPath,
};

/// Why a local item is not synced. The item is left as it is, never uploaded and never deleted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalSkip {
	/// The server refuses its name (a leading space, more than 255 UTF-8 bytes, a reserved
	/// character): the default for the open decision on such names, skip and report.
	Name(EntryNameErrorKind),
	/// The drive refused its name when it was uploaded or created, although the name passes the
	/// server's rule as this build knows it.
	DriveRefusedName,
	/// It matches the built-in ignore patterns (`desktop.ini`, Office lock files, swap files, partial
	/// downloads): what every machine writes and nobody syncs. Reported at debug level only.
	DefaultIgnored,
	/// A link or reparse point, never followed.
	Unsupported(UnsupportedKind),
	/// Its path holds a component the core cannot name; the path reported is its directory.
	Unrepresentable,
	/// A placeholder whose identity this build cannot read, or that names nothing it can sync.
	UnknownPlaceholder,
}

/// Which two versions a conflict kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictKind {
	/// The local file was edited while the remote file got a newer version: the local edit was kept
	/// aside as a new file, and the remote version took the name.
	EditVsEdit,
	/// A new local item took a name a different remote item holds: the local item was kept aside.
	CreateVsExisting,
}

/// A local operation the core undid on the remote after allowing it, because it failed locally.
/// One kind for now, beside [`VetoedOp`]'s two, so a delete undone later fits the same report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UndoneOp {
	/// A rename or move.
	Rename,
}

/// A local operation the core vetoed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VetoedOp {
	/// A delete, or a move out of the root.
	Delete,
	/// A rename or a move inside the root.
	Rename,
}

/// Why a local item was kept when the drive's change would have removed it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeptReason {
	/// The remote item is gone or in the trash, and the local item holds what the drive does not:
	/// it is uploaded as a new item.
	RemoteGone,
	/// The cache held an edit a previous run never finished uploading, and no placeholder holds it
	/// any more (the file was deleted or changed back while the provider was stopped): the edit was
	/// written beside the file as a new one, and is uploaded as a new item.
	UnsentEdit,
}

/// A change on the drive the local side applied to the sync root, or kept local data from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteApplied {
	/// A placeholder of a new remote item was created.
	Created {
		path: RootPath,
		identity: PlaceholderIdentity,
	},
	/// A file placeholder now stands for the remote file's newer version, `identity`, dehydrated.
	Updated {
		path: RootPath,
		identity: PlaceholderIdentity,
	},
	/// A placeholder followed its remote item's rename or move.
	Moved { from: RootPath, to: RootPath },
	/// A placeholder whose remote item is gone, trashed or out of the populated directories was
	/// removed; its data was what the drive holds.
	Removed { path: RootPath },
	/// A local item holding data the drive does not have was kept, though its remote item went.
	KeptLocal { path: RootPath, reason: KeptReason },
	/// A directory whose remote directory went kept `kept` items holding data the drive does not
	/// have; it is created on the drive anew to hold them.
	DirKept { path: RootPath, kept: usize },
}

/// What the local side did or could not do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalReport {
	/// A local file was uploaded, as a new file or a new version, and is now the placeholder
	/// `identity`.
	Uploaded {
		path: RootPath,
		identity: PlaceholderIdentity,
	},
	/// A local item was found equal to a remote one and became its placeholder without an upload.
	Adopted {
		path: RootPath,
		identity: PlaceholderIdentity,
	},
	/// A local directory was created on the drive.
	DirCreated {
		path: RootPath,
		identity: PlaceholderIdentity,
	},
	/// A locally deleted item was moved to the drive's trash.
	Trashed { path: RootPath },
	/// A local rename or move was applied to the drive.
	Moved { from: RootPath, to: RootPath },
	/// Both sides were kept: the local side now sits at `kept_as`, and the remote side at `path`.
	Conflict {
		path: RootPath,
		kept_as: RootPath,
		kind: ConflictKind,
	},
	/// A file was deleted locally while the remote got a version the deleted placeholder never
	/// held: the delete went ahead locally and the remote file was left in place.
	RemoteOutlivedDelete { path: RootPath },
	/// An item that is not synced.
	Skipped { path: RootPath, reason: LocalSkip },
	/// A file has been open for writing for a long time; it is uploaded once it is closed.
	Busy { path: RootPath },
	/// A change failed `attempts` times in a row and waits for its next event, the next reconcile
	/// walk or the drive coming back. Also a change that has waited ten minutes, over `attempts`
	/// looks, for something outside it that has not ended (the local side clearing a name, the
	/// start-up taking over unsent edits), which is looked at once a minute from then on, and a
	/// rename of the drive's item that failed `attempts` times to be put back, which is tried again
	/// for as long as it takes.
	Parked {
		path: RootPath,
		failure: Failure,
		attempts: u32,
	},
	/// A local delete or rename was vetoed: the drive could not apply it.
	Vetoed {
		path: RootPath,
		op: VetoedOp,
		failure: Failure,
	},
	/// An allowed operation failed locally afterwards, and the drive was put back.
	Undone { path: RootPath, op: UndoneOp },
	/// A change on the drive was applied locally.
	Remote(RemoteApplied),
	/// The bytes on disk of the placeholder `identity` did not match the hash the drive declares
	/// for its version, and were removed from the disk. Reads of that version are refused from now
	/// on. Bytes another process keeps the file open over stay on disk until it lets go: the path
	/// is reported [`LocalReport::Parked`] with [`Failure::ContentDamaged`] after a few tries.
	Damaged {
		path: RootPath,
		identity: PlaceholderIdentity,
	},
	/// The user freed a file's space ("Free up space"): its bytes were removed from the disk, and
	/// it is fetched again when it is next read.
	Freed { path: RootPath },
	/// The user freed a file's space, but it holds local changes the drive does not have yet: it
	/// was kept on disk, and is uploaded.
	KeptDownloaded { path: RootPath },
}
