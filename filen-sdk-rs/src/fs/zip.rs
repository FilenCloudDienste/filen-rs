use async_zip::spec::header::{ExtraField, UnknownExtraField};

use crate::fs::{
	dir::traits::HasDirInfo,
	file::{enums::RemoteFileType, traits::HasFileInfo},
};

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
	file: &RemoteFileType<'_>,
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
	dir: &impl HasDirInfo,
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

#[derive(Clone)]
pub(crate) struct ZipState {
	pub(crate) bytes_written: u64,
	pub(crate) total_bytes: u64,
	pub(crate) items_processed: u64,
	pub(crate) total_items: u64,
}

impl ZipState {
	pub(crate) fn new(total_bytes: u64, total_items: u64) -> Self {
		Self {
			bytes_written: 0,
			total_bytes,
			items_processed: 0,
			total_items,
		}
	}
}

pub trait ZipProgressCallback: Fn(u64, u64, u64, u64) + Send + Sync {}

impl<T> ZipProgressCallback for T where T: Fn(u64, u64, u64, u64) + Send + Sync {}

/// Standalone zip helper functions that work with any `SharedClient` implementor
/// (both `Client` and `UnauthClient`), enabling cross-category zip downloads.
pub(crate) mod helpers {
	use std::{
		borrow::Cow,
		cmp::min,
		sync::{Arc, Mutex as StdMutex},
	};

	use async_zip::{ZipEntryBuilder, base::write::ZipFileWriter};
	use futures::{AsyncReadExt, AsyncWrite, AsyncWriteExt, StreamExt, stream::FuturesUnordered};
	use log::warn;
	use tokio::sync::Mutex;

	use crate::{
		Error,
		auth::shared_client::SharedClient,
		fs::{
			HasName, HasUUID,
			categories::{DirType, fs::CategoryFS},
			file::{
				client_impl::FileReaderSharedClientExt, enums::RemoteFileType, traits::HasFileInfo,
			},
			zip::{ZipProgressCallback, ZipState, add_dir_times, add_file_times},
		},
		util::{MaybeSendBoxFuture, MaybeSendSync},
	};

