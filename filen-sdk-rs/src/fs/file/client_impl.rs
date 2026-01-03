use std::{borrow::Cow, sync::Arc};

use chrono::Utc;
use filen_types::fs::{ParentUuid, UuidStr};
use futures::AsyncRead;
#[cfg(feature = "multi-threaded-crypto")]
use rayon::iter::ParallelIterator;

use crate::{
	api,
	auth::Client,
	consts::CHUNK_SIZE_U64,
	crypto::{error::ConversionError, shared::MetaCrypter},
	error::{Error, InvalidNameError, MetadataWasNotDecryptedError},
	fs::{
		HasUUID,
		dir::HasUUIDContents,
		file::{
			FileVersion,
			meta::{FileMeta, FileMetaChanges},
			traits::HasFileMeta,
		},
	},
	runtime::{self, blocking_join, do_cpu_intensive},
	util::{IntoMaybeParallelIterator, MaybeSendCallback},
};

use super::{
	BaseFile, FileBuilder, RemoteFile,
	read::FileReader,
	traits::{File, UpdateFileMeta},
	write::FileWriter,
};

impl Client {
	pub async fn trash_file(&self, file: &mut RemoteFile) -> Result<(), Error> {
		let _lock = self.lock_drive().await?;
		api::v3::file::trash::post(self, &api::v3::file::trash::Request { uuid: *file.uuid() })
			.await?;
		file.parent = ParentUuid::Trash;
		Ok(())
	}

	pub async fn restore_file(&self, file: &mut RemoteFile) -> Result<(), Error> {
		let _lock = self.lock_drive().await?;
		api::v3::file::restore::post(
			self,
			&api::v3::file::restore::Request { uuid: *file.uuid() },
		)
		.await?;
		// api v3 doesn't return the parentUUID we returned to, so we query it separately for now
		let resp =
			api::v3::file::post(self, &api::v3::file::Request { uuid: *file.uuid() }).await?;

		file.parent = resp.parent;
		Ok(())
	}

	pub async fn delete_file_permanently(&self, file: RemoteFile) -> Result<(), Error> {
		let _lock = self.lock_drive().await?;
		api::v3::file::delete::permanent::post(
			self,
			&api::v3::file::delete::permanent::Request { uuid: *file.uuid() },
		)
		.await
	}

