use std::{borrow::Cow, fmt::Debug};

use chrono::{DateTime, Utc};
use filen_sdk_rs::{
	crypto::{error::ConversionError, file::FileKey},
	fs::{
		HasName, HasParent, HasRemoteInfo, HasUUID,
		file::{
			RemoteFile,
			meta::{DecryptedFileMeta, FileMeta},
			traits::{HasFileInfo, HasFileMeta, HasRemoteFileInfo},
		},
	},
};
use filen_types::{
	auth::FileEncryptionVersion,
	crypto::{EncryptedString, rsa::RSAEncryptedString},
	fs::{ParentUuid, StableUuid, Uuid},
	traits::CowHelpers,
};
use rusqlite::{CachedStatement, Connection, Result};
use tracing::trace;

use crate::{
	ffi::ItemType,
	sql::{
		MetaState, SQLError,
		columns::{
			FILE_CREATED, FILE_FAVORITE_RANK, FILE_METADATA_STATE, FILE_NAME, FILE_RAW_METADATA,
			FILE_TIMESTAMP, FILES_BUCKET, FILES_CHUNKS, FILES_HASH, FILES_KEY, FILES_KEY_VERSION,
			FILES_MIME, FILES_MODIFIED, FILES_REGION, FILES_SIZE, ITEMS_PENDING_UPLOAD_AT,
			ITEMS_STABLE_UUID,
		},
		item::{self, DBItemTrait, InnerDBItem},
		object::{DBObject, JsonObject},
		raw_meta_and_state_from_file_meta,
		statements::*,
	},
};

type SQLResult<T> = std::result::Result<T, SQLError>;

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct DBDecryptedFileMeta {
	pub(crate) name: String,
	pub(crate) mime: String,
	pub(crate) key: String,
	pub(crate) key_version: u8,
	pub(crate) modified: i64,
	pub(crate) created: Option<i64>,
	pub(crate) hash: Option<[u8; 32]>,
}

impl Debug for DBDecryptedFileMeta {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		let key_hash_str = hex::encode(blake3::hash(self.key.as_bytes()).as_bytes());
		let hash_hashed_str = self.hash.map(hex::encode);

		f.debug_struct("DBDecryptedFileMeta")
			.field("name", &self.name)
			.field("mime", &self.mime)
			.field("key (hashed)", &key_hash_str)
			.field("key_version", &self.key_version)
			.field("created", &self.created)
			.field("modified", &self.modified)
			.field("hash (hashed)", &hash_hashed_str)
			.finish()
	}
}

impl DBDecryptedFileMeta {
	fn from_row(row: &rusqlite::Row) -> Result<Self> {
		Ok(Self {
			name: row.get(FILE_NAME)?,
			mime: row.get(FILES_MIME)?,
			key: row.get(FILES_KEY)?,
			key_version: row.get(FILES_KEY_VERSION)?,
			created: row.get(FILE_CREATED)?,
			modified: row.get(FILES_MODIFIED)?,
			hash: row.get(FILES_HASH)?,
		})
	}
}

impl PartialEq<DecryptedFileMeta<'_>> for DBDecryptedFileMeta {
	fn eq(&self, other: &DecryptedFileMeta) -> bool {
		self.name == other.name()
			&& self.mime == other.mime()
			&& self.key == other.key().to_str().as_ref()
			&& self.created == other.created().map(|dt| dt.timestamp_millis())
			&& self.modified == other.last_modified().timestamp_millis()
			&& self.hash == other.hash().map(|h| h.into())
	}
}

