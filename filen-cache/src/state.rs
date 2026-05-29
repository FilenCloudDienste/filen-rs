use std::{iter::once, path::Path};

use crossbeam::channel::{Receiver, Sender, TrySendError};
use filen_sdk_rs::{
	fs::{dir::cache::CacheableDir, file::cache::CacheableFile},
	io::{RemoteDirectory, RemoteFile},
	socket::DecryptedSocketEvent,
};
use filen_types::traits::CowHelpers;
use uuid::Uuid;

use crate::{CacheError, handle::CacheMessage};

// Should be enough to buffer events while we're processing them
// if we get more than that we can just drop them and log an error
// since it means our cache is too slow to keep up with the event stream
const EVENT_BUFFER_SIZE: usize = 64;
const CONTROL_BUFFER_SIZE: usize = 8;

pub(crate) struct CacheState {
	pub(crate) db: rusqlite::Connection,
	event_receiver: Receiver<CacheThreadEvent>,
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

#[derive(Debug)]
pub(crate) enum ManualEvent {
	ListDirRecursive(Vec<RemoteDirectory>, Vec<RemoteFile>),
}

/// A message delivered to the cache worker thread: either an event derived from the WebSocket
/// (which may have failed to convert into a cacheable form) or a manually-injected event such as
/// a recursive directory listing.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub(crate) enum CacheThreadEvent {
	Socket(CacheEventMaybeDecrypted<'static>),
	Manual(ManualEvent),
}

pub enum CacheControlMessage {
	Shutdown,
}

fn make_socket_event_callback(
	sender: Sender<CacheThreadEvent>,
) -> impl Fn(&DecryptedSocketEvent<'_>) + Send + 'static {
	move |event| {
		if let Some(event) = CacheEventMaybeDecrypted::from_decrypted_event(event) {
			// don't care if the channel is dropped, assume the cache is shutting down and we can
			// just stop sending events
			if let Err(TrySendError::Full(e)) =
				sender.try_send(CacheThreadEvent::Socket(event.into_owned_cow()))
			{
				log::error!("Cache event channel is full, dropping event {:?}", e)
			}
		}
	}
}

type InitResult = (
	CacheState,
	Box<dyn Fn(&DecryptedSocketEvent<'_>) + Send + 'static>,
	Sender<CacheControlMessage>,
	Sender<CacheThreadEvent>,
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

	fn handle_event(&mut self, event: CacheThreadEvent) -> Result<(), Vec<CacheError>> {
		match event {
			CacheThreadEvent::Socket(CacheEventMaybeDecrypted::Decrypted(CacheEvent {
				event,
				..
			})) => match event {
				CacheEventType::File(file_event) => self.handle_file_event(file_event),
				CacheEventType::Dir(dir_event) => self.handle_dir_event(dir_event),
				CacheEventType::Global(global_event) => self.handle_global_event(global_event),
			},
			CacheThreadEvent::Socket(CacheEventMaybeDecrypted::Undecrypted(error, uuid)) => {
				Err(vec![CacheError::event_conversion(error, uuid)])
			}
			CacheThreadEvent::Manual(manual_event) => self.handle_manual_event(manual_event),
		}
	}

	fn handle_file_event(&mut self, event: FileEvent) -> Result<(), Vec<CacheError>> {
		match event {
			FileEvent::New(file) | FileEvent::Move(file) | FileEvent::Changed(file) => {
				self.upsert_files(once(&file)).map_err(|e| {
					vec![CacheError::db(
						e,
						format!("failed to upsert file: {:?}", file),
					)]
				})
			}
			FileEvent::Archived(uuid) | FileEvent::Removed(uuid) => {
				self.delete_items(once(uuid)).map_err(|e| {
					vec![CacheError::db(
						e,
						format!("failed to delete file with uuid: {}", uuid),
					)]
				})
			}
			FileEvent::MetadataChanged { uuid, meta } => {
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
			DirEvent::New(dir) | DirEvent::Move(dir) | DirEvent::Changed(dir) => {
				self.upsert_dirs(once(&dir)).map_err(|e| {
					vec![CacheError::db(
						e,
						format!("failed to upsert dir: {:?}", dir),
					)]
				})
			}
			DirEvent::Removed(uuid) => self.delete_items(once(uuid)).map_err(|e| {
				vec![CacheError::db(
					e,
					format!("failed to delete dir with uuid: {}", uuid),
				)]
			}),
			DirEvent::MetadataChanged { uuid, meta } => {
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
							Box::new(CacheError::dir_cachable_conversion(dir, e.into()))
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
							Box::new(CacheError::file_cachable_conversion(file, e.into()))
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

mod event {
	use filen_sdk_rs::{
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

	#[derive(Debug, CowHelpers, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
	pub(crate) enum CacheEventType<'a> {
		File(FileEvent<'a>),
		Dir(DirEvent<'a>),
		Global(GlobalEvent),
	}

	#[derive(Debug, CowHelpers, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
	pub(crate) enum FileEvent<'a> {
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

	#[derive(Debug, CowHelpers, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
	pub(crate) enum DirEvent<'a> {
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
	pub(crate) enum GlobalEvent {
		TrashEmpty,
		DeleteAll,
		DeleteVersioned,
	}

	impl<'a> CacheEventType<'a> {
		fn from_decrypted_drive_event(
			event: &'a DecryptedDriveEvent<'a>,
		) -> Result<Self, (CacheableConversionError, Uuid)> {
			Ok(match event {
				DecryptedDriveEvent::FileArchiveRestored(FileArchiveRestored { file, .. })
				| DecryptedDriveEvent::FileRestore(FileRestore(file)) => CacheEventType::File(
					FileEvent::Changed(file.try_into().map_err(|e| (e, file.uuid().into()))?),
				),
				DecryptedDriveEvent::FileNew(FileNew(file)) => CacheEventType::File(
					FileEvent::New((file).try_into().map_err(|e| (e, file.uuid().into()))?),
				),
				DecryptedDriveEvent::FileMove(FileMove(file)) => CacheEventType::File(
					FileEvent::Move(file.try_into().map_err(|e| (e, file.uuid().into()))?),
				),
				DecryptedDriveEvent::FileTrash(FileTrash { uuid })
				| DecryptedDriveEvent::FileDeletedPermanent(FileDeletedPermanent { uuid }) => {
					CacheEventType::File(FileEvent::Removed(uuid.into()))
				}
				DecryptedDriveEvent::FileArchived(FileArchived { uuid }) => {
					CacheEventType::File(FileEvent::Archived(uuid.into()))
				}
				DecryptedDriveEvent::FolderTrash(FolderTrash { uuid, .. })
				| DecryptedDriveEvent::FolderDeletedPermanent(FolderDeletedPermanent { uuid }) => {
					CacheEventType::Dir(DirEvent::Removed(uuid.into()))
				}
				DecryptedDriveEvent::FolderMove(FolderMove(dir)) => CacheEventType::Dir(
					DirEvent::Move(dir.try_into().map_err(|e| (e, dir.uuid().into()))?),
				),
				DecryptedDriveEvent::FolderSubCreated(FolderSubCreated(dir))
				| DecryptedDriveEvent::FolderRestore(FolderRestore(dir)) => CacheEventType::Dir(DirEvent::New(
					dir.try_into().map_err(|e| (e, dir.uuid().into()))?,
				)),
				DecryptedDriveEvent::FolderColorChanged(FolderColorChanged { uuid, color }) => {
					CacheEventType::Dir(DirEvent::ColorChanged {
						uuid: uuid.into(),
						color: color.as_borrowed_cow(),
					})
				}
				DecryptedDriveEvent::TrashEmpty => CacheEventType::Global(GlobalEvent::TrashEmpty),
				DecryptedDriveEvent::ItemFavorite(ItemFavorite(item)) => match item {
					filen_sdk_rs::fs::categories::NonRootItemType::File(file) => {
						CacheEventType::File(FileEvent::Changed(
							file.as_ref()
								.try_into()
								.map_err(|e| (e, file.uuid().into()))?,
						))
					}
					filen_sdk_rs::fs::categories::NonRootItemType::Dir(dir) => {
						CacheEventType::Dir(DirEvent::Changed(
							dir.as_ref()
								.try_into()
								.map_err(|e| (e, dir.uuid().into()))?,
						))
					}
				},
				DecryptedDriveEvent::FolderMetadataChanged(FolderMetadataChanged {
					uuid,
					meta,
				}) => CacheEventType::Dir(DirEvent::MetadataChanged {
					uuid: uuid.into(),
					meta: match meta {
						filen_sdk_rs::fs::dir::meta::DirectoryMeta::Decoded(decoded) => {
							decoded.as_borrowed_cow()
						}
						other => {
							return Err((
								CacheableConversionError::MetadataNotDecrypted(format!(
									"{:?}",
									other
								)),
								uuid.into(),
							));
						}
					},
				}),
				DecryptedDriveEvent::FileMetadataChanged(FileMetadataChanged {
					uuid,
					metadata,
				}) => CacheEventType::File(FileEvent::MetadataChanged {
					uuid: uuid.into(),
					meta: match metadata {
						filen_sdk_rs::fs::file::meta::FileMeta::Decoded(decoded) => {
							decoded.as_borrowed_cow()
						}
						other => {
							return Err((
								CacheableConversionError::MetadataNotDecrypted(format!(
									"{:?}",
									other
								)),
								uuid.into(),
							));
						}
					},
				}),
				DecryptedDriveEvent::DeleteAll => CacheEventType::Global(GlobalEvent::DeleteAll),
				DecryptedDriveEvent::DeleteVersioned => {
					CacheEventType::Global(GlobalEvent::DeleteVersioned)
				}
			})
		}
	}

	#[derive(Debug, CowHelpers, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
	pub(crate) struct CacheEvent<'a> {
		pub id: Option<u64>,
		pub event: CacheEventType<'a>,
	}

	#[allow(clippy::large_enum_variant)]
	#[derive(Debug, CowHelpers, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
	pub(crate) enum CacheEventMaybeDecrypted<'a> {
		Decrypted(CacheEvent<'a>),
		Undecrypted(CacheableConversionError, Uuid),
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
						Err((e, uuid)) => Self::Undecrypted(e, uuid),
					};

					Some(event)
				}
				_ => None,
			}
		}
	}
}
use event::{
	CacheEvent, CacheEventMaybeDecrypted, CacheEventType, DirEvent, FileEvent, GlobalEvent,
};
