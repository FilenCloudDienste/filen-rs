use std::io::{Cursor, Read};

use futures::{StreamExt, future::BoxFuture, stream::FuturesOrdered};

use crate::{
	api,
	auth::Client,
	consts::{
		CHUNK_SIZE, CHUNK_SIZE_U64, DEFAULT_MAX_DOWNLOAD_THREADS_PER_FILE, FILE_CHUNK_SIZE_EXTRA,
	},
	crypto::shared::DataCrypter,
	error::Error,
};

use super::traits::File;

pub(super) struct FileReader<'a> {
	file: &'a dyn File,
	client: &'a Client,
	index: u64,
	limit: u64,
	next_chunk_idx: u64,
	curr_chunk: Option<Cursor<Vec<u8>>>,
	futures: FuturesOrdered<BoxFuture<'a, Result<Vec<u8>, Error>>>,
}

impl<'a> FileReader<'a> {
	pub(crate) fn new(file: &'a dyn File, client: &'a Client) -> Self {
		let size = file.size(); // adjustable in the future
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
		let client = self.client;
		let file = self.file;
		self.futures.push_back(Box::pin(async move {
			api::download::download_file_chunk(client.client(), file, chunk_idx, &mut out_data)
				.await?;
			file.key().decrypt_data(&mut out_data)?;
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
					return std::task::Poll::Ready(Err(std::io::Error::other(e)));
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