impl From<DecryptedFileMeta<'_>> for DBDecryptedFileMeta {
	fn from(meta: DecryptedFileMeta<'_>) -> Self {
		Self {
			name: meta.name.into_owned(),
			mime: meta.mime.into_owned(),
			key: meta.key.to_string(),
			key_version: meta.key.version() as u8,
			created: meta.created.map(|dt| dt.timestamp_millis()),
			modified: meta.last_modified.timestamp_millis(),
			hash: meta.hash.map(|h| h.into()),
		}
	}
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum DBFileMeta {
	Decoded(DBDecryptedFileMeta),
	DecryptedRaw(Vec<u8>),
	DecryptedUTF8(String),
	Encrypted(EncryptedString<'static>),
	RSAEncrypted(RSAEncryptedString<'static>),
}

impl DBFileMeta {
	fn from_row(row: &rusqlite::Row) -> Result<Self> {
		let metadata_state: MetaState = row.get(FILE_METADATA_STATE)?;

		match metadata_state {
			MetaState::Decrypted => match String::from_utf8(row.get(FILE_RAW_METADATA)?) {
				Ok(utf8) => Ok(Self::DecryptedUTF8(utf8)),
				Err(e) => Ok(Self::DecryptedRaw(e.into_bytes())),
			},
			MetaState::Encrypted => Ok(Self::Encrypted(EncryptedString(
				row.get(FILE_RAW_METADATA)?,
			))),
			MetaState::RSAEncrypted => Ok(Self::RSAEncrypted(RSAEncryptedString(
				row.get(FILE_RAW_METADATA)?,
			))),
			MetaState::Decoded => Ok(Self::Decoded(DBDecryptedFileMeta::from_row(row)?)),
		}
	}
}

impl PartialEq<FileMeta<'_>> for DBFileMeta {
	fn eq(&self, other: &FileMeta) -> bool {
		match (self, other) {
			(Self::Decoded(meta), FileMeta::Decoded(other_meta)) => meta == other_meta,
			(Self::DecryptedRaw(data), FileMeta::DecryptedRaw(other_data)) => *data == **other_data,
			(Self::DecryptedUTF8(data), FileMeta::DecryptedUTF8(other_data)) => data == other_data,
			(Self::Encrypted(data), FileMeta::Encrypted(other_data)) => *data == *other_data,
			(Self::RSAEncrypted(data), FileMeta::RSAEncrypted(other_data)) => *data == *other_data,
			_ => false,
		}
	}
}

impl From<FileMeta<'_>> for DBFileMeta {
	fn from(meta: FileMeta<'_>) -> Self {
		match meta {
			FileMeta::Decoded(decrypted_meta) => {
				Self::Decoded(DBDecryptedFileMeta::from(decrypted_meta))
			}
			FileMeta::DecryptedRaw(raw) => Self::DecryptedRaw(raw.into_owned()),
			FileMeta::DecryptedUTF8(utf8) => Self::DecryptedUTF8(utf8.into_owned()),
			FileMeta::Encrypted(encrypted) => Self::Encrypted(encrypted.into_owned_cow()),
			FileMeta::RSAEncrypted(rsa_encrypted) => {
				Self::RSAEncrypted(rsa_encrypted.into_owned_cow())
			}
		}
	}
}

#[derive(Clone, PartialEq, Eq)]
pub struct DBFile {
	pub(crate) id: i64,
	pub(crate) uuid: Uuid,
	/// Server-minted whole-life id — survives content edits and version
	/// restores, unlike `uuid`. This is the identity exposed over FFI. Every
	/// query that builds a `DBFile` selects `items.stable_uuid`, which the
	/// `items` CHECK guarantees is non-NULL for a file row.
	pub(crate) stable_uuid: StableUuid,
	pub(crate) parent: ParentUuid,
	pub(crate) size: i64,
	pub(crate) chunks: i64,
	pub(crate) favorite_rank: i64,
	pub(crate) region: String,
	pub(crate) bucket: String,
	pub(crate) timestamp: i64,
	pub(crate) local_data: Option<JsonObject>,
	/// Millis at which a local edit was marked as not yet on the server, or
	/// `None` when nothing is outstanding.
	pub(crate) pending_upload_at: Option<i64>,
	/// The change sequence this row was carrying when it was read — the file's
	/// metadata version as far as an external replica is concerned.
	pub(crate) change_seq: i64,
	pub(crate) meta: DBFileMeta,
}

impl std::fmt::Debug for DBFile {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("DBFile")
			.field("id", &self.id)
			.field("uuid", &self.uuid)
			.field("stable_uuid", &self.stable_uuid)
			.field("parent", &self.parent)
			.field("size", &self.size)
			.field("chunks", &self.chunks)
			.field("favorite_rank", &self.favorite_rank)
			.field("region", &self.region)
			.field("bucket", &self.bucket)
			.field("meta", &self.meta)
			.finish()
	}
}

