use std::{
	borrow::Cow,
	io::Write,
	num::NonZeroU32,
	sync::{Arc, OnceLock},
};

use bytes::Bytes;
use chrono::{DateTime, Utc};
use filen_types::crypto::Blake3Hash;
use futures::{AsyncWrite, FutureExt, StreamExt, stream::FuturesUnordered};

use super::exif::ExifTeeState;

use crate::{
	api,
	auth::Client,
	connect::ParentTargets,
	consts::{CHUNK_SIZE, CHUNK_SIZE_U64, FILE_CHUNK_SIZE, FILE_CHUNK_SIZE_EXTRA},
	crypto::{
		self,
		shared::{DataCrypter, MetaCrypter},
	},
	error::Error,
	fs::{categories::NonRootItemType, file::chunk::Chunk},
	progress::ThrottledProgress,
	runtime::{blocking_join, do_cpu_intensive},
	sync::lock::{ResourceLock, confirm_or_retake},
	util::{MaybeArc, MaybeSend, MaybeSendBoxFuture, MaybeSendCallback},
};

use super::{BaseFile, RemoteFile, meta::DecryptedFileMeta};

/// Where the server stored an upload's chunks, as reported by the chunk-upload responses.
/// A zero-byte file uploads no chunks and keeps the default.
#[derive(Debug, Clone)]
pub(crate) struct RemoteFileInfo {
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

pub struct DummyFuture;

impl Future for DummyFuture {
	type Output = Result<(), Error>;

	fn poll(
		self: std::pin::Pin<&mut Self>,
		_cx: &mut std::task::Context<'_>,
	) -> std::task::Poll<Self::Output> {
		std::task::Poll::Ready(Ok(()))
	}
}

/// Encrypts the plaintext `chunk` in place and uploads it as chunk `chunk_idx` of `file`.
///
/// Returns the same buffer, cleared and still holding its memory reservation, together with
/// where the server stored the chunk.
async fn encrypt_and_upload_chunk<'a>(
	client: &Client,
	file: &BaseFile,
	upload_key: &str,
	chunk_idx: u64,
	chunk: Chunk<'a>,
) -> Result<(Chunk<'a>, RemoteFileInfo), Error> {
	let (data, permit) = chunk.into_parts();
	let (data, info) =
		encrypt_and_upload_chunk_data(client, file, upload_key, chunk_idx, data).await?;
	Ok((Chunk::from_parts(data, permit), info))
}

/// [`encrypt_and_upload_chunk`] for a caller that accounts for the chunk's memory itself.
/// Encryption runs on the CPU pool.
pub(crate) async fn encrypt_and_upload_chunk_data(
	client: &Client,
	file: &BaseFile,
	upload_key: &str,
	chunk_idx: u64,
	mut data: Vec<u8>,
) -> Result<(Vec<u8>, RemoteFileInfo), Error> {
	let len = data.len() as u64;
	debug_assert!(
		len <= CHUNK_SIZE_U64,
		"Chunk size exceeded {CHUNK_SIZE_U64}: {len}"
	);
	do_cpu_intensive(|| {
		let _span = tracing::debug_span!("upload_encrypt_chunk", chunk_idx, len).entered();
		file.key().blocking_encrypt_data(&mut data)
	})
	.await?;

	let chunk_bytes: Bytes = data.into();
	let result = api::v3::upload::upload_file_chunk(
		client.client(),
		file,
		upload_key,
		chunk_idx,
		chunk_bytes.clone(),
	)
	.await?;
	let info = RemoteFileInfo {
		region: result.region.into_owned(),
		bucket: result.bucket.into_owned(),
	};
	let mut data: Vec<u8> = chunk_bytes.into();
	data.clear();
	Ok((data, info))
}

/// Everything finalizing an upload needs once all of its chunks are on the server.
#[derive(Debug, Clone, Copy)]
pub(crate) struct UploadCompletion {
	/// Plaintext bytes uploaded.
	pub(crate) written: u64,
	/// Chunks uploaded.
	pub(crate) num_chunks: u64,
	/// Blake3 hash of the plaintext, in chunk order.
	pub(crate) hash: Blake3Hash,
	/// `(created, modified)` stored in the file's metadata.
	pub(crate) final_times: (DateTime<Utc>, DateTime<Utc>),
}

