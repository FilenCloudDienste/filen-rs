//! Writing a 7z as its entries stream in: files are compressed into folders (one per file, or
//! solid blocks of up to [`SOLID_BLOCK_BYTES`]), optionally encrypted with AES-256, and the
//! header follows the data. The 32-byte start header points at the header, so it is only known
//! at the end: [`SevenZWriter::finish`] returns it for the caller to patch over the 32 zero bytes
//! the archive starts with.

use std::io::{self, Read, Write};

use chrono::{DateTime, Utc};

use super::{
	crypto::{AesCbcWriter, AesProps, Key, WRITE_CYCLES_POWER, derive_key},
	header::*,
	read::Method,
};

/// The most bytes one solid block holds: extracting a file decodes everything before it in
/// its block.
pub(crate) const SOLID_BLOCK_BYTES: u64 = 2 << 30;

/// The dictionary a packed header is compressed with; headers are small.
const HEADER_DICT_BYTES: u32 = 1 << 20;
/// The level a packed header is compressed at.
const HEADER_LEVEL: u32 = 6;

/// The memory compressing a packed header takes, as [`lzma_encoder_memory`] states it for its
/// dictionary.
///
/// [`lzma_encoder_memory`]: crate::fs::archive::encode::lzma_encoder_memory
pub(crate) fn header_encoder_memory() -> u64 {
	12 * u64::from(HEADER_DICT_BYTES) + (1 << 20)
}

/// PPMd's model order per level (1 to 9), as 7-Zip picks them.
const PPMD_ORDERS: [u8; 10] = [3, 4, 4, 5, 5, 6, 8, 16, 24, 32];

/// How a 7z's files are compressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SevenZMethod {
	/// Stored as they are.
	Copy,
	/// Levels 0 to 9; 7-Zip's default method.
	Lzma2 {
		/// 0 (fastest) to 9 (smallest).
		level: u32,
	},
	/// Levels 0 to 9.
	Lzma {
		/// 0 (fastest) to 9 (smallest).
		level: u32,
	},
	/// Levels 1 to 9; good on text.
	Ppmd {
		/// 1 (fastest) to 9 (smallest).
		level: u32,
	},
	/// Levels 1 to 9.
	Bzip2 {
		/// 1 (fastest) to 9 (smallest).
		level: u32,
	},
	/// Levels 1 to 9.
	Deflate {
		/// 1 (fastest) to 9 (smallest).
		level: u32,
	},
}

impl SevenZMethod {
	/// The method's level, checked against its range.
	pub(crate) fn check(self) -> Result<(), &'static str> {
		let (level, range) = match self {
			Self::Copy => return Ok(()),
			Self::Lzma2 { level } | Self::Lzma { level } => (level, 0..=9),
			Self::Ppmd { level } | Self::Bzip2 { level } | Self::Deflate { level } => {
				(level, 1..=9)
			}
		};
		if range.contains(&level) {
			Ok(())
		} else if range.start() == &0 {
			Err("7z LZMA and LZMA2 take levels 0 to 9")
		} else {
			Err("7z PPMd, BZip2 and Deflate take levels 1 to 9")
		}
	}

	/// The encoder's memory, in bytes, for a level already checked.
	pub(crate) fn encoder_memory(self) -> u64 {
		match self {
			Self::Copy => 0,
			Self::Lzma2 { level } | Self::Lzma { level } => {
				crate::fs::archive::encode::lzma_encoder_memory(level)
			}
			Self::Ppmd { level } => u64::from(ppmd_memory(level)) + (1 << 20),
			// bzip2's documented compression memory: 400 kB + 8 × the block size
			Self::Bzip2 { level } => 400_000 + 8 * u64::from(level) * 100_000,
			// miniz_oxide's deflate state, well under this
			Self::Deflate { .. } => 1 << 20,
		}
	}
}

fn ppmd_memory(level: u32) -> u32 {
	if level >= 9 {
		192 << 20
	} else {
		1 << (level + 19)
	}
}

/// What a 7z's encryption covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SevenZEncryption {
	/// The files' data; names, sizes and CRCs stay readable.
	Entries,
	/// The files' data and the header, so nothing can be listed without the password.
	EntriesAndHeaders,
}

/// A coder of a folder written, and its properties.
struct CoderRecord {
	method: Method,
	props: Vec<u8>,
}

struct FolderRecord {
	/// Outermost first: the compression coder, then AES.
	coders: Vec<CoderRecord>,
	unpack_sizes: Vec<u64>,
	/// Each file's size and CRC-32.
	substreams: Vec<(u64, u32)>,
	crc: Option<u32>,
}