impl DBFile {
	pub(crate) fn from_inner_and_row(item: InnerDBItem, row: &rusqlite::Row) -> Result<Self> {
		Ok(Self {
			id: item.id,
			uuid: item.uuid,
			stable_uuid: row.get(ITEMS_STABLE_UUID)?,
			parent: item.parent.ok_or_else(|| {
				rusqlite::Error::FromSqlConversionFailure(
					0,
					rusqlite::types::Type::Blob,
					"Parent UUID cannot be None for DBFile".into(),
				)
			})?,
			local_data: item.local_data,
			pending_upload_at: row.get(ITEMS_PENDING_UPLOAD_AT)?,
			change_seq: item.change_seq,
			size: row.get(FILES_SIZE)?,
			chunks: row.get(FILES_CHUNKS)?,
			favorite_rank: row.get(FILE_FAVORITE_RANK)?,
			region: row.get(FILES_REGION)?,
			bucket: row.get(FILES_BUCKET)?,
			timestamp: row.get(FILE_TIMESTAMP)?,
			meta: DBFileMeta::from_row(row).unwrap(),
		})
	}

	pub(crate) fn select(conn: &Connection, uuid: Uuid) -> SQLResult<Self> {
		match DBObject::select(conn, uuid)? {
			DBObject::File(file) => Ok(file),
			obj => Err(SQLError::UnexpectedType(obj.item_type(), ItemType::File)),
		}
	}

	pub(crate) fn from_item(item: InnerDBItem, conn: &Connection) -> Result<Self> {
		let mut stmt = conn.prepare_cached(SELECT_FILE)?;
		stmt.query_one([item.id], |row| Self::from_inner_and_row(item, row))
	}

	/// `select_change_seq` is read AFTER every write above, because that is the only way to learn
	/// the stamp: the triggers do it, and a RETURNING clause is evaluated before they run.
	pub(crate) fn upsert_from_remote_stmts(
		remote_file: RemoteFile,
		upsert_item_stmt: &mut CachedStatement<'_>,
		upsert_file: &mut CachedStatement<'_>,
		upsert_file_meta: &mut CachedStatement<'_>,
		delete_file_meta: &mut CachedStatement<'_>,
		select_change_seq: &mut CachedStatement<'_>,
	) -> Result<Self> {
		trace!("Upserting remote file: {remote_file:?}");
		let (id, local_data, pending_upload_at) = item::upsert_file_item_with_stmts(
			remote_file.uuid(),
			remote_file.stable_uuid(),
			Some(*remote_file.parent()),
			remote_file.name(),
			None,
			upsert_item_stmt,
		)?;
		trace!(
			"Upserted item with id: {id} for remote file: {}",
			remote_file.uuid()
		);
		let meta = remote_file.get_meta();
		let (meta_state, meta) = raw_meta_and_state_from_file_meta(meta);

		let favorite_rank = upsert_file.query_one(
			(
				id,
				remote_file.size() as i64,
				remote_file.chunks() as i64,
				remote_file.favorited() as u8,
				remote_file.region(),
				remote_file.bucket(),
				remote_file.timestamp.timestamp_millis(),
				meta_state,
				meta,
			),
			|r| r.get(FILE_FAVORITE_RANK),
		)?;

		if let FileMeta::Decoded(decrypted_meta) = remote_file.get_meta() {
			upsert_file_meta.execute((
				id,
				&decrypted_meta.name,
				&decrypted_meta.mime,
				decrypted_meta.key.to_string(),
				decrypted_meta.key.version() as u8,
				decrypted_meta.created.map(|dt| dt.timestamp_millis()),
				decrypted_meta.last_modified.timestamp_millis(),
				decrypted_meta.hash.map(<[u8; 32]>::from),
			))?;
		} else {
			delete_file_meta.execute([id])?;
		}

		Ok(Self {
			id,
			uuid: remote_file.uuid,
			stable_uuid: remote_file.stable_uuid,
			parent: remote_file.parent,
			size: remote_file.size as i64,
			chunks: remote_file.chunks as i64,
			favorite_rank,
			region: remote_file.region,
			bucket: remote_file.bucket,
			timestamp: remote_file.timestamp.timestamp_millis(),
			local_data,
			pending_upload_at,
			change_seq: select_change_seq.query_one([id], |row| row.get(0))?,
			meta: remote_file.meta.into(),
		})
	}