/// Registers an uploaded file with the server: `upload/empty` for a zero-byte file,
/// `upload/done` otherwise. The caller must hold the drive lock.
pub(crate) async fn complete_upload(
	client: &Client,
	file: &BaseFile,
	upload_key: &str,
	completion: UploadCompletion,
) -> Result<filen_types::api::v3::upload::empty::Response, Error> {
	let crypter = client.crypter();
	let (final_created, final_modified) = completion.final_times;

	let empty_request = do_cpu_intensive(move || {
		let file_key = file.key().to_meta_key()?;
		let (name, size, mime, metadata) = blocking_join!(
			|| file_key.blocking_encrypt_meta(file.name()),
			|| file_key.blocking_encrypt_meta(&completion.written.to_string()),
			|| file_key.blocking_encrypt_meta(file.mime()),
			|| Ok::<_, Error>(crypter.blocking_encrypt_meta(&serde_json::to_string(
				&DecryptedFileMeta {
					name: Cow::Borrowed(file.name()),
					size: completion.written,
					mime: Cow::Borrowed(file.mime()),
					key: *file.key(),
					created: Some(final_created),
					last_modified: final_modified,
					hash: Some(completion.hash),
				},
			)?))
		);

		Ok::<_, Error>(filen_types::api::v3::upload::empty::Request {
			uuid: file.uuid(),
			name,
			name_hashed: Cow::Owned(client.hash_name(file.name())),
			size,
			parent: file.parent,
			mime,
			metadata: metadata?,
			version: client.file_encryption_version(),
		})
	})
	.await?;

	if completion.written == 0 {
		api::v3::upload::empty::post(client.client(), &empty_request).await
	} else {
		let rm = Cow::Owned(crypto::shared::generate_random_base64_values(
			32,
			&mut rand::rng(),
		));
		api::v3::upload::done::post(
			client.client(),
			&api::v3::upload::done::Request {
				empty_request,
				chunks: completion.num_chunks,
				rm,
				upload_key: Cow::Borrowed(upload_key),
			},
		)
		.await
	}
}

/// The [`RemoteFile`] an upload produced, from the local file, the server's registration
/// response and the upload's completion data.
pub(crate) fn remote_file_from_upload(
	file: BaseFile,
	response: filen_types::api::v3::upload::empty::Response,
	remote_file_info: RemoteFileInfo,
	completion: UploadCompletion,
) -> RemoteFile {
	let (final_created, final_modified) = completion.final_times;
	RemoteFile {
		uuid: response.uuid,
		// The edit-vs-new signal: equal to `uuid` when this upload created
		// a new file, the existing lineage's id when it edited one.
		stable_uuid: response.stable_uuid,
		parent: file.parent.into(),
		size: response.size,
		favorited: false,
		region: remote_file_info.region,
		bucket: remote_file_info.bucket,
		timestamp: response.timestamp,
		chunks: response.chunks,
		meta: super::meta::FileMeta::Decoded(DecryptedFileMeta {
			name: Cow::Owned(file.root.name.into()),
			size: response.size,
			mime: Cow::Owned(file.root.mime),
			key: file.root.key,
			last_modified: final_modified,
			created: Some(final_created),
			hash: Some(completion.hash),
		}),
	}
}

struct FileWriterUploadingState<'a, F, Fut>
where
	F: FnOnce(Blake3Hash, u64) -> Fut + MaybeSend + 'a,
{
	file: Arc<BaseFile>,
	progress: Option<MaybeArc<ThrottledProgress<'a>>>,
	futures: FuturesUnordered<MaybeSendBoxFuture<'a, Result<Chunk<'a>, Error>>>,
	alloc_future: Option<MaybeSendBoxFuture<'a, Chunk<'a>>>,
	confirm_upload_callback: Option<F>,
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
	// Overrides file.created/file.last_modified at finalization time when Some.
	final_times: Option<(DateTime<Utc>, DateTime<Utc>)>,
	// Active when EXIF parsing is enabled and the MIME matches a supported kind.
	// Tees written bytes into the parser channel and produces resolved times at
	// finalization time.
	exif_tee: Option<ExifTeeState>,
	client: &'a Client,
}

