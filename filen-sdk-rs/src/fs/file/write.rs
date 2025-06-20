use std::{
	borrow::Cow,
	io::{Cursor, Write},
	sync::{Arc, OnceLock},
};

use filen_types::crypto::Sha512Hash;
use futures::{AsyncWrite, FutureExt, StreamExt, future::BoxFuture, stream::FuturesUnordered};
use sha2::Digest;

use crate::{
	api,
	auth::Client,
	consts::{CHUNK_SIZE, DEFAULT_MAX_UPLOAD_THREADS_PER_FILE, FILE_CHUNK_SIZE_EXTRA},
	crypto::{
		self,
		shared::{DataCrypter, MetaCrypter},
	},
	error::Error,
};

use super::{BaseFile, RemoteFile, meta::FileMeta};

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
	callback: Option<Arc<dyn Fn(u64) + Send + Sync + 'a>>,
	futures: FuturesUnordered<BoxFuture<'a, Result<(), Error>>>,
	curr_chunk: Option<Cursor<Vec<u8>>>,
	next_chunk_idx: u64,
	written: u64,
	hasher: sha2::Sha512,
	remote_file_info: Arc<OnceLock<RemoteFileInfo>>,
	upload_key: Arc<String>,
	max_threads: usize,
	client: &'a Client,
}