	pub(crate) fn upsert_from_remote(
		conn: &mut Connection,
		remote_file: RemoteFile,
	) -> Result<Self> {
		let tx = conn.transaction()?;
		let new = {
			let mut upsert_item_stmt = tx.prepare_cached(UPSERT_ITEM)?;
			let mut upsert_file = tx.prepare_cached(UPSERT_FILE)?;
			let mut upsert_file_meta = tx.prepare_cached(UPSERT_FILE_META)?;
			let mut delete_file_meta = tx.prepare_cached(DELETE_FILE_META)?;
			let mut select_change_seq = tx.prepare_cached(SELECT_CHANGE_SEQ)?;
			Self::upsert_from_remote_stmts(
				remote_file,
				&mut upsert_item_stmt,
				&mut upsert_file,
				&mut upsert_file_meta,
				&mut delete_file_meta,
				&mut select_change_seq,
			)?
		};
		tx.commit()?;
		Ok(new)
	}

	pub(crate) fn update_favorite_rank(
		&mut self,
		conn: &Connection,
		favorite_rank: i64,
	) -> Result<()> {
		let mut stmt = conn.prepare_cached(UPDATE_FILE_FAVORITE_RANK)?;
		stmt.execute((favorite_rank, self.id))?;
		self.favorite_rank = favorite_rank;
		Ok(())
	}

	pub fn name(&self) -> Option<&str> {
		if let DBFileMeta::Decoded(meta) = &self.meta {
			Some(&meta.name)
		} else {
			None
		}
	}
}

impl DBItemTrait for DBFile {
	fn uuid(&self) -> Uuid {
		self.uuid
	}

	fn parent(&self) -> Option<ParentUuid> {
		Some(self.parent)
	}

	fn name(&self) -> Option<&str> {
		if let DBFileMeta::Decoded(decoded) = &self.meta {
			Some(&decoded.name)
		} else {
			None
		}
	}
}

impl TryFrom<DBFile> for RemoteFile {
	type Error = ConversionError;
	fn try_from(value: DBFile) -> Result<Self, Self::Error> {
		Ok(RemoteFile {
			uuid: value.uuid,
			stable_uuid: value.stable_uuid,
			parent: value.parent,
			size: value.size as u64,
			chunks: value.chunks as u64,
			favorited: value.favorite_rank > 0,
			region: value.region,
			bucket: value.bucket,
			timestamp: DateTime::<Utc>::from_timestamp_millis(value.timestamp).unwrap_or_default(),
			meta: match value.meta {
				DBFileMeta::Decoded(decrypted_meta) => FileMeta::Decoded(DecryptedFileMeta {
					name: Cow::Owned(decrypted_meta.name),
					mime: Cow::Owned(decrypted_meta.mime),
					key: FileKey::from_str_with_version(
						&decrypted_meta.key,
						FileEncryptionVersion::try_from(decrypted_meta.key_version)?,
					)?,
					created: decrypted_meta
						.created
						.map(DateTime::<Utc>::from_timestamp_millis)
						.unwrap_or_default(),
					last_modified: DateTime::<Utc>::from_timestamp_millis(decrypted_meta.modified)
						.unwrap_or_default(),
					hash: decrypted_meta.hash.map(|h| h.into()),
					size: value.size as u64,
				}),
				DBFileMeta::DecryptedRaw(raw) => FileMeta::DecryptedRaw(Cow::Owned(raw)),
				DBFileMeta::DecryptedUTF8(utf8) => FileMeta::DecryptedUTF8(Cow::Owned(utf8)),
				DBFileMeta::Encrypted(encrypted) => FileMeta::Encrypted(encrypted),
				DBFileMeta::RSAEncrypted(rsa_encrypted) => FileMeta::RSAEncrypted(rsa_encrypted),
			},
		})
	}
}

impl PartialEq<RemoteFile> for DBFile {
	fn eq(&self, other: &RemoteFile) -> bool {
		self.uuid == other.uuid()
			&& self.stable_uuid == other.stable_uuid()
			&& self.parent == *other.parent()
			&& self.size as u64 == other.size()
			&& self.chunks as u64 == other.chunks()
			&& (self.favorite_rank > 0) == other.favorited()
			&& self.region == other.region()
			&& self.bucket == other.bucket()
			&& self.meta == *other.get_meta()
	}
}