	/// Parent path is assumed to not have a trailing slash.
	/// Works with any `SharedClient` — both `Client` and `UnauthClient`.
	pub(crate) async fn download_file_to_zip<C: SharedClient>(
		client: &C,
		file: &RemoteFileType<'_>,
		zip: Arc<Mutex<ZipFileWriter<impl AsyncWrite + Unpin>>>,
		state: Arc<StdMutex<ZipState>>,
		progress_callback: Option<&impl ZipProgressCallback>,
		parent_path: &str,
	) -> Result<(), Error> {
		let file_name = match file.name() {
			Some(name) => name,
			None => {
				warn!("Skipping file with undecryptable metadata: {}", file.uuid());
				// still update progress so counters stay consistent
				let state_clone = {
					let mut state = state.lock().unwrap();
					state.bytes_written += file.size();
					state.items_processed += 1;
					state.clone()
				};
				if let Some(callback) = progress_callback {
					callback(
						state_clone.bytes_written,
						state_clone.total_bytes,
						state_clone.items_processed,
						state_clone.total_items,
					);
				}
				return Ok(());
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

		let mut reader = client.get_file_reader(file);

		// buffer start of file to minimize time holding the zip lock
		// I hope that one day I won't have to zero initalize the buffer
		// https://github.com/rust-lang/rust/issues/78485
		let mut initial_buffer = vec![0u8; min(8192, file.size().try_into().unwrap_or(8192))];
		let read = reader.read(&mut initial_buffer).await?;

		let mut zip = zip.lock().await;
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
		std::mem::drop(zip);
		let state_clone = {
			let mut state = state.lock().unwrap();
			state.bytes_written += file.size();
			state.items_processed += 1;
			state.clone()
		};

		if let Some(callback) = progress_callback {
			callback(
				state_clone.bytes_written,
				state_clone.total_bytes,
				state_clone.items_processed,
				state_clone.total_items,
			);
		}

		Ok(())
	}

	/// Wrapper to make the async fn fit the type alias.
	#[allow(private_bounds)]
	pub(crate) fn download_dir_to_zip_wrapper<'a, 'b, 'ctx, Cat, T>(
		client: &'a Cat::Client,
		dir: DirType<'a, Cat>,
		zip: Arc<Mutex<ZipFileWriter<T>>>,
		state: Arc<StdMutex<ZipState>>,
		progress_callback: Option<&'a impl ZipProgressCallback>,
		parent_path: &'a str,
		context: Cat::ListDirContext<'ctx>,
	) -> MaybeSendBoxFuture<'a, Result<(), Error>>
	where
		Cat: CategoryFS,
		Cat::Client: SharedClient,
		T: AsyncWrite + Unpin + MaybeSendSync + 'a + 'ctx,
		'ctx: 'a,
		RemoteFileType<'b>: From<&'b Cat::File>,
		RemoteFileType<'static>: From<Cat::File>,
	{
		Box::pin(async move {
			download_dir_to_zip::<Cat, T>(
				client,
				&dir,
				zip,
				state,
				progress_callback,
				parent_path,
				context,
			)
			.await
		}) as MaybeSendBoxFuture<Result<(), Error>>
	}

	/// Parent path is assumed to not have a trailing slash.
	/// Works with any category whose `Client` implements `SharedClient`.
	#[allow(private_bounds)]
	pub(crate) async fn download_dir_to_zip<'a, 'b, 'ctx, Cat, T>(
		client: &Cat::Client,
		dir: &DirType<'_, Cat>,
		zip: Arc<Mutex<ZipFileWriter<T>>>,
		state: Arc<StdMutex<ZipState>>,
		progress_callback: Option<&impl ZipProgressCallback>,
		parent_path: &str,
		context: Cat::ListDirContext<'ctx>,
	) -> Result<(), Error>
	where
		Cat: CategoryFS,
		Cat::Client: SharedClient,
		T: AsyncWrite + Unpin + MaybeSendSync + 'ctx,
		RemoteFileType<'b>: From<&'b Cat::File>,
		RemoteFileType<'static>: From<Cat::File>,
	{
		let dir_path = match dir {
			DirType::Root(_) => Cow::Borrowed(parent_path),
			DirType::Dir(dir) if parent_path.is_empty() => {
				Cow::Borrowed(dir.name().unwrap_or_else(|| dir.uuid().as_ref()))
			}
			DirType::Dir(dir) => Cow::Owned(format!(
				"{}/{}",
				parent_path,
				dir.name().unwrap_or_else(|| dir.uuid().as_ref())
			)),
		};

		let (dirs, files) =
			Cat::list_dir(client, dir, None::<&fn(u64, Option<u64>)>, context.clone()).await?;
		{
			let mut state = state.lock().unwrap();
			state.total_items +=
				u64::try_from(dirs.len() + files.len()).expect("dir listing to fit in u64");
			state.total_bytes += files.iter().map(|f| f.size()).sum::<u64>();
		}
		let mut futures: FuturesUnordered<_> = dirs
			.into_iter()
			.map(|d| {
				let zip = zip.clone();
				let state = state.clone();
				let dir_path = &dir_path;
				let context = context.clone();
				download_dir_to_zip_wrapper::<Cat, T>(
					client,
					DirType::Dir(Cow::Owned(d)),
					zip,
					state,
					progress_callback,
					dir_path,
					context,
				)
			})
			.chain(files.into_iter().map(|f| {
				let zip = zip.clone();
				let state = state.clone();
				let dir_path = &dir_path;
				Box::pin(async move {
					download_file_to_zip(client, &f.into(), zip, state, progress_callback, dir_path)
						.await
				}) as MaybeSendBoxFuture<Result<(), Error>>
			}))
			.collect();
		while let Some(res) = futures.next().await {
			res?;
		}
		std::mem::drop(futures);

		if let DirType::Dir(dir) = dir {
			// this is apparently how you add a directory in async-zip
			// (you add an an empty entry with a trailing slash)
			// todo initially allocate enough memory for this
			let mut dir_entry_path = String::with_capacity(dir_path.len() + 1);
			dir_entry_path.push_str(&dir_path);
			dir_entry_path.push('/');
			let builder =
				ZipEntryBuilder::new(dir_entry_path.into(), async_zip::Compression::Stored);
			let builder = add_dir_times(dir.as_ref(), builder);
			let entry = builder.build();
			let mut zip = zip.lock().await;
			zip.write_entry_whole(entry, &[])
				.await
				.map_err(|e| Error::custom(crate::ErrorKind::IO, e.to_string()))?;
		}

		let state_clone = {
			let mut state = state.lock().unwrap();
			state.items_processed += 1;
			state.clone()
		};

		if let Some(callback) = progress_callback {
			callback(
				state_clone.bytes_written,
				state_clone.total_bytes,
				state_clone.items_processed,
				state_clone.total_items,
			);
		}
		Ok(())
	}
}

