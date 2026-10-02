//! Data and archives the archive tests share: bytes that compress and bytes that do not, and
//! small archives made by the SDK's own writers and the `tar` crate; and the fake drive's side of
//! removing a job's sources.

use std::{
	collections::HashSet,
	io::{self, Read, Seek, Write},
	ops::RangeInclusive,
	sync::atomic::Ordering,
};

use filen_types::{
	crypto::Blake3Hash,
	fs::{ParentUuid, Uuid},
};

use crate::{
	Error, ErrorKind,
	consts::CHUNK_SIZE_U64,
	fs::drive_job::test_support::{FakeBackend, FakeLog, Quirk, Request},
};

use super::{
	bytes::read_at,
	dispose::{DirState, DisposalBackend, FileState, Tree},
	format::TAR_CHECKSUM,
	password::ArchivePassword,
	sevenz::{
		read::SevenZLimits,
		write::{SevenZEncryption, SevenZMethod, SevenZWriter},
	},
	tar_iter::{self, TAR_BLOCK, TarReader},
	zip::{
		crypto::AesStrength,
		read::{EntryLimits, LOCAL_HEADER_LEN, LOCAL_HEADER_LEN_U64, ZipEntry, ZipLimits},
		write::{Encryption, ZipMethod, ZipWriter},
	},
};

/// A decoder budget that reads back anything a test writes.
pub(crate) const READ_BACK_MEMORY: u64 = 512 << 20;
/// Zip index limits that read back anything a test writes.
pub(crate) const READ_BACK_ZIP: ZipLimits = ZipLimits {
	max_index_bytes: 32 << 20,
	max_entries: 1_000_000,
};
pub(crate) const READ_BACK_ZIP_ENTRY: EntryLimits = EntryLimits {
	decoder_memory: READ_BACK_MEMORY,
};
/// 7z index limits that read back anything a test writes.
pub(crate) const READ_BACK_SEVEN_Z: SevenZLimits = SevenZLimits {
	max_index_bytes: 32 << 20,
	max_entries: 1_000_000,
	decoder_memory: READ_BACK_MEMORY,
};

/// The head of an AppleDouble file (the `._` twin holding a file's macOS metadata): its magic
/// number, then version 2.
pub(crate) const APPLE_DOUBLE: [u8; 8] = [0x00, 0x05, 0x16, 0x07, 0x00, 0x02, 0x00, 0x00];

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

/// Bytes no codec compresses: xorshift64 from `seed`, which must not be 0 (xorshift stays at 0).
pub(crate) fn incompressible(len: usize, seed: u64) -> Vec<u8> {
	assert_ne!(seed, 0, "xorshift from 0 gives only zeros");
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

/// A zstd frame of raw blocks, written by hand: a window of `1 << window_log` bytes, the content
/// size in the header when `states_size`, and a dictionary id when `dictionary` is given.
pub(crate) fn zstd_raw_frame(
	data: &[u8],
	window_log: u8,
	states_size: bool,
	dictionary: Option<u8>,
) -> Vec<u8> {
	let mut frame = vec![0x28, 0xB5, 0x2F, 0xFD];
	// FCS_flag 2 (4 bytes) when stating the size, Dictionary_ID_flag 1 (1 byte) with one
	let descriptor = if states_size { 2 << 6 } else { 0 } | u8::from(dictionary.is_some());
	frame.push(descriptor);
	frame.push((window_log - 10) << 3);
	frame.extend(dictionary);
	if states_size {
		frame.extend_from_slice(&u32::try_from(data.len()).unwrap().to_le_bytes());
	}
	let block_max = (1usize << window_log).min(128 << 10);
	let mut blocks = data.chunks(block_max).peekable();
	if blocks.peek().is_none() {
		frame.extend_from_slice(&[1, 0, 0]);
	}
	while let Some(block) = blocks.next() {
		// raw blocks: the last-block bit, type 0, and the size above them
		let header = u32::from(blocks.peek().is_none()) | u32::try_from(block.len()).unwrap() << 3;
		frame.extend_from_slice(&header.to_le_bytes()[..3]);
		frame.extend_from_slice(block);
	}
	frame
}

/// A skippable frame, as lz4 and zstd share them: `magic` (one of 16 each format reserves), the
/// length of `data`, then `data`, which a decoder passes over.
pub(crate) fn skippable_frame(magic: u32, data: &[u8]) -> Vec<u8> {
	[
		&magic.to_le_bytes()[..],
		&u32::try_from(data.len()).unwrap().to_le_bytes(),
		data,
	]
	.concat()
}

/// `block` with its tar header checksum set to the sum of its bytes, as after an edit.
pub(crate) fn tar_checksummed(mut block: [u8; TAR_BLOCK]) -> [u8; TAR_BLOCK] {
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
				append_link_verbatim(&mut builder, header, path, target);
			}
			TarMember::HardLink { path, target } => {
				header.set_entry_type(tar::EntryType::Link);
				append_link_verbatim(&mut builder, header, path, target);
			}
		}
	}
	builder.into_inner().unwrap()
}

