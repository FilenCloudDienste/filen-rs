use std::{
	io::{Cursor, Read},
	num::NonZeroU32,
};

use futures::{StreamExt, future::BoxFuture, stream::FuturesOrdered};

use crate::{
	api,
	auth::Client,
	consts::{FILE_CHUNK_SIZE, FILE_CHUNK_SIZE_EXTRA},
	crypto::shared::DataCrypter,
	error::Error,
};

use super::{chunk::Chunk, traits::File};

pub(super) struct FileReader<'a> {
	file: &'a dyn File,
	client: &'a Client,
	index: u64,
	limit: u64,
	next_chunk_idx: u64,
	curr_chunk: Option<Cursor<Chunk<'a>>>,
	futures: FuturesOrdered<BoxFuture<'a, Result<Chunk<'a>, Error>>>,
	allocate_chunk_future: Option<BoxFuture<'a, Chunk<'a>>>,
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
			allocate_chunk_future: None,
		};

		// allocate memory and prefetch chunks
		while let Some(chunk) = new.try_allocate_next_chunk() {
			new.push_fetch_next_chunk(chunk);
		}
		new.allocate_chunk_future = new.allocate_next_chunk();

		new
	}

	fn next_chunk_size(&self) -> Option<NonZeroU32> {
		if self.file.chunks() == 0 {
			return None;
		}
		if self.next_chunk_idx < self.file.chunks() - 1 {
			Some(FILE_CHUNK_SIZE.saturating_add(FILE_CHUNK_SIZE_EXTRA.get()))
		} else if self.next_chunk_idx == self.file.chunks() - 1 {
			let size: u64 = self.file.size()
				- (self.next_chunk_idx * u64::from(FILE_CHUNK_SIZE.get()))
				+ u64::from(FILE_CHUNK_SIZE_EXTRA.get());
			let size: u32 = size.try_into().unwrap();
			NonZeroU32::new(size)
		} else {
			None
		}
	}

	fn try_allocate_next_chunk(&self) -> Option<Chunk<'a>> {
		let chunk_size = self.next_chunk_size()?;

		Chunk::try_acquire(chunk_size, self.client)
	}

	fn allocate_next_chunk(&self) -> Option<BoxFuture<'a, Chunk<'a>>> {
		let chunk_size = self.next_chunk_size()?;
		Some(Box::pin(Chunk::acquire(chunk_size, self.client)) as BoxFuture<'a, Chunk<'a>>)
	}

	/// Pushes the future to fetch the next chunk.
	///
	/// Requires that `out_data` have the necessary capacity to store the entire chunk returned from the server
	fn push_fetch_next_chunk(&mut self, mut out_data: Chunk<'a>) {
		if self.file.chunks() <= self.next_chunk_idx {
			return;
		}
		let chunk_idx = self.next_chunk_idx;
		self.next_chunk_idx += 1;
		let client = self.client;
		let file = self.file;
		self.futures.push_back(Box::pin(async move {
			api::download::download_file_chunk(client.client(), file, chunk_idx, out_data.as_mut())
				.await?;
			file.key()
				.ok_or(Error::MetadataWasNotDecrypted)?
				.decrypt_data(out_data.as_mut())?;
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
				if (TryInto::<usize>::try_into(cursor.position()).unwrap())
					< cursor.get_ref().as_ref().len()
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
		// first see if our allocation future is ready
		let mut should_pend = false;
		if let Some(mut fut) = self.allocate_chunk_future.take() {
			match fut.as_mut().poll(cx) {
				std::task::Poll::Ready(chunk) => {
					// we have a new chunk, set it to curr_chunk
					self.push_fetch_next_chunk(chunk);
					self.allocate_chunk_future = self.allocate_next_chunk();
				}
				std::task::Poll::Pending => {
					// allocation is still pending, we can't read anything yet
					if self.next_chunk_size().is_some() {
						// we have more chunks to allocate, so we put the future back
						self.allocate_chunk_future = Some(fut);
						should_pend = true;
					}
					// if we don't have more chunks to allocate, we can drop the future
				}
			}
		}

		// then see if we have a stored chunk
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
					if should_pend && read == 0 {
						// if we were waiting for allocation and we haven't read anything,
						// we need to pend
						return std::task::Poll::Pending;
					}
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