enum FileKind {
	Dir,
	/// A file with data in a folder, or an empty one.
	File {
		has_stream: bool,
	},
}

struct FileRecord {
	name: String,
	modified: Option<DateTime<Utc>>,
	kind: FileKind,
}

/// The archive under a folder being written, counting its bytes.
struct Counting<W> {
	inner: W,
	count: u64,
}

impl<W: Write> Write for Counting<W> {
	fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
		let n = self.inner.write(buf)?;
		self.count += n as u64;
		Ok(n)
	}

	fn flush(&mut self) -> io::Result<()> {
		self.inner.flush()
	}
}

/// What a folder's compressed data goes through: nothing, or AES.
enum Packer<W: Write> {
	Plain(Counting<W>),
	Aes(Box<AesCbcWriter<Counting<W>>>),
}

impl<W: Write> Write for Packer<W> {
	fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
		match self {
			Self::Plain(inner) => inner.write(buf),
			Self::Aes(inner) => inner.write(buf),
		}
	}

	fn flush(&mut self) -> io::Result<()> {
		Ok(())
	}
}

impl<W: Write> Packer<W> {
	/// The archive, and the bytes AES took (its coder's unpack size).
	fn finish(self) -> io::Result<(Counting<W>, Option<u64>)> {
		match self {
			Self::Plain(inner) => Ok((inner, None)),
			Self::Aes(inner) => inner.finish().map(|(inner, taken)| (inner, Some(taken))),
		}
	}
}

enum Encoder<W: Write> {
	Copy(Packer<W>),
	Lzma2(Box<lzma_rust2::Lzma2Writer<Packer<W>>>),
	Lzma(Box<lzma_rust2::LzmaWriter<Packer<W>>>),
	Ppmd(Box<ppmd_rust::Ppmd7Encoder<Packer<W>>>),
	Bzip2(bzip2::write::BzEncoder<Packer<W>>),
	Deflate(flate2::write::DeflateEncoder<Packer<W>>),
}

impl<W: Write> Write for Encoder<W> {
	fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
		match self {
			Self::Copy(inner) => inner.write(buf),
			Self::Lzma2(inner) => inner.write(buf),
			Self::Lzma(inner) => inner.write(buf),
			Self::Ppmd(inner) => inner.write(buf),
			Self::Bzip2(inner) => inner.write(buf),
			Self::Deflate(inner) => inner.write(buf),
		}
	}

	fn flush(&mut self) -> io::Result<()> {
		Ok(())
	}
}

impl<W: Write> Encoder<W> {
	fn finish(self) -> io::Result<Packer<W>> {
		match self {
			Self::Copy(inner) => Ok(inner),
			Self::Lzma2(inner) => inner.finish(),
			Self::Lzma(inner) => inner.finish(),
			Self::Ppmd(inner) => inner.finish(false),
			Self::Bzip2(inner) => inner.finish(),
			Self::Deflate(inner) => inner.finish(),
		}
	}
}

/// A folder being written.
struct OpenFolder<W: Write> {
	encoder: Encoder<W>,
	coders: Vec<CoderRecord>,
	/// Where the archive was when the folder started.
	start: u64,
	unpacked: u64,
	substreams: Vec<(u64, u32)>,
}

enum State<W: Write> {
	Idle(Counting<W>),
	Open(Box<OpenFolder<W>>),
	/// An error left the archive in no state to go on.
	Failed,
}

struct Encryption {
	key: Key,
	salt: [u8; 16],
	cycles_power: u8,
	headers: bool,
}

pub(crate) struct SevenZWriter<W: Write> {
	state: State<W>,
	method: SevenZMethod,
	solid: bool,
	encryption: Option<Encryption>,
	folders: Vec<FolderRecord>,
	pack_sizes: Vec<u64>,
	files: Vec<FileRecord>,
}

fn failed() -> io::Error {
	io::Error::other("an earlier error ended the 7z archive")
}

impl<W: Write> SevenZWriter<W> {
	/// Starts an archive over `out` with 32 zero bytes for the start header. `password` is
	/// UTF-16LE; its key is derived once, here.
	pub(crate) fn new(
		out: W,
		method: SevenZMethod,
		solid: bool,
		encryption: Option<(SevenZEncryption, &[u8])>,
	) -> io::Result<Self> {
		Self::with_cycles_power(out, method, solid, encryption, WRITE_CYCLES_POWER)
	}

