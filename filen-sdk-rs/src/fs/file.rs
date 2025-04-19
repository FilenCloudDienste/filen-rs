use std::{
	borrow::Cow,
	fmt::{Debug, Display},
	io::{Cursor, Read, Write},
	str::FromStr,
	sync::{Arc, OnceLock},
};

use chrono::{DateTime, SubsecRound, Utc};
use filen_types::crypto::Sha512Hash;
use futures::{
	AsyncRead, AsyncWrite, FutureExt, StreamExt,
	stream::{FuturesOrdered, FuturesUnordered},
};
use serde::{Deserialize, Serialize};
use sha2::Digest;
use uuid::Uuid;

use crate::{
	api::{self},
	auth::Client,
	consts::{
		CHUNK_SIZE, CHUNK_SIZE_U64, DEFAULT_MAX_DOWNLOAD_THREADS_PER_FILE,
		DEFAULT_MAX_UPLOAD_THREADS_PER_FILE, FILE_CHUNK_SIZE_EXTRA,
	},
	crypto::{
		self,
		shared::{DataCrypter, MetaCrypter},
	},
	error::Error,
};

use super::{HasContents, HasMeta};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileKey {
	V2(crypto::v2::FileKey),
	V3(crypto::v3::EncryptionKey),
}

impl Display for FileKey {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			FileKey::V2(key) => key.fmt(f),
			FileKey::V3(key) => key.fmt(f),
		}
	}
}

impl Serialize for FileKey {
	fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: serde::Serializer,
	{
		match self {
			FileKey::V2(key) => key.serialize(serializer),
			FileKey::V3(key) => key.serialize(serializer),
		}
	}
}

impl<'de> Deserialize<'de> for FileKey {
	fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
	where
		D: serde::Deserializer<'de>,
	{
		let key = String::deserialize(deserializer)?;
		match key.len() {
			32 => Ok(FileKey::V2(
				crypto::v2::FileKey::from_str(&key).map_err(serde::de::Error::custom)?,
			)),
			64 => Ok(FileKey::V3(
				crypto::v3::EncryptionKey::from_str(&key).map_err(serde::de::Error::custom)?,
			)),
			_ => Err(serde::de::Error::custom(format!(
				"Invalid key length: {}",
				key.len()
			))),
		}
	}
}

impl FromStr for FileKey {
	type Err = crypto::error::ConversionError;
	fn from_str(key: &str) -> Result<Self, Self::Err> {
		if key.len() == 32 {
			Ok(FileKey::V2(crypto::v2::FileKey::from_str(key)?))
		} else if key.len() == 64 {
			Ok(FileKey::V3(crypto::v3::EncryptionKey::from_str(key)?))
		} else {
			Err(crypto::error::ConversionError::InvalidStringLength(
				key.len(),
				32,
			))
		}
	}
}

impl crypto::shared::DataCrypter for FileKey {
	fn encrypt_data(&self, data: &mut Vec<u8>) -> Result<(), crypto::error::ConversionError> {
		match self {
			FileKey::V2(key) => key.encrypt_data(data),
			FileKey::V3(key) => key.encrypt_data(data),
		}
	}
	fn decrypt_data(&self, data: &mut Vec<u8>) -> Result<(), crypto::error::ConversionError> {
		match self {
			FileKey::V2(key) => key.decrypt_data(data),
			FileKey::V3(key) => key.decrypt_data(data),
		}
	}
}

pub struct FileBuilder {
	uuid: Uuid,
	key: FileKey,

	name: String,
	parent: Uuid,

	mime: Option<String>,
	created: Option<DateTime<Utc>>,
	modified: Option<DateTime<Utc>>,
}

impl FileBuilder {
	pub fn new(name: impl Into<String>, parent: impl HasContents, client: &Client) -> Self {
		Self {
			uuid: Uuid::new_v4(),
			name: name.into(),
			parent: parent.uuid(),
			key: client.make_file_key(),
			mime: None,
			created: None,
			modified: None,
		}
	}

	pub fn mime(mut self, mime: String) -> Self {
		self.mime = Some(mime);
		self
	}

	pub fn created(mut self, created: DateTime<Utc>) -> Self {
		self.created = Some(created);
		self
	}

	pub fn modified(mut self, modified: DateTime<Utc>) -> Self {
		self.modified = Some(modified);
		self
	}

