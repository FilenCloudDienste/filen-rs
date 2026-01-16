use std::{
	borrow::Cow,
	io::Write,
	num::NonZeroU32,
	sync::{Arc, OnceLock},
};

use bytes::Bytes;
use filen_types::crypto::Blake3Hash;
use futures::{AsyncWrite, FutureExt, StreamExt, stream::FuturesUnordered};

use crate::{
	api,
	auth::Client,
	consts::{CHUNK_SIZE, CHUNK_SIZE_U64, FILE_CHUNK_SIZE, FILE_CHUNK_SIZE_EXTRA},
	crypto::{
		self,
		shared::{DataCrypter, MetaCrypter},
	},
	error::Error,
	fs::file::chunk::Chunk,
	runtime::{blocking_join, do_cpu_intensive},
	sync::lock::ResourceLock,
	util::{MaybeSendBoxFuture, MaybeSendCallback},
};

use super::{BaseFile, RemoteFile, meta::DecryptedFileMeta};

#[derive(Debug, Clone)]
struct RemoteFileInfo {
	region: String,
	bucket: String,
}

impl Default for RemoteFileInfo {
	fn default() -> Self {
		Self {
			region: "de-1".to_string(),
			bucket: "filen-empty".to_string(),
		}
	}
}

struct FileWriterUploadingState<'a> {
	file: Arc<BaseFile>,
	callback: Option<MaybeSendCallback<'a, u64>>,
	futures: FuturesUnordered<MaybeSendBoxFuture<'a, Result<Chunk<'a>, Error>>>,
	alloc_future: Option<MaybeSendBoxFuture<'a, Chunk<'a>>>,
	// annoying that I have to split it up like this, but Cursor doesn't implement Write
	// for impl AsMut<Vec<u8>> so we can't use Cursor<Chunk<'a>> directly
	curr_chunk: Option<Chunk<'a>>,
	allocated_chunks: Vec<Chunk<'a>>,
	size: Option<u64>,
	next_chunk_idx: u64,
	written: u64,
	hasher: blake3::Hasher,
	remote_file_info: Arc<OnceLock<RemoteFileInfo>>,
	upload_key: Arc<String>,
	client: &'a Client,
}

impl<'a> FileWriterUploadingState<'a> {
	fn next_chunk_size(&self) -> Option<NonZeroU32> {
		match self.size {
			Some(0) => None,
			None => Some(FILE_CHUNK_SIZE.saturating_add(FILE_CHUNK_SIZE_EXTRA.get())),
			Some(size) if self.next_chunk_idx < size / CHUNK_SIZE_U64 => {
				Some(FILE_CHUNK_SIZE.saturating_add(FILE_CHUNK_SIZE_EXTRA.get()))
			}
			Some(size) => {
				let remaining = size - (self.next_chunk_idx * u64::from(FILE_CHUNK_SIZE.get()))
					+ u64::from(FILE_CHUNK_SIZE_EXTRA.get());
				let size: u32 = remaining.try_into().unwrap();
				Some(NonZeroU32::new(size).unwrap())
			}
		}
	}

	fn push_upload_next_chunk(&mut self, mut out_data: Chunk<'a>) {
		let chunk_idx = self.next_chunk_idx;
		self.next_chunk_idx += 1;
		let client = self.client;
		let file = self.file.clone();
		let upload_key = self.upload_key.clone();
		let callback = self.callback.clone();
		self.hasher.update_rayon(out_data.as_ref());
		let remote_file_info = self.remote_file_info.clone();
		self.futures.push(Box::pin(async move {
			// encrypt the data
			let len = out_data.as_ref().len() as u64;
			debug_assert!(
				len <= CHUNK_SIZE_U64,
				"Chunk size exceeded {CHUNK_SIZE_U64}: {len}"
			);
			file.key().blocking_encrypt_data(out_data.as_mut())?;

			// upload the data
			let (chunk_bytes, permit) = out_data.into_parts();

			let chunk_bytes: Bytes = chunk_bytes.into();
			let result = api::v3::upload::upload_file_chunk(
				client,
				&file,
				&upload_key,
				chunk_idx,
				chunk_bytes.clone(),
			)
			.await?;
			if let Some(callback) = callback {
				callback(len);
			}
			// don't care if this errors because that means another thread set it
			let _ = remote_file_info.set(RemoteFileInfo {
				region: result.region.into_owned(),
				bucket: result.bucket.into_owned(),
			});
			let mut chunk_bytes: Vec<u8> = chunk_bytes.into();
			chunk_bytes.clear();
			Ok(Chunk::from_parts(chunk_bytes, permit))
		}));
	}