impl<'a, F, Fut> FileWriterUploadingState<'a, F, Fut>
where
	F: FnOnce(Blake3Hash, u64) -> Fut + MaybeSend + 'a,
	Fut: Future<Output = Result<(), Error>> + MaybeSend + 'a,
{
	fn into_waiting_for_drive_lock_state(
		self,
	) -> Result<FileWriterWaitingForDriveLockState<'a>, Error> {
		let lock_future = self.client.lock_drive();
		let hash = Blake3Hash::from(self.hasher.finalize());

		let remote_file_info = match Arc::try_unwrap(self.remote_file_info) {
			Ok(lock) => lock.into_inner().unwrap_or_default(),
			Err(arc) => (*arc).get().cloned().unwrap_or_default(),
		};

		let combined_future = Box::pin(async move {
			futures::try_join!(lock_future, async {
				if let Some(confirm_upload_callback) = self.confirm_upload_callback {
					confirm_upload_callback(hash, self.written).await
				} else {
					Ok(())
				}
			})
		});

		Ok(FileWriterWaitingForDriveLockState {
			file: self.file,
			lock_and_confirm_upload_future: combined_future,
			hash,
			remote_file_info,
			upload_key: self.upload_key,
			written: self.written,
			num_chunks: self.next_chunk_idx,
			final_times: self.final_times,
			client: self.client,
		})
	}
}

/// Whether accepting another non-empty `buf` would carry the total written bytes past a declared
/// `known_size`. A reader that yields more than its declared size (e.g. the file was appended to
/// between the stat and the read) would otherwise stall on a zero-capacity final chunk and surface
/// an opaque [`std::io::ErrorKind::WriteZero`]; detecting the overflow lets us report it clearly.
fn would_exceed_known_size(known_size: Option<u64>, written: u64, buf_len: usize) -> bool {
	known_size.is_some_and(|size| written >= size) && buf_len > 0
}

