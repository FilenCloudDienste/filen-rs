use std::{borrow::Cow, sync::Arc};

use chrono::Utc;
use filen_types::{
	crypto::Blake3Hash,
	fs::{ParentUuid, StableUuid, Uuid},
};
#[cfg(feature = "multi-threaded-crypto")]
use rayon::iter::ParallelIterator;

use crate::{
	api,
	auth::{Client, shared_client::SharedClient},
	connect::ParentTargets,
	consts::CHUNK_SIZE_U64,
	crypto::{error::ConversionError, shared::MetaCrypter},
	error::{Error, ErrorKind, MetadataWasNotDecryptedError, ResultExt},
	fs::{
		HasUUID,
		categories::{DirType, Normal},
		file::{
			FileVersion, FileWithInfo,
			meta::{DecryptedFileMeta, FileMeta, FileMetaChanges},
			traits::HasFileMeta,
			write::FileWriterDefault,
		},
		name::{EntryNameError, ValidatedName},
	},
	runtime::{self, blocking_join, do_cpu_intensive},
	util::{IntoMaybeParallelIterator, MaybeSend, MaybeSendCallback},
};

use super::exif::{ExifTeeState, mime_to_exif_kind};
use super::{
	BaseFile, FileBuilder, RemoteFile,
	read::{FileReader, FileReaderBuilder},
	traits::{File, UpdateFileMeta},
	write::FileWriter,
};

/// Build an `ExifTeeState` from a `FileBuilder` if its flags + effective MIME
/// indicate EXIF parsing should run.
pub(crate) fn build_exif_tee_from_builder(
	builder: FileBuilder,
) -> (Option<ExifTeeState>, BaseFile) {
	if !builder.get_parse_exif() {
		return (None, builder.build());
	}

	let user_created = builder.get_created();
	let user_modified = builder.get_modified();
	let override_with_exif = builder.get_override_with_exif();
	let fallback_time = builder.get_builder_creation_time();

	let base_file = builder.build();

	let kind = match mime_to_exif_kind(base_file.mime()) {
		Some(k) => k,
		None => return (None, base_file),
	};

	(
		Some(ExifTeeState::new(
			kind,
			override_with_exif,
			user_created,
			user_modified,
			fallback_time,
		)),
		base_file,
	)
}

impl Client {
	pub async fn trash_file(&self, file: &mut RemoteFile) -> Result<(), Error> {
		let _lock = self.lock_drive().await?;
		api::v3::file::trash::post(
			self.client(),
			&api::v3::file::trash::Request { uuid: file.uuid() },
		)
		.await?;
		// Remember the original parent so the trashed file knows where it came from. An
		// already-Trash parent stays as it is: the server call above is an idempotent no-op for
		// an already-trashed file (a replay, or a cross-device race), and re-deriving the
		// original parent from a Trash value is impossible — erroring here turned the replay
		// into a spurious failure.
		if !file.parent.is_trash() {
			file.parent = ParentUuid::Trash(
				Uuid::try_from(file.parent).context("setting parent when trashing file")?,
			);
		}
		Ok(())
	}

	pub async fn restore_file(&self, file: &mut RemoteFile) -> Result<(), Error> {
		let lock = self.lock_drive().await?;
		api::v3::file::restore::post(
			self.client(),
			&api::v3::file::restore::Request { uuid: file.uuid() },
		)
		.await?;
		// api v3 doesn't return the parentUUID we returned to, so we query it separately for now
		let resp =
			api::v3::file::post(self.client(), &api::v3::file::Request { uuid: file.uuid() })
				.await?;

		file.parent = resp.parent;
		// The caller's `file` came over the FFI, so its stable id is not
		// trustworthy — heal it from the server's response like
		// `get_file_with_info` and `restore_file_version` do.
		file.stable_uuid = resp.stable_uuid;
		// Mirror the restored file's metadata into the (now current) parent's
		// shares/links (as upload does); otherwise recipients of a connected parent
		// cannot see or decrypt the restored file.
		self.update_item_with_maybe_connected_parent(&lock, (&*file).into(), ParentTargets::All)
			.await?;
		Ok(())
	}