	/// [`SevenZWriter::new`] with a key derivation of `2^cycles_power` rounds, which tests
	/// lower to stay fast.
	pub(crate) fn with_cycles_power(
		mut out: W,
		method: SevenZMethod,
		solid: bool,
		encryption: Option<(SevenZEncryption, &[u8])>,
		cycles_power: u8,
	) -> io::Result<Self> {
		let encryption = match encryption {
			None => None,
			Some((what, password)) => {
				let mut salt = [0u8; 16];
				rand::RngCore::fill_bytes(&mut rand::rng(), &mut salt);
				let props = AesProps {
					cycles_power,
					salt: salt.to_vec(),
					iv: [0; 16],
				};
				// 2^19 rounds take about a second: nothing to show progress for
				let key = derive_key(password, &props, &mut || Ok(())).map_err(io::Error::other)?;
				Some(Encryption {
					key,
					salt,
					cycles_power,
					headers: what == SevenZEncryption::EntriesAndHeaders,
				})
			}
		};
		out.write_all(&[0; START_HEADER_LEN])?;
		Ok(Self {
			state: State::Idle(Counting {
				inner: out,
				count: 0,
			}),
			method,
			solid,
			encryption,
			folders: Vec::new(),
			pack_sizes: Vec::new(),
			files: Vec::new(),
		})
	}

	pub(crate) fn add_dir(&mut self, path: &str, modified: Option<DateTime<Utc>>) {
		self.files.push(FileRecord {
			name: path.to_owned(),
			modified,
			kind: FileKind::Dir,
		});
	}

	/// Adds a file of `size` bytes read from `data`; the bytes read, which the caller checks
	/// against `size` (a mismatch leaves the archive unusable).
	pub(crate) fn add_file(
		&mut self,
		path: &str,
		modified: Option<DateTime<Utc>>,
		size: u64,
		data: &mut dyn Read,
	) -> io::Result<u64> {
		if size == 0 {
			self.files.push(FileRecord {
				name: path.to_owned(),
				modified,
				kind: FileKind::File { has_stream: false },
			});
			return io::copy(data, &mut io::sink());
		}
		if let State::Open(folder) = &self.state
			&& (!self.solid || folder.unpacked.saturating_add(size) > SOLID_BLOCK_BYTES)
		{
			self.close_folder()?;
		}
		if matches!(self.state, State::Idle(_)) {
			self.open_folder(self.method, None)?;
		}
		let State::Open(folder) = &mut self.state else {
			return Err(failed());
		};
		let mut crc = crc32fast::Hasher::new();
		let mut buf = vec![0u8; 64 * 1024];
		let mut read = 0u64;
		loop {
			let n = match data.read(&mut buf) {
				Ok(0) => break,
				Ok(n) => n,
				Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
				Err(error) => {
					self.state = State::Failed;
					return Err(error);
				}
			};
			crc.update(&buf[..n]);
			if let Err(error) = folder.encoder.write_all(&buf[..n]) {
				self.state = State::Failed;
				return Err(error);
			}
			read += n as u64;
		}
		folder.unpacked += read;
		folder.substreams.push((read, crc.finalize()));
		self.files.push(FileRecord {
			name: path.to_owned(),
			modified,
			kind: FileKind::File { has_stream: true },
		});
		if !self.solid {
			self.close_folder()?;
		}
		Ok(read)
	}