/// Public API for typed, single-category zip downloads on any `SharedClient`.
mod client_impl {
	use std::{
		borrow::Cow,
		sync::{Arc, Mutex as StdMutex},
	};

	use async_zip::base::write::ZipFileWriter;
	use futures::{AsyncWrite, AsyncWriteExt, StreamExt, stream::FuturesUnordered};
	use tokio::sync::Mutex;

	use crate::{
		Error,
		auth::{Client, shared_client::SharedClient, unauth::UnauthClient},
		fs::{
			categories::{DirType, NonRootFileType, fs::CategoryFS},
			file::{enums::RemoteFileType, traits::HasFileInfo},
			zip::{
				ZipProgressCallback, ZipState,
				helpers::{download_dir_to_zip, download_file_to_zip},
			},
		},
		util::{MaybeSendBoxFuture, MaybeSendSync},
	};

	#[allow(private_bounds)]
	async fn download_items_to_zip_inner<'a, 'b, 'ctx, Cat, T>(
		client: &Cat::Client,
		items: &'b [NonRootFileType<'a, Cat>],
		writer: T,
		progress_callback: Option<&impl ZipProgressCallback>,
		context: Cat::ListDirContext<'ctx>,
	) -> Result<T, Error>
	where
		Cat: CategoryFS,
		Cat::Client: SharedClient,
		T: AsyncWrite + MaybeSendSync + Unpin + 'ctx,
		RemoteFileType<'b>: From<&'b Cat::File>,
		RemoteFileType<'static>: From<Cat::File>,
		'a: 'b,
	{
		let writer = ZipFileWriter::new(writer);
		let zip = Arc::new(Mutex::new(writer));
		let state = Arc::new(StdMutex::new(ZipState::new(
			items
				.iter()
				.filter_map(|i| match i {
					NonRootFileType::File(f) => Some(f.size()),
					_ => None,
				})
				.sum(),
			items.len().try_into().expect("items to fit in u64"),
		)));

		let root_path = "";
		let mut futures: FuturesUnordered<MaybeSendBoxFuture<Result<(), Error>>> = items
			.iter()
			.map(|i| {
				let zip = zip.clone();
				let state = state.clone();
				let context = context.clone();
				Box::pin(async move {
					match i {
						NonRootFileType::Root(root) => {
							download_dir_to_zip::<Cat, T>(
								client,
								&DirType::Root(Cow::Borrowed(root.as_ref())),
								zip,
								state,
								progress_callback,
								root_path,
								context,
							)
							.await
						}
						NonRootFileType::Dir(dir) => {
							download_dir_to_zip::<Cat, T>(
								client,
								&DirType::Dir(Cow::Borrowed(dir.as_ref())),
								zip,
								state,
								progress_callback,
								root_path,
								context,
							)
							.await
						}
						NonRootFileType::File(file) => {
							download_file_to_zip(
								client,
								&Into::<RemoteFileType>::into(file.as_ref()),
								zip,
								state,
								progress_callback,
								root_path,
							)
							.await
						}
					}
				}) as MaybeSendBoxFuture<Result<(), Error>>
			})
			.collect();

		while let Some(res) = futures.next().await {
			if let Err(e) = res {
				drop(futures);
				return Err(e);
			}
		}
		drop(futures);
		let mut writer = Arc::into_inner(zip)
			.expect("all futures to have run to completion in download_items_to_zip")
			.into_inner()
			.close()
			.await
			.map_err(|e| Error::custom(crate::ErrorKind::IO, e.to_string()))?;
		writer.close().await?;
		Ok(writer)
	}

	impl Client {
		#[allow(private_bounds)]
		pub async fn download_items_to_zip<'a, 'ctx, Cat, T>(
			&self,
			items: &'a [NonRootFileType<'a, Cat>],
			writer: T,
			progress_callback: Option<&impl ZipProgressCallback>,
			context: Cat::ListDirContext<'ctx>,
		) -> Result<T, Error>
		where
			Cat: CategoryFS<Client = Self>,
			T: AsyncWrite + MaybeSendSync + Unpin + 'ctx,
			RemoteFileType<'a>: From<&'a Cat::File>,
			RemoteFileType<'static>: From<Cat::File>,
		{
			download_items_to_zip_inner::<Cat, T>(self, items, writer, progress_callback, context)
				.await
		}
	}

	impl UnauthClient {
		#[allow(private_bounds)]
		pub async fn download_items_to_zip<'a, 'ctx, Cat, T>(
			&self,
			items: &'a [NonRootFileType<'a, Cat>],
			writer: T,
			progress_callback: Option<&impl ZipProgressCallback>,
			context: Cat::ListDirContext<'ctx>,
		) -> Result<T, Error>
		where
			Cat: CategoryFS<Client = Self>,
			T: AsyncWrite + MaybeSendSync + Unpin + 'ctx,
			Cat::File: Into<RemoteFileType<'static>>,
			RemoteFileType<'a>: From<&'a Cat::File>,
			RemoteFileType<'static>: From<Cat::File>,
		{
			download_items_to_zip_inner::<Cat, T>(self, items, writer, progress_callback, context)
				.await
		}
	}
}