	pub fn write_next_chunk(&mut self, buf: &[u8]) -> std::io::Result<usize> {
		// take the chunk out of curr_chunk
		let mut chunk = match self.curr_chunk.take() {
			Some(cursor) => cursor,
			// this could be optimized, we currently allocated a full MiB for every chunk
			// but we only need to allocate the size of the chunk
			// the problem is that we don't know if there's another write_next_chunk coming
			None => {
				if let Some(chunk) = self.allocated_chunks.pop() {
					chunk
				} else {
					if let Some(size) = self.next_chunk_size()
						&& self.alloc_future.is_none()
					{
						self.alloc_future = Some(Box::pin(Chunk::acquire(size, self.client))
							as MaybeSendBoxFuture<'a, Chunk<'a>>);
					}
					return Ok(0);
				}
			}
		};
		let written = chunk.write(&buf[..Ord::min(buf.len(), CHUNK_SIZE)])?;
		// SAFETY: chunk should never be more than u32 in length
		self.written += u64::try_from(written).unwrap();
		if chunk.len() < CHUNK_SIZE {
			// didn't write the whole chunk, put it back
			self.curr_chunk = Some(chunk);
		} else {
			// wrote the whole chunk, so we need to upload it
			self.push_upload_next_chunk(chunk);
		}

		Ok(written)
	}

	fn into_waiting_for_drive_lock_state(
		self,
	) -> Result<FileWriterWaitingForDriveLockState<'a>, Error> {
		let lock_future = self.client.lock_drive();
		let hash = Blake3Hash::from(self.hasher.finalize());

		let remote_file_info = match Arc::try_unwrap(self.remote_file_info) {
			Ok(lock) => lock.into_inner().unwrap_or_default(),
			Err(arc) => (*arc).get().cloned().unwrap_or_default(),
		};