	pub fn key(mut self, key: FileKey) -> Self {
		self.key = key;
		self
	}

	/// Should not be used outside of testing
	pub fn uuid(mut self, uuid: Uuid) -> Self {
		self.uuid = uuid;
		self
	}

	pub fn build(self) -> File {
		File {
			uuid: self.uuid,
			parent: self.parent,
			mime: self.mime.unwrap_or_else(|| {
				mime_guess::from_ext(self.name.rsplit('.').next().unwrap_or_else(|| &self.name))
					.first_or_octet_stream()
					.to_string()
			}),
			name: self.name,
			key: self.key,
			created: self.created.unwrap_or_else(Utc::now).round_subsecs(3),
			modified: self.modified.unwrap_or_else(Utc::now).round_subsecs(3),
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct File {
	uuid: Uuid,
	name: String,
	parent: Uuid,

	mime: String,
	key: FileKey,
	created: DateTime<Utc>,
	modified: DateTime<Utc>,
}

impl File {
	pub fn uuid(&self) -> Uuid {
		self.uuid
	}

	pub fn name(&self) -> &str {
		&self.name
	}

	pub fn parent(&self) -> Uuid {
		self.parent
	}

	pub fn key(&self) -> &FileKey {
		&self.key
	}

	pub fn into_writer<'a>(self, client: Arc<Client>) -> FileWriter<'a> {
		FileWriter::new(Arc::new(self), client)
	}

	pub fn get_writer<'a>(self: Arc<Self>, client: Arc<Client>) -> FileWriter<'a> {
		FileWriter::new(self, client)
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteFile {
	file: File,
	size: u64,
	favorited: bool,
	region: String,
	bucket: String,
	chunks: u64,
	hash: Option<Sha512Hash>,
}

impl HasMeta for &RemoteFile {
	fn name(&self) -> &str {
		&self.file.name
	}

	fn meta(
		&self,
		crypter: impl crypto::shared::MetaCrypter,
	) -> Result<filen_types::crypto::EncryptedString, crypto::error::ConversionError> {
		// SAFETY if this fails, I want it to panic
		// as this is a logic error
		let string = serde_json::to_string(&FileMeta {
			name: Cow::Borrowed(&self.file.name),
			size: self.size,
			mime: Cow::Borrowed(&self.file.mime),
			key: Cow::Borrowed(&self.file.key),
			created: Some(self.file.created),
			last_modified: self.file.modified,
			hash: self.hash,
		})
		.unwrap();
		crypter.encrypt_meta(&string)
	}
}

impl RemoteFile {
	pub fn from_encrypted(
		file: filen_types::api::v3::dir::content::File,
		decrypter: impl crypto::shared::MetaCrypter,
	) -> Result<Self, Error> {
		let meta = FileMeta::from_encrypted(&file.metadata, decrypter)?;
		Ok(Self {
			file: File {
				name: meta.name.into_owned(),
				uuid: file.uuid,
				parent: file.parent,
				mime: meta.mime.into_owned(),
				key: meta.key.into_owned(),
				created: meta.created.unwrap_or_default(),
				modified: meta.last_modified,
			},
			size: file.size,
			favorited: file.favorited,
			region: file.region,
			bucket: file.bucket,
			chunks: file.chunks,
			hash: meta.hash,
		})
	}

	pub fn name(&self) -> &str {
		&self.file.name
	}

	pub fn region(&self) -> &str {
		&self.region
	}

	pub fn bucket(&self) -> &str {
		&self.bucket
	}

	pub fn chunks(&self) -> u64 {
		self.chunks
	}

	pub fn uuid(&self) -> Uuid {
		self.file.uuid
	}

	pub fn size(&self) -> u64 {
		self.size
	}

	pub fn inner_file(&self) -> &File {
		&self.file
	}

	pub fn into_reader(self, client: Arc<Client>) -> impl AsyncRead {
		FileReader::new(Arc::new(self), client)
	}

	pub fn get_reader(self: Arc<Self>, client: Arc<Client>) -> impl AsyncRead {
		FileReader::new(self, client)
	}
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FileMeta<'a> {
	name: Cow<'a, str>,
	size: u64,
	mime: Cow<'a, str>,
	key: Cow<'a, FileKey>,
	#[serde(with = "chrono::serde::ts_milliseconds")]
	last_modified: DateTime<Utc>,
	#[serde(with = "filen_types::serde::time::optional")]
	#[serde(rename = "creation")]
	#[serde(default)]
	created: Option<DateTime<Utc>>,
	hash: Option<Sha512Hash>,
}

impl FileMeta<'_> {
	fn from_encrypted(
		meta: &filen_types::crypto::EncryptedString,
		decrypter: impl crypto::shared::MetaCrypter,
	) -> Result<Self, Error> {
		let decrypted = decrypter.decrypt_meta(meta)?;
		let meta: FileMeta = serde_json::from_str(&decrypted)?;
		Ok(meta)
	}
}

struct FileReader<'a> {
	file: Arc<RemoteFile>,
	client: Arc<Client>,
	index: u64,
	limit: u64,
	next_chunk_idx: u64,
	curr_chunk: Option<Cursor<Vec<u8>>>,
	futures: FuturesOrdered<futures::future::BoxFuture<'a, Result<Vec<u8>, Error>>>,
}

impl FileReader<'_> {
	pub fn new(file: Arc<RemoteFile>, client: Arc<Client>) -> Self {
		let size = file.size; // adjustable in the future
		let mut new = Self {
			file,
			client,
			index: 0,
			limit: size,
			curr_chunk: None,
			futures: FuturesOrdered::new(),
			next_chunk_idx: 0,
		};

		let num_threads: u64 = Ord::min(DEFAULT_MAX_DOWNLOAD_THREADS_PER_FILE, new.file.chunks());
		if num_threads == 0 {
			// if we have no threads, we can just return
			return new;
		}
		// this should never exceed u32 as DEFAULT_MAX_DOWNLOAD_THREADS_PER_FILE should be relatively low
		let num_threads: usize = num_threads.try_into().unwrap();

		// allocate memory
		let mut chunks: Vec<Vec<u8>> = Vec::with_capacity(num_threads);
		for _ in 0..(num_threads - 1) {
			let chunk = Vec::with_capacity(CHUNK_SIZE + FILE_CHUNK_SIZE_EXTRA);
			chunks.push(chunk);
		}
		if DEFAULT_MAX_DOWNLOAD_THREADS_PER_FILE < new.file.chunks() || size % CHUNK_SIZE_U64 == 0 {
			// if we have more chunks than threads, we need to add a full chunk for the last thread
			let chunk = Vec::with_capacity(CHUNK_SIZE + FILE_CHUNK_SIZE_EXTRA);
			chunks.push(chunk);
		} else {
			// if we have more threads than chunks, we add a smaller chunk for the last thread
			let final_chunk_size: usize = (size % CHUNK_SIZE_U64).try_into().unwrap();
			chunks.push(Vec::with_capacity(final_chunk_size + FILE_CHUNK_SIZE_EXTRA));
		}

		// prefetch chunks
		for chunk in chunks.into_iter() {
			new.push_fetch_next_chunk(chunk);
		}

		new
	}

	/// Pushes the future to fetch the next chunk.
	///
	/// Requires that `out_data` have the necessary capacity to store the entire chunk returned from the server
	fn push_fetch_next_chunk(&mut self, mut out_data: Vec<u8>) {
		if self.file.chunks() <= self.next_chunk_idx {
			return;
		}
		let chunk_idx = self.next_chunk_idx;
		self.next_chunk_idx += 1;
		let client = self.client.clone();
		let file = self.file.clone();
		self.futures.push_back(Box::pin(async move {
			api::download::download_file_chunk(client.client(), &file, chunk_idx, &mut out_data)
				.await?;
			file.file.key.decrypt_data(&mut out_data)?;
			Ok(out_data)
		}));
	}

	/// Reads into `buf` from `self.curr_chunk` and returns the number of bytes read
	/// if `curr_chunk` is `None`, it returns 0
	///
	/// If `curr_chunk` is not `None`, it will read from it and return the number of bytes read.
	/// If the whole chunk was read, it will fetch the next chunk
	fn read_next_chunk(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
		// take the chunk out of curr_chunk
		match self.curr_chunk.take() {
			Some(mut cursor) => {
				let read = cursor.read(buf)?;
				if (TryInto::<usize>::try_into(cursor.position()).unwrap()) < cursor.get_ref().len()
				{
					// didn't read the whole chunk, put it back and return
					self.curr_chunk = Some(cursor);
				} else {
					// read the whole chunk, so we need to fetch the next one
					self.push_fetch_next_chunk(cursor.into_inner());
				}
				Ok(read)
			}
			None => Ok(0),
		}
	}
}

impl futures::io::AsyncRead for FileReader<'_> {
	fn poll_read(
		mut self: std::pin::Pin<&mut Self>,
		cx: &mut std::task::Context<'_>,
		buf: &mut [u8],
	) -> std::task::Poll<std::io::Result<usize>> {
		// first see if we have a stored chunk
		let mut read = self.read_next_chunk(buf)?;
		if read >= buf.len() {
			// we've filled the buffer
			return std::task::Poll::Ready(Ok(read));
		}

		loop {
			// loop through futures
			match self.futures.poll_next_unpin(cx) {
				std::task::Poll::Ready(Some(Ok(data))) => {
					// we have a new chunk, make a cursor and read from it
					self.curr_chunk = Some(Cursor::new(data));
					read += self.read_next_chunk(&mut buf[read..])?;
					if read >= buf.len() {
						// we've filled the buffer
						return std::task::Poll::Ready(Ok(read));
					}
				}
				std::task::Poll::Ready(Some(Err(e))) => {
					return std::task::Poll::Ready(Err(std::io::Error::new(
						std::io::ErrorKind::Other,
						e,
					)));
				}
				std::task::Poll::Ready(None) => {
					return std::task::Poll::Ready(Ok(read));
				}
				std::task::Poll::Pending => {
					if read > 0 {
						// we have read some data, return it
						return std::task::Poll::Ready(Ok(read));
					}
					return std::task::Poll::Pending;
				}
			}
		}
	}
}

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
	file: Arc<File>,
	futures: FuturesUnordered<futures::future::BoxFuture<'a, Result<(), Error>>>,
	curr_chunk: Option<Cursor<Vec<u8>>>,
	next_chunk_idx: u64,
	written: u64,
	hasher: sha2::Sha512,
	remote_file_info: Arc<OnceLock<RemoteFileInfo>>,
	upload_key: Arc<String>,
	max_threads: usize,
	client: Arc<Client>,
}

