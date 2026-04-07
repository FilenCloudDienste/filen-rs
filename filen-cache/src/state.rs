use std::{iter::once, path::Path};

use crossbeam::channel::{Receiver, Sender, TrySendError};
use filen_sdk_rs::{
	fs::{HasUUID, dir::cache::CacheableDir, file::cache::CacheableFile},
	io::{RemoteDirectory, RemoteFile},
	socket::{DecryptedSocketEvent, DecryptedSocketEventType},
};
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
	Irrelevant,
	File(FileEvent),
	Dir(DirEvent),
	Manual(ManualEvent),
}

enum FileEvent {
	New(RemoteFile),
	Move(RemoteFile),
	Trash(Uuid),
}

enum DirEvent {
	New(RemoteDirectory),
	Move(RemoteDirectory),
	Trash(Uuid),
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
		let event_type = match event.inner() {
			DecryptedSocketEventType::FileNew(file_new) => {
				CacheEventType::File(FileEvent::New(file_new.0.clone()))
			}
			DecryptedSocketEventType::FileMove(file_move) => {
				CacheEventType::File(FileEvent::Move(file_move.0.clone()))
			}
			DecryptedSocketEventType::FileTrash(file_trash) => {
				CacheEventType::File(FileEvent::Trash((&file_trash.uuid).into()))
			}
			DecryptedSocketEventType::FolderSubCreated(dir_created) => {
				CacheEventType::Dir(DirEvent::New(dir_created.0.clone()))
			}
			DecryptedSocketEventType::FolderMove(dir_moved) => {
				CacheEventType::Dir(DirEvent::Move(dir_moved.0.clone()))
			}
			DecryptedSocketEventType::FolderTrash(dir_trash) => {
				CacheEventType::Dir(DirEvent::Trash((&dir_trash.uuid).into()))
			}
			_ => CacheEventType::Irrelevant,
		};

		// don't care if the channel is dropped, assume the cache is shutting down and we can just stop sending events
		if let Err(TrySendError::Full(e)) = sender.try_send(CacheEvent {
			id: event.global_message_id(),
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
			CacheEventType::Irrelevant => {
				// we don't care about this event, just ignore it
			}
			CacheEventType::File(file_event) => self.handle_file_event(file_event),
			CacheEventType::Dir(dir_event) => self.handle_dir_event(dir_event),
			CacheEventType::Manual(manual_event) => self.handle_manual_event(manual_event),
		}
	}

	fn handle_file_event(&mut self, event: FileEvent) {
		match event {
			FileEvent::New(file) | FileEvent::Move(file) => {
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
			FileEvent::Trash(uuid) => {
				if let Err(e) = self.delete_items(once(uuid)) {
					log::error!("Failed to delete file {:?} from cache: {:?}", uuid, e);
				}
			}
		}
	}

	fn handle_dir_event(&mut self, event: DirEvent) {
		match event {
			DirEvent::New(dir) | DirEvent::Move(dir) => {
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
			DirEvent::Trash(uuid) => {
				if let Err(e) = self.delete_items(once(uuid)) {
					log::error!("Failed to delete dir {:?} from cache: {:?}", uuid, e);
				}
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