impl<'a, F, Fut> FileWriterUploadingState<'a, F, Fut>
where
	F: FnOnce(Blake3Hash, u64) -> Fut + MaybeSend + 'a,
{
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

	fn push_upload_next_chunk(&mut self, out_data: Chunk<'a>) {
		let chunk_idx = self.next_chunk_idx;
		self.next_chunk_idx += 1;
		let client = self.client;
		let file = self.file.clone();
		let upload_key = self.upload_key.clone();
		let progress = self.progress.clone();
		self.hasher.update_rayon(out_data.as_ref());
		let remote_file_info = self.remote_file_info.clone();
		self.futures.push(Box::pin(async move {
			let len = out_data.as_ref().len() as u64;
			let (chunk, info) =
				encrypt_and_upload_chunk(client, &file, &upload_key, chunk_idx, out_data).await?;
			if let Some(progress) = progress {
				progress.report(len);
			}
			// don't care if this errors because that means another thread set it
			let _ = remote_file_info.set(info);
			Ok(chunk)
		}));
	}

	pub fn write_next_chunk(&mut self, buf: &[u8]) -> std::io::Result<usize> {
		// A reader that yields more than the caller's declared size would otherwise stall on a
		// zero-capacity final chunk and fail with an opaque WriteZero, orphaning the already
		// uploaded chunks. Surface the size mismatch with a descriptive error instead.
		if would_exceed_known_size(self.size, self.written, buf.len()) {
			return Err(std::io::Error::new(
				std::io::ErrorKind::InvalidData,
				format!(
					"reader produced more bytes than the declared size of {} bytes",
					self.size.unwrap_or_default()
				),
			));
		}
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
						self.alloc_future =
							Some(Box::pin(Chunk::acquire(size, self.client.client().state()))
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

	/// See [`FileWriter::poll_next_chunk`].
	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	fn poll_next_chunk(
		&mut self,
		cx: &mut std::task::Context<'_>,
	) -> std::task::Poll<std::io::Result<Chunk<'a>>> {
		// a chunk with room left was the file's last, and a declared size is the file's end
		let open = self.curr_chunk.is_none() && self.size.is_none_or(|size| self.written < size);
		let Some(chunk_size) = open.then(|| self.next_chunk_size()).flatten() else {
			return std::task::Poll::Ready(Err(std::io::Error::other(
				"Cannot add a chunk past the end of the file",
			)));
		};
		// EXIF backpressure, as in `poll_write`: no new chunk while the tee still holds bytes
		if let Some(tee) = self.exif_tee.as_mut()
			&& tee.poll_tee(cx, &[]).is_pending()
		{
			return std::task::Poll::Pending;
		}
		// an uploaded chunk's buffer is reused, and a failed upload fails the file
		while let std::task::Poll::Ready(Some(uploaded)) = self.futures.poll_next_unpin(cx) {
			self.allocated_chunks
				.push(uploaded.map_err(std::io::Error::other)?);
		}
		if let Some(chunk) = self.allocated_chunks.pop() {
			return std::task::Poll::Ready(Ok(chunk));
		}
		let client = self.client;
		let alloc = self
			.alloc_future
			.get_or_insert_with(|| Box::pin(Chunk::acquire(chunk_size, client.client().state())));
		let chunk = std::task::ready!(alloc.poll_unpin(cx));
		self.alloc_future = None;
		std::task::Poll::Ready(Ok(chunk))
	}

	/// See [`FileWriter::submit_chunk`].
	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	fn submit_chunk(&mut self, chunk: Chunk<'a>) {
		if chunk.as_ref().is_empty() {
			// nothing to upload, and the budget gets the memory back
			return;
		}
		self.written += chunk.len() as u64;
		if let Some(tee) = self.exif_tee.as_mut() {
			// No waker needed: on Pending the tee keeps the bytes, and the next `poll_next_chunk`
			// or `poll_close` sends them first and waits on the parser then, with the caller's.
			let _ = tee.poll_tee(
				&mut std::task::Context::from_waker(std::task::Waker::noop()),
				chunk.as_ref(),
			);
		}
		if chunk.len() < CHUNK_SIZE {
			// the file's last chunk, uploaded on close like a partly written one
			self.curr_chunk = Some(chunk);
		} else {
			self.push_upload_next_chunk(chunk);
		}
	}

	fn poll_write(
		&mut self,
		cx: &mut std::task::Context<'_>,
		buf: &[u8],
	) -> std::task::Poll<std::io::Result<usize>> {
		// EXIF backpressure: if the tee has bytes still pending into the parser
		// channel, block here until they drain. Bounds the parser-side buffer to
		// ~channel-cap + 1 stashed chunk (~5 MiB).
		if let Some(tee) = self.exif_tee.as_mut()
			&& let std::task::Poll::Pending = tee.poll_tee(cx, &[])
		{
			return std::task::Poll::Pending;
		}

		// Compute the result inside a labeled block so that every Ready(Ok(n))
		// exit goes through the EXIF tee at the bottom of this function. Early
		// `return` from the original implementation would have skipped teeing
		// when write_next_chunk filled the buffer in one go.
		let res: std::task::Poll<std::io::Result<usize>> = 'compute: {
			let mut written = match self.write_next_chunk(buf) {
				Ok(n) => n,
				Err(e) => break 'compute std::task::Poll::Ready(Err(e)),
			};
			if written >= buf.len() {
				break 'compute std::task::Poll::Ready(Ok(written));
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
						match self.write_next_chunk(&buf[written..]) {
							Ok(n) => written += n,
							Err(e) => break 'compute std::task::Poll::Ready(Err(e)),
						}
						if written >= buf.len() {
							break 'compute std::task::Poll::Ready(Ok(written));
						}
					}
					std::task::Poll::Pending => {
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
							match self.write_next_chunk(&buf[written..]) {
								Ok(n) => written += n,
								Err(e) => break 'compute std::task::Poll::Ready(Err(e)),
							}
						}
						if written >= buf.len() {
							break 'compute std::task::Poll::Ready(Ok(written));
						}
					}
					std::task::Poll::Ready(Some(Err(e))) => {
						break 'compute std::task::Poll::Ready(Err(std::io::Error::other(e)));
					}
					std::task::Poll::Ready(None) => {
						if should_pend && written == 0 {
							break 'compute std::task::Poll::Pending;
						}
						break 'compute std::task::Poll::Ready(Ok(written));
					}
					std::task::Poll::Pending => {
						if written > 0 {
							break 'compute std::task::Poll::Ready(Ok(written));
						}
						break 'compute std::task::Poll::Pending;
					}
				}
			}
		};

		// Tee the bytes that were actually accepted to the EXIF parser.
		// poll_tee may register Pending internally (stashed in `pending`); we
		// don't care here — the NEXT poll_write call drains it first (see top).
		if let std::task::Poll::Ready(Ok(n)) = res
			&& n > 0 && let Some(tee) = self.exif_tee.as_mut()
		{
			let _ = tee.poll_tee(cx, &buf[..n]);
		}
		res
	}

	fn poll_close(
		&mut self,
		cx: &mut std::task::Context<'_>,
	) -> std::task::Poll<std::io::Result<()>> {
		if let Some(last_chunk) = self.curr_chunk.take() {
			// we have a chunk to upload
			self.push_upload_next_chunk(last_chunk);
		}

		// First, drain all in-flight chunk uploads.
		loop {
			match self.futures.poll_next_unpin(cx) {
				std::task::Poll::Ready(Some(Ok(_))) => {}
				std::task::Poll::Ready(Some(Err(e))) => {
					return std::task::Poll::Ready(Err(std::io::Error::other(e)));
				}
				std::task::Poll::Ready(None) => break,
				std::task::Poll::Pending => {
					return std::task::Poll::Pending;
				}
			}
		}

		// All chunks are uploaded — deliver any bytes the throttle was still holding back so the
		// final progress update isn't dropped. Idempotent, so a re-poll after this point is safe.
		if let Some(progress) = &self.progress {
			progress.flush();
		}

		// Then finalize EXIF (if active) so its result is applied to final_times
		// before the writer transitions to the Waiting state.
		if let Some(tee) = self.exif_tee.as_mut() {
			match tee.poll_finalize(cx) {
				std::task::Poll::Ready(res) => {
					match res {
						Ok((c, m)) => {
							if self.final_times.is_none() {
								self.final_times = Some((c, m));
							}
						}
						Err(e) => {
							// we don't fail the whole upload if EXIF parsing fails, so just log the error and continue without the EXIF times
							// In the future, ideally callback to something higher up to show the error to the user, but for now just log it
							tracing::debug!("EXIF tee failed to finalize: {e}");
						}
					}
				}
				std::task::Poll::Pending => return std::task::Poll::Pending,
			}
		}

		std::task::Poll::Ready(Ok(()))
	}
}