/// JS/WASM bindings for cross-category zip downloads.
#[cfg(any(
	feature = "wasm-full",
	feature = "uniffi",
	feature = "service-worker",
	all(target_family = "wasm", target_os = "unknown")
))]
pub(crate) mod js_impl {
	use std::{
		borrow::Cow,
		sync::{Arc, Mutex as StdMutex},
	};

	use async_zip::base::write::ZipFileWriter;
	use filen_macros::js_type;
	use futures::{AsyncWrite, AsyncWriteExt, StreamExt, stream::FuturesUnordered};
	use tokio::sync::Mutex;

	use crate::{
		Error,
		auth::Client,
		fs::{
			categories::{Linked, Normal, Shared},
			file::{enums::RemoteFileType, traits::HasFileInfo},
			zip::{
				ZipProgressCallback, ZipState,
				helpers::{download_dir_to_zip, download_file_to_zip},
			},
		},
		js::{AnyDirWithContext, AnyFile, DirByCategoryWithContext},
		util::{MaybeSendBoxFuture, MaybeSendSync},
	};

	#[js_type(import, wasm_all)]
	pub enum ZipItem {
		File(AnyFile),
		Dir(AnyDirWithContext),
	}

	/// Dispatches a directory zip download based on its runtime category.
	/// Handles Normal, Shared, and Linked directories uniformly.
	#[allow(private_bounds)]
	async fn download_dir_by_category_to_zip<T>(
		client: &Client,
		dir: DirByCategoryWithContext,
		zip: Arc<Mutex<ZipFileWriter<T>>>,
		state: Arc<StdMutex<ZipState>>,
		progress_callback: Option<&impl ZipProgressCallback>,
		parent_path: &str,
	) -> Result<(), Error>
	where
		T: AsyncWrite + Unpin + MaybeSendSync,
	{
		match dir {
			DirByCategoryWithContext::Normal(dir) => {
				download_dir_to_zip::<Normal, T>(
					client,
					&dir,
					zip,
					state,
					progress_callback,
					parent_path,
					(),
				)
				.await
			}
			DirByCategoryWithContext::Shared(dir, role) => {
				download_dir_to_zip::<Shared, T>(
					client,
					&dir,
					zip,
					state,
					progress_callback,
					parent_path,
					&role,
				)
				.await
			}
			DirByCategoryWithContext::Linked(dir, link) => {
				download_dir_to_zip::<Linked, T>(
					client.unauthed(),
					&dir,
					zip,
					state,
					progress_callback,
					parent_path,
					Cow::Owned(link.try_into()?),
				)
				.await
			}
		}
	}

