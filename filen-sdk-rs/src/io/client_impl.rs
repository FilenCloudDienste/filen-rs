use std::sync::Arc;

use futures::{AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::{
	Error,
	auth::Client,
	fs::file::{BaseFile, RemoteFile, traits::File},
	util::MaybeSendCallback,
};

const IO_BUFFER_SIZE: usize = 1024 * 64; // 64 KiB

impl Client {
	pub async fn download_file_to_writer<'a, T>(
		&'a self,
		file: &'a dyn File,
		writer: &mut T,
		callback: Option<MaybeSendCallback<'a, u64>>,
	) -> Result<(), Error>
	where
		T: 'a + AsyncWrite + Unpin,
	{
		self.download_file_to_writer_for_range(file, writer, callback, 0, file.size())
			.await
	}

	pub async fn download_file_to_writer_for_range<'a, T>(
		&'a self,
		file: &'a dyn File,
		writer: &mut T,
		callback: Option<MaybeSendCallback<'a, u64>>,
		start: u64,
		end: u64,
	) -> Result<(), Error>
	where
		T: 'a + AsyncWrite + Unpin,
	{
		let mut reader = self.get_file_reader_for_range(file, start, end);
		let mut buffer = [0u8; IO_BUFFER_SIZE];
		loop {
			let bytes_read = reader.read(&mut buffer).await?;
			if bytes_read == 0 {
				break;
			}
			writer.write_all(&buffer[..bytes_read]).await?;
			if let Some(callback) = &callback {
				callback(bytes_read as u64);
			}
		}
		writer.close().await?;
		Ok(())
	}

	// this could be optimized to avoid allocations by downloading directly to the writer
	// would need to allocate a buffer of file.size() + FILE_CHUNK_SIZE_EXTRA
	// and download to it sequentially, decrypting in place
	// and finally shrinking the buffer to file.size()
	pub async fn download_file(&self, file: &dyn File) -> Result<Vec<u8>, Error> {
		let mut writer = Vec::with_capacity(file.size() as usize);
		self.download_file_to_writer(file, &mut writer, None)
			.await?;
		Ok(writer)
	}

	pub async fn upload_file_from_reader<'a, T>(
		&'a self,
		base_file: Arc<BaseFile>,
		reader: &mut T,
		callback: Option<MaybeSendCallback<'a, u64>>,
		known_size: Option<u64>,
	) -> Result<RemoteFile, Error>
	where
		T: 'a + AsyncReadExt + Unpin,
	{
		let mut writer = self.inner_get_file_writer(base_file, callback, known_size)?;
		let mut buffer = [0u8; IO_BUFFER_SIZE];
		loop {
			let bytes_read = reader.read(&mut buffer).await?;
			if bytes_read == 0 {
				break;
			}
			writer.write_all(&buffer[..bytes_read]).await?;
		}
		writer.close().await?;
		// SAFETY: conversion will always succeed because we called close on the writer
		Ok(writer.into_remote_file().unwrap())
	}

	pub async fn upload_file(&self, file: Arc<BaseFile>, data: &[u8]) -> Result<RemoteFile, Error> {
		let mut reader = data;
		self.upload_file_from_reader(
			file,
			&mut reader,
			None,
			Some(data.len().try_into().unwrap()),
		)
		.await
	}
}
