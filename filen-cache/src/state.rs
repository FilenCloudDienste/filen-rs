use std::{iter::once, path::Path};

use crossbeam::channel::{Receiver, Sender, TrySendError};
use filen_sdk_rs::{
	fs::{
		HasUUID,
		categories::NonRootItemType,
		dir::{DecryptedDirectoryMeta, cache::CacheableDir, meta::DirectoryMeta},
		file::{
			cache::CacheableFile,
			meta::{DecryptedFileMeta, FileMeta},
		},
	},
	io::{RemoteDirectory, RemoteFile},
	socket::{DecryptedDriveEvent, DecryptedSocketEvent},
};
use filen_types::{api::v3::dir::color::DirColor, traits::CowHelpers};
use uuid::Uuid;

// Should be enough to buffer events while we're processing them
// if we get more than that we can just drop them and log an error
// since it means our cache is too slow to keep up with the event stream
const EVENT_BUFFER_SIZE: usize = 64;
const CONTROL_BUFFER_SIZE: usize = 8;

pub(crate) struct CacheState {
	pub(crate) db: rusqlite::Connection,
	event_receiver: Receiver<CacheEvent>,
	control_receiver: Receiver<CacheControlMessage>,
	pub(crate) root_uuid: Uuid,
}

#[cfg(test)]
impl CacheState {
	/// Create a CacheState with an in-memory DB for unit testing.
	pub(crate) fn new_in_memory() -> Self {
		let root_uuid = Uuid::new_v4();
		let (event_sender, event_receiver) = crossbeam::channel::bounded(8);
		let (control_sender, control_receiver) = crossbeam::channel::bounded(8);
		drop(event_sender);
		drop(control_sender);

		let mut state = Self {
			db: rusqlite::Connection::open_in_memory().unwrap(),
			event_receiver,
			control_receiver,
			root_uuid,
		};
		state.init_db().unwrap();
		state
	}
}

#[allow(clippy::large_enum_variant)]
enum CacheEventType {
	/// We don't care about this event, we just need to know it happened so we can increment global_message_id
	File(FileEvent),
	Dir(DirEvent),
	Global(GlobalEvent),
	Manual(ManualEvent),
}

enum FileEvent {
	New(RemoteFile),
	Move(RemoteFile),
	Trash(Uuid),
	Delete(Uuid),
	Archived(Uuid),
	Restore(RemoteFile),
	ArchiveRestored(RemoteFile),
	MetadataChanged {
		uuid: Uuid,
		meta: Option<DecryptedFileMeta<'static>>,
	},
	Favorite(RemoteFile),
}

enum DirEvent {
	New(RemoteDirectory),
	Move(RemoteDirectory),
	Trash(Uuid),
	Delete(Uuid),
	Restore(RemoteDirectory),
	MetadataChanged {
		uuid: Uuid,
		meta: Option<DecryptedDirectoryMeta<'static>>,
	},
	ColorChanged {
		uuid: Uuid,
		color: DirColor<'static>,
	},
	Favorite(RemoteDirectory),
}

enum GlobalEvent {
	TrashEmpty,
	DeleteAll,
	DeleteVersioned,
}

pub(crate) enum ManualEvent {
	ListDirRecursive(Vec<RemoteDirectory>, Vec<RemoteFile>),
}

pub(crate) struct CacheEvent {
	id: Option<u64>,
	event: CacheEventType,
}

impl CacheEvent {
	pub(crate) fn manual(event: ManualEvent) -> Self {
		Self {
			id: None,
			event: CacheEventType::Manual(event),
		}
	}
}

pub enum CacheControlMessage {
	Shutdown,
}

