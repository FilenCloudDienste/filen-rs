//! Data and archives the archive tests share: bytes that compress and bytes that do not, and
//! small archives made by the SDK's own writers and the `tar` crate.

use std::{borrow::Cow, io::Write, ops::RangeInclusive};

use chrono::Utc;
use filen_types::{crypto::Blake3Hash, fs::Uuid};

use crate::{
	Error, ErrorKind,
	consts::CHUNK_SIZE_U64,
	crypto::{file::FileKey, shared::CreateRandom, v3::EncryptionKey},
	fs::file::{
		AnonymousRemoteFile, RemoteFile,
		enums::RemoteFileType,
		meta::{DecryptedFileMeta, FileMeta},
	},
};

use super::{
	password::ArchivePassword,
	sevenz::write::{SevenZEncryption, SevenZMethod, SevenZWriter},
	zip::{
		crypto::AesStrength,
		write::{Encryption, ZipMethod, ZipWriter},
	},
};

/// `password`, checked.
pub(crate) fn archive_password(password: &str) -> ArchivePassword {
	ArchivePassword::new(password.to_owned()).unwrap()
}

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

/// Copies of `bytes` each damaged at one of `positions`: the byte there with its lowest bit
/// flipped, then with its highest; with where, and which bit, for a failing test to say.
pub(crate) fn damaged_copies(
	bytes: &[u8],
	positions: impl IntoIterator<Item = usize>,
) -> impl Iterator<Item = (usize, u8, Vec<u8>)> {
	positions.into_iter().flat_map(move |at| {
		[0x01, 0x80].map(|bit| {
			let mut damaged = bytes.to_vec();
			damaged[at] ^= bit;
			(at, bit, damaged)
		})
	})
}

/// A compression method built at a level, and the levels it takes.
pub(crate) type LeveledMethod<M> = (fn(u32) -> M, RangeInclusive<u32>);

/// Checks a compression method's levels: each of `methods` (built at a level) states its
/// levels, takes both ends of them, and refuses one past either as an invalid state.
pub(crate) fn assert_levels_checked<M: Copy + std::fmt::Debug>(
	methods: &[LeveledMethod<M>],
	levels: impl Fn(M) -> Option<RangeInclusive<u32>>,
	check: impl Fn(M) -> Result<(), Error>,
) {
	for (method, range) in methods {
		assert_eq!(levels(method(5)), Some(range.clone()), "{:?}", method(5));
		check(method(*range.start())).unwrap();
		check(method(*range.end())).unwrap();
		let below = range.start().checked_sub(1);
		for outside in below.into_iter().chain([range.end() + 1]) {
			assert_eq!(
				check(method(outside)).unwrap_err().kind(),
				ErrorKind::InvalidState,
				"{:?}",
				method(outside)
			);
		}
	}
}

/// The BLAKE3 hash of `data`, as a file's metadata holds it.
pub(crate) fn hash(data: &[u8]) -> Blake3Hash {
	Blake3Hash::from(blake3::hash(data))
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
	let members: Vec<TarMember> = members
		.iter()
		.map(|&(path, data)| TarMember::Data(path, data))
		.collect();
	tar_with(&members)
}

/// A member of a tar [`tar_with`] builds.
pub(crate) enum TarMember<'a> {
	/// A file holding the data, or a directory when its path ends in `/`.
	Data(&'a str, &'a [u8]),
	Symlink {
		path: &'a str,
		target: &'a str,
	},
	/// A second name for the file stored before it at `target`.
	HardLink {
		path: &'a str,
		target: &'a str,
	},
}

/// A GNU tar of `members`.
pub(crate) fn tar_with(members: &[TarMember]) -> Vec<u8> {
	let mut builder = tar::Builder::new(Vec::new());
	for member in members {
		let mut header = tar::Header::new_gnu();
		header.set_mtime(1_700_000_000);
		header.set_mode(0o644);
		match *member {
			TarMember::Data(path, _) if path.ends_with('/') => {
				header.set_entry_type(tar::EntryType::Directory);
				header.set_size(0);
				builder.append_data(&mut header, path, &b""[..]).unwrap();
			}
			TarMember::Data(path, data) => {
				header.set_entry_type(tar::EntryType::Regular);
				header.set_size(data.len() as u64);
				builder.append_data(&mut header, path, data).unwrap();
			}
			TarMember::Symlink { path, target } => {
				header.set_entry_type(tar::EntryType::Symlink);
				header.set_size(0);
				builder.append_link(&mut header, path, target).unwrap();
			}
			TarMember::HardLink { path, target } => {
				header.set_entry_type(tar::EntryType::Link);
				header.set_size(0);
				builder.append_link(&mut header, path, target).unwrap();
			}
		}
	}
	builder.into_inner().unwrap()
}

/// A zip of `entries` (a directory where there is no data), deflated, and encrypted with
/// AES-256 under `password` when there is one.
pub(crate) fn zip_of(entries: &[(&str, Option<&[u8]>)], password: Option<&str>) -> Vec<u8> {
	let password = password.map(archive_password);
	let mut writer = ZipWriter::new(Vec::new());
	for (path, data) in entries {
		match data {
			None => writer.add_dir(path, None).unwrap(),
			Some(data) => {
				let encryption = password.as_ref().map(|password| Encryption {
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
	let password = encryption.map(|(_, password)| archive_password(password));
	let mut writer = SevenZWriter::with_cycles_power(
		Vec::new(),
		method,
		solid,
		encryption.map(|(what, _)| (what, password.as_ref().unwrap())),
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
