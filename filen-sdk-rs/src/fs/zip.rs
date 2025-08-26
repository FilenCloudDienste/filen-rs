use async_zip::spec::header::{ExtraField, UnknownExtraField};

use crate::fs::file::{RemoteFile, traits::HasFileInfo};

struct ZipExtendedTime {
	modification: Option<u32>,
	creation: Option<u32>,
}

impl ZipExtendedTime {
	fn count(&self) -> u16 {
		u16::from(self.modification.is_some()) + u16::from(self.creation.is_some())
	}

	fn to_extra_data(&self) -> Vec<u8> {
		let mut bytes = Vec::with_capacity(1 + 4 * usize::from(self.count()));
		let mut flags = 0u8;
		if self.modification.is_some() {
			flags |= 0b00000001;
		}
		if self.creation.is_some() {
			flags |= 0b00000100;
		}
		bytes.push(flags);
		if let Some(mod_time) = self.modification {
			bytes.extend_from_slice(&mod_time.to_le_bytes());
		}
		if let Some(cr_time) = self.creation {
			bytes.extend_from_slice(&cr_time.to_le_bytes());
		}
		bytes
	}
}

impl From<ZipExtendedTime> for UnknownExtraField {
	fn from(value: ZipExtendedTime) -> Self {
		let data = value.to_extra_data();
		UnknownExtraField {
			header_id: async_zip::spec::header::HeaderId(0x5455),
			data_size: data.len() as u16,
			content: data,
		}
	}
}

struct ZipNTFSTime {
	modification: u64,
	access: u64,
	creation: u64,
}

impl ZipNTFSTime {
	fn to_extra_data(&self) -> Vec<u8> {
		let mut bytes = Vec::with_capacity(4 + 2 + 2 + 8 * 3);
		bytes.extend([0u8; 4]); // reserved
		bytes.extend(0x0001u16.to_le_bytes()); // tag 1
		bytes.extend(24u16.to_le_bytes()); // size
		bytes.extend(self.modification.to_le_bytes());
		bytes.extend(self.access.to_le_bytes());
		bytes.extend(self.creation.to_le_bytes());
		bytes
	}
}

impl From<ZipNTFSTime> for UnknownExtraField {
	fn from(value: ZipNTFSTime) -> Self {
		let data = value.to_extra_data();
		UnknownExtraField {
			header_id: async_zip::spec::header::HeaderId(0x000A),
			data_size: data.len() as u16,
			content: data,
		}
	}
}

fn add_file_times(
	file: &RemoteFile,
	builder: async_zip::ZipEntryBuilder,
) -> async_zip::ZipEntryBuilder {
	let (modified, created) = (file.last_modified(), file.created());
	if modified.is_none() && created.is_none() {
		return builder;
	}

	let extended_time = ZipExtendedTime {
		modification: modified.and_then(|dt| dt.timestamp().try_into().ok()),
		creation: created.and_then(|dt| dt.timestamp().try_into().ok()),
	};

	let ntfs_time = ZipNTFSTime {
		modification: modified.map(crate::io::unix_time_to_nt_time).unwrap_or(0),
		access: 0,
		creation: created.map(crate::io::unix_time_to_nt_time).unwrap_or(0),
	};

	builder.extra_fields(vec![
		ExtraField::Unknown(extended_time.into()),
		ExtraField::Unknown(ntfs_time.into()),
	])
}

fn add_dir_times(
	dir: &crate::fs::dir::RemoteDirectory,
	builder: async_zip::ZipEntryBuilder,
) -> async_zip::ZipEntryBuilder {
	let Some(created_time) = dir.created() else {
		return builder;
	};

	let time_data = ZipExtendedTime {
		modification: None,
		creation: created_time.timestamp().try_into().ok(),
	};

	let ntfs_time = ZipNTFSTime {
		modification: 0,
		access: 0,
		creation: crate::io::unix_time_to_nt_time(created_time),
	};

	builder.extra_fields(vec![
		ExtraField::Unknown(time_data.into()),
		ExtraField::Unknown(ntfs_time.into()),
	])
}

mod client_impl {
	use std::{borrow::Cow, cmp::min, sync::Arc};