fn try_extract_file_meta(meta: &FileMeta<'_>) -> Option<DecryptedFileMeta<'static>> {
	match meta {
		FileMeta::Decoded(decoded) => Some(decoded.clone().into_owned_cow()),
		_ => None,
	}
}

fn try_extract_dir_meta(meta: &DirectoryMeta<'_>) -> Option<DecryptedDirectoryMeta<'static>> {
	match meta {
		DirectoryMeta::Decoded(decoded) => Some(decoded.clone().into_owned_cow()),
		_ => None,
	}
}

fn make_socket_event_callback(
	sender: Sender<CacheEvent>,
) -> impl Fn(&DecryptedSocketEvent<'_>) + Send + 'static {
	move |event| {
		let DecryptedSocketEvent::Drive {
			inner,
			drive_message_id,
		} = event
		else {
			// We don't care about non-drive events in the cache, we only care about drive events that affect the file and folder structure
			return;
		};
		let event_type = match inner {
			// ── File events ─────────────────────────────────────────────
			DecryptedDriveEvent::FileNew(file_new) => {
				CacheEventType::File(FileEvent::New(file_new.0.clone()))
			}
			DecryptedDriveEvent::FileMove(file_move) => {
				CacheEventType::File(FileEvent::Move(file_move.0.clone()))
			}
			DecryptedDriveEvent::FileTrash(file_trash) => {
				CacheEventType::File(FileEvent::Trash((&file_trash.uuid).into()))
			}
			DecryptedDriveEvent::FileRestore(file_restore) => {
				CacheEventType::File(FileEvent::Restore(file_restore.0.clone()))
			}
			DecryptedDriveEvent::FileArchiveRestored(restored) => {
				CacheEventType::File(FileEvent::ArchiveRestored(restored.file.clone()))
			}
			DecryptedDriveEvent::FileArchived(archived) => {
				CacheEventType::File(FileEvent::Archived((&archived.uuid).into()))
			}
			DecryptedDriveEvent::FileDeletedPermanent(deleted) => {
				CacheEventType::File(FileEvent::Delete((&deleted.uuid).into()))
			}
			DecryptedDriveEvent::FileMetadataChanged(changed) => {
				CacheEventType::File(FileEvent::MetadataChanged {
					uuid: (&changed.uuid).into(),
					meta: try_extract_file_meta(&changed.metadata),
				})
			}

			// ── Folder events ───────────────────────────────────────────
			DecryptedDriveEvent::FolderSubCreated(dir_created) => {
				CacheEventType::Dir(DirEvent::New(dir_created.0.clone()))
			}
			DecryptedDriveEvent::FolderMove(dir_moved) => {
				CacheEventType::Dir(DirEvent::Move(dir_moved.0.clone()))
			}
			DecryptedDriveEvent::FolderTrash(dir_trash) => {
				CacheEventType::Dir(DirEvent::Trash((&dir_trash.uuid).into()))
			}
			DecryptedDriveEvent::FolderRestore(dir_restore) => {
				CacheEventType::Dir(DirEvent::Restore(dir_restore.0.clone()))
			}
			DecryptedDriveEvent::FolderDeletedPermanent(deleted) => {
				CacheEventType::Dir(DirEvent::Delete((&deleted.uuid).into()))
			}
			DecryptedDriveEvent::FolderMetadataChanged(changed) => {
				CacheEventType::Dir(DirEvent::MetadataChanged {
					uuid: (&changed.uuid).into(),
					meta: try_extract_dir_meta(&changed.meta),
				})
			}
			DecryptedDriveEvent::FolderColorChanged(changed) => {
				CacheEventType::Dir(DirEvent::ColorChanged {
					uuid: (&changed.uuid).into(),
					color: changed.color.clone().into_owned_cow(),
				})
			}

			// ── Item events (file or folder) ────────────────────────────
			DecryptedDriveEvent::ItemFavorite(favorite) => match &favorite.0 {
				NonRootItemType::File(file) => {
					CacheEventType::File(FileEvent::Favorite(file.as_ref().clone()))
				}
				NonRootItemType::Dir(dir) => {
					CacheEventType::Dir(DirEvent::Favorite(dir.as_ref().clone()))
				}
			},

			// ── Global events ────────────────────────────────────────────
			DecryptedDriveEvent::TrashEmpty => CacheEventType::Global(GlobalEvent::TrashEmpty),
			DecryptedDriveEvent::DeleteAll => CacheEventType::Global(GlobalEvent::DeleteAll),
			DecryptedDriveEvent::DeleteVersioned => {
				CacheEventType::Global(GlobalEvent::DeleteVersioned)
			}
		};

		// don't care if the channel is dropped, assume the cache is shutting down and we can just stop sending events
		if let Err(TrySendError::Full(e)) = sender.try_send(CacheEvent {
			id: Some(*drive_message_id),
			event: event_type,
		}) {
			log::error!("Cache event channel is full, dropping event {:?}", e.id)
		}
	}
}

type InitResult = (
	CacheState,
	Box<dyn Fn(&DecryptedSocketEvent<'_>) + Send + 'static>,
	Sender<CacheControlMessage>,
	Sender<CacheEvent>,
);

impl CacheState {
	pub(crate) fn new(
		cache_path: &Path,
		root_uuid: Uuid,
	) -> Result<InitResult, filen_sdk_rs::Error> {
		let connection = rusqlite::Connection::open(cache_path).map_err(|e| {
			filen_sdk_rs::Error::custom_with_source(
				filen_sdk_rs::ErrorKind::Internal,
				e,
				Some("Failed to open SQLite database"),
			)
		})?;

		let (event_sender, event_receiver) = crossbeam::channel::bounded(EVENT_BUFFER_SIZE);
		let (control_sender, control_receiver) = crossbeam::channel::bounded(CONTROL_BUFFER_SIZE);

		let mut cache_state = CacheState {
			db: connection,
			event_receiver,
			control_receiver,
			root_uuid,
		};

		cache_state.init_db().map_err(|e| {
			filen_sdk_rs::Error::custom_with_source(
				filen_sdk_rs::ErrorKind::Internal,
				e,
				Some("Failed to set up SQLite database"),
			)
		})?;

		let callback = make_socket_event_callback(event_sender.clone());

		Ok((
			cache_state,
			Box::new(callback),
			control_sender,
			event_sender,
		))
	}

	pub(crate) fn run(mut self) {
		loop {
			crossbeam::channel::select_biased! {
				recv(self.control_receiver) -> control_event => {
					match control_event {
						Ok(CacheControlMessage::Shutdown) | Err(_) => {
							log::info!("Cache received shutdown signal, shutting down...");
							return;
						}
					}
				},
				recv(self.event_receiver) -> event => {
					let Ok(event) = event else {
						log::warn!("Event channel closed unexpectedly, shutting down cache...");
						return;
					};

					self.handle_event(event);
				}
			}
		}
	}

	fn handle_event(&mut self, event: CacheEvent) {
		match event.event {
			CacheEventType::File(file_event) => self.handle_file_event(file_event),
			CacheEventType::Dir(dir_event) => self.handle_dir_event(dir_event),
			CacheEventType::Global(global) => self.handle_global_event(global),
			CacheEventType::Manual(manual_event) => self.handle_manual_event(manual_event),
		}
	}

	fn handle_file_event(&mut self, event: FileEvent) {
		match event {
			FileEvent::New(file)
			| FileEvent::Move(file)
			| FileEvent::Restore(file)
			| FileEvent::ArchiveRestored(file)
			| FileEvent::Favorite(file) => {
				let cacheable = match CacheableFile::try_from(&file) {
					Ok(cacheable) => cacheable,
					Err(e) => {
						log::error!(
							"Failed to convert RemoteFile to CacheableFile for file {:?}: {:?}",
							file.uuid(),
							e
						);
						// TODO: proper handling with error callbacks
						return;
					}
				};
				if let Err(e) = self.upsert_files(once(&cacheable)) {
					log::error!(
						"Failed to upsert file {:?} into cache: {:?}",
						cacheable.uuid,
						e
					);
					// TODO: proper handling with error callbacks
				}
			}
			FileEvent::Trash(uuid) | FileEvent::Delete(uuid) | FileEvent::Archived(uuid) => {
				if let Err(e) = self.delete_items(once(uuid)) {
					log::error!("Failed to delete file {:?} from cache: {:?}", uuid, e);
				}
			}
			FileEvent::MetadataChanged { uuid, meta } => {
				let Some(meta) = meta else {
					log::error!(
						"MetadataChanged event for file {:?} does not contain decrypted metadata, cannot update cache",
						uuid
					);
					return;
				};
				if let Err(e) = self.update_file_meta(uuid, &meta) {
					log::error!(
						"Failed to update file metadata for {:?} in cache: {:?}",
						uuid,
						e
					);
				}
			}
		}
	}

	fn handle_dir_event(&mut self, event: DirEvent) {
		match event {
			DirEvent::New(dir)
			| DirEvent::Move(dir)
			| DirEvent::Restore(dir)
			| DirEvent::Favorite(dir) => {
				let cacheable = match CacheableDir::try_from(&dir) {
					Ok(cacheable) => cacheable,
					Err(e) => {
						log::error!(
							"Failed to convert RemoteDirectory to CacheableDir for dir {:?}: {:?}",
							dir.uuid(),
							e
						);
						// TODO: proper handling with error callbacks
						return;
					}
				};
				if let Err(e) = self.upsert_dirs(once(&cacheable)) {
					log::error!(
						"Failed to upsert dir {:?} into cache: {:?}",
						cacheable.uuid,
						e
					);
				}
			}
			DirEvent::Trash(uuid) | DirEvent::Delete(uuid) => {
				if let Err(e) = self.delete_items(once(uuid)) {
					log::error!("Failed to delete dir {:?} from cache: {:?}", uuid, e);
				}
			}
			DirEvent::MetadataChanged { uuid, meta } => {
				let Some(meta) = meta else {
					log::error!(
						"MetadataChanged event for dir {:?} does not contain decrypted metadata, cannot update cache",
						uuid
					);
					return;
				};
				if let Err(e) = self.update_dir_name(uuid, &meta) {
					log::error!("Failed to update dir name for {:?} in cache: {:?}", uuid, e);
				}
			}
			DirEvent::ColorChanged { uuid, color } => {
				if let Err(e) = self.update_dir_color(uuid, &color) {
					log::error!(
						"Failed to update dir color for {:?} in cache: {:?}",
						uuid,
						e
					);
				}
			}
		}
	}

	fn handle_global_event(&mut self, event: GlobalEvent) {
		match event {
			GlobalEvent::DeleteAll => {
				if let Err(e) = self.delete_all_non_root() {
					log::error!("Failed to delete all non-root in cache: {:?}", e);
				}
			}
			GlobalEvent::TrashEmpty => {
				// noop, we don't track trashed items
			}
			GlobalEvent::DeleteVersioned => {
				// todo, implement version tracking
			}
		}
	}

	fn handle_manual_event(&mut self, event: ManualEvent) {
		match event {
			ManualEvent::ListDirRecursive(dirs, files) => {
				let cacheable_dirs = dirs.iter().filter_map(|dir| {
					CacheableDir::try_from(dir)
						.map_err(|e| {
							// TODO: proper handling with error callbacks
							log::error!(
								"Failed to convert RemoteDirectory to CacheableDir for dir {:?}: {:?}",
								dir.uuid(),
								e
							);
							e
						})
						.ok()
				});
				if let Err(e) = self.upsert_dirs(cacheable_dirs) {
					log::error!("Failed to upsert dirs into cache: {:?}", e);
				}

				let cacheable_files = files.iter().filter_map(|file| {
					CacheableFile::try_from(file)
						.map_err(|e| {
							// TODO: proper handling with error callbacks
							log::error!(
								"Failed to convert RemoteFile to CacheableFile for file {:?}: {:?}",
								file.uuid(),
								e
							);
							e
						})
						.ok()
				});
				if let Err(e) = self.upsert_files(cacheable_files) {
					log::error!("Failed to upsert files into cache: {:?}", e);
				}
			}
		}
	}
}