/// Appends a link member with `path` and `target` stored byte for byte. The `tar` crate's own
/// `append_link` parses both as host paths, and on Windows a name such as `d:e` parses as a drive
/// prefix, which it refuses as not relative.
fn append_link_verbatim(
	builder: &mut tar::Builder<Vec<u8>>,
	mut header: tar::Header,
	path: &str,
	target: &str,
) {
	header.set_size(0);
	header
		.as_old_mut()
		.name
		.get_mut(..path.len())
		.expect("a link fixture's path fits the header's name field")
		.copy_from_slice(path.as_bytes());
	header.set_link_name_literal(target).unwrap();
	header.set_cksum();
	builder.append(&header, io::empty()).unwrap();
}

/// Every member of a tar with its data, read back through the SDK's reader in small pieces, so
/// a body is read in many.
pub(crate) fn tar_members<R: Read>(tar: &mut TarReader<R>) -> Vec<(tar_iter::TarMember, Vec<u8>)> {
	let mut members = Vec::new();
	while let Some(member) = tar.next_member().unwrap() {
		let mut data = Vec::new();
		let mut buf = [0u8; 100];
		loop {
			let read = tar.read_body(&mut buf).unwrap();
			if read == 0 {
				break;
			}
			data.extend_from_slice(&buf[..read]);
		}
		members.push((member, data));
	}
	members
}

/// A zip of `entries` (a directory where there is no data), deflated, and encrypted with
/// AES-256 under `password` when there is one.
pub(crate) fn zip_of(entries: &[(&str, Option<&[u8]>)], password: Option<&str>) -> Vec<u8> {
	let entries: Vec<_> = entries
		.iter()
		.map(|&(path, data)| (path, data, password))
		.collect();
	zip_with_passwords(&entries)
}

/// An entry of [`zip_with_passwords`]: its path, its data (none for a directory), and the
/// password its data is encrypted under, if any.
pub(crate) type ZipEntryUnder<'a> = (&'a str, Option<&'a [u8]>, Option<&'a str>);