struct FileWriterWaitingForDriveLockState<'a> {
	file: Arc<BaseFile>,
	lock_and_confirm_upload_future: MaybeSendBoxFuture<'a, Result<(Arc<ResourceLock>, ()), Error>>,
	hash: Blake3Hash,
	remote_file_info: RemoteFileInfo,
	upload_key: Arc<String>,
	written: u64,
	num_chunks: u64,
	final_times: Option<(DateTime<Utc>, DateTime<Utc>)>,
	client: &'a Client,
}

impl<'a> FileWriterWaitingForDriveLockState<'a> {
	fn into_completing_state(
		self,
		drive_lock: Arc<ResourceLock>,
	) -> Result<FileWriterCompletingState<'a>, Error> {
		let completion = UploadCompletion {
			written: self.written,
			num_chunks: self.num_chunks,
			hash: self.hash,
			final_times: self
				.final_times
				.unwrap_or_else(|| (self.file.created(), self.file.last_modified())),
		};
		let client = self.client;
		let file = self.file.clone();
		let upload_key = self.upload_key.clone();
		let future: MaybeSendBoxFuture<'a, Result<(UploadResponse, Arc<ResourceLock>), Error>> =
			Box::pin(async move {
				// The lock was waited for together with the confirm callback, which can take
				// long. Nothing was read under it, so a lost lease is taken again rather than
				// the uploaded chunks thrown away.
				let drive_lock = confirm_or_retake(&drive_lock, || client.lock_drive())
					.await?
					.unwrap_or(drive_lock);
				let response = complete_upload(client, &file, &upload_key, completion).await?;
				Ok((response, drive_lock))
			});

		Ok(FileWriterCompletingState {
			file: self.file,
			future,
			completion,
			remote_file_info: self.remote_file_info,
			client: self.client,
		})
	}

	fn poll_close(
		&mut self,
		cx: &mut std::task::Context<'_>,
	) -> std::task::Poll<std::io::Result<Arc<ResourceLock>>> {
		match self.lock_and_confirm_upload_future.poll_unpin(cx) {
			std::task::Poll::Ready(Ok((res, _))) => std::task::Poll::Ready(Ok(res)),
			std::task::Poll::Ready(Err(e)) => std::task::Poll::Ready(Err(std::io::Error::other(e))),
			std::task::Poll::Pending => std::task::Poll::Pending,
		}
	}
}

