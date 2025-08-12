use std::{path::PathBuf, sync::Arc};

use filen_sdk_rs::{
	fs::{
		HasUUID,
		file::{RemoteFile, traits::HasFileInfo},
	},
	io::FilenMetaExt,
};
use futures::{StreamExt, stream::FuturesUnordered};
use image::ImageError;
use log::debug;
use tokio::sync::OwnedRwLockReadGuard;

use crate::{
	CacheError,
	auth::{AuthCacheState, CacheState, FilenMobileCacheState},
	ffi::FfiId,
	sql::{self, object::DBObject},
};

impl AuthCacheState {
	async fn get_or_make_thumbnail(
		&self,
		file: &RemoteFile,
		target_width: u32,
		target_height: u32,
	) -> Result<Option<PathBuf>, CacheError> {
		let Some(mime) = file.mime() else {
			debug!("File has no mime type, no thumbnail will be made");
			return Ok(None);
		};

		if !mime.starts_with("image/") {
			debug!("File is not an image, no thumbnail will be made: {mime}");
			return Ok(None);
		}
		let uuid_str = file.uuid().to_string();
		let file_path = self.cache_dir.join(&uuid_str);
		let file_thumbnails_path = self.thumbnail_dir.join(&uuid_str);
		tokio::fs::create_dir_all(&file_thumbnails_path).await?;
		let thumbnail_path =
			file_thumbnails_path.join(format!("{target_width}x{target_height}.webp"));
		let thumbnail_file = tokio::fs::OpenOptions::new()
			.append(true)
			.create(true)
			.open(&thumbnail_path)
			.await?;
		debug!("made thumbnail path: {}", thumbnail_path.display());
		if FilenMetaExt::size(&thumbnail_file.metadata().await?) != 0 {
			return Ok(Some(thumbnail_path));
		}
		let image_file = match tokio::fs::File::open(&file_path).await {
			Ok(file) => file,
			Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
				debug!(
					"Thumbnail file not found, downloading: {}",
					file_path.display()
				);
				let path = self.download_file_io(file, None).await?;
				tokio::fs::File::open(&path).await?
			}
			Err(e) => {
				debug!(
					"Failed to open file for thumbnail: {} at path {}",
					e,
					file_path.display()
				);
				return Err(e.into());
			}
		};

		let (os_file, mut thumbnail_file) =
			futures::join!(image_file.into_std(), thumbnail_file.into_std());

		let mime = file.mime().map(|m| m.to_string());
		let size = file.size();

		if let Err(e) =
			tokio::task::spawn_blocking(move || -> Result<(), filen_sdk_rs::error::Error> {
				let image_reader = std::io::BufReader::new(os_file);
				filen_sdk_rs::thumbnail::make_thumbnail(
					mime.as_deref(),
					size,
					image_reader,
					target_width,
					target_height,
					&mut thumbnail_file,
				)?;
				Ok(())
			})
			.await
			.unwrap()
		{
			tokio::fs::remove_file(&thumbnail_path).await?;
			if let filen_sdk_rs::error::Error::ImageError(ImageError::Unsupported(_)) = e {
				Ok(None)
			} else {
				Err(CacheError::from(e))
			}
		} else {
			Ok(Some(thumbnail_path))
		}
	}

	async fn make_thumbnail_for_path(
		&self,
		path: &FfiId,
		requested_width: u32,
		requested_height: u32,
	) -> ThumbnailResult {
		let pvs = match path.as_parsed() {
			Ok(pvs) => pvs,
			Err(e) => return ThumbnailResult::Err(e),
		};
		let file = {
			let conn: std::sync::MutexGuard<'_, rusqlite::Connection> = self.conn();
			match sql::select_object_at_parsed_id(&conn, &pvs) {
				Ok(Some(DBObject::File(file))) => file,
				Ok(Some(_)) => return ThumbnailResult::NoThumbnail,
				Ok(None) => return ThumbnailResult::NotFound,
				Err(e) => return ThumbnailResult::Err(e),
			}
		};

		let remote_file = match file.try_into() {
			Ok(remote_file) => remote_file,
			Err(e) => return ThumbnailResult::Err(CacheError::from(e)),
		};
		match self
			.get_or_make_thumbnail(&remote_file, requested_width, requested_height)
			.await
		{
			Ok(Some(path)) => ThumbnailResult::Ok(path.to_string_lossy().to_string()),
			Ok(None) => ThumbnailResult::NoThumbnail,
			Err(e) => ThumbnailResult::Err(e),
		}
	}
}

#[derive(uniffi::Enum)]
pub enum ThumbnailResult {
	Ok(String),
	Err(CacheError),
	NotFound,
	NoThumbnail,
}

impl From<CacheError> for ThumbnailResult {
	fn from(e: CacheError) -> Self {
		ThumbnailResult::Err(e)
	}
}

#[uniffi::export(with_foreign)]
pub trait ThumbnailCallback: Send + Sync {
	fn process(&self, id: FfiId, result: ThumbnailResult);
	fn complete(&self);
}

#[derive(uniffi::Object)]
pub struct BulkThumbnailResponse {
	task: tokio::task::JoinHandle<()>,
}

#[uniffi::export]
impl BulkThumbnailResponse {
	pub fn cancel(&self) {
		if !self.task.is_finished() {
			self.task.abort();
		}
	}
}

impl AuthCacheState {
	pub(crate) fn get_thumbnails(
		this: OwnedRwLockReadGuard<CacheState, Self>,
		items: Vec<FfiId>,
		requested_width: u32,
		requested_height: u32,
		callback: Arc<dyn ThumbnailCallback>,
	) -> BulkThumbnailResponse {
		let arc = Arc::new(this);
		let handle = crate::env::get_runtime().spawn(async move {
			let mut futures = FuturesUnordered::new();
			for item in items {
				let self_ref = arc.clone();
				let callback_ref = callback.clone();
				futures.push(async move {
					let result = self_ref
						.make_thumbnail_for_path(&item, requested_width, requested_height)
						.await;
					callback_ref.process(item, result);
				});
			}
			while (futures.next().await).is_some() {}
			callback.complete();
		});

		BulkThumbnailResponse { task: handle }
	}
}

#[uniffi::export]
impl FilenMobileCacheState {
	pub fn get_thumbnails(
		self: Arc<Self>,
		items: Vec<FfiId>,
		requested_width: u32,
		requested_height: u32,
		callback: Arc<dyn ThumbnailCallback>,
	) -> Result<BulkThumbnailResponse, CacheError> {
		self.sync_execute_authed_owned(move |auth_state| {
			Ok(AuthCacheState::get_thumbnails(
				auth_state,
				items,
				requested_width,
				requested_height,
				callback,
			))
		})
	}
}

#[filen_macros::create_uniffi_wrapper]
impl FilenMobileCacheState {
	// not sure why this is necessary for this specific function,
	// but otherwise it seems like the macro wasn't adding this
	#[uniffi::method(name = "get_thumbnail")]
	pub async fn get_thumbnail(
		self: Arc<Self>,
		item: FfiId,
		requested_width: u32,
		requested_height: u32,
	) -> Result<ThumbnailResult, CacheError> {
		self.async_execute_authed_owned(async move |auth_state| {
			Ok(auth_state
				.make_thumbnail_for_path(&item, requested_width, requested_height)
				.await)
		})
		.await
	}
}