/// A zip of `entries` (a directory where there is no data), deflated, each file encrypted with
/// AES-256 under its own password when it has one.
pub(crate) fn zip_with_passwords(entries: &[ZipEntryUnder]) -> Vec<u8> {
	let mut writer = ZipWriter::new(Vec::new());
	for &(path, data, password) in entries {
		match data {
			None => writer.add_dir(path, None).unwrap(),
			Some(data) => {
				let password = password.map(archive_password);
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
	sevenz_finished(writer)
}

/// A 7z of the `encrypted` files, each in a folder of its own encrypted under `password`, then
/// the `plain` ones, each in a plain folder; its header stays readable. Compressed and keyed as
/// [`sevenz_of`]'s are.
pub(crate) fn sevenz_partly_encrypted(
	encrypted: &[(&str, &[u8])],
	plain: &[(&str, &[u8])],
	password: &str,
) -> Vec<u8> {
	let password = archive_password(password);
	let mut writer = SevenZWriter::with_cycles_power(
		Vec::new(),
		SevenZMethod::Lzma2 { level: 1 },
		false,
		Some((SevenZEncryption::Entries, &password)),
		4,
	)
	.unwrap();
	for (path, data) in encrypted {
		writer
			.add_file(path, None, data.len() as u64, &mut &data[..])
			.unwrap();
	}
	writer.stop_encrypting();
	for (path, data) in plain {
		writer
			.add_file(path, None, data.len() as u64, &mut &data[..])
			.unwrap();
	}
	sevenz_finished(writer)
}

/// The 7z `writer` wrote, finished, with its start header written over the zeros it began with.
pub(crate) fn sevenz_finished(writer: SevenZWriter<Vec<u8>>) -> Vec<u8> {
	let (mut archive, start) = writer.finish().unwrap();
	archive[..start.len()].copy_from_slice(&start);
	archive
}

/// What a zip entry's local header and data descriptor say of it, read from the bytes.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct LocalRecords {
	/// The local header's compressed and uncompressed sizes.
	pub(crate) sizes: (u32, u32),
	/// The values of its zip64 extra field, if it has one.
	pub(crate) zip64: Option<Vec<u64>>,
	/// The descriptor's compressed and uncompressed sizes, 8 bytes each with zip64, else 4.
	pub(crate) descriptor: (u64, u64),
}

/// The local records of `entry`, which ends in a data descriptor, in a zip that starts `shift`
/// bytes into `source`. Read by the zip specification's offsets, not the reader's.
pub(crate) fn local_records<R: Read + Seek>(
	source: &mut R,
	shift: u64,
	entry: &ZipEntry,
) -> LocalRecords {
	const ZIP64_EXTRA_ID: u16 = 0x0001;
	const DATA_DESCRIPTOR_SIG: u32 = 0x0807_4b50;
	let u16_at = |bytes: &[u8], at: usize| u16::from_le_bytes([bytes[at], bytes[at + 1]]);
	let u32_at =
		|bytes: &[u8], at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
	let u64_at =
		|bytes: &[u8], at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap());
	let mut read = |at: u64, len: usize| {
		read_at(
			source,
			shift + at,
			len,
			io::Error::from(io::ErrorKind::UnexpectedEof),
		)
		.unwrap()
	};
	let header = read(entry.header_offset, LOCAL_HEADER_LEN);
	let (name_len, extra_len) = (
		usize::from(u16_at(&header, 26)),
		usize::from(u16_at(&header, 28)),
	);
	let extra = read(
		entry.header_offset + LOCAL_HEADER_LEN_U64 + name_len as u64,
		extra_len,
	);
	let mut zip64 = None;
	let mut fields = &extra[..];
	while fields.len() >= 4 {
		let len = usize::from(u16_at(fields, 2));
		if u16_at(fields, 0) == ZIP64_EXTRA_ID {
			zip64 = Some(
				fields[4..4 + len]
					.chunks_exact(8)
					.map(|value| u64_at(value, 0))
					.collect(),
			);
		}
		fields = &fields[4 + len..];
	}
	let wide = zip64.is_some();
	let descriptor = read(
		entry.header_offset
			+ LOCAL_HEADER_LEN_U64
			+ (name_len + extra_len) as u64
			+ entry.compressed_size,
		if wide { 24 } else { 16 },
	);
	assert_eq!(
		u32_at(&descriptor, 0),
		DATA_DESCRIPTOR_SIG,
		"{}",
		entry.name
	);
	assert_eq!(u32_at(&descriptor, 4), entry.crc, "{}", entry.name);
	LocalRecords {
		sizes: (u32_at(&header, 18), u32_at(&header, 22)),
		zip64,
		descriptor: if wide {
			(u64_at(&descriptor, 8), u64_at(&descriptor, 16))
		} else {
			(
				u64::from(u32_at(&descriptor, 8)),
				u64::from(u32_at(&descriptor, 12)),
			)
		},
	}
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

	fn fail_state(&self, uuid: Uuid) -> Result<(), Error> {
		if self.fail_state_of.contains(&uuid) {
			return Err(Error::custom(ErrorKind::Server, "state failed"));
		}
		Ok(())
	}
}

/// Every directory below `dir` in the fake drive.
fn subtree(log: &FakeLog, dir: Uuid) -> HashSet<Uuid> {
	let mut found = HashSet::new();
	let mut below = vec![dir];
	while let Some(parent) = below.pop() {
		for (&child, &of) in &log.dir_parents {
			if of == parent && found.insert(child) {
				below.push(child);
			}
		}
	}
	found
}

impl DisposalBackend for FakeBackend {
	async fn file_state(&self, uuid: Uuid) -> Result<FileState, Error> {
		self.hold(Request::State, uuid).await;
		tokio::time::sleep(self.delay).await;
		self.fail_state(uuid)?;
		let log = self.log();
		match log.file_parents.get(&uuid) {
			Some(&(parent, size, chunks)) => Ok(FileState {
				size,
				chunks,
				parent: ParentUuid::Uuid(parent),
				versioned: self.superseded.contains(&uuid),
				trash: self.in_trash.contains(&uuid),
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
		self.fail_state(uuid)?;
		match self.log().dir_parents.get(&uuid) {
			Some(&parent) => Ok(DirState {
				parent: ParentUuid::Uuid(parent),
				trash: self.in_trash.contains(&uuid),
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
		if self.quirks.contains(&Quirk::FailTrees) {
			return Err(Error::custom(ErrorKind::Server, "listing failed"));
		}
		let log = self.log();
		let dirs = subtree(&log, dir);
		let files = log
			.file_parents
			.iter()
			.filter(|(_, (parent, ..))| *parent == dir || dirs.contains(parent))
			.map(|(&file, &(_, size, _))| (file, size))
			.collect();
		Ok(Tree {
			files,
			dirs: dirs.into_iter().collect(),
		})
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
		let mut gone = subtree(&log, uuid);
		gone.insert(uuid);
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