/// What registering an upload answers.
type UploadResponse = filen_types::api::v3::upload::empty::Response;

struct FileWriterCompletingState<'a> {
	file: Arc<BaseFile>,
	/// Registers the upload, under a drive lock it hands on to the finalizing state.
	future: MaybeSendBoxFuture<'a, Result<(UploadResponse, Arc<ResourceLock>), Error>>,
	completion: UploadCompletion,
	remote_file_info: RemoteFileInfo,
	client: &'a Client,
}

impl<'a> FileWriterCompletingState<'a> {
	fn poll_close(
		&mut self,
		cx: &mut std::task::Context<'_>,
	) -> std::task::Poll<std::io::Result<(UploadResponse, Arc<ResourceLock>)>> {
		match self.future.poll_unpin(cx) {
			std::task::Poll::Ready(Err(e)) => std::task::Poll::Ready(Err(std::io::Error::other(e))),
			std::task::Poll::Ready(Ok(registered)) => std::task::Poll::Ready(Ok(registered)),
			std::task::Poll::Pending => std::task::Poll::Pending,
		}
	}

	fn into_finalizing_state(
		self,
		response: UploadResponse,
		drive_lock: Arc<ResourceLock>,
	) -> FileWriterFinalizingState<'a> {
		let file = Arc::try_unwrap(self.file).unwrap_or_else(|arc| (*arc).clone());
		let file = Arc::new(remote_file_from_upload(
			file,
			response,
			self.remote_file_info,
			self.completion,
		));
		let futures: FuturesUnordered<MaybeSendBoxFuture<'a, Result<(), Error>>> =
			FuturesUnordered::new();

		let temp_file = file.clone();
		// The future owns the drive lock, so the lock is held until the finalizing API calls
		// complete; dropping it early would release it mid-finalize. See `ResourceLock`'s `Drop`.
		futures.push(Box::pin(async move {
			self.client
				.update_item_with_maybe_connected_parent(
					&drive_lock,
					NonRootItemType::File(Cow::Borrowed(temp_file.as_ref())),
					ParentTargets::All,
				)
				.await?;
			Ok(())
		}) as MaybeSendBoxFuture<'a, Result<(), Error>>);
		FileWriterFinalizingState { file, futures }
	}
}