	pub async fn delete_file_permanently(&self, file: RemoteFile) -> Result<(), Error> {
		let _lock = self.lock_drive().await?;
		api::v3::file::delete::permanent::post(
			self.client(),
			&api::v3::file::delete::permanent::Request { uuid: file.uuid() },
		)
		.await
	}

	pub async fn move_file(
		&self,
		file: &mut RemoteFile,
		new_parent: &DirType<'_, Normal>,
	) -> Result<(), Error> {
		self.inner_move_file(file, new_parent.uuid()).await
	}

	/// [`move_file`](Self::move_file) into the directory `new_parent`, by its uuid alone, which is
	/// all a move reads of it.
	pub(crate) async fn inner_move_file(
		&self,
		file: &mut RemoteFile,
		new_parent: Uuid,
	) -> Result<(), Error> {
		let lock = self.lock_drive().await?;
		api::v3::file::r#move::post(
			self.client(),
			&api::v3::file::r#move::Request {
				uuid: file.uuid(),
				new_parent,
			},
		)
		.await?;
		file.parent = new_parent.into();
		// Mirror the moved file's metadata into the new parent's shares/links (as
		// upload does); otherwise recipients of a shared or publicly-linked
		// destination cannot see or decrypt the moved-in file.
		self.update_item_with_maybe_connected_parent(&lock, (&*file).into(), ParentTargets::All)
			.await?;
		Ok(())
	}

	pub async fn update_file_metadata(
		&self,
		file: &mut RemoteFile,
		changes: FileMetaChanges,
	) -> Result<(), Error> {
		let lock = self.lock_drive().await?;

		let temp_meta = file.get_meta().borrow_with_changes(&changes)?;
		let FileMeta::Decoded(temp_meta) = temp_meta else {
			return Err(MetadataWasNotDecryptedError.into());
		};

		let request = self.file_metadata_request(file.uuid(), &temp_meta).await?;
		api::v3::file::metadata::post(self.client(), &request).await?;

		file.update_meta(changes)?;

		self.update_maybe_connected_item(&lock, file).await?;
		Ok(())
	}

	/// The `file/metadata` request that gives the file `uuid` the metadata `meta`: its name
	/// encrypted with the file's own key, the name hash and the metadata encrypted with the
	/// account's key.
	pub(crate) async fn file_metadata_request(
		&self,
		uuid: Uuid,
		meta: &DecryptedFileMeta<'_>,
	) -> Result<api::v3::file::metadata::Request<'static>, Error> {
		let crypter = self.crypter();

		let (name, metadata) = do_cpu_intensive(|| {
			blocking_join!(
				|| Ok::<_, ConversionError>(
					meta.key()
						.to_meta_key()?
						.blocking_encrypt_meta(meta.name.as_ref())
				),
				|| {
					let meta_json = serde_json::to_string(meta)?;
					Ok::<_, Error>(crypter.blocking_encrypt_meta(&meta_json))
				}
			)
		})
		.await;

		Ok(api::v3::file::metadata::Request {
			uuid,
			name: name?,
			name_hashed: Cow::Owned(self.hash_name(meta.name())),
			metadata: metadata?,
		})
	}

	pub async fn get_file(&self, uuid: Uuid) -> Result<RemoteFile, Error> {
		Ok(self.get_file_with_info(uuid).await?.file)
	}

	pub async fn get_file_with_info(&self, uuid: Uuid) -> Result<FileWithInfo, Error> {
		let response = api::v3::file::post(self.client(), &api::v3::file::Request { uuid }).await?;
		let versioned = response.versioned;
		let trash = response.trash;
		// The requested uuid, not `response.uuid`: for a superseded (archived) uuid the
		// caller asked about THAT row, and the response's stable id is the lineage's.
		let file = self.decrypt_file_response(uuid, response).await?;
		Ok(FileWithInfo {
			file,
			versioned,
			trash,
		})
	}

	/// Fetch the CURRENT head of a file's lineage by its whole-life id. A file's `uuid` is
	/// re-minted on every content edit, so this is the only way to follow a file across edits;
	/// the returned file carries the head's (possibly new) uuid.
	///
	/// A lineage the server does not know answers [`crate::ErrorKind::FileNotFound`], and only
	/// that error means gone — every other failure is a failure to ask.
	pub async fn get_file_by_stable_uuid(
		&self,
		stable_uuid: StableUuid,
	) -> Result<RemoteFile, Error> {
		let response = api::v3::file::stable::post(
			self.client(),
			&api::v3::file::stable::Request { stable_uuid },
		)
		.await?;
		// The head's own uuid — the whole point of the by-stable fetch.
		let uuid = response.uuid;
		self.decrypt_file_response(uuid, response).await
	}

	// Test seam: the live tests send `bulk/file/stable` to the server. Gated behind the
	// `test-seams` feature, which production builds never enable.
	/// [`Client::get_file_by_stable_uuid`] for each of `items` in one `bulk/file/stable` request,
	/// answered in the same order and decrypted as the single fetch decrypts its file. It takes no
	/// lock, as a read. Test code only.
	#[cfg(feature = "test-seams")]
	pub async fn get_files_by_stable_uuid_for_test(
		&self,
		items: &[api::v3::bulk::file::stable::Item],
	) -> Result<Vec<Result<RemoteFile, Error>>, Error> {
		let responses = crate::fs::bulk::test_seams::answered_by_bulk(
			api::v3::bulk::file::stable::ENDPOINT,
			api::v3::bulk::file::stable::post(
				self.client(),
				&api::v3::bulk::file::stable::Request { items },
			)
			.await,
			|item| api::v3::file::stable::post(self.client(), item),
		)
		.await?;
		let mut files = Vec::with_capacity(responses.len());
		for response in responses {
			files.push(match response {
				Ok(response) => self.decrypt_file_response(response.uuid, response).await,
				Err(e) => Err(e),
			});
		}
		Ok(files)
	}

	/// Decrypt one single-file API response into a [`RemoteFile`] under `uuid`. Shared by the
	/// by-uuid and by-stable-id fetches, which differ only in which uuid the row is filed under.
	async fn decrypt_file_response(
		&self,
		uuid: Uuid,
		response: api::v3::file::Response<'static>,
	) -> Result<RemoteFile, Error> {
		let meta = runtime::do_cpu_intensive(|| {
			FileMeta::blocking_from_encrypted(response.metadata, &*self.crypter(), response.version)
		})
		.await;
		Ok(RemoteFile::from_meta(
			uuid,
			// For a superseded (archived) uuid this is the lineage's stable id
			// resolved from the live head — a single call recovers the file's
			// identity after a remote edit.
			response.stable_uuid,
			// v3 api returns the original parent as the parent if the file is in the trash;
			// keep it as the remembered original parent instead of discarding it.
			if response.trash {
				ParentUuid::Trash(Uuid::try_from(response.parent).context("getting trashed file")?)
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
		parent: &DirType<'_, Normal>,
	) -> Result<Option<Uuid>, Error> {
		// Hash the NFC-normalized name (as the upload path does via ValidatedName) so an
		// NFD-decomposed query still matches a file stored under its NFC form.
		self.inner_file_exists(&ValidatedName::try_from(name)?, parent.uuid())
			.await
	}

	pub(crate) async fn inner_file_exists(
		&self,
		name: &ValidatedName,
		parent: Uuid,
	) -> Result<Option<Uuid>, Error> {
		api::v3::file::exists::post(
			self.client(),
			&api::v3::file::exists::Request {
				name_hashed: self.hash_name(name.as_ref()),
				parent: parent.into(),
			},
		)
		.await
		.map(|r| r.0)
	}

	pub fn make_file_builder(
		&self,
		name: &str,
		parent_uuid: Uuid,
	) -> Result<FileBuilder, EntryNameError> {
		FileBuilder::new(name, parent_uuid, self)
	}

	#[allow(clippy::too_many_arguments)]
	pub(crate) fn inner_get_file_writer<'a, F, Fut>(
		&'a self,
		file: Arc<BaseFile>,
		callback: Option<MaybeSendCallback<'a, u64>>,
		size: Option<u64>,
		confirm_completion_callback: Option<F>,
		exif_tee: Option<ExifTeeState>,
	) -> FileWriter<'a, F, Fut>
	where
		F: FnOnce(Blake3Hash, u64) -> Fut + MaybeSend + 'a,
		Fut: Future<Output = Result<(), Error>> + MaybeSend + 'a,
	{
		FileWriter::new(
			file,
			self,
			callback,
			size,
			confirm_completion_callback,
			exif_tee,
		)
	}

	pub fn get_file_writer(&self, builder: FileBuilder) -> FileWriterDefault<'_> {
		let (exif_tee, base_file) = build_exif_tee_from_builder(builder);
		let base_file = Arc::new(base_file);
		self.inner_get_file_writer(base_file, None, None, None, exif_tee)
	}

	pub fn get_file_writer_with_callback<'a>(
		&'a self,
		builder: FileBuilder,
		callback: MaybeSendCallback<'a, u64>,
	) -> FileWriterDefault<'a> {
		let (exif_tee, base_file) = build_exif_tee_from_builder(builder);
		let base_file = Arc::new(base_file);
		self.inner_get_file_writer(base_file, Some(callback), None, None, exif_tee)
	}

	pub async fn list_file_versions(&self, file: &RemoteFile) -> Result<Vec<FileVersion>, Error> {
		let response = api::v3::file::versions::post(
			self.client(),
			&api::v3::file::versions::Request { uuid: file.uuid() },
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
		let lock = self.lock_drive().await?;
		let response = match api::v3::file::version::restore::post(
			self.client(),
			&api::v3::file::version::restore::Request {
				current: file.uuid(),
				uuid: version.uuid,
			},
		)
		.await
		{
			Ok(response) => response,
			// The server accepts a stale `current` only for the recognized
			// lost-response retry pattern (and restoring the already-current
			// version is a success no-op). Any other stale `current` is
			// rejected with invalid_params: our view of the file raced a newer
			// edit — refresh the file and its versions, then retry.
			//
			// invalid_params is this endpoint's generic validation bucket
			// though, and a `uuid` the server no longer accepts as a version of
			// this file lands in it too. Only the raced case is worth retrying,
			// so the message has to say it was `current` that was rejected — a
			// caller that retries on StaleState alone would otherwise spin
			// forever on a restore that can never succeed.
			//
			// The wording ("Invalid current UUID.") is the only discriminator
			// the endpoint offers; there is no head-by-lineage query to confirm
			// it against, and v3/file/versions answers for the exact uuid it is
			// given, so it cannot tell us the head moved. If the server ever
			// rewords this, the mapping stops firing and the raw server error
			// surfaces — the safe direction to fail in.
			Err(e) if e.server_code().as_deref() == Some("invalid_params") => {
				let stale_current = e
					.server_message()
					.is_some_and(|m| m.to_ascii_lowercase().contains("current"));
				return Err(if stale_current {
					Error::custom_with_source(
						ErrorKind::StaleState,
						e,
						Some(
							"file version restore raced a newer change, refresh the file and retry",
						),
					)
				} else {
					e
				});
			}
			Err(e) => return Err(e),
		};
		// `response.uuid` echoes the restored version, which is the live head
		// after the call — including the no-op resolutions (restoring the
		// already-current version, retrying a lost restore), where the server
		// reports the head it settled on. The response's storage fields
		// describe the DISPLACED head, not the restored version (observed
		// live: downloading the restored uuid from the response's bucket
		// 404s), so the data location and metadata come from the version row
		// we were handed; the response contributes the settled head uuid and
		// the lineage's stable id.
		file.uuid = response.uuid;
		file.stable_uuid = response.stable_uuid;
		file.bucket = version.bucket;
		file.region = version.region;
		file.size = version.size;
		file.chunks = version.chunks;
		file.timestamp = version.timestamp;
		file.meta = version.metadata;
		// need to do this or the old sync engine doesn't work properly because it relies purely on modtime.
		// The touch writes back the version's metadata, as the caller read it, with a new
		// modification time. While this lock's lease is confirmed nobody else changed it, and
		// update_file_metadata gets this lock from the client's lock cache.
		match lock.confirm().await {
			Ok(()) => {
				self.update_file_metadata(
					file,
					FileMetaChanges::default().last_modified(Utc::now()),
				)
				.await
			}
			Err(error) if error.kind() == ErrorKind::LockLost => {
				drop(lock);
				tracing::warn!("{error}; touching the restored file under a new lock");
				self.touch_restored_file(file).await
			}
			Err(error) => Err(error),
		}
	}

	/// Gives `file`, a version restore that landed, a new modification time after the drive
	/// lock the restore was made under lost its lease. Another client may have changed the
	/// file's metadata since (a rename), so it is read again under a new lock and only the
	/// modification time is changed; a file that is no longer the current version is left as is.
	async fn touch_restored_file(&self, file: &mut RemoteFile) -> Result<(), Error> {
		// held across the read and the touch, which gets it from the client's lock cache
		let _lock = self.lock_drive().await?;
		let current = self.get_file_with_info(file.uuid()).await?;
		if current.versioned {
			tracing::warn!(
				"{} is no longer the current version, leaving its modification time",
				file.uuid()
			);
			return Ok(());
		}
		let mut current = current.file;
		self.update_file_metadata(
			&mut current,
			FileMetaChanges::default().last_modified(Utc::now()),
		)
		.await?;
		*file = current;
		Ok(())
	}

	pub async fn delete_file_version(&self, version: FileVersion) -> Result<(), Error> {
		let _lock = self.lock_drive().await?;
		api::v3::file::delete::permanent::post(
			self.client(),
			&api::v3::file::delete::permanent::Request { uuid: version.uuid },
		)
		.await
	}

	#[cfg(feature = "malformed")]
	pub async fn create_malformed_file(
		&self,
		parent: &DirType<'_, Normal>,
		name: &str,
		meta: &str,
		mime: &str,
		size: &str,
	) -> Result<Uuid, Error> {
		use filen_types::crypto::EncryptedString;
		let uuid = Uuid::new_v4();
		api::v3::upload::empty::post(
			self.client(),
			&api::v3::upload::empty::Request {
				name_hashed: Cow::Owned(self.hash_name(name)),
				uuid,
				parent: parent.uuid(),
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

	/// Downloads a single file chunk by raw `region`/`bucket`/`uuid`/`chunk_idx`,
	/// bypassing any local file metadata.
	///
	/// Exposed for tests that need to exercise the chunk-download path with
	/// arbitrary identifiers (e.g. a UUID that does not exist on the backend,
	/// to verify the `FileChunkNotFound` mapping).
	#[cfg(feature = "malformed")]
	pub async fn download_file_chunk_by_uuid(
		&self,
		region: &str,
		bucket: &str,
		uuid: Uuid,
		chunk_idx: u64,
	) -> Result<Vec<u8>, Error> {
		api::download::download_file_chunk_by_uuid::<fn(u64, Option<u64>)>(
			self.unauthed(),
			region,
			bucket,
			uuid,
			chunk_idx,
			None,
		)
		.await
	}
}

#[allow(private_bounds)]
pub trait FileReaderSharedClientExt<'a>: SharedClient {
	fn get_file_reader(&'a self, file: &'a dyn File) -> FileReader<'a> {
		FileReader::new(file, self.get_unauth_client())
	}

	fn get_file_reader_for_range(
		&'a self,
		file: &'a dyn File,
		start: u64,
		end: u64,
	) -> FileReader<'a> {
		FileReader::new_for_range(file, self.get_unauth_client(), start, end)
	}

	/// Like [`get_file_reader_for_range`](Self::get_file_reader_for_range) but reports each chunk's
	/// plaintext length to `callback` as it finishes downloading (not at read-out), so progress is
	/// smooth instead of bursting when the ordered reader releases head-of-line-blocked chunks.
	fn get_file_reader_for_range_with_callback(
		&'a self,
		file: &'a dyn File,
		start: u64,
		end: u64,
		callback: Option<MaybeSendCallback<'a, u64>>,
	) -> FileReader<'a> {
		FileReaderBuilder::new(self.get_unauth_client(), file)
			.with_start(start)
			.with_end(end)
			.with_progress_callback(callback)
			.build()
	}
}

impl<'a, T> FileReaderSharedClientExt<'a> for T where T: SharedClient {}
