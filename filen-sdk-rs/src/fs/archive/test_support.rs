//! Data and archives the archive tests share: bytes that compress and bytes that do not, and
//! small archives made by the SDK's own writers and the `tar` crate; and the fake drive's side of
//! removing a job's sources.

use std::{io::Write, ops::RangeInclusive, sync::atomic::Ordering};

use filen_types::{
	crypto::Blake3Hash,
	fs::{ParentUuid, Uuid},
};

use crate::{
	Error, ErrorKind,
	consts::CHUNK_SIZE_U64,
	fs::drive_job::test_support::{FakeBackend, Request},
};

use super::{
	dispose::{DirState, DisposalBackend, FileState, Tree},
	format::TAR_CHECKSUM,
	password::ArchivePassword,
	sevenz::write::{SevenZEncryption, SevenZMethod, SevenZWriter},
	tar_iter::TAR_BLOCK_LEN,
	zip::{
		crypto::AesStrength,
		write::{Encryption, ZipMethod, ZipWriter},
	},
};

/// `password`, checked.
pub(crate) fn archive_password(password: &str) -> ArchivePassword {
	ArchivePassword::new(password.to_owned()).unwrap()
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

/// `block` with its tar header checksum set to the sum of its bytes, as after an edit.
pub(crate) fn tar_checksummed(mut block: [u8; TAR_BLOCK_LEN]) -> [u8; TAR_BLOCK_LEN] {
	block[TAR_CHECKSUM].fill(b' ');
	let sum: u32 = block.iter().map(|&b| u32::from(b)).sum();
	block[TAR_CHECKSUM][..7].copy_from_slice(format!("{sum:06o}\0").as_bytes());
	block
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

impl FakeBackend {
	/// Places an existing file in the fake drive, as a source a job may remove.
	pub(crate) fn place_file(&self, uuid: Uuid, parent: Uuid, size: u64) {
		self.log()
			.file_parents
			.insert(uuid, (parent, size, size.div_ceil(CHUNK_SIZE_U64)));
	}

	/// Places an existing directory in the fake drive, or moves one already there.
	pub(crate) fn place_dir(&self, uuid: Uuid, parent: Uuid) {
		self.log().dir_parents.insert(uuid, parent);
	}

	fn assert_locked(&self) {
		assert!(
			self.live_locks.load(Ordering::SeqCst) > 0,
			"removals hold the drive lock"
		);
	}
}

impl DisposalBackend for FakeBackend {
	async fn file_state(&self, uuid: Uuid) -> Result<FileState, Error> {
		self.hold(Request::State, uuid).await;
		tokio::time::sleep(self.delay).await;
		let log = self.log();
		match log.file_parents.get(&uuid) {
			Some(&(parent, size, chunks)) => Ok(FileState {
				size,
				chunks,
				parent: ParentUuid::Uuid(parent),
				versioned: false,
				trash: false,
			}),
			None if log.trashed_files.contains(&uuid) => {
				Err(Error::custom(ErrorKind::FileNotFound, "trashed"))
			}
			None => Err(Error::custom(ErrorKind::FileNotFound, "no such file")),
		}
	}

	async fn dir_state(&self, uuid: Uuid) -> Result<DirState, Error> {
		self.hold(Request::State, uuid).await;
		tokio::time::sleep(self.delay).await;
		match self.log().dir_parents.get(&uuid) {
			Some(&parent) => Ok(DirState {
				parent: ParentUuid::Uuid(parent),
				trash: false,
			}),
			None => Err(Error::custom(
				ErrorKind::FolderNotFound,
				"no such directory",
			)),
		}
	}

	async fn list_tree(&self, dir: Uuid) -> Result<Tree, Error> {
		self.hold(Request::List, dir).await;
		tokio::time::sleep(self.delay).await;
		let log = self.log();
		let mut tree = Tree::default();
		let mut below = vec![dir];
		while let Some(parent) = below.pop() {
			for (&child, &of) in &log.dir_parents {
				if of == parent && tree.dirs.insert(child) {
					below.push(child);
				}
			}
		}
		for (&file, &(parent, size, _)) in &log.file_parents {
			if parent == dir || tree.dirs.contains(&parent) {
				tree.files.insert(file, size);
			}
		}
		Ok(tree)
	}

	async fn trash_file(&self, uuid: Uuid) -> Result<(), Error> {
		self.assert_locked();
		let mut log = self.log();
		log.file_parents.remove(&uuid);
		log.trashed_files.push(uuid);
		Ok(())
	}

	async fn delete_file_permanently(&self, uuid: Uuid) -> Result<(), Error> {
		self.assert_locked();
		self.hold(Request::Delete, uuid).await;
		if self.fail_deletes_of.contains(&uuid) {
			return Err(Error::custom(ErrorKind::Server, "delete failed"));
		}
		let mut log = self.log();
		log.file_parents.remove(&uuid);
		log.deleted_files.push(uuid);
		Ok(())
	}

	async fn trash_dir(&self, uuid: Uuid) -> Result<(), Error> {
		self.assert_locked();
		let mut log = self.log();
		// the whole subtree goes with it
		let mut gone = vec![uuid];
		let mut index = 0;
		while index < gone.len() {
			let parent = gone[index];
			gone.extend(
				log.dir_parents
					.iter()
					.filter(|(_, of)| **of == parent)
					.map(|(child, _)| *child),
			);
			index += 1;
		}
		log.dir_parents.retain(|dir, _| !gone.contains(dir));
		log.file_parents
			.retain(|_, (parent, ..)| !gone.contains(parent));
		log.trashed_dirs.push(uuid);
		Ok(())
	}

	async fn has_older_versions(&self, uuid: Uuid) -> Result<bool, Error> {
		Ok(self.versioned_files.contains(&uuid))
	}
}
