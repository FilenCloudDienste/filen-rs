use std::{path::PathBuf, thread::JoinHandle};

use crossbeam::channel::{Sender, TrySendError};
use filen_sdk_rs::{
	Error, ErrorKind,
	auth::Client,
	fs::HasUUID,
	io::{RemoteDirectory, RemoteFile},
	socket::ListenerHandle,
};

use crate::{
	CacheControlMessage, CacheState,
	state::{CacheEvent, ManualEvent},
};

pub struct CacheHandle {
	task_handle: JoinHandle<()>,
	control_sender: Sender<CacheControlMessage>,
	manual_event_sender: Sender<CacheEvent>,
	_listener_handle: ListenerHandle,
}

impl CacheHandle {
	pub async fn new(client: &Client, cache_path: PathBuf) -> Result<Self, Error> {
		let (res_sender, res_receiver) = tokio::sync::oneshot::channel();

		let root_uuid = client.root().uuid().into();
		let handle = std::thread::spawn(move || {
			let state = match CacheState::new(&cache_path, root_uuid) {
				Ok((state, callback, control_sender, event_sender)) => {
					if res_sender
						.send(Ok((callback, control_sender, event_sender)))
						.is_err()
					{
						panic!("Failed to send cache initialization result");
					}
					state
				}
				Err(e) => {
					if res_sender.send(Err(e)).is_err() {
						panic!("Failed to send cache initialization result");
					}
					return;
				}
			};

			state.run();
		});

		let (callback, control_sender, manual_event_sender) = res_receiver.await.unwrap()?;

		// need to track all event types to make sure we don't miss any so we can increment global_message_id correctly
		let listener_handle = client.add_event_listener(callback, None).await?;

		Ok(Self {
			task_handle: handle,
			_listener_handle: listener_handle,
			control_sender,
			manual_event_sender,
		})
	}

	pub async fn update_list_dir_recursive(
		&self,
		dirs: Vec<RemoteDirectory>,
		files: Vec<RemoteFile>,
	) -> Result<(), Error> {
		self.manual_event_sender
			.send(CacheEvent::manual(ManualEvent::ListDirRecursive(
				dirs, files,
			)))
			.map_err(|e| {
				Error::custom_with_source(
					ErrorKind::Internal,
					e,
					Some("Failed to send manual event to cache thread"),
				)
			})
	}
}

impl Drop for CacheHandle {
	fn drop(&mut self) {
		if let Err(TrySendError::Full(_)) =
			self.control_sender.try_send(CacheControlMessage::Shutdown)
			&& !self.task_handle.is_finished()
		{
			log::error!("Failed to send shutdown signal to cache thread because it was full");
		}
	}
}
