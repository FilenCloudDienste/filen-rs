use std::fmt::Display;

use async_zip::spec::header::{ExtraField, UnknownExtraField};
use chrono::{DateTime, Utc};

use crate::fs::file::{enums::RemoteFileType, traits::HasFileInfo};

mod queue;
mod walk;

/// The Unix extended timestamp extra field: flags, then each time they name, in seconds since the
/// Unix epoch.
pub(crate) struct ZipExtendedTime {
	pub(crate) modification: Option<u32>,
	pub(crate) creation: Option<u32>,
}

impl ZipExtendedTime {
	pub(crate) const HEADER_ID: u16 = 0x5455;

	fn count(&self) -> u16 {
		u16::from(self.modification.is_some()) + u16::from(self.creation.is_some())
	}

	pub(crate) fn to_extra_data(&self) -> Vec<u8> {
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
			header_id: async_zip::spec::header::HeaderId(ZipExtendedTime::HEADER_ID),
			data_size: u16::try_from(data.len())
				.expect("a flags byte and at most two u32 times (should be impossible)"),
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
			data_size: u16::try_from(data.len()).expect(
				"reserved, tag, size and three u64 times are 32 bytes (should be impossible)",
			),
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
	created: Option<DateTime<Utc>>,
	builder: async_zip::ZipEntryBuilder,
) -> async_zip::ZipEntryBuilder {
	let Some(created_time) = created else {
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

/// An archive's progress: the totals grow as its directories are listed.
#[derive(Clone)]
struct ZipState {
	bytes_written: u64,
	total_bytes: u64,
	items_processed: u64,
	total_items: u64,
}

impl ZipState {
	fn new(total_bytes: u64, total_items: u64) -> Self {
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

/// True if `name` is safe to use as a single component in a portable zip entry
/// path (Zip Slip). Zip entry names come from decrypted remote metadata and the
/// archive may be extracted on any OS, so this rejects — on every platform — an
/// empty name, `.`/`..`, either path separator, and a Windows drive-relative
/// prefix (`C:evil`, which resets the extraction destination on Windows).
/// Legitimate names never contain these, so callers fall back to the item UUID
/// (always a safe component) rather than dropping the entry.
fn is_safe_zip_component(name: &str) -> bool {
	fn starts_with_drive_letter(name: &str) -> bool {
		let bytes = name.as_bytes();
		bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
	}
	!name.is_empty()
		&& name != "."
		&& name != ".."
		&& !name.contains(['/', '\\'])
		&& !starts_with_drive_letter(name)
}

/// Joins `name` onto `parent_path` as a zip entry path, first confining `name`
/// to a safe single component — falling back to `uuid` (always safe) on a
/// Zip-Slip-unsafe name so the entry is still archived, just under a
/// non-traversing path. `parent_path` is already composed of confined
/// components and carries no trailing slash.
fn zip_entry_path(parent_path: &str, name: &str, uuid: impl Display) -> String {
	match (parent_path.is_empty(), is_safe_zip_component(name)) {
		(true, true) => name.to_owned(),
		(true, false) => uuid.to_string(),
		(false, true) => format!("{parent_path}/{name}"),
		(false, false) => format!("{parent_path}/{uuid}"),
	}
}

/// Public API for typed, single-category zip downloads on any `SharedClient`.
mod client_impl {
	use std::{borrow::Cow, sync::Mutex};

	use futures::{AsyncWrite, StreamExt, stream};

	use crate::{
		Error,
		auth::{Client, shared_client::SharedClient, unauth::UnauthClient},
		fs::{
			categories::{DirType, NonRootFileType, fs::CategoryFS},
			file::{enums::RemoteFileType, traits::HasFileInfo},
			zip::{
				ZipProgressCallback, ZipState,
				queue::write_entries,
				walk::{ClientLister, DirWalk, Entry, dir_entries, file_entries},
			},
		},
		util::MaybeSendSync,
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
		let state = Mutex::new(ZipState::new(
			items
				.iter()
				.filter_map(|i| match i {
					NonRootFileType::File(f) => Some(f.size()),
					_ => None,
				})
				.fold(0u64, u64::saturating_add),
			items.len().try_into().expect("items to fit in u64"),
		));
		let entries = stream::iter(items)
			.map(|item| {
				let dir = match item {
					NonRootFileType::File(file) => {
						return file_entries(Entry::file(RemoteFileType::from(file.as_ref()), ""));
					}
					NonRootFileType::Root(root) => DirType::Root(Cow::Borrowed(root.as_ref())),
					NonRootFileType::Dir(dir) => DirType::Dir(Cow::Borrowed(dir.as_ref())),
				};
				let lister = ClientLister::<Cat> {
					client,
					context: context.clone(),
				};
				dir_entries(DirWalk::new(dir, lister, &state))
			})
			.flatten();
		write_entries(
			client.get_unauth_client(),
			entries,
			writer,
			&state,
			progress_callback,
		)
		.await
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
#[cfg(any(feature = "wasm-full", feature = "service-worker"))]
pub(crate) mod js_impl {
	use std::{borrow::Cow, sync::Mutex};

	use filen_types::traits::CowHelpers;
	use futures::{AsyncWrite, StreamExt, stream};

	use crate::{
		Error,
		auth::Client,
		connect::{DirPublicLink, fs::SharingRole},
		fs::{
			categories::{DirType, Linked, Normal, Shared},
			file::{enums::RemoteFileType, traits::HasFileInfo},
			zip::{
				ZipProgressCallback, ZipState,
				queue::write_entries,
				walk::{ClientLister, DirWalk, Entry, dir_entries, file_entries},
			},
		},
		js::{AnyItemWithContext, DirByCategoryWithContext},
		util::{MaybeSendBoxStream, MaybeSendSync},
	};

	/// A requested item, with what listing it takes. A drive job's `ItemSource` without the
	/// refusal of the drive's root, which a zip takes.
	enum ZipItem {
		File(RemoteFileType<'static>),
		Normal(DirType<'static, Normal>),
		Shared(DirType<'static, Shared>, SharingRole),
		Linked(DirType<'static, Linked>, DirPublicLink),
	}

	impl TryFrom<AnyItemWithContext> for ZipItem {
		type Error = Error;

		fn try_from(item: AnyItemWithContext) -> Result<Self, Error> {
			Ok(match item {
				AnyItemWithContext::File(file) => Self::File(RemoteFileType::try_from(file)?),
				AnyItemWithContext::Dir(dir) => match DirByCategoryWithContext::from(dir) {
					DirByCategoryWithContext::Normal(dir) => Self::Normal(dir),
					DirByCategoryWithContext::Shared(dir, role) => Self::Shared(dir, role),
					DirByCategoryWithContext::Linked(dir, link) => {
						Self::Linked(dir, link.try_into()?)
					}
				},
			})
		}
	}

	impl ZipItem {
		/// The item's entries: the file, or the directory and everything below it.
		fn entries<'a>(
			&'a self,
			client: &'a Client,
			state: &'a Mutex<ZipState>,
		) -> MaybeSendBoxStream<'a, Result<Entry<'a>, Error>> {
			match self {
				Self::File(file) => file_entries(Entry::file(file.as_borrowed_cow(), "")),
				Self::Normal(dir) => dir_entries(DirWalk::new(
					dir.as_borrowed_cow(),
					ClientLister::<Normal> {
						client,
						context: (),
					},
					state,
				)),
				Self::Shared(dir, role) => dir_entries(DirWalk::new(
					dir.as_borrowed_cow(),
					ClientLister::<Shared> {
						client,
						context: role,
					},
					state,
				)),
				Self::Linked(dir, link) => dir_entries(DirWalk::new(
					dir.as_borrowed_cow(),
					ClientLister::<Linked> {
						client: client.unauthed(),
						context: Cow::Borrowed(link),
					},
					state,
				)),
			}
		}
	}

	/// Downloads a list of mixed-category items to a zip writer.
	#[allow(private_bounds)]
	pub(crate) async fn download_zip_items<T>(
		client: &Client,
		items: Vec<AnyItemWithContext>,
		writer: T,
		progress_callback: Option<&impl ZipProgressCallback>,
	) -> Result<T, Error>
	where
		T: AsyncWrite + Unpin + MaybeSendSync,
	{
		let items = items
			.into_iter()
			.map(ZipItem::try_from)
			.collect::<Result<Vec<_>, _>>()?;
		let initial_file_bytes = items
			.iter()
			.filter_map(|item| match item {
				ZipItem::File(file) => Some(file.size()),
				_ => None,
			})
			.fold(0u64, u64::saturating_add);
		let state = Mutex::new(ZipState::new(
			initial_file_bytes,
			items.len().try_into().expect("items to fit in u64"),
		));
		let entries = stream::iter(&items)
			.map(|item| item.entries(client, &state))
			.flatten();
		write_entries(
			client.unauthed(),
			entries,
			writer,
			&state,
			progress_callback,
		)
		.await
	}
}

#[cfg(feature = "wasm-full")]
mod js_client_impl {
	use wasm_bindgen::{JsValue, prelude::wasm_bindgen};

	use crate::{
		Error, ErrorKind,
		auth::JsClient,
		fs::zip::js_impl::download_zip_items,
		js::{AnyItemWithContext, ManagedFuture, stream_writer},
	};

	#[filen_macros::js_exports]
	#[wasm_bindgen(js_class = "Client")]
	impl JsClient {
		#[wasm_bindgen(js_name = "downloadItemsToZip")]
		pub async fn download_items_to_zip(
			&self,
			items: Vec<AnyItemWithContext>,
			#[wasm_bindgen(unchecked_param_type = "WritableStream<Uint8Array>")]
			writable_stream: web_sys::WritableStream,
			#[wasm_bindgen(
				unchecked_param_type = "(bytesWritten: bigint, totalBytes: bigint, itemsProcessed: bigint, totalItems: bigint) => void | undefined"
			)]
			progress: web_sys::js_sys::Function,
			managed_future: ManagedFuture,
		) -> Result<(), Error> {
			let (writer, result_receiver) = stream_writer(
				writable_stream,
				None::<fn(u64)>,
				"failed to convert WritableStream to AsyncWrite",
			)?;

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
				.into_js_managed_commander_future(move || async move {
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
	use wasm_bindgen::{JsValue, prelude::wasm_bindgen};

	use crate::{
		Error, ErrorKind,
		fs::zip::js_impl::download_zip_items,
		js::{AnyItemWithContext, ManagedFuture, ServiceWorkerClient, stream_writer},
	};

	#[filen_macros::js_exports]
	#[wasm_bindgen(js_class = "Client")]
	impl ServiceWorkerClient {
		#[wasm_bindgen(js_name = "downloadItemsToZip")]
		pub async fn download_items_to_zip(
			&self,
			items: Vec<AnyItemWithContext>,
			#[wasm_bindgen(unchecked_param_type = "WritableStream<Uint8Array>")]
			writable_stream: web_sys::WritableStream,
			#[wasm_bindgen(
				unchecked_param_type = "(bytesWritten: bigint, totalBytes: bigint, itemsProcessed: bigint, totalItems: bigint) => void | undefined"
			)]
			progress: web_sys::js_sys::Function,
			managed_future: ManagedFuture,
		) -> Result<(), Error> {
			let (writer, result_receiver) = stream_writer(
				writable_stream,
				None::<fn(u64)>,
				"failed to convert WritableStream to AsyncWrite",
			)?;

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
	#[cfg(feature = "uniffi")]
	use std::sync::Arc;

	#[cfg(all(target_family = "wasm", target_os = "unknown"))]
	use wasm_bindgen::JsValue;

	use crate::{
		Error,
		auth::js_impls::UnauthJsClient,
		fs::categories::{DirType, Linked, NonRootFileType},
		js::AnyLinkedDirWithContext,
	};
	#[cfg(all(target_family = "wasm", target_os = "unknown"))]
	use crate::{ErrorKind, js::stream_writer};
	#[cfg(feature = "uniffi")]
	use crate::{js::spawn_ordered_dispatch, runtime::do_on_commander};

	#[cfg(feature = "uniffi")]
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
			callback: Option<Arc<dyn ZipDownloadProgressCallback>>,
		) -> Result<Vec<u8>, Error> {
			let Some(callback) = callback else {
				return self
					.inner_download_linked_dir_to_zip(dir, None::<fn(u64, u64, u64, u64)>)
					.await;
			};
			let (sender, delivered) =
				spawn_ordered_dispatch(move |(bw, tb, ip, ti): (u64, u64, u64, u64)| {
					callback.on_progress(bw, tb, ip, ti);
				});
			let result = self
				.inner_download_linked_dir_to_zip(
					dir,
					Some(move |bw: u64, tb: u64, ip: u64, ti: u64| {
						let _ = sender.send((bw, tb, ip, ti));
					}),
				)
				.await;
			// the download dropped its sender: this returns once every report was delivered
			let _ = delivered.await;
			result
		}
	}

	#[filen_macros::js_exports]
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
			let (writer, result_receiver) = stream_writer(
				writable_stream,
				None::<fn(u64)>,
				"failed to convert WritableStream to AsyncWrite",
			)?;

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
			let parsed_dir = DirType::<Linked>::from(dir.dir);
			let link = dir.link;

			managed_future
				.into_js_managed_commander_future(move || async move {
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

#[cfg(test)]
mod tests {
	use super::{is_safe_zip_component, zip_entry_path};

	#[test]
	fn zip_component_rejects_traversal_and_separators() {
		for bad in [
			"..",
			".",
			"",
			"../evil",
			"../../../evil",
			"a/b",
			"a\\b",
			"..\\..\\evil",
			"/etc/passwd",
			"C:evil",
			"c:evil",
		] {
			assert!(!is_safe_zip_component(bad), "{bad:?} should be unsafe");
		}
	}

	#[test]
	fn zip_component_accepts_legitimate_names() {
		for good in [
			"file.txt",
			"..evil",
			"...",
			"a..b",
			"photo (1).jpg",
			"résumé.pdf",
			"file:stream",
		] {
			assert!(is_safe_zip_component(good), "{good:?} should be safe");
		}
	}

	#[test]
	fn zip_entry_path_falls_back_to_uuid_for_unsafe_names() {
		assert_eq!(
			zip_entry_path("parent", "../../evil", "uuid-1"),
			"parent/uuid-1"
		);
		assert_eq!(zip_entry_path("", "../../evil", "uuid-1"), "uuid-1");
		assert_eq!(zip_entry_path("a/b", "c/d", "uuid-2"), "a/b/uuid-2");
	}

	#[test]
	fn zip_entry_path_keeps_safe_names() {
		assert_eq!(
			zip_entry_path("parent", "file.txt", "uuid"),
			"parent/file.txt"
		);
		assert_eq!(zip_entry_path("", "file.txt", "uuid"), "file.txt");
	}
}