		Ok(FileWriterWaitingForDriveLockState {
			file: self.file,
			lock_future: Box::pin(lock_future),
			hash,
			remote_file_info,
			upload_key: self.upload_key,
			written: self.written,
			num_chunks: self.next_chunk_idx,
			client: self.client,
		})
	}

	fn poll_write(
		&mut self,
		cx: &mut std::task::Context<'_>,
		buf: &[u8],
	) -> std::task::Poll<std::io::Result<usize>> {
		let mut written = self.write_next_chunk(buf)?;
		if written >= buf.len() {
			// we've filled the buffer
			return std::task::Poll::Ready(Ok(written));
		}

		let mut should_pend = false;
		while let Some(mut future) = self.alloc_future.take() {
			if self.next_chunk_size().is_none() {
				self.allocated_chunks.clear();
				break;
			}
			match future.poll_unpin(cx) {
				std::task::Poll::Ready(chunk) => {
					self.allocated_chunks.push(chunk);
					written += self.write_next_chunk(buf)?;
					if written >= buf.len() {
						// we've filled the buffer
						return std::task::Poll::Ready(Ok(written));
					}
				}
				std::task::Poll::Pending => {
					// put the future back, we need to wait for it
					self.alloc_future = Some(future);
					should_pend = true;
					break;
				}
			}
		}

		loop {
			match self.futures.poll_next_unpin(cx) {
				std::task::Poll::Ready(Some(Ok(chunk))) => {
					if self.next_chunk_size().is_some() {
						self.allocated_chunks.push(chunk);
					} else {
						self.allocated_chunks.clear();
					}
					if written < buf.len() {
						written += self.write_next_chunk(&buf[written..])?;
					}
					if written >= buf.len() {
						// we've filled the buffer
						return std::task::Poll::Ready(Ok(written));
					}
				}
				std::task::Poll::Ready(Some(Err(e))) => {
					return std::task::Poll::Ready(Err(std::io::Error::other(e.to_string())));
				}
				std::task::Poll::Ready(None) => {
					// all futures are done, return the number of bytes written
					if should_pend && written == 0 {
						return std::task::Poll::Pending;
					}
					return std::task::Poll::Ready(Ok(written));
				}
				std::task::Poll::Pending => {
					if written > 0 {
						// we have written some data, return it
						return std::task::Poll::Ready(Ok(written));
					}
					return std::task::Poll::Pending;
				}
			}
		}
	}

	fn poll_close(
		&mut self,
		cx: &mut std::task::Context<'_>,
	) -> std::task::Poll<std::io::Result<()>> {
		if let Some(last_chunk) = self.curr_chunk.take() {
			// we have a chunk to upload
			self.push_upload_next_chunk(last_chunk);
		}

		loop {
			match self.futures.poll_next_unpin(cx) {
				std::task::Poll::Ready(Some(Ok(_))) => {}
				std::task::Poll::Ready(Some(Err(e))) => {
					return std::task::Poll::Ready(Err(std::io::Error::other(e.to_string())));
				}
				std::task::Poll::Ready(None) => {
					return std::task::Poll::Ready(Ok(()));
				}
				std::task::Poll::Pending => {
					return std::task::Poll::Pending;
				}
			}
		}
	}
}

struct FileWriterWaitingForDriveLockState<'a> {
	file: Arc<BaseFile>,
	lock_future: MaybeSendBoxFuture<'a, Result<Arc<ResourceLock>, Error>>,
	hash: Blake3Hash,
	remote_file_info: RemoteFileInfo,
	upload_key: Arc<String>,
	written: u64,
	num_chunks: u64,
	client: &'a Client,
}

impl<'a> FileWriterWaitingForDriveLockState<'a> {
	fn into_completing_state(
		self,
		drive_lock: Arc<ResourceLock>,
	) -> Result<FileWriterCompletingState<'a>, Error> {
		let file = self.file.clone();
		let crypter = self.client.crypter();

		let empty_request_future = do_cpu_intensive(move || {
			let file_key = file.key().to_meta_key()?;
			let (name, size, mime, metadata) = blocking_join!(
				|| file_key.blocking_encrypt_meta(file.name()),
				|| file_key.blocking_encrypt_meta(&self.written.to_string()),
				|| file_key.blocking_encrypt_meta(file.as_ref().mime()),
				|| Ok::<_, Error>(crypter.blocking_encrypt_meta(&serde_json::to_string(
					&DecryptedFileMeta {
						name: Cow::Borrowed(file.name()),
						size: self.written,
						mime: Cow::Borrowed(file.mime()),
						key: Cow::Borrowed(file.key()),
						created: Some(file.created()),
						last_modified: file.last_modified(),
						hash: Some(self.hash),
					},
				)?))
			);

			Ok::<_, Error>(filen_types::api::v3::upload::empty::Request {
				uuid: file.uuid(),
				name,
				name_hashed: Cow::Owned(self.client.hash_name(file.name())),
				size,
				parent: file.parent,
				mime,
				metadata: metadata?,
				version: self.client.file_encryption_version(),
			})
		});