	/// Downloads a list of mixed-category items to a zip writer.
	#[allow(private_bounds)]
	pub(crate) async fn download_zip_items<T>(
		client: &Client,
		items: Vec<ZipItem>,
		writer: T,
		progress_callback: Option<&impl ZipProgressCallback>,
	) -> Result<T, Error>
	where
		T: AsyncWrite + Unpin + MaybeSendSync,
	{
		let initial_file_bytes: u64 = items
			.iter()
			.filter_map(|i| match i {
				ZipItem::File(f) => RemoteFileType::try_from(f.clone()).ok().map(|f| f.size()),
				_ => None,
			})
			.sum();

		let writer = ZipFileWriter::new(writer);
		let zip = Arc::new(Mutex::new(writer));
		let state = Arc::new(StdMutex::new(ZipState::new(
			initial_file_bytes,
			items.len().try_into().expect("items to fit in u64"),
		)));

		let root_path = "";
		let mut futures: FuturesUnordered<MaybeSendBoxFuture<Result<(), Error>>> = items
			.into_iter()
			.map(|item| {
				let zip = zip.clone();
				let state = state.clone();
				Box::pin(async move {
					match item {
						ZipItem::Dir(dir) => {
							let dir = DirByCategoryWithContext::from(dir);
							download_dir_by_category_to_zip(
								client,
								dir,
								zip,
								state,
								progress_callback,
								root_path,
							)
							.await
						}
						ZipItem::File(file) => {
							let file = RemoteFileType::try_from(file)?;
							download_file_to_zip(
								client,
								&file,
								zip,
								state,
								progress_callback,
								root_path,
							)
							.await
						}
					}
				}) as MaybeSendBoxFuture<Result<(), Error>>
			})
			.collect();

		while let Some(res) = futures.next().await {
			if let Err(e) = res {
				drop(futures);
				return Err(e);
			}
		}
		drop(futures);
		let mut writer = Arc::into_inner(zip)
			.expect("all futures to have run to completion in download_zip_items")
			.into_inner()
			.close()
			.await
			.map_err(|e| Error::custom(crate::ErrorKind::IO, e.to_string()))?;
		writer.close().await?;
		Ok(writer)
	}
}

#[cfg(feature = "wasm-full")]
mod js_client_impl {
	use wasm_bindgen::prelude::wasm_bindgen;

	use crate::{
		Error, ErrorKind,
		auth::JsClient,
		fs::{
			file::service_worker::StreamWriter,
			zip::js_impl::{ZipItem, download_zip_items},
		},
		js::{ManagedFuture, spawn_buffered_write_future},
	};

	#[wasm_bindgen(js_class = "Client")]
	impl JsClient {
		#[wasm_bindgen(js_name = "downloadItemsToZip")]
		pub async fn download_items_to_zip(
			&self,
			items: Vec<ZipItem>,
			#[wasm_bindgen(unchecked_param_type = "WritableStream<Uint8Array>")]
			writable_stream: web_sys::WritableStream,
			#[wasm_bindgen(
				unchecked_param_type = "(bytesWritten: bigint, totalBytes: bigint, itemsProcessed: bigint, totalItems: bigint) => void | undefined"
			)]
			progress: web_sys::js_sys::Function,
			managed_future: ManagedFuture,
		) -> Result<(), Error> {
			let (data_sender, data_receiver) = tokio::sync::mpsc::channel::<Vec<u8>>(10);

			let writer = wasm_streams::WritableStream::from_raw(writable_stream)
				.try_into_async_write()
				.map_err(|(e, _)| {
					Error::custom(
						ErrorKind::Conversion,
						format!("failed to convert WritableStream to AsyncWrite: {:?}", e),
					)
				})?;

			let (result_sender, result_receiver) =
				tokio::sync::oneshot::channel::<Result<(), Error>>();

			spawn_buffered_write_future(data_receiver, writer, None::<fn(u64)>, result_sender);

			let progress_callback = if progress.is_undefined() {
				None
			} else {
				use wasm_bindgen::JsValue;

				let (sender, mut receiver) =
					tokio::sync::mpsc::unbounded_channel::<(u64, u64, u64, u64)>();
				crate::runtime::spawn_local(async move {
					while let Some((bw, tb, ip, ti)) = receiver.recv().await {
						let _ = progress.call4(
							&JsValue::UNDEFINED,
							&JsValue::from(bw),
							&JsValue::from(tb),
							&JsValue::from(ip),
							&JsValue::from(ti),
						);
					}
				});
				Some(move |bw: u64, tb: u64, ip: u64, ti: u64| {
					let _ = sender.send((bw, tb, ip, ti));
				})
			};

			let this = self.inner();

			managed_future
				.into_js_managed_commander_future(move || async move {
					let writer = StreamWriter::new(data_sender);
					download_zip_items(&this, items, writer, progress_callback.as_ref()).await?;
					result_receiver.await.unwrap_or_else(|e| {
						Err(Error::custom(
							ErrorKind::IO,
							format!("zip download result_sender dropped: {}", e),
						))
					})
				})?
				.await
		}
	}
}