	pub async fn move_file(
		&self,
		file: &mut RemoteFile,
		new_parent: &impl HasUUIDContents,
	) -> Result<(), Error> {
		let _lock = self.lock_drive().await?;
		api::v3::file::r#move::post(
			self,
			&api::v3::file::r#move::Request {
				uuid: *file.uuid(),
				new_parent: *new_parent.uuid(),
			},
		)
		.await?;
		file.parent = (*new_parent.uuid()).into();
		Ok(())
	}

	pub async fn update_file_metadata(
		&self,
		file: &mut RemoteFile,
		changes: FileMetaChanges,
	) -> Result<(), Error> {
		let _lock = self.lock_drive().await?;

		let temp_meta = file.get_meta().borrow_with_changes(&changes)?;
		let FileMeta::Decoded(temp_meta) = temp_meta else {
			return Err(MetadataWasNotDecryptedError.into());
		};

		let crypter = self.crypter();

		let (name, metadata) = do_cpu_intensive(|| {
			blocking_join!(
				|| Ok::<_, ConversionError>(
					temp_meta
						.key()
						.to_meta_key()?
						.blocking_encrypt_meta(&temp_meta.name)
				),
				|| {
					let meta_json = serde_json::to_string(&temp_meta)?;
					Ok::<_, Error>(crypter.blocking_encrypt_meta(&meta_json))
				}
			)
		})
		.await;

		api::v3::file::metadata::post(
			self,
			&api::v3::file::metadata::Request {
				uuid: *file.uuid(),
				name: name?,
				name_hashed: Cow::Owned(self.hash_name(temp_meta.name())),
				metadata: metadata?,
			},
		)
		.await?;

		file.update_meta(changes)?;

		self.update_maybe_connected_item(file).await?;
		Ok(())
	}

	pub async fn get_file(&self, uuid: UuidStr) -> Result<RemoteFile, Error> {
		let response = api::v3::file::post(self, &api::v3::file::Request { uuid }).await?;
		let meta = runtime::do_cpu_intensive(|| {
			FileMeta::blocking_from_encrypted(response.metadata, &*self.crypter(), response.version)
		})
		.await;
		Ok(RemoteFile::from_meta(
			uuid,
			// v3 api returns the original parent as the parent if the file is in the trash
			if response.trash {
				ParentUuid::Trash
			} else {
				response.parent
			},
			response.size,
			response.size.div_ceil(CHUNK_SIZE_U64),
			response.region,
			response.bucket,
			response.timestamp,
			response.favorited,
			meta,
		))
	}

	pub async fn file_exists(
		&self,
		name: &str,
		parent: &impl HasUUIDContents,
	) -> Result<Option<UuidStr>, Error> {
		api::v3::file::exists::post(
			self,
			&api::v3::file::exists::Request {
				name_hashed: self.hash_name(name),
				parent: (*parent.uuid()).into(),
			},
		)
		.await
		.map(|r| r.0)
	}

	pub fn get_file_reader<'a>(&'a self, file: &'a dyn File) -> impl AsyncRead + 'a {
		FileReader::new(file, self)
	}

	pub fn get_file_reader_for_range<'a>(
		&'a self,
		file: &'a dyn File,
		start: u64,
		end: u64,
	) -> impl AsyncRead + 'a {
		FileReader::new_for_range(file, self, start, end)
	}

	pub fn make_file_builder(
		&self,
		name: impl Into<String>,
		parent: &impl HasUUIDContents,
	) -> FileBuilder {
		FileBuilder::new(name, parent, self)
	}

	pub(crate) fn inner_get_file_writer<'a>(
		&'a self,
		file: Arc<BaseFile>,
		callback: Option<MaybeSendCallback<'a, u64>>,
		size: Option<u64>,
	) -> Result<FileWriter<'a>, Error> {
		if file.root.name.is_empty() {
			let name = match Arc::try_unwrap(file).map(|f| f.root.name) {
				Ok(name) => name,
				Err(file) => file.name().to_string(),
			};
			Err(InvalidNameError(name).into())
		} else {
			Ok(FileWriter::new(file, self, callback, size))
		}
	}

	pub fn get_file_writer(&self, file: impl Into<Arc<BaseFile>>) -> Result<FileWriter<'_>, Error> {
		self.inner_get_file_writer(file.into(), None, None)
	}

	pub fn get_file_writer_with_callback<'a>(
		&'a self,
		file: impl Into<Arc<BaseFile>>,
		callback: MaybeSendCallback<'a, u64>,
	) -> Result<FileWriter<'a>, Error> {
		self.inner_get_file_writer(file.into(), Some(callback), None)
	}

	pub async fn list_file_versions(&self, file: &RemoteFile) -> Result<Vec<FileVersion>, Error> {
		let response = api::v3::file::versions::post(
			self,
			&api::v3::file::versions::Request { uuid: *file.uuid() },
		)
		.await?;
		let crypter = self.crypter();
		do_cpu_intensive(move || {
			let mut versions: Vec<FileVersion> = response
				.versions
				.into_maybe_par_iter()
				.map(|v| FileVersion::blocking_from_response(&*crypter, v))
				.collect();

			// newest first
			versions.sort_by_key(|v| -v.timestamp().timestamp());
			Ok(versions)
		})
		.await
	}

	pub async fn restore_file_version(
		&self,
		file: &mut RemoteFile,
		version: FileVersion,
	) -> Result<(), Error> {
		let _lock = self.lock_drive().await?;
		api::v3::file::version::restore::post(
			self,
			&api::v3::file::version::restore::Request {
				current: *file.uuid(),
				uuid: version.uuid,
			},
		)
		.await?;

		file.bucket = version.bucket;
		file.region = version.region;
		file.size = version.size;
		file.chunks = version.chunks;
		file.timestamp = version.timestamp;
		file.meta = version.metadata;
		file.uuid = version.uuid;
		// need to do this or the old sync engine doesn't work properly because it relies purely on modtime.
		self.update_file_metadata(file, FileMetaChanges::default().last_modified(Utc::now()))
			.await?;
		Ok(())
	}

	#[cfg(feature = "malformed")]
	pub async fn create_malformed_file(
		&self,
		parent: &impl HasUUIDContents,
		name: &str,
		meta: &str,
		mime: &str,
		size: &str,
	) -> Result<UuidStr, Error> {
		use filen_types::crypto::EncryptedString;
		let uuid = UuidStr::new_v4();
		api::v3::upload::empty::post(
			self,
			&api::v3::upload::empty::Request {
				name_hashed: Cow::Owned(self.hash_name(name)),
				uuid,
				parent: *parent.uuid(),
				metadata: EncryptedString(Cow::Borrowed(meta)),
				name: EncryptedString(Cow::Borrowed(name)),
				size: EncryptedString(Cow::Borrowed(size)),
				mime: EncryptedString(Cow::Borrowed(mime)),
				version: filen_types::auth::FileEncryptionVersion::V2,
			},
		)
		.await?;
		Ok(uuid)
	}
}