		let future: MaybeSendBoxFuture<
			'a,
			Result<filen_types::api::v3::upload::empty::Response, Error>,
		> = if self.written == 0 {
			Box::pin(async move {
				api::v3::upload::empty::post(self.client, &empty_request_future.await?).await
			})
		} else {
			let upload_key = self.upload_key.clone();
			Box::pin(async move {
				let rm = Cow::Owned(crypto::shared::generate_random_base64_values(
					32,
					&mut rand::rng(),
				));
				api::v3::upload::done::post(
					self.client,
					&api::v3::upload::done::Request {
						empty_request: empty_request_future.await?,
						chunks: self.num_chunks,
						rm,
						upload_key: Cow::Borrowed(&upload_key),
					},
				)
				.await
			})
		};

		Ok(FileWriterCompletingState {
			file: self.file,
			future,
			hash: self.hash,
			remote_file_info: self.remote_file_info,
			client: self.client,
			drive_lock,
		})
	}

	fn poll_close(
		&mut self,
		cx: &mut std::task::Context<'_>,
	) -> std::task::Poll<std::io::Result<Arc<ResourceLock>>> {
		match self.lock_future.poll_unpin(cx) {
			std::task::Poll::Ready(Ok(drive_lock)) => std::task::Poll::Ready(Ok(drive_lock)),
			std::task::Poll::Ready(Err(e)) => {
				std::task::Poll::Ready(Err(std::io::Error::other(e.to_string())))
			}
			std::task::Poll::Pending => std::task::Poll::Pending,
		}
	}
}

struct FileWriterCompletingState<'a> {
	file: Arc<BaseFile>,
	drive_lock: Arc<ResourceLock>,
	future: MaybeSendBoxFuture<'a, Result<filen_types::api::v3::upload::empty::Response, Error>>,
	hash: Blake3Hash,
	remote_file_info: RemoteFileInfo,
	client: &'a Client,
}

impl<'a> FileWriterCompletingState<'a> {
	fn poll_close(
		&mut self,
		cx: &mut std::task::Context<'_>,
	) -> std::task::Poll<std::io::Result<filen_types::api::v3::upload::empty::Response>> {
		match self.future.poll_unpin(cx) {
			std::task::Poll::Ready(Err(e)) => {
				std::task::Poll::Ready(Err(std::io::Error::other(e.to_string())))
			}
			std::task::Poll::Ready(Ok(response)) => std::task::Poll::Ready(Ok(response)),
			std::task::Poll::Pending => std::task::Poll::Pending,
		}
	}

	fn into_finalizing_state(
		self,
		response: filen_types::api::v3::upload::empty::Response,
	) -> FileWriterFinalizingState<'a> {
		let file = Arc::try_unwrap(self.file).unwrap_or_else(|arc| (*arc).clone());
		let file = Arc::new(RemoteFile {
			uuid: file.root.uuid,
			parent: file.parent.into(),
			size: response.size,
			favorited: false,
			region: self.remote_file_info.region,
			bucket: self.remote_file_info.bucket,
			timestamp: response.timestamp,
			chunks: response.chunks,
			meta: super::meta::FileMeta::Decoded(DecryptedFileMeta {
				name: Cow::Owned(file.root.name),
				size: response.size,
				mime: Cow::Owned(file.root.mime),
				key: Cow::Owned(file.root.key),
				last_modified: file.root.modified,
				created: Some(file.root.created),
				hash: Some(self.hash),
			}),
		});
		let futures: FuturesUnordered<MaybeSendBoxFuture<'a, Result<(), Error>>> =
			FuturesUnordered::new();

		let temp_file = file.clone();
		futures.push(Box::pin(async move {
			self.client
				.update_item_with_maybe_connected_parent(temp_file.as_ref())
				.await?;
			Ok(())
		}) as MaybeSendBoxFuture<'a, Result<(), Error>>);
		let temp_file = file.clone();
		futures.push(Box::pin(async move {
			self.client
				.update_search_hashes_for_item(temp_file.as_ref())
				.await?;
			Ok(())
		}) as MaybeSendBoxFuture<'a, Result<(), Error>>);

		FileWriterFinalizingState {
			file,
			client: self.client,
			futures,
			drive_lock: self.drive_lock,
		}
	}
}

struct FileWriterFinalizingState<'a> {
	drive_lock: Arc<ResourceLock>,
	file: Arc<RemoteFile>,
	futures: FuturesUnordered<MaybeSendBoxFuture<'a, Result<(), Error>>>,
	client: &'a Client,
}