	fn open_folder(&mut self, method: SevenZMethod, dict: Option<u32>) -> io::Result<()> {
		let State::Idle(out) = std::mem::replace(&mut self.state, State::Failed) else {
			return Err(failed());
		};
		let start = out.count;
		let mut coders = Vec::with_capacity(2);
		let packer = match &self.encryption {
			None => Packer::Plain(out),
			Some(encryption) => {
				// a fresh IV per folder: two folders under one key never share a keystream
				let mut iv = [0u8; 16];
				rand::RngCore::fill_bytes(&mut rand::rng(), &mut iv);
				coders.push(CoderRecord {
					method: Method::Aes,
					props: AesProps {
						cycles_power: encryption.cycles_power,
						salt: encryption.salt.to_vec(),
						iv,
					}
					.encode(),
				});
				Packer::Aes(Box::new(AesCbcWriter::new(out, &encryption.key, iv)))
			}
		};
		let (encoder, coder) = match method {
			SevenZMethod::Copy => (Encoder::Copy(packer), (Method::Copy, Vec::new())),
			SevenZMethod::Lzma2 { level } => {
				let mut options = lzma_rust2::Lzma2Options::with_preset(level);
				if let Some(dict) = dict {
					options.lzma_options.dict_size = dict;
				}
				let dict = options.lzma_options.dict_size;
				(
					Encoder::Lzma2(Box::new(lzma_rust2::Lzma2Writer::new(packer, options))),
					(Method::Lzma2, vec![lzma2_dict_prop(dict)]),
				)
			}
			SevenZMethod::Lzma { level } => {
				let options = lzma_rust2::LzmaOptions::with_preset(level);
				let mut props = vec![options.get_props()];
				props.extend_from_slice(&options.dict_size.to_le_bytes());
				let writer = lzma_rust2::LzmaWriter::new_no_header(packer, &options, false)?;
				(Encoder::Lzma(Box::new(writer)), (Method::Lzma, props))
			}
			SevenZMethod::Ppmd { level } => {
				let (order, memory) = (PPMD_ORDERS[level as usize], ppmd_memory(level));
				let mut props = vec![order];
				props.extend_from_slice(&memory.to_le_bytes());
				let encoder = ppmd_rust::Ppmd7Encoder::new(packer, u32::from(order), memory)
					.map_err(|_| io::Error::other("the PPMd encoder could not start"))?;
				(Encoder::Ppmd(Box::new(encoder)), (Method::Ppmd, props))
			}
			SevenZMethod::Bzip2 { level } => (
				Encoder::Bzip2(bzip2::write::BzEncoder::new(
					packer,
					bzip2::Compression::new(level),
				)),
				(Method::Bzip2, Vec::new()),
			),
			SevenZMethod::Deflate { level } => (
				Encoder::Deflate(flate2::write::DeflateEncoder::new(
					packer,
					flate2::Compression::new(level),
				)),
				(Method::Deflate, Vec::new()),
			),
		};
		coders.insert(
			0,
			CoderRecord {
				method: coder.0,
				props: coder.1,
			},
		);
		self.state = State::Open(Box::new(OpenFolder {
			encoder,
			coders,
			start,
			unpacked: 0,
			substreams: Vec::new(),
		}));
		Ok(())
	}

	fn close_folder(&mut self) -> io::Result<()> {
		let State::Open(folder) = std::mem::replace(&mut self.state, State::Failed) else {
			return Err(failed());
		};
		let (out, aes_taken) = folder.encoder.finish()?.finish()?;
		let packed = out.count - folder.start;
		let mut unpack_sizes = vec![folder.unpacked];
		unpack_sizes.extend(aes_taken);
		self.pack_sizes.push(packed);
		self.folders.push(FolderRecord {
			coders: folder.coders,
			unpack_sizes,
			substreams: folder.substreams,
			crc: None,
		});
		self.state = State::Idle(out);
		Ok(())
	}

	/// Writes the header; the archive, and the start header to write over its first 32 bytes.
	pub(crate) fn finish(mut self) -> io::Result<(W, [u8; START_HEADER_LEN])> {
		if matches!(self.state, State::Open(_)) {
			self.close_folder()?;
		}
		let header = self.header();
		let (mut out, next) = if self.encryption.as_ref().is_some_and(|e| e.headers) {
			// the header, compressed and encrypted as a folder of its own, then a small plain
			// header saying where it is
			let pack_pos = self.data_len()?;
			self.open_folder(
				SevenZMethod::Lzma2 {
					level: HEADER_LEVEL,
				},
				Some(HEADER_DICT_BYTES),
			)?;
			let State::Open(folder) = &mut self.state else {
				return Err(failed());
			};
			folder.encoder.write_all(&header)?;
			folder.unpacked = header.len() as u64;
			self.close_folder()?;
			let (Some(mut folder), Some(packed)) = (self.folders.pop(), self.pack_sizes.pop())
			else {
				return Err(failed());
			};
			folder.crc = Some(crc32fast::hash(&header));
			let mut next = vec![K_ENCODED_HEADER];
			write_streams(&mut next, pack_pos, &[packed], &[folder], false);
			(self.into_out()?, next)
		} else {
			(self.into_out()?, header)
		};
		let next_offset = out.count;
		out.write_all(&next)?;
		let mut start = [0u8; START_HEADER_LEN];
		start[..6].copy_from_slice(&SIGNATURE);
		start[7] = 4;
		start[12..20].copy_from_slice(&next_offset.to_le_bytes());
		start[20..28].copy_from_slice(&(next.len() as u64).to_le_bytes());
		start[28..32].copy_from_slice(&crc32fast::hash(&next).to_le_bytes());
		let start_crc = crc32fast::hash(&start[12..]);
		start[8..12].copy_from_slice(&start_crc.to_le_bytes());
		Ok((out.inner, start))
	}

