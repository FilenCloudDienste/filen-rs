use std::{
	borrow::Cow,
	fmt::{Debug, Display},
	io::{Cursor, Read},
	str::FromStr,
	sync::Arc,
};

use chrono::{DateTime, Utc};
use filen_types::crypto::Sha512Hash;
use futures::{AsyncRead, StreamExt, stream::FuturesOrdered};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
	api::{self},
	auth::Client,
	consts::{
		CHUNK_SIZE, CHUNK_SIZE_U64, DEFAULT_MAX_DOWNLOAD_THREADS_PER_FILE, FILE_CHUNK_SIZE_EXTRA,
	},
	crypto::{self, shared::DataCrypter},
	error::Error,
};

use super::HasMeta;

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
