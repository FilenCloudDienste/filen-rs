use std::{iter::once, path::Path};

use crossbeam::channel::{Receiver, Sender, TrySendError};
use filen_sdk_rs::{
	fs::{
		categories::NonRootItemType,
		dir::{cache::CacheableDir, meta::DirectoryMeta},
		file::{cache::CacheableFile, meta::FileMeta},
	},
	io::{RemoteDirectory, RemoteFile},
	socket::{DecryptedDriveEvent, DecryptedSocketEvent},
};
use filen_types::{api::v3::dir::color::DirColor, traits::CowHelpers};
use uuid::Uuid;

use crate::{CacheError, handle::CacheMessage};

// Should be enough to buffer events while we're processing them
// if we get more than that we can just drop them and log an error
// since it means our cache is too slow to keep up with the event stream
const EVENT_BUFFER_SIZE: usize = 64;
const CONTROL_BUFFER_SIZE: usize = 8;

pub(crate) struct CacheState {
	pub(crate) db: rusqlite::Connection,
	event_receiver: Receiver<CacheEvent>,
	control_receiver: Receiver<CacheControlMessage>,
	msg_sender: tokio::sync::mpsc::Sender<Vec<CacheMessage>>,
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
		let (msg_sender, msg_receiver) = tokio::sync::mpsc::channel(1);
		drop(msg_receiver);