	/// The bytes written after the start header.
	fn data_len(&self) -> io::Result<u64> {
		match &self.state {
			State::Idle(out) => Ok(out.count),
			_ => Err(failed()),
		}
	}

	fn into_out(self) -> io::Result<Counting<W>> {
		match self.state {
			State::Idle(out) => Ok(out),
			_ => Err(failed()),
		}
	}

	fn header(&mut self) -> Vec<u8> {
		// files with data first, in the order their data was written, then directories and
		// empty files: some readers take a solid block's files to be listed next to each other
		self.files
			.sort_by_key(|file| !matches!(file.kind, FileKind::File { has_stream: true }));
		let mut out = vec![K_HEADER];
		if !self.folders.is_empty() {
			out.push(K_MAIN_STREAMS_INFO);
			write_streams(&mut out, 0, &self.pack_sizes, &self.folders, true);
		}
		if !self.files.is_empty() {
			out.push(K_FILES_INFO);
			write_number(&mut out, self.files.len() as u64);
			let empty_stream: Vec<bool> = self
				.files
				.iter()
				.map(|file| !matches!(file.kind, FileKind::File { has_stream: true }))
				.collect();
			if empty_stream.contains(&true) {
				let mut bits = Vec::new();
				write_bits(&mut bits, &empty_stream);
				property(&mut out, K_EMPTY_STREAM, &bits);
				let empty_file: Vec<bool> = self
					.files
					.iter()
					.filter_map(|file| match file.kind {
						FileKind::Dir => Some(false),
						FileKind::File { has_stream: false } => Some(true),
						FileKind::File { has_stream: true } => None,
					})
					.collect();
				if empty_file.contains(&true) {
					let mut bits = Vec::new();
					write_bits(&mut bits, &empty_file);
					property(&mut out, K_EMPTY_FILE, &bits);
				}
			}
			let mut names = vec![0];
			for file in &self.files {
				names.extend(
					file.name
						.encode_utf16()
						.chain([0])
						.flat_map(u16::to_le_bytes),
				);
			}
			property(&mut out, K_NAME, &names);
			let times: Vec<Option<u64>> = self
				.files
				.iter()
				.map(|file| file.modified.and_then(to_filetime))
				.collect();
			if times.iter().any(Option::is_some) {
				let mut data = Vec::new();
				write_defined(
					&mut data,
					&times.iter().map(Option::is_some).collect::<Vec<_>>(),
				);
				data.push(0);
				for time in times.into_iter().flatten() {
					data.extend_from_slice(&time.to_le_bytes());
				}
				property(&mut out, K_MTIME, &data);
			}
			let mut attributes = vec![1, 0];
			for file in &self.files {
				let attribute = match file.kind {
					FileKind::Dir => {
						ATTRIBUTE_DIRECTORY | ATTRIBUTE_UNIX_EXTENSION | ((UNIX_DIR | 0o755) << 16)
					}
					FileKind::File { .. } => ATTRIBUTE_UNIX_EXTENSION | (0o100_644 << 16),
				};
				attributes.extend_from_slice(&attribute.to_le_bytes());
			}
			property(&mut out, K_WIN_ATTRIBUTES, &attributes);
			out.push(K_END);
		}
		out.push(K_END);
		out
	}
}

/// A time as a Windows FILETIME, if it is after 1601.
fn to_filetime(time: DateTime<Utc>) -> Option<u64> {
	let secs = u64::try_from(time.timestamp().checked_add(FILETIME_UNIX_OFFSET_SECS)?).ok()?;
	secs.checked_mul(10_000_000)?
		.checked_add(u64::from(time.timestamp_subsec_nanos() / 100))
}

/// LZMA2's property byte for the smallest dictionary it can state that holds `dict`.
fn lzma2_dict_prop(dict: u32) -> u8 {
	(0u8..40)
		.find(|&bits| (2 | u32::from(bits & 1)) << (bits / 2 + 11) >= dict)
		.unwrap_or(40)
}

fn property(out: &mut Vec<u8>, id: u8, data: &[u8]) {
	out.push(id);
	write_number(out, data.len() as u64);
	out.extend_from_slice(data);
}

fn write_defined(out: &mut Vec<u8>, defined: &[bool]) {
	if defined.iter().all(|&defined| defined) {
		out.push(1);
	} else {
		out.push(0);
		write_bits(out, defined);
	}
}