impl<'a> FileWriterUploadingState<'a> {
	fn push_upload_next_chunk(&mut self, mut out_data: Vec<u8>) {
		let chunk_idx = self.next_chunk_idx;
		self.next_chunk_idx += 1;
		let client = self.client.clone();
		let file = self.file.clone();
		let upload_key = self.upload_key.clone();
		let callback = self.callback.clone();
		self.hasher.update(&out_data);
		let remote_file_info = self.remote_file_info.clone();
		self.futures.push(Box::pin(async move {
			// encrypt the data
			let len = out_data.len() as u64;
			file.key().encrypt_data(&mut out_data)?;
			// upload the data
			let result = api::v3::upload::upload_file_chunk(
				client.client(),
				&file,
				&upload_key,
				chunk_idx,
				out_data,
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
			Ok(())
		}));
	}

	pub fn write_next_chunk(&mut self, buf: &[u8]) -> std::io::Result<usize> {
		// take the chunk out of curr_chunk
		let mut cursor = match self.curr_chunk.take() {
			Some(cursor) => cursor,
			// this could be optimized, we currently allocated a full MiB for every chunk
			// but we only need to allocate the size of the chunk
			// the problem is that we don't know if there's another write_next_chunk coming
			None => Cursor::new(Vec::with_capacity(CHUNK_SIZE + FILE_CHUNK_SIZE_EXTRA)),
		};
		// todo double check if this write doesn't reallocate more memory
		// maybe do this another way to guarantee that buf is only max CHUNK_SIZE at this point
		let written = cursor.write(&buf[..Ord::min(buf.len(), CHUNK_SIZE)])?;
		// SAFETY: chunk should never be more than u32 in length
		self.written += TryInto::<u64>::try_into(written).unwrap();
		if (TryInto::<usize>::try_into(cursor.position()).unwrap()) < CHUNK_SIZE {
			// didn't write the whole chunk, put it back
			self.curr_chunk = Some(cursor);
		} else {
			// wrote the whole chunk, so we need to upload it
			self.push_upload_next_chunk(cursor.into_inner());
		}

		Ok(written)
	}

	fn into_completing_state(self) -> Result<FileWriterCompletingState<'a>, Error> {
		let file = self.file.clone();
		let hash = self.hasher.finalize();
		let client = self.client.clone();

		let empty_request = filen_types::api::v3::upload::empty::Request {
			uuid: file.uuid(),
			name: Cow::Owned(self.client.crypter().encrypt_meta(file.name())?),
			name_hashed: Cow::Owned(self.client.hash_name(file.name())),
			size: Cow::Owned(
				self.client
					.crypter()
					.encrypt_meta(&self.written.to_string())?,
			),
			parent: file.parent,
			mime: Cow::Owned(
				self.client
					.crypter()
					.encrypt_meta(self.file.as_ref().mime())?,
			),
			metadata: Cow::Owned(self.client.crypter().encrypt_meta(&serde_json::to_string(
				&FileMeta {
					name: Cow::Borrowed(file.name()),
					size: self.written,
					mime: Cow::Borrowed(file.mime()),
					key: Cow::Borrowed(file.key()),
					created: Some(file.created()),
					last_modified: file.last_modified(),
					hash: Some(hash.into()),
				},
			)?)?),
			version: self.client.file_encryption_version(),
		};

		let future: BoxFuture<'a, Result<filen_types::api::v3::upload::empty::Response, Error>> =
			if self.written == 0 {
				Box::pin(async move {
					api::v3::upload::empty::post(client.client(), &empty_request).await
				})
			} else {
				let upload_key = self.upload_key.clone();
				Box::pin(async move {
					api::v3::upload::done::post(
						client.client(),
						&api::v3::upload::done::Request {
							empty_request,
							chunks: self.next_chunk_idx,
							rm: Cow::Borrowed(&crypto::shared::generate_random_base64_values(32)),
							upload_key: Cow::Borrowed(&upload_key),
						},
					)
					.await
				})
			};

		let remote_file_info = match Arc::try_unwrap(self.remote_file_info) {
			Ok(lock) => lock.into_inner().unwrap_or_default(),
			Err(arc) => (*arc).get().cloned().unwrap_or_default(),
		};

		Ok(FileWriterCompletingState {
			file: self.file.clone(),
			future,
			hash: hash.into(),
			remote_file_info,
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

		while self.futures.len() < self.max_threads && written < buf.len() {
			// we can push a new chunk
			written += self.write_next_chunk(&buf[written..])?;
		}

		loop {
			match self.futures.poll_next_unpin(cx) {
				std::task::Poll::Ready(Some(Ok(()))) => {
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
			self.push_upload_next_chunk(last_chunk.into_inner());
		}

		loop {
			match self.futures.poll_next_unpin(cx) {
				std::task::Poll::Ready(Some(Ok(()))) => {}
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

struct FileWriterCompletingState<'a> {
	file: Arc<BaseFile>,
	future: BoxFuture<'a, Result<filen_types::api::v3::upload::empty::Response, Error>>,
	hash: Sha512Hash,
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

		FileWriterFinalizingState::new(
			Arc::new(RemoteFile {
				file: file.root,
				parent: file.parent.into(),
				size: response.size,
				favorited: false,
				region: self.remote_file_info.region,
				bucket: self.remote_file_info.bucket,
				chunks: response.chunks,
				hash: Some(self.hash),
			}),
			self.client,
		)
	}
}

struct FileWriterFinalizingState<'a> {
	file: Arc<RemoteFile>,
	futures: FuturesUnordered<BoxFuture<'a, Result<(), Error>>>,
	client: &'a Client,
}

impl<'a> FileWriterFinalizingState<'a> {
	fn new(file: Arc<RemoteFile>, client: &'a Client) -> Self {
		let futures: FuturesUnordered<BoxFuture<'a, Result<(), Error>>> = FuturesUnordered::new();

		let temp_file = file.clone();
		futures.push(Box::pin(async move {
			client
				.update_item_with_maybe_connected_parent(temp_file.as_ref())
				.await?;
			Ok(())
		}) as BoxFuture<'a, Result<(), Error>>);
		let temp_file = file.clone();
		futures.push(Box::pin(async move {
			client
				.update_search_hashes_for_item(temp_file.as_ref())
				.await?;
			Ok(())
		}) as BoxFuture<'a, Result<(), Error>>);

		Self {
			file,
			futures,
			client,
		}
	}

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
	Completing(FileWriterCompletingState<'a>),
	Finalizing(FileWriterFinalizingState<'a>),
	Complete(FileWriterCompleteState),
	Error(&'a str),
}

impl<'a> FileWriterState<'a> {
	fn new(
		file: Arc<BaseFile>,
		client: &'a Client,
		callback: Option<Arc<dyn Fn(u64) + Send + Sync + 'a>>,
	) -> Self {
		FileWriterState::Uploading(FileWriterUploadingState {
			file,
			callback,
			futures: FuturesUnordered::new(),
			curr_chunk: None,
			next_chunk_idx: 0,
			written: 0,
			hasher: sha2::Sha512::new(),
			remote_file_info: Arc::new(OnceLock::new()),
			upload_key: Arc::new(crypto::shared::generate_random_base64_values(32)),
			max_threads: DEFAULT_MAX_UPLOAD_THREADS_PER_FILE,
			client,
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
		callback: Option<Arc<dyn Fn(u64) + Send + Sync + 'a>>,
	) -> Self {
		Self {
			state: FileWriterState::new(file, client, callback),
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
			FileWriterState::Completing(_)
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
					std::task::Poll::Ready(Ok(())) => match uploading.into_completing_state() {
						Ok(completing) => FileWriterState::Completing(completing),
						Err(e) => {
							return std::task::Poll::Ready(Err(std::io::Error::other(e)));
						}
					},
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
			FileWriterState::Uploading(_) | FileWriterState::Completing(_) => {
				unreachable!("Should be handled by the first part of this function")
			}
		}
	}
}