		let mut state = Self {
			db: rusqlite::Connection::open_in_memory().unwrap(),
			event_receiver,
			control_receiver,
			msg_sender,
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
	MetadataChanged { uuid: Uuid, meta: FileMeta<'static> },
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
		meta: DirectoryMeta<'static>,
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
					meta: changed.metadata.clone().into_owned_cow(),
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
					meta: changed.meta.clone().into_owned_cow(),
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
		msg_sender: tokio::sync::mpsc::Sender<Vec<CacheMessage>>,
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
			msg_sender,
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

					if let Err(e) = self.handle_event(event)
						&& let Err(e) = self.msg_sender.try_send(vec![CacheMessage::Error(e)]) {
							log::error!("Failed to send cache error message to main thread: {:?}, assuming main thread has died, shutting down", e);
							return;
						}
				}
			}
		}
	}

	fn handle_event(&mut self, event: CacheEvent) -> Result<(), Vec<CacheError>> {
		match event.event {
			CacheEventType::File(file_event) => self.handle_file_event(file_event),
			CacheEventType::Dir(dir_event) => self.handle_dir_event(dir_event),
			CacheEventType::Global(global) => self.handle_global_event(global),
			CacheEventType::Manual(manual_event) => self.handle_manual_event(manual_event),
		}
	}

	fn handle_file_event(&mut self, event: FileEvent) -> Result<(), Vec<CacheError>> {
		match event {
			FileEvent::New(file)
			| FileEvent::Move(file)
			| FileEvent::Restore(file)
			| FileEvent::ArchiveRestored(file)
			| FileEvent::Favorite(file) => {
				let cacheable = match CacheableFile::try_from(&file) {
					Ok(file) => file,
					Err(e) => {
						return Err(vec![CacheError::file_cachable_conversion(file, e)]);
					}
				};
				self.upsert_files(once(&cacheable)).map_err(|e| {
					vec![CacheError::db(
						e,
						format!("failed to upsert file: {:?}", cacheable),
					)]
				})
			}
			FileEvent::Trash(uuid) | FileEvent::Delete(uuid) | FileEvent::Archived(uuid) => {
				self.delete_items(once(uuid)).map_err(|e| {
					vec![CacheError::db(
						e,
						format!("failed to delete file with uuid: {}", uuid),
					)]
				})
			}
			FileEvent::MetadataChanged { uuid, meta } => {
				let meta = match meta {
					FileMeta::Decoded(decoded) => decoded,
					meta => return Err(vec![CacheError::file_meta_not_decryptable(meta, uuid)]),
				};
				self.update_file_meta(uuid, &meta).map_err(|e| {
					vec![CacheError::db(
						e,
						format!("updating file meta for uuid {} meta: {:?}", uuid, meta),
					)]
				})
			}
		}
	}

	fn handle_dir_event(&mut self, event: DirEvent) -> Result<(), Vec<CacheError>> {
		match event {
			DirEvent::New(dir)
			| DirEvent::Move(dir)
			| DirEvent::Restore(dir)
			| DirEvent::Favorite(dir) => {
				let cacheable = match CacheableDir::try_from(&dir) {
					Ok(cacheable) => cacheable,
					Err(e) => {
						return Err(vec![CacheError::dir_cachable_conversion(dir, e)]);
					}
				};
				self.upsert_dirs(once(&cacheable)).map_err(|e| {
					vec![CacheError::db(
						e,
						format!("failed to upsert dir: {:?}", cacheable),
					)]
				})
			}
			DirEvent::Trash(uuid) | DirEvent::Delete(uuid) => {
				self.delete_items(once(uuid)).map_err(|e| {
					vec![CacheError::db(
						e,
						format!("failed to delete dir with uuid: {}", uuid),
					)]
				})
			}
			DirEvent::MetadataChanged { uuid, meta } => {
				let DirectoryMeta::Decoded(meta) = meta else {
					return Err(vec![CacheError::dir_meta_not_decryptable(meta, uuid)]);
				};
				self.update_dir_name(uuid, &meta).map_err(|e| {
					vec![CacheError::db(
						e,
						format!(
							"failed to update dir name for uuid {} and meta {:?}",
							uuid, meta
						),
					)]
				})
			}
			DirEvent::ColorChanged { uuid, color } => {
				self.update_dir_color(uuid, &color).map_err(|e| {
					vec![CacheError::db(
						e,
						format!(
							"failed to update dir color for uuid {} and color {:?}",
							uuid, color
						),
					)]
				})
			}
		}
	}

	fn handle_global_event(&mut self, event: GlobalEvent) -> Result<(), Vec<CacheError>> {
		match event {
			GlobalEvent::DeleteAll => self.delete_all_non_root().map_err(|e| {
				vec![CacheError::db(
					e,
					"failed to delete all non-root items from cache".to_string(),
				)]
			}),
			GlobalEvent::TrashEmpty => {
				Ok(())
				// noop, we don't track trashed items
			}
			GlobalEvent::DeleteVersioned => {
				Ok(())
				// todo, implement version tracking
			}
		}
	}

	fn handle_manual_event(&mut self, event: ManualEvent) -> Result<(), Vec<CacheError>> {
		match event {
			ManualEvent::ListDirRecursive(dirs, files) => {
				let mut cache_errors = Vec::new();

				let cacheable_dirs = IteratorWithErrors::new(
					&mut cache_errors,
					dirs.into_iter().map(|dir| {
						CacheableDir::try_from(dir).map_err(|(dir, e)| {
							Box::new(CacheError::dir_cachable_conversion(dir, e))
						})
					}),
				);
				self.upsert_dirs(cacheable_dirs).map_err(|e| {
					vec![CacheError::db(
						e,
						"Failed when bulk inserting CacheableDirs".to_string(),
					)]
				})?;

				let cacheable_files = IteratorWithErrors::new(
					&mut cache_errors,
					files.into_iter().map(|file| {
						CacheableFile::try_from(file).map_err(|(file, e)| {
							Box::new(CacheError::file_cachable_conversion(file, e))
						})
					}),
				);
				self.upsert_files(cacheable_files).map_err(|e| {
					vec![CacheError::db(
						e,
						"Failed when bulk inserting CacheableFiles".to_string(),
					)]
				})?;

				if cache_errors.is_empty() {
					Ok(())
				} else {
					Err(cache_errors)
				}
			}
		}
	}
}

struct IteratorWithErrors<'a, Iter, E> {
	iter: Iter,
	errors: &'a mut Vec<E>,
}

impl<Iter, Item, E> Iterator for IteratorWithErrors<'_, Iter, E>
where
	Iter: Iterator<Item = Result<Item, Box<E>>>,
{
	type Item = Item;

	fn next(&mut self) -> Option<Self::Item> {
		for result in self.iter.by_ref() {
			match result {
				Ok(item) => return Some(item),
				Err(e) => self.errors.push(*e),
			}
		}
		None
	}
}

impl<'a, Iter, E> IteratorWithErrors<'a, Iter, E> {
	fn new(errors: &'a mut Vec<E>, iter: Iter) -> Self {
		Self { iter, errors }
	}
}