#[cfg(feature = "service-worker")]
mod service_worker_impl {
	use wasm_bindgen::prelude::wasm_bindgen;

	use crate::{
		Error, ErrorKind,
		fs::{
			file::service_worker::StreamWriter,
			zip::js_impl::{ZipItem, download_zip_items},
		},
		js::{ManagedFuture, ServiceWorkerClient, spawn_buffered_write_future},
	};

	#[wasm_bindgen(js_class = "Client")]
	impl ServiceWorkerClient {
		#[wasm_bindgen(js_name = "downloadItemsToZip")]
		pub async fn download_items_to_zip(
			&self,
			items: Vec<ZipItem>,
			#[wasm_bindgen(unchecked_param_type = "WritableStream<Uint8Array>")]
			writable_stream: web_sys::WritableStream,
			#[wasm_bindgen(
				unchecked_param_type = "(bytesWritten: bigint, totalBytes: bigint, itemsProcessed: bigint, totalItems: bigint) => void | undefined"
			)]
			progress: web_sys::js_sys::Function,
			managed_future: ManagedFuture,
		) -> Result<(), Error> {
			use wasm_bindgen::JsValue;

			let (data_sender, data_receiver) = tokio::sync::mpsc::channel::<Vec<u8>>(10);

			let writer = wasm_streams::WritableStream::from_raw(writable_stream)
				.try_into_async_write()
				.map_err(|(e, _)| {
					Error::custom(
						ErrorKind::Conversion,
						format!("failed to convert WritableStream to AsyncWrite: {:?}", e),
					)
				})?;

			let (result_sender, result_receiver) =
				tokio::sync::oneshot::channel::<Result<(), Error>>();

			spawn_buffered_write_future(data_receiver, writer, None::<fn(u64)>, result_sender);

			let progress_callback = if progress.is_undefined() {
				None
			} else {
				let (sender, mut receiver) =
					tokio::sync::mpsc::unbounded_channel::<(u64, u64, u64, u64)>();
				crate::runtime::spawn_local(async move {
					while let Some((bw, tb, ip, ti)) = receiver.recv().await {
						let _ = progress.call4(
							&JsValue::UNDEFINED,
							&JsValue::from(bw),
							&JsValue::from(tb),
							&JsValue::from(ip),
							&JsValue::from(ti),
						);
					}
				});
				Some(move |bw: u64, tb: u64, ip: u64, ti: u64| {
					let _ = sender.send((bw, tb, ip, ti));
				})
			};

			let this = self.inner();

			managed_future
				.into_js_managed_future(async move {
					let writer = StreamWriter::new(data_sender);
					download_zip_items(this, items, writer, progress_callback.as_ref()).await?;
					result_receiver.await.unwrap_or_else(|e| {
						Err(Error::custom(
							ErrorKind::IO,
							format!("zip download result_sender dropped: {}", e),
						))
					})
				})?
				.await
		}
	}
}

#[cfg(any(feature = "wasm-full", feature = "uniffi"))]
mod unauth_js_client_impl {
	use std::borrow::Cow;

	use crate::{
		Error,
		auth::js_impls::UnauthJsClient,
		fs::categories::{DirType, Linked, NonRootFileType},
		js::AnyLinkedDirWithContext,
		runtime::do_on_commander,
	};

	impl UnauthJsClient {
		async fn inner_download_linked_dir_to_zip<F>(
			&self,
			dir: AnyLinkedDirWithContext,
			callback: Option<F>,
		) -> Result<Vec<u8>, Error>
		where
			F: Fn(u64, u64, u64, u64) + Send + Sync + 'static,
		{
			let this = self.inner();
			do_on_commander(move || async move {
				let parsed_dir = DirType::<Linked>::from(dir.dir);
				let link = dir.link;

				let items = [NonRootFileType::<Linked>::from(parsed_dir)];
				let writer = this
					.download_items_to_zip::<Linked, _>(
						&items,
						Vec::new(),
						callback.as_ref(),
						Cow::Owned(link.try_into()?),
					)
					.await?;
				Ok(writer)
			})
			.await
		}
	}