	use async_zip::{ZipEntryBuilder, base::write::ZipFileWriter};
	use futures::{AsyncReadExt, AsyncWrite, AsyncWriteExt, StreamExt, stream::FuturesUnordered};
	use log::warn;
	use tokio::sync::Mutex;

	use crate::{
		Error,
		auth::Client,
		fs::{
			HasName, HasUUID, UnsharedFSObject,
			dir::UnsharedDirectoryType,
			file::{RemoteFile, traits::HasFileInfo},
			zip::{add_dir_times, add_file_times},
		},
		util::{MaybeSendBoxFuture, MaybeSendSync},
	};

	impl Client {
		/// Parent path is assumed to not have a trailing slash
		async fn download_file_to_zip(
			&self,
			file: &RemoteFile,
			zip: Arc<Mutex<ZipFileWriter<impl AsyncWrite + Unpin>>>,
			parent_path: &str,
		) -> Result<(), Error> {
			let file_name = match file.name() {
				Some(name) => name,
				None => {
					warn!("Skipping file with undecryptable metadata: {}", file.uuid());
					return Ok(()); // skip files without decrypted metadata
				}
			};
			let name = if parent_path.is_empty() {
				file_name.to_string()
			} else {
				format!("{}/{}", parent_path, file_name)
			};
			let mut builder = ZipEntryBuilder::new(name.into(), async_zip::Compression::Stored)
				.uncompressed_size(file.size());

			if let Some(modified_time) = file.last_modified() {
				builder = builder.last_modification_date(modified_time.into());
			}

			builder = add_file_times(file, builder);
			let entry = builder.build();

			let mut reader = self.get_file_reader(file);

			// buffer start of file to minimize time holding the zip lock
			// I hope that one day I won't have to zero initalize the buffer
			// https://github.com/rust-lang/rust/issues/78485
			let mut initial_buffer = vec![0u8; min(8192, file.size().try_into().unwrap_or(8192))];
			let read = reader.read(&mut initial_buffer).await?;

			let mut zip = zip.lock().await;
			println!(
				"Adding file to zip: {}, contents: ",
				entry.filename().as_str().unwrap(),
				// String::from_utf8_lossy(initial_buffer[..read].as_ref())
			);
			let mut writer = zip.write_entry_stream(entry).await.map_err(|e| {
				Error::custom(
					crate::ErrorKind::IO,
					format!("Failed to start zip entry: {}", e),
				)
			})?;

			// first write the initial buffer
			writer.write_all(&initial_buffer[..read]).await?;

			// then stream the rest of the file
			// todo consider implementing AsyncBufRead for the file reader
			futures::io::copy(reader, &mut writer).await?;
			writer
				.close()
				.await
				.map_err(|e| Error::custom(crate::ErrorKind::IO, e.to_string()))?;
			Ok(())
		}

		/// Wrapper to make the async fn fit the type alias.
		/// I'm honestly not sure why this is necessary as a separate function.
		fn download_dir_to_zip_wrapper<'a, T>(
			&'a self,
			dir: UnsharedDirectoryType<'a>,
			zip: Arc<Mutex<ZipFileWriter<T>>>,
			parent_path: &'a str,
		) -> MaybeSendBoxFuture<'a, Result<(), Error>>
		where
			T: AsyncWrite + Unpin + MaybeSendSync + 'a,
		{
			Box::pin(async move { self.download_dir_to_zip(&dir, zip, parent_path).await })
				as MaybeSendBoxFuture<Result<(), Error>>
		}

