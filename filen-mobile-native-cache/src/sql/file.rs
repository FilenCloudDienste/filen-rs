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
	fs::{ParentUuid, UuidStr},
};
use log::trace;
use rusqlite::{CachedStatement, Connection, Result};
use sha2::Digest;

use crate::{
	ffi::ItemType,
	sql::{
		MetaState, SQLError,
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
	pub(crate) hash: Option<[u8; 64]>,
}

impl Debug for DBDecryptedFileMeta {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		let key_hash_str = faster_hex::hex_string(&sha2::Sha256::digest(self.key.as_bytes()));
		let hash_hashed_str = self.hash.map(|h| faster_hex::hex_string(&h));

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
	fn from_row(row: &rusqlite::Row, idx: usize) -> Result<Self> {
		Ok(Self {
			name: row.get(idx)?,
			mime: row.get(idx + 1)?,
			key: row.get(idx + 2)?,
			key_version: row.get(idx + 3)?,
			created: row.get(idx + 4)?,
			modified: row.get(idx + 5)?,
			hash: row.get(idx + 6)?,
		})
	}
}

impl PartialEq<DecryptedFileMeta<'_>> for DBDecryptedFileMeta {
	fn eq(&self, other: &DecryptedFileMeta) -> bool {
		self.name == other.name()
			&& self.mime == other.mime()
			&& self.key == other.key().to_str()
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
			key: meta.key.to_str().to_string(),
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
	Encrypted(EncryptedString),
	RSAEncrypted(RSAEncryptedString),
}

impl DBFileMeta {
	fn from_row(row: &rusqlite::Row, idx: usize) -> Result<Self> {
		let metadata_state: MetaState = row.get(idx)?;

		match metadata_state {
			MetaState::Decrypted => match String::from_utf8(row.get(idx + 1)?) {
				Ok(utf8) => Ok(Self::DecryptedUTF8(utf8)),
				Err(e) => Ok(Self::DecryptedRaw(e.into_bytes())),
			},
			MetaState::Encrypted => Ok(Self::Encrypted(EncryptedString(row.get(idx + 1)?))),
			MetaState::RSAEncrypted => {
				Ok(Self::RSAEncrypted(RSAEncryptedString(row.get(idx + 1)?)))
			}
			MetaState::Decoded => Ok(Self::Decoded(DBDecryptedFileMeta::from_row(row, idx + 2)?)),
		}
	}
}

impl PartialEq<FileMeta<'_>> for DBFileMeta {
	fn eq(&self, other: &FileMeta) -> bool {
		match (self, other) {
			(Self::Decoded(meta), FileMeta::Decoded(other_meta)) => meta == other_meta,
			(Self::DecryptedRaw(data), FileMeta::DecryptedRaw(other_data)) => *data == **other_data,
			(Self::DecryptedUTF8(data), FileMeta::DecryptedUTF8(other_data)) => data == other_data,
			(Self::Encrypted(data), FileMeta::Encrypted(other_data)) => *data == **other_data,
			(Self::RSAEncrypted(data), FileMeta::RSAEncrypted(other_data)) => *data == **other_data,
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
			FileMeta::Encrypted(encrypted) => Self::Encrypted(encrypted.into_owned()),
			FileMeta::RSAEncrypted(rsa_encrypted) => Self::RSAEncrypted(rsa_encrypted.into_owned()),
		}
	}
}

#[derive(Clone, PartialEq, Eq)]
pub struct DBFile {
	pub(crate) id: i64,
	pub(crate) uuid: UuidStr,
	pub(crate) parent: ParentUuid,
	pub(crate) size: i64,
	pub(crate) chunks: i64,
	pub(crate) favorite_rank: i64,
	pub(crate) region: String,
	pub(crate) bucket: String,
	pub(crate) local_data: Option<JsonObject>,
	pub(crate) meta: DBFileMeta,
}

impl std::fmt::Debug for DBFile {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("DBFile")
			.field("id", &self.id)
			.field("uuid", &self.uuid)
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
	pub(crate) fn from_inner_and_row(
		item: InnerDBItem,
		row: &rusqlite::Row,
		idx: usize,
	) -> Result<Self> {
		Ok(Self {
			id: item.id,
			uuid: item.uuid,
			parent: item.parent.ok_or_else(|| {
				rusqlite::Error::FromSqlConversionFailure(
					0,
					rusqlite::types::Type::Blob,
					"Parent UUID cannot be None for DBFile".into(),
				)
			})?,
			local_data: item.local_data,
			size: row.get(idx)?,
			chunks: row.get(idx + 1)?,
			favorite_rank: row.get(idx + 2)?,
			region: row.get(idx + 3)?,
			bucket: row.get(idx + 4)?,
			meta: DBFileMeta::from_row(row, idx + 5).unwrap(),
		})
	}

