use crate::{
	fs::{
		HasUUID,
		cache::CacheableConversionError,
		dir::{DecryptedDirectoryMeta, cache::CacheableDir},
		file::{cache::CacheableFile, meta::DecryptedFileMeta},
	},
	socket::{
		DecryptedDriveEvent, DecryptedSocketEvent, FileArchiveRestored, FileArchived,
		FileDeletedPermanent, FileMetadataChanged, FileMove, FileNew, FileRestore, FileTrash,
		FolderColorChanged, FolderDeletedPermanent, FolderMetadataChanged, FolderMove,
		FolderRestore, FolderSubCreated, FolderTrash, ItemFavorite,
	},
};
use filen_types::{api::v3::dir::color::DirColor, traits::CowHelpers};
use uuid::Uuid;

#[derive(Debug, Clone, CowHelpers, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub enum CacheEventType<'a> {
	File(FileEvent<'a>),
	Dir(DirEvent<'a>),
	Global(GlobalEvent),
	/// A frontier-advance marker: it carries a real `drive_message_id` (on its `CacheEvent`) but
	/// no replayable item state, so the drain advances the watermark past it while mutating
	/// nothing. This is how a `FrontierAdvance` event participates in the ordered,
	/// persisted contiguous-frontier computation instead of looking like a hole.
	NoOp,
}

#[derive(Debug, Clone, CowHelpers, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub enum FileEvent<'a> {
	New(CacheableFile<'a>),
	Move(CacheableFile<'a>),
	Changed(CacheableFile<'a>),
	Archived(Uuid),
	Removed(Uuid),
	MetadataChanged {
		uuid: Uuid,
		meta: DecryptedFileMeta<'a>,
	},
}

#[derive(Debug, Clone, CowHelpers, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub enum DirEvent<'a> {
	New(CacheableDir<'a>),
	Move(CacheableDir<'a>),
	Changed(CacheableDir<'a>),
	Removed(Uuid),
	MetadataChanged {
		uuid: Uuid,
		meta: DecryptedDirectoryMeta<'a>,
	},
	ColorChanged {
		uuid: Uuid,
		color: DirColor<'a>,
	},
}

#[derive(Debug, Clone, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub enum GlobalEvent {
	TrashEmpty,
	DeleteAll,
	DeleteVersioned,
}

impl<'a> CacheEventType<'a> {
	/// Convert a decrypted drive event into the cache's applied-event type. `Ok` for every event we
	/// understand (including a move out of the tracked tree, approximated as a `Removed`); `Err((error,
	/// uuid))` when the item's metadata fails to convert into a cacheable form. The caller
	/// ([`CacheEventMaybeDecrypted::from_decrypted_event`]) demotes that `Err` to a `FrontierAdvance` —
	/// logging the cause but still advancing the watermark, so a non-cacheable event is not mistaken for
	/// a gap.
	fn from_decrypted_drive_event(
		event: &'a DecryptedDriveEvent<'a>,
	) -> Result<Self, (CacheableConversionError, Uuid)> {
		Ok(match event {
			DecryptedDriveEvent::FileArchiveRestored(FileArchiveRestored { file, .. })
			| DecryptedDriveEvent::FileRestore(FileRestore(file)) => CacheEventType::File(
				FileEvent::Changed(file.try_into().map_err(|e| (e, file.uuid()))?),
			),
			DecryptedDriveEvent::FileNew(FileNew(file)) => CacheEventType::File(FileEvent::New(
				(file).try_into().map_err(|e| (e, file.uuid()))?,
			)),
			DecryptedDriveEvent::FileMove(FileMove(file)) => match file.try_into() {
				Ok(cacheable) => CacheEventType::File(FileEvent::Move(cacheable)),
				// a move whose new parent is a non-navigable virtual container
				// (trash/recents/favorites/links) takes the item out of the synced tree —
				// treat it as a removal rather than a non-cacheable frontier-advance event.
				Err(CacheableConversionError::ParentNotUuid(_)) => {
					CacheEventType::File(FileEvent::Removed(file.uuid()))
				}
				Err(e) => return Err((e, file.uuid())),
			},
			DecryptedDriveEvent::FileTrash(FileTrash { uuid })
			| DecryptedDriveEvent::FileDeletedPermanent(FileDeletedPermanent { uuid }) => {
				CacheEventType::File(FileEvent::Removed(*uuid))
			}
			DecryptedDriveEvent::FileArchived(FileArchived { uuid }) => {
				CacheEventType::File(FileEvent::Archived(*uuid))
			}
			DecryptedDriveEvent::FolderTrash(FolderTrash { uuid, .. })
			| DecryptedDriveEvent::FolderDeletedPermanent(FolderDeletedPermanent { uuid }) => {
				CacheEventType::Dir(DirEvent::Removed(*uuid))
			}
			DecryptedDriveEvent::FolderMove(FolderMove(dir)) => match dir.try_into() {
				Ok(cacheable) => CacheEventType::Dir(DirEvent::Move(cacheable)),
				// a folder move into a virtual container leaves the synced tree → removal.
				Err(CacheableConversionError::ParentNotUuid(_)) => {
					CacheEventType::Dir(DirEvent::Removed(dir.uuid()))
				}
				Err(e) => return Err((e, dir.uuid())),
			},
			DecryptedDriveEvent::FolderSubCreated(FolderSubCreated(dir))
			| DecryptedDriveEvent::FolderRestore(FolderRestore(dir)) => {
				CacheEventType::Dir(DirEvent::New(dir.try_into().map_err(|e| (e, dir.uuid()))?))
			}
			DecryptedDriveEvent::FolderColorChanged(FolderColorChanged { uuid, color }) => {
				CacheEventType::Dir(DirEvent::ColorChanged {
					uuid: *uuid,
					color: color.as_borrowed_cow(),
				})
			}
			DecryptedDriveEvent::TrashEmpty => CacheEventType::Global(GlobalEvent::TrashEmpty),
			DecryptedDriveEvent::ItemFavorite(ItemFavorite(item)) => match item {
				crate::fs::categories::NonRootItemType::File(file) => CacheEventType::File(
					FileEvent::Changed(file.as_ref().try_into().map_err(|e| (e, file.uuid()))?),
				),
				crate::fs::categories::NonRootItemType::Dir(dir) => CacheEventType::Dir(
					DirEvent::Changed(dir.as_ref().try_into().map_err(|e| (e, dir.uuid()))?),
				),
			},
			DecryptedDriveEvent::FolderMetadataChanged(FolderMetadataChanged { uuid, meta }) => {
				CacheEventType::Dir(DirEvent::MetadataChanged {
					uuid: *uuid,
					meta: match meta {
						crate::fs::dir::meta::DirectoryMeta::Decoded(decoded) => {
							decoded.as_borrowed_cow()
						}
						other => {
							return Err((
								CacheableConversionError::MetadataNotDecrypted(format!(
									"{:?}",
									other
								)),
								*uuid,
							));
						}
					},
				})
			}
			DecryptedDriveEvent::FileMetadataChanged(FileMetadataChanged { uuid, metadata }) => {
				CacheEventType::File(FileEvent::MetadataChanged {
					uuid: *uuid,
					meta: match metadata {
						crate::fs::file::meta::FileMeta::Decoded(decoded) => {
							decoded.as_borrowed_cow()
						}
						other => {
							return Err((
								CacheableConversionError::MetadataNotDecrypted(format!(
									"{:?}",
									other
								)),
								*uuid,
							));
						}
					},
				})
			}
			DecryptedDriveEvent::DeleteAll => CacheEventType::Global(GlobalEvent::DeleteAll),
			DecryptedDriveEvent::DeleteVersioned => {
				CacheEventType::Global(GlobalEvent::DeleteVersioned)
			}
		})
	}
}

#[derive(Debug, Clone, CowHelpers, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub struct CacheEvent<'a> {
	pub id: Option<u64>,
	pub event: CacheEventType<'a>,
}

#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, CowHelpers, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub(crate) enum CacheEventMaybeDecrypted<'a> {
	Decrypted(CacheEvent<'a>),
	/// A drive event that decrypted but could not be converted into a cacheable form. It still
	/// happened, so its `drive_message_id` must advance the watermark or it looks like a
	/// gap and forces a resync; the marker carries the id but no replayable item state and never
	/// mutates the database.
	FrontierAdvance {
		id: u64,
	},
	/// A socket reconnect (`DecryptedSocketEvent::Reconnecting`). It carries no applied state and
	/// is never persisted; it cues the worker to gap-check the disconnect window, because the
	/// socket does not redeliver drive events that landed while it was down. Without this, a hole
	/// opened during the reconnect window only heals on the next unrelated drive event or the next
	/// worker boot.
	ResyncSignal,
}

impl<'a> CacheEventMaybeDecrypted<'a> {
	pub(super) fn from_decrypted_event(event: &'a DecryptedSocketEvent<'a>) -> Option<Self> {
		match event {
			DecryptedSocketEvent::Drive {
				inner,
				drive_message_id,
			} => {
				let event = match CacheEventType::from_decrypted_drive_event(inner) {
					Ok(event) => Self::Decrypted(CacheEvent {
						id: Some(*drive_message_id),
						event,
					}),
					// surface the cause at ingest (log) and emit a frontier-advance marker
					// instead of dropping the event or raising a fatal error — the id must still
					// advance the watermark so a non-cacheable event is not mistaken for a gap.
					Err((e, uuid)) => {
						tracing::debug!(
							"drive event {drive_message_id} for {uuid} is not cacheable, advancing frontier: {e}"
						);
						Self::FrontierAdvance {
							id: *drive_message_id,
						}
					}
				};

				Some(event)
			}
			// The event's payload was unknown/undecryptable upstream but its id was
			// recovered; advance the watermark so it is not mistaken for a gap.
			DecryptedSocketEvent::DriveMalformed { drive_message_id } => {
				Some(Self::FrontierAdvance {
					id: *drive_message_id,
				})
			}
			// A reconnect: the socket won't redeliver drive events missed during the
			// disconnect window, so signal the worker to gap-check proactively. (The initial
			// connect fires AuthSuccess, not Reconnecting, and is already covered by the
			// startup gap-check, so only Reconnecting is routed here.)
			DecryptedSocketEvent::Reconnecting => Some(Self::ResyncSignal),
			_ => None,
		}
	}
}