impl<'a> FileWriterUploadingState<'a> {
	fn push_upload_next_chunk(&mut self, mut out_data: Vec<u8>) {
		let chunk_idx = self.next_chunk_idx;
		self.next_chunk_idx += 1;
		let client = self.client.clone();
		let file = self.file.clone();
		let upload_key = self.upload_key.clone();
		self.hasher.update(&out_data);
		let remote_file_info = self.remote_file_info.clone();
		self.futures.push(Box::pin(async move {
			// encrypt the data
			file.key.encrypt_data(&mut out_data)?;
			// upload the data
			let result = api::v3::upload::upload_file_chunk(
				client.client(),
				&file,
				&upload_key,
				chunk_idx,
				out_data,
			)
			.await?;
			// don't care if this errors because that means another thread set it
			let _ = remote_file_info.set(RemoteFileInfo {
				region: result.region,
				bucket: result.bucket,
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
		if (TryInto::<usize>::try_into(cursor.position()).unwrap()) < cursor.get_ref().len() {
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
			uuid: file.uuid,
			name: self.client.crypter().encrypt_meta(&file.name)?,
			name_hashed: self.client.hash_name(&file.name),
			size: self
				.client
				.crypter()
				.encrypt_meta(&self.written.to_string())?,
			parent: file.parent,
			mime: self.client.crypter().encrypt_meta(&self.file.mime)?,
			metadata: self
				.client
				.crypter()
				.encrypt_meta(&serde_json::to_string(&FileMeta {
					name: Cow::Borrowed(&file.name),
					size: self.written,
					mime: Cow::Borrowed(&file.mime),
					key: Cow::Borrowed(&file.key),
					created: Some(file.created),
					last_modified: file.modified,
					hash: Some(hash.into()),
				})?)?,
			version: self.client.file_encryption_version(),
		};

		let future: futures::future::BoxFuture<
			'a,
			Result<
				filen_types::api::v3::upload::empty::Response,
				filen_types::error::ResponseError,
			>,
		> = if self.written == 0 {
			Box::pin(
				async move { api::v3::upload::empty::post(client.client(), &empty_request).await },
			)
		} else {
			let upload_key = self.upload_key.clone();
			Box::pin(async move {
				api::v3::upload::done::post(
					client.client(),
					&api::v3::upload::done::Request {
						empty_request,
						chunks: self.next_chunk_idx,
						rm: crypto::shared::generate_random_base64_values(32),
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
					return std::task::Poll::Ready(Err(std::io::Error::new(
						std::io::ErrorKind::Other,
						e.to_string(),
					)));
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
					return std::task::Poll::Ready(Err(std::io::Error::new(
						std::io::ErrorKind::Other,
						e.to_string(),
					)));
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
	file: Arc<File>,
	future: futures::future::BoxFuture<
		'a,
		Result<filen_types::api::v3::upload::empty::Response, filen_types::error::ResponseError>,
	>,
	hash: Sha512Hash,
	remote_file_info: RemoteFileInfo,
	client: Arc<Client>,
}

impl FileWriterCompletingState<'_> {
	fn poll_close(
		&mut self,
		cx: &mut std::task::Context<'_>,
	) -> std::task::Poll<std::io::Result<filen_types::api::v3::upload::empty::Response>> {
		match self.future.poll_unpin(cx) {
			std::task::Poll::Ready(Err(e)) => std::task::Poll::Ready(Err(std::io::Error::new(
				std::io::ErrorKind::Other,
				e.to_string(),
			))),
			std::task::Poll::Ready(Ok(response)) => std::task::Poll::Ready(Ok(response)),
			std::task::Poll::Pending => std::task::Poll::Pending,
		}
	}

	fn into_complete_state(
		self,
		response: filen_types::api::v3::upload::empty::Response,
	) -> FileWriterCompleteState {
		FileWriterCompleteState {
			file: RemoteFile {
				file: Arc::try_unwrap(self.file).unwrap_or_else(|arc| (*arc).clone()),
				size: response.size,
				favorited: false,
				region: self.remote_file_info.region,
				bucket: self.remote_file_info.bucket,
				chunks: response.chunks,
				hash: Some(self.hash),
			},
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
	Complete(FileWriterCompleteState),
	Error(&'a str),
}

impl<'a> FileWriterState<'a> {
	fn new(file: Arc<File>, client: Arc<Client>) -> Self {
		FileWriterState::Uploading(FileWriterUploadingState {
			file,
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

impl FileWriter<'_> {
	pub fn new(file: Arc<File>, client: Arc<Client>) -> Self {
		Self {
			state: FileWriterState::new(file, client),
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
			FileWriterState::Completing(_) | FileWriterState::Complete(_) => {
				// we are in the completing state, we can't write anymore
				std::task::Poll::Ready(Err(std::io::Error::new(
					std::io::ErrorKind::Other,
					"Cannot write to a completed file",
				)))
			}
			FileWriterState::Error(e) => {
				std::task::Poll::Ready(Err(std::io::Error::new(std::io::ErrorKind::Other, *e)))
			}
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
							return std::task::Poll::Ready(Err(std::io::Error::new(
								std::io::ErrorKind::Other,
								e,
							)));
						}
					},
					std::task::Poll::Ready(Err(e)) => {
						return std::task::Poll::Ready(Err(std::io::Error::new(
							std::io::ErrorKind::Other,
							e,
						)));
					}
					std::task::Poll::Pending => {
						self.state = FileWriterState::Uploading(uploading);
						return std::task::Poll::Pending;
					}
				}
			}
			state => state,
		};

		// now state cannot be uploading anymore, ideally this would be part of a match

		match state {
			FileWriterState::Completing(mut completing) => match completing.poll_close(cx) {
				std::task::Poll::Ready(Ok(response)) => {
					self.state =
						FileWriterState::Complete(completing.into_complete_state(response));
					std::task::Poll::Ready(Ok(()))
				}
				std::task::Poll::Ready(Err(e)) => std::task::Poll::Ready(Err(e)),
				std::task::Poll::Pending => {
					self.state = FileWriterState::Completing(completing);
					std::task::Poll::Pending
				}
			},
			FileWriterState::Complete(complete) => {
				self.state = FileWriterState::Complete(complete);
				std::task::Poll::Ready(Ok(()))
			}
			FileWriterState::Error(e) => {
				std::task::Poll::Ready(Err(std::io::Error::new(std::io::ErrorKind::Other, e)))
			}
			FileWriterState::Uploading(_) => {
				unreachable!("Should be handled by the first part of this function")
			}
		}
	}
}