		/// Parent path is assumed to not have a trailing slash
		async fn download_dir_to_zip<T>(
			&self,
			dir: &UnsharedDirectoryType<'_>,
			zip: Arc<Mutex<ZipFileWriter<T>>>,
			parent_path: &str,
		) -> Result<(), Error>
		where
			T: AsyncWrite + Unpin + MaybeSendSync,
		{
			let dir_path = match dir {
				UnsharedDirectoryType::Root(_) => Cow::Borrowed(parent_path),
				UnsharedDirectoryType::Dir(dir) if parent_path.is_empty() => {
					Cow::Borrowed(dir.name().unwrap_or_else(|| dir.uuid().as_ref()))
				}
				UnsharedDirectoryType::Dir(dir) => Cow::Owned(format!(
					"{}/{}",
					parent_path,
					dir.name().unwrap_or_else(|| dir.uuid().as_ref())
				)),
			};

			let (dirs, files) = self.list_dir(dir).await?;
			let mut futures: FuturesUnordered<MaybeSendBoxFuture<Result<(), Error>>> = dirs
				.into_iter()
				.map(|d| {
					let zip = zip.clone();
					let dir_path = &dir_path;
					self.download_dir_to_zip_wrapper(
						UnsharedDirectoryType::Dir(Cow::Owned(d)),
						zip,
						dir_path,
					)
				})
				.chain(files.into_iter().map(|f| {
					let zip = zip.clone();
					let dir_path = &dir_path;
					Box::pin(async move {
						println!(
							"Downloading file to zip: {}: {:?}",
							f.name().unwrap_or("unnamed file"),
							f
						);
						self.download_file_to_zip(&f, zip, dir_path).await
					}) as MaybeSendBoxFuture<Result<(), Error>>
				}))
				.collect();
			while let Some(res) = futures.next().await {
				res?;
			}
			std::mem::drop(futures);

			if let UnsharedDirectoryType::Dir(dir) = dir {
				// this is apparently how you add a directory in async-zip
				// (you add an an empty entry with a trailing slash)
				// todo initially allocate enough memory for this
				let mut dir_path = dir_path.into_owned();
				dir_path.push('/');
				let builder = ZipEntryBuilder::new(dir_path.into(), async_zip::Compression::Stored);
				let builder = add_dir_times(dir, builder);
				let entry = builder.build();
				let mut zip = zip.lock().await;
				zip.write_entry_whole(entry, &[]).await.unwrap();
			}

			Ok(())
		}

		pub async fn download_items_to_zip<T>(
			&self,
			items: &[UnsharedFSObject<'_>],
			writer: T,
		) -> Result<T, Error>
		where
			T: AsyncWrite + MaybeSendSync + Unpin,
		{
			let writer = ZipFileWriter::new(writer);
			let zip = Arc::new(Mutex::new(writer));

			let root_path = "";
			let mut futures: FuturesUnordered<MaybeSendBoxFuture<Result<(), Error>>> = items
				.iter()
				.map(|i| {
					let zip = zip.clone();
					Box::pin(async move {
						let dir = match i {
							UnsharedFSObject::File(f) => {
								return self.download_file_to_zip(f, zip, root_path).await;
							}
							UnsharedFSObject::Dir(cow) => {
								UnsharedDirectoryType::Dir(Cow::Borrowed(cow))
							}
							UnsharedFSObject::Root(cow) => {
								UnsharedDirectoryType::Root(Cow::Borrowed(cow))
							}
						};
						self.download_dir_to_zip(&dir, zip, root_path).await
					}) as MaybeSendBoxFuture<Result<(), Error>>
				})
				.collect();

			while let Some(res) = futures.next().await {
				res?;
			}
			let mut writer = Arc::into_inner(zip)
				.expect("all futures to have run to completion in download_items_to_zip")
				.into_inner()
				.close()
				.await
				.map_err(|e| Error::custom(crate::ErrorKind::IO, e.to_string()))?;
			writer.close().await?;
			Ok(writer)
		}
	}
}

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
mod js_impl {
	use crate::{Error, auth::Client, crypto::error::ConversionError, js::DownloadFileToZipParams};

	#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
	use wasm_bindgen::prelude::*;

	#[cfg_attr(all(target_arch = "wasm32", target_os = "unknown"), wasm_bindgen)]
	#[cfg_attr(feature = "node", napi)]
	impl Client {
		#[cfg_attr(
			all(target_arch = "wasm32", target_os = "unknown"),
			wasm_bindgen(js_name = "downloadItemsToZip")
		)]
		pub async fn download_items_to_zip_js(
			&self,
			params: DownloadFileToZipParams,
		) -> Result<(), JsValue> {
			let writer = wasm_streams::WritableStream::from_raw(params.writer)
				.try_into_async_write()
				.map_err(|(e, _)| e)?;
			let items = params
				.items
				.into_iter()
				.map(TryInto::try_into)
				.collect::<Result<Vec<_>, ConversionError>>()
				.map_err(Error::from)?;
			let _ = self.download_items_to_zip(&items, writer).await?;
			Ok(())
		}
	}
}