impl<'a> FileWriterFinalizingState<'a> {
	fn poll_close(
		&mut self,
		cx: &mut std::task::Context<'_>,
	) -> std::task::Poll<std::io::Result<()>> {
		loop {
			match self.futures.poll_next_unpin(cx) {
				std::task::Poll::Ready(Some(Ok(()))) => {}
				std::task::Poll::Ready(Some(Err(e))) => {
					return std::task::Poll::Ready(Err(std::io::Error::other(e)));
				}
				std::task::Poll::Ready(None) => {
					return std::task::Poll::Ready(Ok(()));
				}
				std::task::Poll::Pending => {
					return std::task::Poll::Pending;
				}
			}
		}
	}

	fn into_complete_state(self) -> FileWriterCompleteState {
		FileWriterCompleteState {
			file: Arc::try_unwrap(self.file).unwrap_or_else(|arc| (*arc).clone()),
		}
	}
}

struct FileWriterCompleteState {
	file: RemoteFile,
}

#[allow(clippy::large_enum_variant)]
enum FileWriterState<'a> {
	Uploading(FileWriterUploadingState<'a>),
	WaitingForDriveLock(FileWriterWaitingForDriveLockState<'a>),
	Completing(FileWriterCompletingState<'a>),
	Finalizing(FileWriterFinalizingState<'a>),
	Complete(FileWriterCompleteState),
	Error(&'a str),
}

impl<'a> FileWriterState<'a> {
	fn new(
		file: Arc<BaseFile>,
		client: &'a Client,
		callback: Option<MaybeSendCallback<'a, u64>>,
		size: Option<u64>,
	) -> Self {
		FileWriterState::Uploading(FileWriterUploadingState {
			file,
			callback,
			futures: FuturesUnordered::new(),
			curr_chunk: None,
			next_chunk_idx: 0,
			written: 0,
			hasher: blake3::Hasher::new(),
			remote_file_info: Arc::new(OnceLock::new()),
			upload_key: Arc::new(crypto::shared::generate_random_base64_values(
				32,
				&mut rand::rng(),
			)),
			client,
			alloc_future: None,
			allocated_chunks: Vec::new(),
			size,
		})
	}

	fn take_with_err(&mut self, error: &'a str) -> FileWriterState<'a> {
		std::mem::replace(self, FileWriterState::Error(error))
	}
}

pub struct FileWriter<'a> {
	state: FileWriterState<'a>,
}

impl<'a> FileWriter<'a> {
	pub(crate) fn new(
		file: Arc<BaseFile>,
		client: &'a Client,
		callback: Option<MaybeSendCallback<'a, u64>>,
		size: Option<u64>,
	) -> Self {
		Self {
			state: FileWriterState::new(file, client, callback, size),
		}
	}

	pub fn into_remote_file(self) -> Option<RemoteFile> {
		match self.state {
			FileWriterState::Complete(complete) => Some(complete.file),
			_ => None,
		}
	}
}

impl AsyncWrite for FileWriter<'_> {
	fn poll_write(
		mut self: std::pin::Pin<&mut Self>,
		cx: &mut std::task::Context<'_>,
		buf: &[u8],
	) -> std::task::Poll<std::io::Result<usize>> {
		match &mut self.state {
			FileWriterState::Uploading(uploading) => uploading.poll_write(cx, buf),
			FileWriterState::WaitingForDriveLock(_)
			| FileWriterState::Completing(_)
			| FileWriterState::Finalizing(_)
			| FileWriterState::Complete(_) => {
				// we are in the completing state, we can't write anymore
				std::task::Poll::Ready(Err(std::io::Error::other(
					"Cannot write to a completed file",
				)))
			}
			FileWriterState::Error(e) => std::task::Poll::Ready(Err(std::io::Error::other(*e))),
		}
	}

	fn poll_flush(
		self: std::pin::Pin<&mut Self>,
		_cx: &mut std::task::Context<'_>,
	) -> std::task::Poll<std::io::Result<()>> {
		// we can't flush because we cannot upload partial chunks
		// and we don't know if the upload is done
		// so we just return Ok
		std::task::Poll::Ready(Ok(()))
	}

	fn poll_close(
		mut self: std::pin::Pin<&mut Self>,
		cx: &mut std::task::Context<'_>,
	) -> std::task::Poll<std::io::Result<()>> {
		let state = match self.state.take_with_err("Failed to close file writer") {
			FileWriterState::Uploading(mut uploading) => {
				// we are in the uploading state, we need to poll the futures
				match uploading.poll_close(cx) {
					std::task::Poll::Ready(Ok(())) => {
						match uploading.into_waiting_for_drive_lock_state() {
							Ok(waiting) => FileWriterState::WaitingForDriveLock(waiting),
							Err(e) => {
								return std::task::Poll::Ready(Err(std::io::Error::other(e)));
							}
						}
					}
					std::task::Poll::Ready(Err(e)) => {
						return std::task::Poll::Ready(Err(std::io::Error::other(e)));
					}
					std::task::Poll::Pending => {
						self.state = FileWriterState::Uploading(uploading);
						return std::task::Poll::Pending;
					}
				}
			}
			state => state,
		};

		let state = if let FileWriterState::WaitingForDriveLock(mut waiting) = state {
			match waiting.poll_close(cx) {
				std::task::Poll::Ready(Ok(drive_lock)) => {
					match waiting.into_completing_state(drive_lock) {
						Ok(completing) => FileWriterState::Completing(completing),
						Err(e) => return std::task::Poll::Ready(Err(std::io::Error::other(e))),
					}
				}
				std::task::Poll::Ready(Err(e)) => {
					return std::task::Poll::Ready(Err(std::io::Error::other(e)));
				}
				std::task::Poll::Pending => {
					self.state = FileWriterState::WaitingForDriveLock(waiting);
					return std::task::Poll::Pending;
				}
			}
		} else {
			state
		};

		let state = match state {
			FileWriterState::Uploading(_) => {
				unreachable!("Should be handled by the first part of this function")
			}
			FileWriterState::Completing(mut completing) => match completing.poll_close(cx) {
				std::task::Poll::Ready(Ok(response)) => {
					FileWriterState::Finalizing(completing.into_finalizing_state(response))
				}
				std::task::Poll::Ready(Err(e)) => {
					return std::task::Poll::Ready(Err(std::io::Error::other(e)));
				}
				std::task::Poll::Pending => {
					self.state = FileWriterState::Completing(completing);
					return std::task::Poll::Pending;
				}
			},
			state => state,
		};

		// now state cannot be uploading anymore, ideally this would be part of a match

		match state {
			FileWriterState::Finalizing(mut finalizing) => match finalizing.poll_close(cx) {
				std::task::Poll::Ready(Ok(())) => {
					self.state = FileWriterState::Complete(finalizing.into_complete_state());
					std::task::Poll::Ready(Ok(()))
				}
				std::task::Poll::Ready(Err(e)) => std::task::Poll::Ready(Err(e)),
				std::task::Poll::Pending => {
					self.state = FileWriterState::Finalizing(finalizing);
					std::task::Poll::Pending
				}
			},
			FileWriterState::Complete(complete) => {
				self.state = FileWriterState::Complete(complete);
				std::task::Poll::Ready(Ok(()))
			}
			FileWriterState::Error(e) => std::task::Poll::Ready(Err(std::io::Error::other(e))),
			FileWriterState::Uploading(_)
			| FileWriterState::Completing(_)
			| FileWriterState::WaitingForDriveLock(_) => {
				unreachable!("Should be handled by the first part of this function")
			}
		}
	}
}