	#[cfg(feature = "uniffi")]
	#[uniffi::export(with_foreign)]
	pub trait ZipDownloadProgressCallback: Send + Sync {
		fn on_progress(
			&self,
			bytes_written: u64,
			total_bytes: u64,
			items_processed: u64,
			total_items: u64,
		);
	}

	#[cfg(feature = "uniffi")]
	#[uniffi::export]
	impl UnauthJsClient {
		pub async fn download_linked_dir_to_zip(
			&self,
			dir: AnyLinkedDirWithContext,
			callback: Option<std::sync::Arc<dyn ZipDownloadProgressCallback>>,
		) -> Result<Vec<u8>, Error> {
			let callback = callback.map(|cb| {
				move |bw: u64, tb: u64, ip: u64, ti: u64| {
					let callback = std::sync::Arc::clone(&cb);
					tokio::task::spawn_blocking(move || {
						callback.on_progress(bw, tb, ip, ti);
					});
				}
			});
			self.inner_download_linked_dir_to_zip(dir, callback).await
		}
	}

	#[cfg(all(target_family = "wasm", target_os = "unknown"))]
	#[wasm_bindgen::prelude::wasm_bindgen(js_class = "UnauthClient")]
	impl UnauthJsClient {
		#[wasm_bindgen::prelude::wasm_bindgen(js_name = "downloadLinkedDirToZip")]
		pub async fn download_linked_dir_to_zip(
			&self,
			dir: AnyLinkedDirWithContext,
			#[wasm_bindgen(unchecked_param_type = "WritableStream<Uint8Array>")]
			writable_stream: web_sys::WritableStream,
			#[wasm_bindgen(
				unchecked_param_type = "(bytesWritten: bigint, totalBytes: bigint, itemsProcessed: bigint, totalItems: bigint) => void | undefined"
			)]
			progress: web_sys::js_sys::Function,
			managed_future: crate::js::ManagedFuture,
		) -> Result<(), Error> {
			use crate::{
				ErrorKind, fs::file::service_worker::StreamWriter, js::spawn_buffered_write_future,
			};

			let (data_sender, data_receiver) = tokio::sync::mpsc::channel::<Vec<u8>>(10);

			let writer = wasm_streams::WritableStream::from_raw(writable_stream)
				.try_into_async_write()
				.map_err(|(e, _)| {
					Error::custom(
						ErrorKind::Conversion,
						format!("failed to convert WritableStream to AsyncWrite: {:?}", e),
					)
				})?;

			let (result_sender, result_receiver) =
				tokio::sync::oneshot::channel::<Result<(), Error>>();

			spawn_buffered_write_future(data_receiver, writer, None::<fn(u64)>, result_sender);

			let progress_callback = if progress.is_undefined() {
				None
			} else {
				use wasm_bindgen::JsValue;

				let (sender, mut receiver) =
					tokio::sync::mpsc::unbounded_channel::<(u64, u64, u64, u64)>();
				crate::runtime::spawn_local(async move {
					while let Some((bw, tb, ip, ti)) = receiver.recv().await {
						let _ = progress.call4(
							&JsValue::UNDEFINED,
							&JsValue::from(bw),
							&JsValue::from(tb),
							&JsValue::from(ip),
							&JsValue::from(ti),
						);
					}
				});
				Some(move |bw: u64, tb: u64, ip: u64, ti: u64| {
					let _ = sender.send((bw, tb, ip, ti));
				})
			};

			let this = self.inner();
			let parsed_dir = DirType::<Linked>::from(dir.dir);
			let link = dir.link;

			managed_future
				.into_js_managed_commander_future(move || async move {
					let writer = StreamWriter::new(data_sender);
					let items = [NonRootFileType::<Linked>::from(parsed_dir)];
					this.download_items_to_zip::<Linked, _>(
						&items,
						writer,
						progress_callback.as_ref(),
						Cow::Owned(link.try_into()?),
					)
					.await?;
					result_receiver.await.unwrap_or_else(|e| {
						Err(Error::custom(
							ErrorKind::IO,
							format!("zip download result_sender dropped: {}", e),
						))
					})
				})?
				.await
		}
	}
}