struct FileWriterFinalizingState<'a> {
	file: Arc<RemoteFile>,
	futures: FuturesUnordered<MaybeSendBoxFuture<'a, Result<(), Error>>>,
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
enum FileWriterState<'a, F, Fut>
where
	F: FnOnce(Blake3Hash, u64) -> Fut + MaybeSend + 'a,
{
	Uploading(FileWriterUploadingState<'a, F, Fut>),
	WaitingForDriveLock(FileWriterWaitingForDriveLockState<'a>),
	Completing(FileWriterCompletingState<'a>),
	Finalizing(FileWriterFinalizingState<'a>),
	Complete(FileWriterCompleteState),
	Error(&'a str),
}

impl<'a, F, Fut> FileWriterState<'a, F, Fut>
where
	F: FnOnce(Blake3Hash, u64) -> Fut + MaybeSend + 'a,
	Fut: Future<Output = Result<(), Error>> + MaybeSend + 'a,
{
	#[allow(clippy::too_many_arguments)]
	fn new(
		file: Arc<BaseFile>,
		client: &'a Client,
		callback: Option<MaybeSendCallback<'a, u64>>,
		size: Option<u64>,
		confirm_upload_callback: Option<F>,
		exif_tee: Option<ExifTeeState>,
	) -> Self {
		FileWriterState::Uploading(FileWriterUploadingState {
			file,
			progress: ThrottledProgress::new(callback),
			futures: FuturesUnordered::new(),
			curr_chunk: None,
			next_chunk_idx: 0,
			written: 0,
			confirm_upload_callback,
			hasher: blake3::Hasher::new(),
			remote_file_info: Arc::new(OnceLock::new()),
			upload_key: Arc::new(crypto::shared::generate_random_base64_values(
				32,
				&mut rand::rng(),
			)),
			final_times: None,
			exif_tee,
			client,
			alloc_future: None,
			allocated_chunks: Vec::new(),
			size,
		})
	}

	fn take_with_err(&mut self, error: &'a str) -> FileWriterState<'a, F, Fut> {
		std::mem::replace(self, FileWriterState::Error(error))
	}
}

impl<'a, F, Fut> FileWriterState<'a, F, Fut>
where
	F: FnOnce(Blake3Hash, u64) -> Fut + MaybeSend + 'a,
{
	/// The uploading phase, or the error writing in any later phase answers with.
	fn uploading(&mut self) -> std::io::Result<&mut FileWriterUploadingState<'a, F, Fut>> {
		match self {
			FileWriterState::Uploading(uploading) => Ok(uploading),
			FileWriterState::WaitingForDriveLock(_)
			| FileWriterState::Completing(_)
			| FileWriterState::Finalizing(_)
			| FileWriterState::Complete(_) => Err(std::io::Error::other("Cannot write to a completed file")),
			FileWriterState::Error(e) => Err(std::io::Error::other(*e)),
		}
	}
}

pub struct FileWriter<'a, F, Fut>
where
	F: FnOnce(Blake3Hash, u64) -> Fut + MaybeSend + 'a,
{
	state: FileWriterState<'a, F, Fut>,
}

pub type FileWriterDefault<'a> = FileWriter<'a, fn(Blake3Hash, u64) -> DummyFuture, DummyFuture>;

impl<'a, F, Fut> FileWriter<'a, F, Fut>
where
	F: FnOnce(Blake3Hash, u64) -> Fut + MaybeSend + 'a,
	Fut: Future<Output = Result<(), Error>> + MaybeSend + 'a,
{
	#[allow(clippy::too_many_arguments)]
	pub(crate) fn new(
		file: Arc<BaseFile>,
		client: &'a Client,
		callback: Option<MaybeSendCallback<'a, u64>>,
		size: Option<u64>,
		confirm_upload_callback: Option<F>,
		exif_tee: Option<ExifTeeState>,
	) -> Self {
		Self {
			state: FileWriterState::new(
				file,
				client,
				callback,
				size,
				confirm_upload_callback,
				exif_tee,
			),
		}
	}

	pub fn into_remote_file(self) -> Option<RemoteFile> {
		match self.state {
			FileWriterState::Complete(complete) => Some(complete.file),
			_ => None,
		}
	}
}

// Chunk by chunk, for a caller that reads the file's bytes into its chunks itself instead of
// writing them through `AsyncWrite`. A file is written one way or the other, not both.
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
impl<'a, F, Fut> FileWriter<'a, F, Fut>
where
	F: FnOnce(Blake3Hash, u64) -> Fut + MaybeSend + 'a,
{
	/// The file's next chunk, empty: one an earlier chunk's upload is done with, or a new one
	/// once the client's memory budget grants it. Fill it to its [`Chunk::capacity`] or to the
	/// file's end, whichever comes first, and hand it back with [`Self::submit_chunk`].
	///
	/// Fails once the file is complete: a chunk with room left was submitted, or the declared
	/// size is reached.
	pub(crate) fn poll_next_chunk(
		&mut self,
		cx: &mut std::task::Context<'_>,
	) -> std::task::Poll<std::io::Result<Chunk<'a>>> {
		match self.state.uploading() {
			Ok(uploading) => uploading.poll_next_chunk(cx),
			Err(e) => std::task::Poll::Ready(Err(e)),
		}
	}

	/// Uploads `chunk`, filled from [`Self::poll_next_chunk`], as the file's next chunk: hashed,
	/// teed to the EXIF parser and reported as progress like the bytes [`AsyncWrite`] takes.
	/// A chunk with room left is the file's last and is uploaded on close; an empty one is
	/// dropped.
	pub(crate) fn submit_chunk(&mut self, chunk: Chunk<'a>) -> std::io::Result<()> {
		self.state.uploading()?.submit_chunk(chunk);
		Ok(())
	}
}

impl<'a, F, Fut> AsyncWrite for FileWriter<'a, F, Fut>
where
	F: FnOnce(Blake3Hash, u64) -> Fut + MaybeSend + 'a,
	Fut: Future<Output = Result<(), Error>> + MaybeSend + 'a,
	Self: Unpin,
{
	fn poll_write(
		mut self: std::pin::Pin<&mut Self>,
		cx: &mut std::task::Context<'_>,
		buf: &[u8],
	) -> std::task::Poll<std::io::Result<usize>> {
		match self.state.uploading() {
			Ok(uploading) => uploading.poll_write(cx, buf),
			Err(e) => std::task::Poll::Ready(Err(e)),
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
						return std::task::Poll::Ready(Err(e));
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
					return std::task::Poll::Ready(Err(e));
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
				std::task::Poll::Ready(Ok((response, drive_lock))) => FileWriterState::Finalizing(
					completing.into_finalizing_state(response, drive_lock),
				),
				std::task::Poll::Ready(Err(e)) => {
					return std::task::Poll::Ready(Err(e));
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

#[cfg(test)]
mod tests {
	use chrono::{TimeZone, Utc};
	use filen_types::{
		api::v3::upload::empty::Response,
		crypto::Blake3Hash,
		fs::{ParentUuid, StableUuid, Uuid},
	};

	use super::{
		RemoteFileInfo, UploadCompletion, remote_file_from_upload, would_exceed_known_size,
	};
	use crate::{
		crypto::{file::FileKey, shared::CreateRandom, v3::EncryptionKey},
		fs::{
			HasUUID,
			file::{
				BaseFile, RootFile,
				meta::FileMeta,
				traits::{HasFileInfo, HasRemoteFileInfo},
			},
			name::ValidatedName,
		},
	};

	#[test]
	fn remote_file_from_upload_takes_server_fields_and_completion_metadata() {
		let key = FileKey::V3(EncryptionKey::generate());
		let parent = Uuid::new_v4();
		let created = Utc.with_ymd_and_hms(2020, 1, 2, 3, 4, 5).unwrap();
		let modified = Utc.with_ymd_and_hms(2021, 6, 7, 8, 9, 10).unwrap();
		let base = BaseFile {
			root: RootFile {
				uuid: Uuid::new_v4(),
				name: ValidatedName::try_from("a.txt").unwrap(),
				mime: "text/plain".to_string(),
				key,
				created: Utc::now(),
				modified: Utc::now(),
			},
			parent,
		};
		let uuid = Uuid::new_v4();
		let response = Response {
			uuid,
			stable_uuid: StableUuid::new_for_test(uuid),
			chunks: 2,
			size: 1_500_000,
			timestamp: Utc.with_ymd_and_hms(2022, 1, 1, 0, 0, 0).unwrap(),
		};
		let hash = Blake3Hash::from(blake3::hash(b"plaintext"));
		let completion = UploadCompletion {
			written: 1_500_000,
			num_chunks: 2,
			hash,
			final_times: (created, modified),
		};
		let info = RemoteFileInfo {
			region: "de-2".to_string(),
			bucket: "bucket".to_string(),
		};

		let file = remote_file_from_upload(base, response.clone(), info, completion);

		assert_eq!(file.uuid(), uuid);
		assert_eq!(file.stable_uuid, response.stable_uuid);
		assert_eq!(file.parent, ParentUuid::Uuid(parent));
		assert_eq!(file.size(), 1_500_000);
		assert_eq!(file.chunks, 2);
		assert_eq!(file.region(), "de-2");
		assert_eq!(file.bucket(), "bucket");
		assert_eq!(file.timestamp, response.timestamp);
		assert!(!file.favorited);
		assert_eq!(file.created(), Some(created));
		assert_eq!(file.last_modified(), Some(modified));
		assert_eq!(file.hash(), Some(hash));
		assert_eq!(file.key(), Some(&key));
		assert_eq!(file.mime(), Some("text/plain"));
		assert!(matches!(file.meta, FileMeta::Decoded(_)));
	}

	#[test]
	fn unbounded_size_never_overflows() {
		assert!(!would_exceed_known_size(None, 0, 4096));
		assert!(!would_exceed_known_size(None, u64::MAX, 1));
	}

	#[test]
	fn within_declared_size_is_not_overflow() {
		// Room remains before the declared size.
		assert!(!would_exceed_known_size(Some(100), 40, 60));
		// Exactly reached the declared size with nothing more to write (final flush/close).
		assert!(!would_exceed_known_size(Some(100), 100, 0));
	}

	#[test]
	fn bytes_past_declared_size_overflow() {
		// The declared size is reached but the reader still has bytes.
		assert!(would_exceed_known_size(Some(100), 100, 1));
		// Already past the declared size (defensive).
		assert!(would_exceed_known_size(Some(100), 101, 1));
		// A zero declared size with any incoming byte is an overflow.
		assert!(would_exceed_known_size(Some(0), 0, 1));
	}
}
