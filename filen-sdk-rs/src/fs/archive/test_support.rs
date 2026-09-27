//! Data and archives the archive tests share: bytes that compress and bytes that do not, and
//! small archives made by the SDK's own writers and the `tar` crate.

use std::{borrow::Cow, io::Write};

use chrono::Utc;
use filen_types::{crypto::Blake3Hash, fs::Uuid};

use crate::{
	consts::CHUNK_SIZE_U64,
	crypto::{file::FileKey, shared::CreateRandom, v3::EncryptionKey},
	fs::file::{
		AnonymousRemoteFile, RemoteFile,
		enums::RemoteFileType,
		meta::{DecryptedFileMeta, FileMeta},
	},
};

use super::{
	sevenz::write::{SevenZEncryption, SevenZMethod, SevenZWriter},
	zip::{
		crypto::AesStrength,
		write::{Encryption, ZipMethod, ZipWriter},
	},
};

/// A file `name` in `parent` holding `bytes`, with `hash` in its metadata.
pub(crate) fn remote_file(
	uuid: Uuid,
	parent: Uuid,
	name: &str,
	bytes: &[u8],
	hash: Option<Blake3Hash>,
) -> RemoteFileType<'static> {
	let size = bytes.len() as u64;
	let meta = FileMeta::Decoded(DecryptedFileMeta {
		name: Cow::Owned(name.to_owned()),
		size,
		mime: Cow::Borrowed("application/octet-stream"),
		key: FileKey::V3(EncryptionKey::generate()),
		last_modified: Utc::now(),
		created: None,
		hash,
	});
	let file: AnonymousRemoteFile = RemoteFile::from_meta(
		uuid,
		(),
		parent.into(),
		size,
		size.div_ceil(CHUNK_SIZE_U64),
		"de-1",
		"bucket",
		Utc::now(),
		false,
		meta,
	);
	RemoteFileType::File(Cow::Owned(file))
}

/// Bytes that compress well, told apart by `seed`: a ramp over 251 values, a prime, so its
/// period lines up with no chunk or block size.
pub(crate) fn pattern(len: usize, seed: u8) -> Vec<u8> {
	(0..len)
		.map(|i| (i % 251).to_le_bytes()[0] ^ seed)
		.collect()
}

/// Bytes no codec compresses: xorshift64 from `seed` (which must not be 0).
pub(crate) fn incompressible(len: usize, seed: u64) -> Vec<u8> {
	let mut state = seed;
	(0..len)
		.map(|_| {
			state ^= state << 13;
			state ^= state >> 7;
			state ^= state << 17;
			state.to_le_bytes()[0]
		})
		.collect()
}

pub(crate) fn gzip(data: &[u8]) -> Vec<u8> {
	let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
	encoder.write_all(data).unwrap();
	encoder.finish().unwrap()
}

/// A GNU tar of `members`, each a directory when its path ends in `/`.
pub(crate) fn tar_of(members: &[(&str, &[u8])]) -> Vec<u8> {
	let mut builder = tar::Builder::new(Vec::new());
	for (path, data) in members {
		let mut header = tar::Header::new_gnu();
		header.set_mtime(1_700_000_000);
		header.set_mode(0o644);
		if path.ends_with('/') {
			header.set_entry_type(tar::EntryType::Directory);
			header.set_size(0);
			builder.append_data(&mut header, path, &b""[..]).unwrap();
		} else {
			header.set_entry_type(tar::EntryType::Regular);
			header.set_size(data.len() as u64);
			builder.append_data(&mut header, path, *data).unwrap();
		}
	}
	builder.into_inner().unwrap()
}

/// A zip of `entries` (a directory where there is no data), deflated, and encrypted with
/// AES-256 under `password` when there is one.
pub(crate) fn zip_of(entries: &[(&str, Option<&[u8]>)], password: Option<&[u8]>) -> Vec<u8> {
	let mut writer = ZipWriter::new(Vec::new());
	for (path, data) in entries {
		match data {
			None => writer.add_dir(path, None).unwrap(),
			Some(data) => {
				let encryption = password.map(|password| Encryption {
					password,
					strength: AesStrength::Aes256,
					salt: vec![9; AesStrength::Aes256.salt_len()],
				});
				writer
					.add_file(
						path,
						None,
						data.len() as u64,
						ZipMethod::Deflate { level: 6 },
						encryption,
						&mut &data[..],
					)
					.unwrap();
			}
		}
	}
	writer.finish().unwrap()
}

/// A 7z of `entries` (a directory where there is no data) compressed with `method`, in one
/// solid block or a folder per file, and encrypted as `encryption` says under its password. Its
/// keys take 2^4 rounds, not the 2^19 the SDK writes, so tests derive them at once.
pub(crate) fn sevenz_of(
	entries: &[(&str, Option<&[u8]>)],
	method: SevenZMethod,
	solid: bool,
	encryption: Option<(SevenZEncryption, &str)>,
) -> Vec<u8> {
	let password: Option<Vec<u8>> = encryption
		.map(|(_, password)| password.encode_utf16().flat_map(u16::to_le_bytes).collect());
	let mut writer = SevenZWriter::with_cycles_power(
		Vec::new(),
		method,
		solid,
		encryption.map(|(what, _)| (what, &password.as_ref().unwrap()[..])),
		4,
	)
	.unwrap();
	for (path, data) in entries {
		match data {
			None => writer.add_dir(path, None),
			Some(data) => {
				writer
					.add_file(path, None, data.len() as u64, &mut &data[..])
					.unwrap();
			}
		}
	}
	let (mut archive, start) = writer.finish().unwrap();
	archive[..32].copy_from_slice(&start);
	archive
}
