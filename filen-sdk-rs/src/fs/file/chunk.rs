use std::{io::Write, mem::ManuallyDrop, num::NonZeroU32};

use log::debug;
use tokio::sync::SemaphorePermit;

use crate::{auth::Client, consts::FILE_CHUNK_SIZE_EXTRA_USIZE};

pub(crate) struct Chunk<'a> {
	data: Vec<u8>,
	permits: SemaphorePermit<'a>,
}

impl AsRef<[u8]> for Chunk<'_> {
	fn as_ref(&self) -> &[u8] {
		&self.data
	}
}

impl AsMut<[u8]> for Chunk<'_> {
	fn as_mut(&mut self) -> &mut [u8] {
		&mut self.data
	}
}

impl AsMut<Vec<u8>> for Chunk<'_> {
	fn as_mut(&mut self) -> &mut Vec<u8> {
		&mut self.data
	}
}

impl<'a> Chunk<'a> {
	pub fn try_acquire(chunk_size: NonZeroU32, client: &'a Client) -> Option<Chunk<'a>> {
		client
			.memory_semaphore
			.try_acquire_many(chunk_size.get())
			.ok()
			.map(|permits| {
				debug!(
					"try_acquire: Acquired chunk with {} permits",
					permits.num_permits()
				);
				Chunk {
					// SAFETY: this can only fail if usize < u32, which is not the case in any of our targets.
					data: Vec::with_capacity(unsafe {
						chunk_size.get().try_into().unwrap_unchecked()
					}),
					permits,
				}
			})
	}

	pub async fn acquire(chunk_size: NonZeroU32, client: &'a Client) -> Chunk<'a> {
		let permits = client
			.memory_semaphore
			.acquire_many(chunk_size.get())
			.await
			.unwrap();
		debug!(
			"acquire: Acquired chunk with {} permits",
			permits.num_permits()
		);
		Chunk {
			// SAFETY: this can only fail if usize < u32, which is not the case in any of our targets.
			data: Vec::with_capacity(unsafe { chunk_size.get().try_into().unwrap_unchecked() }),
			permits,
		}
	}

	pub fn capacity(&self) -> usize {
		self.data.capacity() - FILE_CHUNK_SIZE_EXTRA_USIZE
	}

	pub fn len(&self) -> usize {
		self.data.len()
	}

	pub fn from_parts(data: Vec<u8>, permit: SemaphorePermit<'a>) -> Chunk<'a> {
		Chunk {
			data,
			permits: permit,
		}
	}

	pub fn into_parts(self) -> (Vec<u8>, SemaphorePermit<'a>) {
		let manual = ManuallyDrop::new(self);

		unsafe {
			let data = std::ptr::read(&manual.data);
			let permits = std::ptr::read(&manual.permits);
			(data, permits)
		}
	}
}

impl Write for Chunk<'_> {
	fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
		let max_write = self
			.capacity()
			.saturating_sub(self.data.len())
			.min(buf.len());
		self.data.write(&buf[..max_write])
	}

	fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()> {
		let written = self.write(buf)?;
		if written < buf.len() {
			return Err(std::io::Error::new(
				std::io::ErrorKind::WriteZero,
				"Not enough space in chunk",
			));
		}
		Ok(())
	}

	fn flush(&mut self) -> std::io::Result<()> {
		self.data.flush()
	}
}

impl Drop for Chunk<'_> {
	fn drop(&mut self) {
		// note, this not getting printed doesn't mean the permits aren't released
		// they might have been dropped after being moved out with into_parts
		debug!(
			"Releasing chunk with {} permits",
			self.permits.num_permits()
		);
	}
}