	pub(crate) fn select(conn: &Connection, uuid: UuidStr) -> SQLResult<Self> {
		match DBObject::select(conn, uuid)? {
			DBObject::File(file) => Ok(file),
			obj => Err(SQLError::UnexpectedType(obj.item_type(), ItemType::File)),
		}
	}

	pub(crate) fn from_item(item: InnerDBItem, conn: &Connection) -> Result<Self> {
		let mut stmt = conn.prepare_cached(SELECT_FILE)?;
		stmt.query_one([item.id], |row| Self::from_inner_and_row(item, row, 0))
	}

	pub(crate) fn upsert_from_remote_stmts(
		remote_file: RemoteFile,
		upsert_item_stmt: &mut CachedStatement<'_>,
		upsert_file: &mut CachedStatement<'_>,
		upsert_file_meta: &mut CachedStatement<'_>,
		delete_file_meta: &mut CachedStatement<'_>,
	) -> Result<Self> {
		trace!("Upserting remote file: {remote_file:?}");
		let (id, local_data) = item::upsert_item_with_stmts(
			*remote_file.uuid(),
			Some(*remote_file.parent()),
			remote_file.name(),
			None,
			ItemType::File,
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
				meta_state,
				meta,
			),
			|r| r.get(0),
		)?;

		if let FileMeta::Decoded(decrypted_meta) = remote_file.get_meta() {
			upsert_file_meta.execute((
				id,
				&decrypted_meta.name,
				&decrypted_meta.mime,
				decrypted_meta.key.to_str(),
				decrypted_meta.key.version() as u8,
				decrypted_meta.created.map(|dt| dt.timestamp_millis()),
				decrypted_meta.last_modified.timestamp_millis(),
				decrypted_meta.hash.map(<[u8; 64]>::from),
			))?;
		} else {
			delete_file_meta.execute([id])?;
		}

		Ok(Self {
			id,
			uuid: remote_file.uuid,
			parent: remote_file.parent,
			size: remote_file.size as i64,
			chunks: remote_file.chunks as i64,
			favorite_rank,
			region: remote_file.region,
			bucket: remote_file.bucket,
			local_data,
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
			Self::upsert_from_remote_stmts(
				remote_file,
				&mut upsert_item_stmt,
				&mut upsert_file,
				&mut upsert_file_meta,
				&mut delete_file_meta,
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
	fn id(&self) -> i64 {
		self.id
	}

	fn uuid(&self) -> UuidStr {
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

	fn item_type(&self) -> ItemType {
		ItemType::File
	}
}

impl TryFrom<DBFile> for RemoteFile {
	type Error = ConversionError;
	fn try_from(value: DBFile) -> Result<Self, Self::Error> {
		Ok(RemoteFile {
			uuid: value.uuid,
			parent: value.parent,
			size: value.size as u64,
			chunks: value.chunks as u64,
			favorited: value.favorite_rank > 0,
			region: value.region,
			bucket: value.bucket,
			meta: match value.meta {
				DBFileMeta::Decoded(decrypted_meta) => FileMeta::Decoded(DecryptedFileMeta {
					name: Cow::Owned(decrypted_meta.name),
					mime: Cow::Owned(decrypted_meta.mime),
					key: Cow::Owned(FileKey::from_str_with_version(
						&decrypted_meta.key,
						FileEncryptionVersion::from(decrypted_meta.key_version),
					)?),
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
				DBFileMeta::Encrypted(encrypted) => FileMeta::Encrypted(Cow::Owned(encrypted)),
				DBFileMeta::RSAEncrypted(rsa_encrypted) => {
					FileMeta::RSAEncrypted(Cow::Owned(rsa_encrypted))
				}
			},
		})
	}
}

impl PartialEq<RemoteFile> for DBFile {
	fn eq(&self, other: &RemoteFile) -> bool {
		self.uuid == *other.uuid()
			&& self.parent == *other.parent()
			&& self.size as u64 == other.size()
			&& self.chunks as u64 == other.chunks()
			&& (self.favorite_rank > 0) == other.favorited()
			&& self.region == other.region()
			&& self.bucket == other.bucket()
			&& self.meta == *other.get_meta()
	}
}