fn write_streams(
	out: &mut Vec<u8>,
	pack_pos: u64,
	pack_sizes: &[u64],
	folders: &[FolderRecord],
	substreams: bool,
) {
	out.push(K_PACK_INFO);
	write_number(out, pack_pos);
	write_number(out, pack_sizes.len() as u64);
	out.push(K_SIZE);
	for &size in pack_sizes {
		write_number(out, size);
	}
	out.push(K_END);

	out.push(K_UNPACK_INFO);
	out.push(K_FOLDER);
	write_number(out, folders.len() as u64);
	out.push(0);
	for folder in folders {
		write_number(out, folder.coders.len() as u64);
		for coder in &folder.coders {
			let id = coder.method.id().to_be_bytes();
			let skip = id.iter().take(7).take_while(|&&byte| byte == 0).count();
			let id = &id[skip..];
			let props_flag = if coder.props.is_empty() { 0 } else { 0x20 };
			let id_len = u8::try_from(id.len())
				.expect("a method id is at most the 8 bytes of a u64 (should be impossible)");
			out.push(id_len | props_flag);
			out.extend_from_slice(id);
			if !coder.props.is_empty() {
				write_number(out, coder.props.len() as u64);
				out.extend_from_slice(&coder.props);
			}
		}
		// a chain: each coder's one input is the next coder's output
		for coder in 1..folder.coders.len() as u64 {
			write_number(out, coder - 1);
			write_number(out, coder);
		}
	}
	out.push(K_CODERS_UNPACK_SIZE);
	for folder in folders {
		for &size in &folder.unpack_sizes {
			write_number(out, size);
		}
	}
	let crcs: Vec<Option<u32>> = folders.iter().map(|folder| folder.crc).collect();
	if crcs.iter().any(Option::is_some) {
		out.push(K_CRC);
		write_defined(out, &crcs.iter().map(Option::is_some).collect::<Vec<_>>());
		for crc in crcs.into_iter().flatten() {
			out.extend_from_slice(&crc.to_le_bytes());
		}
	}
	out.push(K_END);

	if substreams {
		out.push(K_SUBSTREAMS_INFO);
		out.push(K_NUM_UNPACK_STREAM);
		for folder in folders {
			write_number(out, folder.substreams.len() as u64);
		}
		out.push(K_SIZE);
		for folder in folders {
			let (_, sizes) = folder
				.substreams
				.split_last()
				.expect("a folder holds a file");
			for &(size, _) in sizes {
				write_number(out, size);
			}
		}
		out.push(K_CRC);
		out.push(1);
		for folder in folders {
			for &(_, crc) in &folder.substreams {
				out.extend_from_slice(&crc.to_le_bytes());
			}
		}
		out.push(K_END);
	}
	out.push(K_END);
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn lzma2_dictionary_properties_round_up() {
		assert_eq!(lzma2_dict_prop(4096), 0);
		assert_eq!(lzma2_dict_prop(1 << 20), 16);
		assert_eq!(lzma2_dict_prop((1 << 20) + 1), 17);
		assert_eq!(lzma2_dict_prop(3 << 20), 19);
		assert_eq!(lzma2_dict_prop(64 << 20), 28);
		assert_eq!(lzma2_dict_prop(u32::MAX), 40);
	}

	#[test]
	fn levels_are_checked() {
		assert!(SevenZMethod::Copy.check().is_ok());
		assert!(SevenZMethod::Lzma2 { level: 0 }.check().is_ok());
		assert!(SevenZMethod::Lzma2 { level: 10 }.check().is_err());
		assert!(SevenZMethod::Ppmd { level: 0 }.check().is_err());
		assert!(SevenZMethod::Deflate { level: 9 }.check().is_ok());
		assert!(
			SevenZMethod::Lzma2 { level: 9 }.encoder_memory()
				> SevenZMethod::Lzma2 { level: 1 }.encoder_memory()
		);
		assert_eq!(
			SevenZMethod::Ppmd { level: 9 }.encoder_memory(),
			(192 << 20) + (1 << 20)
		);
	}

	#[test]
	fn filetimes_start_in_1601() {
		assert_eq!(
			to_filetime(DateTime::from_timestamp(0, 0).unwrap()),
			Some(116_444_736_000_000_000)
		);
		assert_eq!(
			to_filetime(DateTime::from_timestamp(-FILETIME_UNIX_OFFSET_SECS - 1, 0).unwrap()),
			None
		);
	}
}
