//! Reading a 7z: its header (plain, or compressed and encrypted as a "packed" header), then
//! each folder's coders chained into one reader of the folder's unpacked data.
//!
//! Every allocation is bounded before it is made: the header by `max_index_bytes`, counts by
//! what is left of the header and by `max_entries`, the parsed index by a heap budget, and each
//! folder's decoders by the codec budget (LZMA dictionaries clamped to the data they decode).

use std::{
	cell::RefCell,
	io::{self, BufReader, Read, Seek, SeekFrom},
	mem,
	rc::Rc,
};

use chrono::{DateTime, Utc};

use super::{
	SevenZError,
	crypto::{AES_ID, AesCbcReader, AesProps, Key, derive_key},
	from_source,
	header::*,
};
use crate::fs::archive::decode::clamp_lzma_dict;

/// Most coders in one folder; 7-Zip writes at most four (BCJ2 with its three LZMA coders).
const MAX_CODERS: u64 = 8;
/// Most input streams of one coder (BCJ2 has four).
const MAX_CODER_INPUTS: u64 = 8;
/// Most bytes of one coder's properties (AES's are 34 at most).
const MAX_PROPS: usize = 64;
/// Heap the parsed index may take, per byte of the header's budget.
const HEAP_PER_INDEX_BYTE: u64 = 3;
/// Rounds of key derivation one archive may cost, over all its keys: four at 7-Zip's
/// strongest setting the SDK reads.
const KDF_ROUNDS_BUDGET: u64 = 4 << super::crypto::MAX_CYCLES_POWER;
/// A decoder's input buffer.
const INPUT_BUFFER: usize = 64 * 1024;
/// Most distinct keys (salts and round counts) one archive may use; 7-Zip uses one.
const MAX_KEYS: usize = 16;

#[derive(Debug, Clone, Copy)]
pub(crate) struct SevenZLimits {
	pub(crate) max_index_bytes: u64,
	pub(crate) max_entries: u64,
	pub(crate) decoder_memory: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Branch {
	X86,
	Arm,
	ArmThumb,
	Arm64,
	Ppc,
	Sparc,
	Ia64,
	RiscV,
}

/// A coder the SDK decodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Method {
	Copy,
	Lzma,
	Lzma2,
	Ppmd,
	Bzip2,
	Deflate,
	Deflate64,
	Bcj(Branch),
	Bcj2,
	Delta,
	Aes,
}

impl Method {
	pub(crate) fn from_id(id: u64) -> Option<Self> {
		Some(match id {
			0x00 => Self::Copy,
			0x21 => Self::Lzma2,
			0x03 => Self::Delta,
			0x0A => Self::Bcj(Branch::Arm64),
			0x0B => Self::Bcj(Branch::RiscV),
			0x03_0101 => Self::Lzma,
			0x03_0401 => Self::Ppmd,
			0x0303_0103 => Self::Bcj(Branch::X86),
			0x0303_011B => Self::Bcj2,
			0x0303_0205 => Self::Bcj(Branch::Ppc),
			0x0303_0401 => Self::Bcj(Branch::Ia64),
			0x0303_0501 => Self::Bcj(Branch::Arm),
			0x0303_0701 => Self::Bcj(Branch::ArmThumb),
			0x0303_0805 => Self::Bcj(Branch::Sparc),
			0x04_0108 => Self::Deflate,
			0x04_0109 => Self::Deflate64,
			0x04_0202 => Self::Bzip2,
			AES_ID => Self::Aes,
			_ => return None,
		})
	}

	pub(crate) fn id(self) -> u64 {
		match self {
			Self::Copy => 0x00,
			Self::Lzma2 => 0x21,
			Self::Delta => 0x03,
			Self::Bcj(Branch::Arm64) => 0x0A,
			Self::Bcj(Branch::RiscV) => 0x0B,
			Self::Lzma => 0x03_0101,
			Self::Ppmd => 0x03_0401,
			Self::Bcj(Branch::X86) => 0x0303_0103,
			Self::Bcj2 => 0x0303_011B,
			Self::Bcj(Branch::Ppc) => 0x0303_0205,
			Self::Bcj(Branch::Ia64) => 0x0303_0401,
			Self::Bcj(Branch::Arm) => 0x0303_0501,
			Self::Bcj(Branch::ArmThumb) => 0x0303_0701,
			Self::Bcj(Branch::Sparc) => 0x0303_0805,
			Self::Deflate => 0x04_0108,
			Self::Deflate64 => 0x04_0109,
			Self::Bzip2 => 0x04_0202,
			Self::Aes => AES_ID,
		}
	}
}

#[derive(Debug, Clone)]
pub(crate) struct Coder {
	/// `None` for a coder the SDK does not decode.
	pub(crate) method: Option<Method>,
	pub(crate) props: Box<[u8]>,
	pub(crate) inputs: usize,
}

/// A chain of coders turning one or more packed streams into one unpacked stream. Every coder
/// has one output, so a coder's index is its output stream's.
#[derive(Debug, Clone)]
pub(crate) struct Folder {
	pub(crate) coders: Vec<Coder>,
	/// `(input stream, output stream)`: the coder output feeding a coder input.
	pub(crate) bind_pairs: Vec<(usize, usize)>,
	/// The input streams fed from packed streams, in the order of those.
	pub(crate) packed: Vec<usize>,
	pub(crate) unpack_sizes: Vec<u64>,
	pub(crate) crc: Option<u32>,
	/// This folder's first packed stream.
	pub(crate) first_pack: usize,
	/// The coder whose output is the folder's.
	pub(crate) main: usize,
}

impl Folder {
	pub(crate) fn size(&self) -> u64 {
		self.unpack_sizes[self.main]
	}

	pub(crate) fn encrypted(&self) -> bool {
		self.coders
			.iter()
			.any(|coder| coder.method == Some(Method::Aes))
	}

	pub(crate) fn supported(&self) -> bool {
		self.coders.iter().all(|coder| coder.method.is_some())
	}

	fn first_input(&self, coder: usize) -> usize {
		self.coders[..coder].iter().map(|c| c.inputs).sum()
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SevenZKind {
	File,
	Dir,
	Symlink,
	/// A deletion marker of an update archive: no content.
	Anti,
}

/// A file's data: its folder, and where in the folder's unpacked data it starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StreamRef {
	pub(crate) folder: usize,
	pub(crate) offset: u64,
}

#[derive(Debug, Clone)]
pub(crate) struct SevenZEntry {
	/// The entry's index among the archive's files.
	pub(crate) ordinal: u64,
	pub(crate) name: String,
	/// The name was not valid UTF-16 and was decoded lossily.
	pub(crate) name_rewritten: bool,
	pub(crate) kind: SevenZKind,
	pub(crate) size: u64,
	pub(crate) crc: Option<u32>,
	pub(crate) modified: Option<DateTime<Utc>>,
	pub(crate) stream: Option<StreamRef>,
}

#[derive(Debug, Clone)]
pub(crate) struct SevenZIndex {
	pub(crate) entries: Vec<SevenZEntry>,
	pub(crate) folders: Vec<Folder>,
	/// Where each packed stream starts in the archive.
	pub(crate) pack_offsets: Vec<u64>,
	pub(crate) pack_sizes: Vec<u64>,
	/// The header itself was encrypted (so the password is verified: it decoded it).
	pub(crate) headers_encrypted: bool,
	/// Bytes belonging to no packed stream and no header.
	pub(crate) unaccounted_bytes: u64,
}

impl SevenZIndex {
	/// The most packed streams a folder reads in turns, over the folders that can be read.
	pub(crate) fn max_packed_streams(&self) -> usize {
		self.folders
			.iter()
			.filter(|folder| folder.supported())
			.map(|folder| folder.packed.len())
			.max()
			.unwrap_or(1)
	}
}

/// What reading may spend on its index's heap.
struct Heap {
	left: u64,
}

impl Heap {
	fn charge(&mut self, bytes: u64) -> Result<(), SevenZError> {
		self.left = self
			.left
			.checked_sub(bytes)
			.ok_or(SevenZError::TooLarge("a 7z index over the memory budget"))?;
		Ok(())
	}
}

/// Derived keys, kept for the folders that share them, and the rounds still allowed.
pub(crate) struct Keys<'p> {
	password: Option<&'p [u8]>,
	derived: Vec<(AesProps, Key)>,
	rounds_left: u64,
}

impl<'p> Keys<'p> {
	/// `password` as UTF-16LE.
	pub(crate) fn new(password: Option<&'p [u8]>) -> Self {
		Self {
			password,
			derived: Vec::new(),
			rounds_left: KDF_ROUNDS_BUDGET,
		}
	}

	fn key(&mut self, props: &AesProps) -> Result<Key, SevenZError> {
		let password = self.password.ok_or(SevenZError::PasswordRequired)?;
		// the IV is not part of the key
		if let Some((_, key)) = self.derived.iter().find(|(derived, _)| {
			derived.cycles_power == props.cycles_power && derived.salt == props.salt
		}) {
			return Ok(key.clone());
		}
		if self.derived.len() >= MAX_KEYS {
			return Err(SevenZError::Unsupported("a 7z archive with too many keys"));
		}
		let rounds = match props.cycles_power {
			// every key costs at least a round, so salts alone cannot make keys without end
			0x3F => 1,
			power if power <= super::crypto::MAX_CYCLES_POWER => 1u64 << power,
			_ => {
				return Err(SevenZError::Unsupported(
					"a 7z key derivation over the rounds the SDK spends",
				));
			}
		};
		self.rounds_left = self
			.rounds_left
			.checked_sub(rounds)
			.ok_or(SevenZError::Unsupported(
				"7z keys that together take too long to derive",
			))?;
		let key = derive_key(password, props, &mut || {})?;
		self.derived.push((props.clone(), key.clone()));
		Ok(key)
	}
}

/// Reads the index of the 7z `source` of `len` bytes.
pub(crate) fn read_index<R: Read + Seek>(
	source: &mut R,
	len: u64,
	limits: SevenZLimits,
	keys: &mut Keys<'_>,
) -> Result<SevenZIndex, SevenZError> {
	let start = read_at(source, 0, START_HEADER_LEN as usize)?;
	if start[..6] != SIGNATURE {
		return Err(SevenZError::Corrupt("not a 7z archive"));
	}
	if start[6] != 0 {
		return Err(SevenZError::Unsupported("a 7z format version after 0.x"));
	}
	let mut fields = HeaderReader::new(&start[8..]);
	let start_crc = fields.u32()?;
	if crc32fast::hash(&start[12..]) != start_crc {
		return Err(SevenZError::Corrupt(
			"the 7z start header's CRC does not match",
		));
	}
	let next_offset = fields.u64()?;
	let next_size = fields.u64()?;
	let next_crc = fields.u32()?;
	let mut heap = Heap {
		left: limits.max_index_bytes.saturating_mul(HEAP_PER_INDEX_BYTE),
	};
	if next_size == 0 {
		// an empty archive
		return Ok(SevenZIndex {
			entries: Vec::new(),
			folders: Vec::new(),
			pack_offsets: Vec::new(),
			pack_sizes: Vec::new(),
			headers_encrypted: false,
			unaccounted_bytes: len.saturating_sub(START_HEADER_LEN),
		});
	}
	let header_at = START_HEADER_LEN
		.checked_add(next_offset)
		.filter(|at| at.checked_add(next_size).is_some_and(|end| end <= len))
		.ok_or(SevenZError::Corrupt(
			"the 7z header lies outside the archive",
		))?;
	if next_size > limits.max_index_bytes {
		return Err(SevenZError::TooLarge("a 7z header over the index budget"));
	}
	let header = read_at(source, header_at, next_size as usize)?;
	if crc32fast::hash(&header) != next_crc {
		return Err(SevenZError::Corrupt("the 7z header's CRC does not match"));
	}
	let mut covered = vec![(0, START_HEADER_LEN), (header_at, header_at + next_size)];

	let mut reader = HeaderReader::new(&header);
	let mut header_checked = false;
	let (header, headers_encrypted) = match reader.property()? {
		K_HEADER => (header, false),
		K_ENCODED_HEADER => {
			let streams = read_streams_info(&mut reader, limits, &mut heap)?;
			let [folder] = &streams.folders[..] else {
				return Err(SevenZError::Corrupt(
					"a packed 7z header of other than one folder",
				));
			};
			if folder.size() > limits.max_index_bytes {
				return Err(SevenZError::TooLarge("a 7z header over the index budget"));
			}
			let offsets = pack_offsets(&streams, len)?;
			covered.extend(
				offsets
					.iter()
					.zip(&streams.pack_sizes)
					.map(|(&at, &size)| (at, at + size)),
			);
			let encrypted = folder.encrypted();
			// a CRC proves the key decoded it: what fails after that is the header's own damage
			header_checked = folder.crc.is_some();
			let decoded =
				decode_header(source, folder, &offsets, &streams.pack_sizes, limits, keys)?;
			if decoded.first() != Some(&K_HEADER) {
				return Err(if encrypted && !header_checked {
					SevenZError::WrongPassword
				} else {
					SevenZError::Corrupt("a packed 7z header that is not a header")
				});
			}
			(decoded, encrypted)
		}
		_ => return Err(SevenZError::Corrupt("the 7z header has an unknown type")),
	};
	let mut reader = HeaderReader::new(&header);
	reader.byte()?;
	let mut index =
		read_header(&mut reader, limits, &mut heap, len).map_err(|error| match error {
			SevenZError::Corrupt(_) if headers_encrypted && !header_checked => {
				SevenZError::WrongPassword
			}
			error => error,
		})?;
	index.headers_encrypted = headers_encrypted;
	covered.extend(
		index
			.pack_offsets
			.iter()
			.zip(&index.pack_sizes)
			.map(|(&at, &size)| (at, at + size)),
	);
	index.unaccounted_bytes = unaccounted(covered, len)?;
	// a folder no file takes its data from holds bytes nothing extracts
	let mut used = vec![false; index.folders.len()];
	for stream in index.entries.iter().filter_map(|entry| entry.stream) {
		used[stream.folder] = true;
	}
	for (folder, _) in index.folders.iter().zip(used).filter(|(_, used)| !used) {
		let packed = &index.pack_sizes[folder.first_pack..folder.first_pack + folder.packed.len()];
		index.unaccounted_bytes = packed.iter().fold(index.unaccounted_bytes, |total, &size| {
			total.saturating_add(size)
		});
	}
	Ok(index)
}

/// Bytes of `0..len` in none of the `covered` ranges; overlapping ranges are corrupt.
fn unaccounted(mut covered: Vec<(u64, u64)>, len: u64) -> Result<u64, SevenZError> {
	covered.retain(|(start, end)| end > start);
	covered.sort_unstable();
	let mut end = 0;
	let mut total = 0u64;
	for (start, stop) in covered {
		if start < end {
			return Err(SevenZError::Corrupt("7z packed streams overlap"));
		}
		total += stop - start;
		end = stop;
	}
	Ok(len - total)
}

fn read_at<R: Read + Seek>(source: &mut R, at: u64, len: usize) -> Result<Vec<u8>, SevenZError> {
	source.seek(SeekFrom::Start(at))?;
	let mut bytes = vec![0; len];
	source.read_exact(&mut bytes).map_err(|error| {
		if error.kind() == io::ErrorKind::UnexpectedEof {
			SevenZError::Corrupt("the 7z archive ends early")
		} else {
			SevenZError::Read(error)
		}
	})?;
	Ok(bytes)
}

/// Decodes a packed header's one folder in full, checked against its CRC.
fn decode_header<R: Read + Seek>(
	source: &mut R,
	folder: &Folder,
	offsets: &[u64],
	sizes: &[u64],
	limits: SevenZLimits,
	keys: &mut Keys<'_>,
) -> Result<Vec<u8>, SevenZError> {
	let shared = Rc::new(RefCell::new(source));
	// setting the coders up (their properties, the streams they name) fails for the archive's
	// own reasons; decoding under a wrong key fails for the key's
	let mut reader = open_folder(&shared, folder, offsets, sizes, limits.decoder_memory, keys)?;
	let decoding = |error| {
		if folder.encrypted() {
			wrong_key(error)
		} else {
			error
		}
	};
	let mut decoded = Vec::new();
	(&mut reader)
		.take(folder.size())
		.read_to_end(&mut decoded)
		.map_err(|error| decoding(read_error(error)))?;
	if decoded.len() as u64 != folder.size() {
		return Err(decoding(SevenZError::Corrupt(
			"a packed 7z header ends early",
		)));
	}
	if folder
		.crc
		.is_some_and(|crc| crc32fast::hash(&decoded) != crc)
	{
		return Err(decoding(SevenZError::Corrupt(
			"a packed 7z header's CRC does not match",
		)));
	}
	Ok(decoded)
}

#[derive(Default)]
struct StreamsInfo {
	pack_pos: u64,
	pack_sizes: Vec<u64>,
	folders: Vec<Folder>,
	/// Per folder, its substreams.
	substreams: Vec<Substreams>,
}

/// A folder's substreams (its files' data, one after the other): each one's size and CRC-32.
type Substreams = Vec<(u64, Option<u32>)>;

fn pack_offsets(streams: &StreamsInfo, len: u64) -> Result<Vec<u64>, SevenZError> {
	let mut at = START_HEADER_LEN
		.checked_add(streams.pack_pos)
		.ok_or(SevenZError::Corrupt(
			"7z packed streams lie outside the archive",
		))?;
	streams
		.pack_sizes
		.iter()
		.map(|&size| {
			let start = at;
			at = at
				.checked_add(size)
				.filter(|&end| end <= len)
				.ok_or(SevenZError::Corrupt(
					"7z packed streams lie outside the archive",
				))?;
			Ok(start)
		})
		.collect()
}

fn read_streams_info(
	reader: &mut HeaderReader<'_>,
	limits: SevenZLimits,
	heap: &mut Heap,
) -> Result<StreamsInfo, SevenZError> {
	let mut info = StreamsInfo::default();
	let mut id = reader.property()?;
	if id == K_PACK_INFO {
		info.pack_pos = reader.number()?;
		let count = reader.count(limits.max_entries)?;
		heap.charge(count as u64 * 8)?;
		reader.wait_for(K_SIZE)?;
		info.pack_sizes = (0..count)
			.map(|_| reader.number())
			.collect::<Result<_, _>>()?;
		loop {
			match reader.property()? {
				K_END => break,
				K_CRC => drop(reader.digests(count)?),
				_ => reader.skip_data()?,
			}
		}
		id = reader.property()?;
	}
	if id == K_UNPACK_INFO {
		reader.wait_for(K_FOLDER)?;
		let count = reader.count(limits.max_entries)?;
		reader.not_external()?;
		let mut first_pack = 0;
		for _ in 0..count {
			let mut folder = read_folder(reader, heap)?;
			folder.first_pack = first_pack;
			first_pack += folder.packed.len();
			info.folders.push(folder);
		}
		if first_pack != info.pack_sizes.len() {
			return Err(SevenZError::Corrupt(
				"7z folders and packed streams do not match",
			));
		}
		reader.wait_for(K_CODERS_UNPACK_SIZE)?;
		for folder in &mut info.folders {
			folder.unpack_sizes = (0..folder.coders.len())
				.map(|_| reader.number())
				.collect::<Result<_, _>>()?;
		}
		loop {
			match reader.property()? {
				K_END => break,
				K_CRC => {
					let crcs = reader.digests(count)?;
					for (folder, crc) in info.folders.iter_mut().zip(crcs) {
						folder.crc = crc;
					}
				}
				_ => reader.skip_data()?,
			}
		}
		id = reader.property()?;
	} else if !info.pack_sizes.is_empty() {
		return Err(SevenZError::Corrupt("7z packed streams without folders"));
	}
	if id == K_SUBSTREAMS_INFO {
		info.substreams = read_substreams(reader, &info.folders, limits, heap)?;
		id = reader.property()?;
	} else {
		info.substreams = info
			.folders
			.iter()
			.map(|folder| vec![(folder.size(), folder.crc)])
			.collect();
	}
	if id != K_END {
		return Err(SevenZError::Corrupt(
			"7z streams info has an unknown section",
		));
	}
	Ok(info)
}

fn read_folder(reader: &mut HeaderReader<'_>, heap: &mut Heap) -> Result<Folder, SevenZError> {
	let coder_count = reader.count(MAX_CODERS)?;
	if coder_count == 0 {
		return Err(SevenZError::Corrupt("a 7z folder without coders"));
	}
	let mut coders = Vec::with_capacity(coder_count);
	for _ in 0..coder_count {
		let flags = reader.byte()?;
		if flags & 0xC0 != 0 {
			return Err(SevenZError::Unsupported("7z alternative coder methods"));
		}
		let id_len = usize::from(flags & 0x0F);
		if id_len > 8 {
			return Err(SevenZError::Unsupported("a 7z coder id over 8 bytes"));
		}
		let id = reader
			.bytes(id_len)?
			.iter()
			.fold(0u64, |id, &byte| (id << 8) | u64::from(byte));
		let (inputs, outputs) = if flags & 0x10 != 0 {
			(
				reader.count(MAX_CODER_INPUTS)?,
				reader.count(MAX_CODER_INPUTS)?,
			)
		} else {
			(1, 1)
		};
		if inputs == 0 || outputs != 1 {
			return Err(SevenZError::Unsupported(
				"a 7z coder with other than one output",
			));
		}
		// only BCJ2 takes several streams, and exactly four: anything else would only make the
		// reader keep more of the archive at hand
		if inputs != 1 && !(Method::from_id(id) == Some(Method::Bcj2) && inputs == 4) {
			return Err(SevenZError::Unsupported("a 7z coder with several inputs"));
		}
		let props: Box<[u8]> = if flags & 0x20 != 0 {
			let len = reader.length()?;
			if len > MAX_PROPS {
				return Err(SevenZError::Unsupported(
					"7z coder properties over 64 bytes",
				));
			}
			reader.bytes(len)?.into()
		} else {
			Box::default()
		};
		heap.charge((mem::size_of::<Coder>() + props.len()) as u64)?;
		coders.push(Coder {
			method: Method::from_id(id),
			props,
			inputs,
		});
	}
	let total_inputs: usize = coders.iter().map(|coder| coder.inputs).sum();
	let bind_count = coder_count - 1;
	if total_inputs < bind_count {
		return Err(SevenZError::Corrupt(
			"a 7z folder with too few coder inputs",
		));
	}
	let index = |reader: &mut HeaderReader<'_>, below: usize| {
		let at = reader.number()?;
		usize::try_from(at)
			.ok()
			.filter(|&at| at < below)
			.ok_or(SevenZError::Corrupt(
				"a 7z folder refers to a stream it lacks",
			))
	};
	let mut bind_pairs = Vec::with_capacity(bind_count);
	for _ in 0..bind_count {
		let input = index(reader, total_inputs)?;
		let output = index(reader, coder_count)?;
		if bind_pairs.iter().any(|&(i, o)| i == input || o == output) {
			return Err(SevenZError::Corrupt("a 7z folder binds a stream twice"));
		}
		bind_pairs.push((input, output));
	}
	let packed_count = total_inputs - bind_count;
	let bound = |input: usize| bind_pairs.iter().any(|&(i, _)| i == input);
	let packed = if packed_count == 1 {
		vec![
			(0..total_inputs)
				.find(|&input| !bound(input))
				.expect("one input is left unbound"),
		]
	} else {
		let mut packed = Vec::with_capacity(packed_count);
		for _ in 0..packed_count {
			let input = index(reader, total_inputs)?;
			if bound(input) || packed.contains(&input) {
				return Err(SevenZError::Corrupt("a 7z folder feeds a stream twice"));
			}
			packed.push(input);
		}
		packed
	};
	let main = (0..coder_count)
		.find(|&coder| bind_pairs.iter().all(|&(_, output)| output != coder))
		.expect("one output is left unbound");
	heap.charge(
		(mem::size_of::<Folder>() + 16 * (bind_count + packed_count + coder_count)) as u64,
	)?;
	let folder = Folder {
		coders,
		bind_pairs,
		packed,
		unpack_sizes: Vec::new(),
		crc: None,
		first_pack: 0,
		main,
	};
	check_acyclic(&folder)?;
	Ok(folder)
}

/// Every coder has to be reachable from the folder's output without going round in a circle.
fn check_acyclic(folder: &Folder) -> Result<(), SevenZError> {
	fn visit(folder: &Folder, coder: usize, depth: usize) -> Result<(), SevenZError> {
		if depth > folder.coders.len() {
			return Err(SevenZError::Corrupt("7z coders bound in a circle"));
		}
		let first = folder.first_input(coder);
		for input in first..first + folder.coders[coder].inputs {
			if let Some(&(_, output)) = folder.bind_pairs.iter().find(|(i, _)| *i == input) {
				visit(folder, output, depth + 1)?;
			}
		}
		Ok(())
	}
	visit(folder, folder.main, 0)
}

fn read_substreams(
	reader: &mut HeaderReader<'_>,
	folders: &[Folder],
	limits: SevenZLimits,
	heap: &mut Heap,
) -> Result<Vec<Substreams>, SevenZError> {
	let mut counts = vec![1u64; folders.len()];
	let mut id;
	loop {
		id = reader.property()?;
		match id {
			K_NUM_UNPACK_STREAM => {
				for count in &mut counts {
					*count = reader.number()?;
				}
			}
			K_CRC | K_SIZE | K_END => break,
			_ => reader.skip_data()?,
		}
	}
	let total = counts
		.iter()
		.try_fold(0u64, |total, &count| total.checked_add(count))
		.filter(|&total| total <= limits.max_entries)
		.ok_or(SevenZError::TooLarge("a 7z header lists too many items"))?;
	heap.charge(total * 24)?;
	let mut substreams: Vec<Substreams> = Vec::with_capacity(folders.len());
	for (folder, &count) in folders.iter().zip(&counts) {
		let mut sizes = Vec::with_capacity(count.min(reader.remaining() as u64 + 1) as usize);
		if count > 0 {
			let mut sum = 0u64;
			if id == K_SIZE {
				for _ in 1..count {
					let size = reader.number()?;
					sum = sum
						.checked_add(size)
						.ok_or(SevenZError::Corrupt("7z substream sizes overflow"))?;
					sizes.push((size, None));
				}
			} else if count > 1 {
				return Err(SevenZError::Corrupt("7z substreams without sizes"));
			}
			let last = folder.size().checked_sub(sum).ok_or(SevenZError::Corrupt(
				"7z substreams larger than their folder",
			))?;
			sizes.push((last, None));
		}
		substreams.push(sizes);
	}
	if id == K_SIZE {
		id = reader.property()?;
	}
	// a folder of one substream with a CRC of its own needs no other
	let inherits = |folder: &Folder, count: u64| count == 1 && folder.crc.is_some();
	for (folder, streams) in folders.iter().zip(&mut substreams) {
		if inherits(folder, streams.len() as u64) {
			streams[0].1 = folder.crc;
		}
	}
	let digest_count: u64 = folders
		.iter()
		.zip(&counts)
		.filter(|&(folder, &count)| !inherits(folder, count))
		.map(|(_, &count)| count)
		.sum();
	loop {
		match id {
			K_END => break,
			K_CRC => {
				let mut digests = reader.digests(digest_count as usize)?.into_iter();
				for (folder, streams) in folders.iter().zip(&mut substreams) {
					if !inherits(folder, streams.len() as u64) {
						for stream in streams.iter_mut() {
							stream.1 = digests.next().flatten();
						}
					}
				}
			}
			_ => reader.skip_data()?,
		}
		id = reader.property()?;
	}
	Ok(substreams)
}

fn read_header(
	reader: &mut HeaderReader<'_>,
	limits: SevenZLimits,
	heap: &mut Heap,
	len: u64,
) -> Result<SevenZIndex, SevenZError> {
	let mut id = reader.property()?;
	if id == K_ARCHIVE_PROPERTIES {
		while reader.number()? != 0 {
			reader.skip_data()?;
		}
		id = reader.property()?;
	}
	if id == K_ADDITIONAL_STREAMS_INFO {
		return Err(SevenZError::Unsupported("7z additional streams"));
	}
	let mut streams = StreamsInfo::default();
	if id == K_MAIN_STREAMS_INFO {
		streams = read_streams_info(reader, limits, heap)?;
		id = reader.property()?;
	}
	let pack_offsets = pack_offsets(&streams, len)?;
	let mut entries = Vec::new();
	if id == K_FILES_INFO {
		entries = read_files(reader, &streams, limits, heap)?;
		id = reader.property()?;
	} else if streams.substreams.iter().any(|streams| !streams.is_empty()) {
		return Err(SevenZError::Corrupt("7z data without files"));
	}
	if id != K_END {
		return Err(SevenZError::Corrupt("the 7z header has an unknown section"));
	}
	Ok(SevenZIndex {
		entries,
		folders: streams.folders,
		pack_offsets,
		pack_sizes: streams.pack_sizes,
		headers_encrypted: false,
		unaccounted_bytes: 0,
	})
}

fn read_files(
	reader: &mut HeaderReader<'_>,
	streams: &StreamsInfo,
	limits: SevenZLimits,
	heap: &mut Heap,
) -> Result<Vec<SevenZEntry>, SevenZError> {
	let count = reader.count(limits.max_entries)?;
	heap.charge(count as u64 * mem::size_of::<SevenZEntry>() as u64)?;
	let mut empty_stream = vec![false; count];
	let mut empty_file = Vec::new();
	let mut anti = Vec::new();
	let mut names: Vec<(String, bool)> = Vec::new();
	let mut modified: Vec<Option<u64>> = vec![None; count];
	let mut attributes: Vec<Option<u32>> = vec![None; count];
	loop {
		let id = reader.number()?;
		if id == u64::from(K_END) {
			break;
		}
		let size = reader.length()?;
		let mut property = HeaderReader::new(reader.bytes(size)?);
		match u8::try_from(id) {
			Ok(K_EMPTY_STREAM) => {
				empty_stream = property.bits(count)?;
				let empties = empty_stream.iter().filter(|&&empty| empty).count();
				empty_file = vec![false; empties];
				anti = vec![false; empties];
			}
			Ok(K_EMPTY_FILE) => empty_file = property.bits(empty_file.len())?,
			Ok(K_ANTI) => anti = property.bits(anti.len())?,
			Ok(K_NAME) => {
				property.not_external()?;
				names = read_names(&mut property, count, heap)?;
			}
			Ok(K_MTIME) => {
				let defined = property.defined(count)?;
				property.not_external()?;
				for (time, defined) in modified.iter_mut().zip(defined) {
					*time = defined.then(|| property.u64()).transpose()?;
				}
			}
			Ok(K_WIN_ATTRIBUTES) => {
				let defined = property.defined(count)?;
				property.not_external()?;
				for (attribute, defined) in attributes.iter_mut().zip(defined) {
					*attribute = defined.then(|| property.u32()).transpose()?;
				}
			}
			// padding, and times other than the modification time, are not kept
			_ => {}
		}
	}
	if names.len() != count {
		return Err(SevenZError::Corrupt("7z files without names"));
	}

	let mut substreams = streams
		.substreams
		.iter()
		.enumerate()
		.flat_map(|(folder, streams)| {
			streams.iter().scan(0u64, move |offset, &(size, crc)| {
				let stream = StreamRef {
					folder,
					offset: *offset,
				};
				// the sizes sum to the folder's, a u64
				*offset += size;
				Some((stream, size, crc))
			})
		});
	let mut empty_at = 0;
	let mut entries = Vec::with_capacity(count);
	for (ordinal, (name, name_rewritten)) in names.into_iter().enumerate() {
		let attribute = attributes[ordinal];
		let unix_type = attribute
			.filter(|attribute| attribute & ATTRIBUTE_UNIX_EXTENSION != 0)
			.map(|attribute| (attribute >> 16) & UNIX_TYPE_MASK);
		let symlink = unix_type == Some(UNIX_SYMLINK)
			|| attribute.is_some_and(|attribute| attribute & ATTRIBUTE_REPARSE_POINT != 0);
		let (kind, stream, size, crc) = if empty_stream[ordinal] {
			let (is_empty_file, is_anti) = (empty_file[empty_at], anti[empty_at]);
			empty_at += 1;
			let kind = if is_anti {
				SevenZKind::Anti
			} else if symlink && is_empty_file {
				SevenZKind::Symlink
			} else if is_empty_file {
				SevenZKind::File
			} else {
				SevenZKind::Dir
			};
			(kind, None, 0, None)
		} else {
			let (stream, size, crc) = substreams.next().ok_or(SevenZError::Corrupt(
				"7z files outnumber their data streams",
			))?;
			let kind = if symlink {
				SevenZKind::Symlink
			} else {
				SevenZKind::File
			};
			(kind, Some(stream), size, crc)
		};
		entries.push(SevenZEntry {
			ordinal: ordinal as u64,
			name,
			name_rewritten,
			kind,
			size,
			crc,
			modified: modified[ordinal].and_then(filetime),
			stream,
		});
	}
	if substreams.next().is_some() {
		return Err(SevenZError::Corrupt(
			"7z data streams outnumber their files",
		));
	}
	Ok(entries)
}

/// `count` null-terminated UTF-16LE names filling the property.
fn read_names(
	property: &mut HeaderReader<'_>,
	count: usize,
	heap: &mut Heap,
) -> Result<Vec<(String, bool)>, SevenZError> {
	let bytes = property.bytes(property.remaining())?;
	if bytes.len() % 2 != 0 {
		return Err(SevenZError::Corrupt("7z names of an odd length"));
	}
	heap.charge(bytes.len() as u64 * 2)?;
	let units: Vec<u16> = bytes
		.chunks_exact(2)
		.map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
		.collect();
	let names: Vec<(String, bool)> = units
		.split_inclusive(|&unit| unit == 0)
		.map(|name| {
			let name = name
				.strip_suffix(&[0])
				.ok_or(SevenZError::Corrupt("a 7z name without its end"))?;
			let mut rewritten = false;
			let decoded = char::decode_utf16(name.iter().copied())
				.map(|unit| {
					unit.unwrap_or_else(|_| {
						rewritten = true;
						char::REPLACEMENT_CHARACTER
					})
				})
				.collect();
			Ok((decoded, rewritten))
		})
		.collect::<Result<_, SevenZError>>()?;
	if names.len() != count {
		return Err(SevenZError::Corrupt("7z names do not match the files"));
	}
	Ok(names)
}

/// A Windows FILETIME (100 ns ticks since 1601) as a time, if chrono can hold it.
pub(crate) fn filetime(ticks: u64) -> Option<DateTime<Utc>> {
	let secs = i64::try_from(ticks / 10_000_000).ok()? - FILETIME_UNIX_OFFSET_SECS;
	DateTime::from_timestamp(secs, (ticks % 10_000_000) as u32 * 100)
}

/// A packed stream read through the archive shared by all of a folder's packed streams, each
/// at its own position.
struct PackStream<R> {
	source: Rc<RefCell<R>>,
	at: u64,
	end: u64,
}

impl<R: Read + Seek> Read for PackStream<R> {
	fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
		let left = self.end - self.at;
		if left == 0 || buf.is_empty() {
			return Ok(0);
		}
		let want = buf.len().min(usize::try_from(left).unwrap_or(usize::MAX));
		let mut source = self.source.borrow_mut();
		source.seek(SeekFrom::Start(self.at))?;
		let n = source.read(&mut buf[..want])?;
		if n == 0 {
			return Err(io::ErrorKind::UnexpectedEof.into());
		}
		self.at += n as u64;
		Ok(n)
	}
}

/// The memory `coder` of `folder` decodes with, in bytes; `None` when its properties are not
/// ones the SDK reads.
fn coder_memory(folder: &Folder, coder: usize) -> Result<u64, SevenZError> {
	let Coder { method, props, .. } = &folder.coders[coder];
	let size = folder.unpack_sizes[coder];
	let buffers = INPUT_BUFFER as u64 * folder.coders[coder].inputs as u64;
	Ok(buffers
		+ match method.ok_or(SevenZError::Unsupported("a 7z coder"))? {
			Method::Lzma => {
				let (props, dict) = lzma_props(props)?;
				u64::from(
					lzma_rust2::lzma_get_memory_usage_by_props(
						clamp_lzma_dict(dict, Some(size)),
						props,
					)
					.map_err(|_| SevenZError::Corrupt("invalid 7z LZMA properties"))?,
				) * 1024
			}
			Method::Lzma2 => {
				let dict = clamp_lzma_dict(lzma2_dict(props)?, Some(size));
				u64::from(lzma_rust2::lzma2_get_memory_usage(dict)) * 1024
			}
			Method::Ppmd => u64::from(ppmd_props(props)?.1),
			// bzip2's worst case (-9) is under 4 MiB
			Method::Bzip2 => 4 << 20,
			Method::Deflate64 => 256 << 10,
			// its four stream buffers, 256 KiB each
			Method::Bcj2 => 1 << 20,
			Method::Copy | Method::Deflate | Method::Bcj(_) | Method::Delta | Method::Aes => 0,
		})
}

fn lzma_props(props: &[u8]) -> Result<(u8, u32), SevenZError> {
	match props {
		[props, dict @ ..] if dict.len() == 4 => Ok((
			*props,
			u32::from_le_bytes(dict.try_into().expect("4 bytes")),
		)),
		_ => Err(SevenZError::Corrupt("invalid 7z LZMA properties")),
	}
}

/// LZMA2's dictionary size, from its one property byte.
fn lzma2_dict(props: &[u8]) -> Result<u32, SevenZError> {
	match props {
		[40] => Ok(u32::MAX),
		&[bits] if bits < 40 => Ok((2 | u32::from(bits & 1)) << (bits / 2 + 11)),
		_ => Err(SevenZError::Corrupt("invalid 7z LZMA2 properties")),
	}
}

/// PPMd's model order and memory size.
fn ppmd_props(props: &[u8]) -> Result<(u32, u32), SevenZError> {
	match props {
		[order, memory @ ..] if memory.len() == 4 => {
			let memory = u32::from_le_bytes(memory.try_into().expect("4 bytes"));
			let order = u32::from(*order);
			if !(ppmd_rust::PPMD7_MIN_ORDER..=ppmd_rust::PPMD7_MAX_ORDER).contains(&order)
				|| !(ppmd_rust::PPMD7_MIN_MEM_SIZE..=ppmd_rust::PPMD7_MAX_MEM_SIZE)
					.contains(&memory)
			{
				return Err(SevenZError::Corrupt("invalid 7z PPMd properties"));
			}
			Ok((order, memory))
		}
		_ => Err(SevenZError::Corrupt("invalid 7z PPMd properties")),
	}
}

/// A branch filter's start offset: none, or four bytes.
fn branch_start(props: &[u8]) -> Result<usize, SevenZError> {
	match props {
		[] => Ok(0),
		start if start.len() == 4 => {
			usize::try_from(u32::from_le_bytes(start.try_into().expect("4 bytes")))
				.map_err(|_| SevenZError::Unsupported("a 7z branch filter offset"))
		}
		_ => Err(SevenZError::Corrupt("invalid 7z branch filter properties")),
	}
}

/// Opens `folder`'s unpacked data, its coders chained from the packed streams at `offsets`.
/// Fails with [`SevenZError::TooLarge`] before allocating when its decoders need more than
/// `decoder_memory`.
pub(crate) fn open_folder<'s, R: Read + Seek + 's>(
	source: &Rc<RefCell<R>>,
	folder: &Folder,
	offsets: &[u64],
	sizes: &[u64],
	decoder_memory: u64,
	keys: &mut Keys<'_>,
) -> Result<Box<dyn Read + 's>, SevenZError> {
	if !folder.supported() {
		return Err(SevenZError::Unsupported("a 7z coder"));
	}
	let memory = (0..folder.coders.len()).try_fold(0u64, |total, coder| {
		Ok::<_, SevenZError>(total.saturating_add(coder_memory(folder, coder)?))
	})?;
	if memory > decoder_memory {
		return Err(SevenZError::TooLarge(
			"a 7z folder's decoders are over the codec budget",
		));
	}
	let mut builder = Builder {
		source,
		folder,
		offsets,
		sizes,
		keys,
	};
	builder.output(folder.main)
}

struct Builder<'b, 'k, 'p, R> {
	source: &'b Rc<RefCell<R>>,
	folder: &'b Folder,
	offsets: &'b [u64],
	sizes: &'b [u64],
	keys: &'k mut Keys<'p>,
}

impl<'s, R: Read + Seek + 's> Builder<'_, '_, '_, R> {
	/// The reader of `coder`'s output (acyclic: checked when the folder was read).
	fn output(&mut self, coder: usize) -> Result<Box<dyn Read + 's>, SevenZError> {
		let folder = self.folder;
		let first = folder.first_input(coder);
		let mut inputs = Vec::with_capacity(folder.coders[coder].inputs);
		for input in first..first + folder.coders[coder].inputs {
			let reader = match folder.bind_pairs.iter().find(|(i, _)| *i == input) {
				Some(&(_, output)) => self.output(output)?,
				None => {
					let packed = folder
						.packed
						.iter()
						.position(|&packed| packed == input)
						.expect("every unbound input is packed");
					let stream = folder.first_pack + packed;
					let (&at, &size) = self.offsets.get(stream).zip(self.sizes.get(stream)).ok_or(
						SevenZError::Corrupt("a 7z folder refers to a missing stream"),
					)?;
					Box::new(PackStream {
						source: Rc::clone(self.source),
						at,
						end: at + size,
					}) as Box<dyn Read + 's>
				}
			};
			inputs.push(reader);
		}
		let Coder { method, props, .. } = &folder.coders[coder];
		let size = folder.unpack_sizes[coder];
		let method = method.expect("checked supported");
		if method != Method::Bcj2 && inputs.len() != 1 {
			return Err(SevenZError::Unsupported("a 7z coder with several inputs"));
		}
		let buffered = |input: Box<dyn Read + 's>| BufReader::with_capacity(INPUT_BUFFER, input);
		let reader: Box<dyn Read + 's> = match method {
			Method::Bcj2 => {
				if inputs.len() != 4 {
					return Err(SevenZError::Corrupt("a 7z BCJ2 coder without four inputs"));
				}
				// the filter keeps its size as a usize
				if usize::try_from(size).is_err() {
					return Err(SevenZError::Unsupported(
						"a 7z BCJ2 folder over what this platform can address",
					));
				}
				let inputs = inputs.into_iter().map(buffered).collect();
				Box::new(lzma_rust2::filter::bcj2::Bcj2Reader::new(inputs, size))
			}
			_ => {
				let input = inputs.pop().expect("one input");
				match method {
					Method::Copy => input,
					Method::Lzma => {
						let (props, dict) = lzma_props(props)?;
						Box::new(
							lzma_rust2::LzmaReader::new_with_props(
								buffered(input),
								size,
								props,
								clamp_lzma_dict(dict, Some(size)),
								None,
							)
							.map_err(|_| SevenZError::Corrupt("invalid 7z LZMA properties"))?,
						)
					}
					Method::Lzma2 => Box::new(lzma_rust2::Lzma2Reader::new(
						buffered(input),
						clamp_lzma_dict(lzma2_dict(props)?, Some(size)),
						None,
					)),
					Method::Ppmd => {
						let (order, memory) = ppmd_props(props)?;
						Box::new(
							ppmd_rust::Ppmd7Decoder::new(buffered(input), order, memory)
								.map_err(|_| SevenZError::Corrupt("damaged 7z PPMd data"))?,
						)
					}
					Method::Bzip2 => Box::new(bzip2::read::MultiBzDecoder::new(input)),
					Method::Deflate => Box::new(flate2::read::DeflateDecoder::new(input)),
					Method::Deflate64 => Box::new(deflate64::Deflate64Decoder::new(input)),
					Method::Bcj(branch) => {
						use lzma_rust2::filter::bcj::BcjReader;
						let start = branch_start(props)?;
						Box::new(match branch {
							Branch::X86 => BcjReader::new_x86(input, start),
							Branch::Arm => BcjReader::new_arm(input, start),
							Branch::ArmThumb => BcjReader::new_arm_thumb(input, start),
							Branch::Arm64 => BcjReader::new_arm64(input, start),
							Branch::Ppc => BcjReader::new_ppc(input, start),
							Branch::Sparc => BcjReader::new_sparc(input, start),
							Branch::Ia64 => BcjReader::new_ia64(input, start),
							Branch::RiscV => BcjReader::new_riscv(input, start),
						})
					}
					Method::Delta => match **props {
						[distance] => Box::new(lzma_rust2::filter::delta::DeltaReader::new(
							input,
							usize::from(distance) + 1,
						)),
						_ => return Err(SevenZError::Corrupt("invalid 7z delta properties")),
					},
					Method::Aes => {
						let props = AesProps::parse(props)?;
						let key = self.keys.key(&props)?;
						Box::new(AesCbcReader::new(input, &key, props.iv))
					}
					Method::Bcj2 => unreachable!("handled above"),
				}
			}
		};
		Ok(Box::new(reader.take(size)))
	}
}

/// Reads entries' data folder by folder: an entry after the last one read in the same folder
/// continues decoding (skipping what lies between), any other opens its folder afresh.
pub(crate) struct FolderCursor<'s, R> {
	source: Rc<RefCell<R>>,
	decoder_memory: u64,
	/// The open folder, its reader, and how far into its unpacked data that is.
	open: Option<(usize, Box<dyn Read + 's>, u64)>,
}

impl<'s, R: Read + Seek + 's> FolderCursor<'s, R> {
	pub(crate) fn new(source: R, decoder_memory: u64) -> Self {
		Self {
			source: Rc::new(RefCell::new(source)),
			decoder_memory,
			open: None,
		}
	}

	/// The data of `entry`, which has a stream: its size is enforced, and its CRC-32 (when it
	/// has one) checked once it is read to its end.
	pub(crate) fn open(
		&mut self,
		index: &SevenZIndex,
		entry: &SevenZEntry,
		keys: &mut Keys<'_>,
	) -> Result<EntryData<'_, 's>, SevenZError> {
		let stream = entry
			.stream
			.ok_or(SevenZError::Corrupt("a 7z entry without data"))?;
		let reusable = self
			.open
			.as_ref()
			.is_some_and(|(folder, _, at)| *folder == stream.folder && *at <= stream.offset);
		if !reusable {
			self.open = None;
			let reader = open_folder(
				&self.source,
				&index.folders[stream.folder],
				&index.pack_offsets,
				&index.pack_sizes,
				self.decoder_memory,
				keys,
			)?;
			self.open = Some((stream.folder, reader, 0));
		}
		let (_, reader, at) = self.open.as_mut().expect("opened above");
		let skip = stream.offset - *at;
		let skipped =
			io::copy(&mut reader.by_ref().take(skip), &mut io::sink()).map_err(read_error)?;
		*at += skipped;
		if skipped != skip {
			return Err(SevenZError::Corrupt(FOLDER_ENDS_EARLY));
		}
		Ok(EntryData {
			reader,
			at,
			size: entry.size,
			expected_crc: entry.crc,
			crc: crc32fast::Hasher::new(),
			read: 0,
			checked: false,
		})
	}
}

/// One entry's data, read out of its folder.
pub(crate) struct EntryData<'c, 's> {
	reader: &'c mut Box<dyn Read + 's>,
	at: &'c mut u64,
	size: u64,
	expected_crc: Option<u32>,
	crc: crc32fast::Hasher,
	read: u64,
	checked: bool,
}

impl Read for EntryData<'_, '_> {
	fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
		let want = buf
			.len()
			.min(usize::try_from(self.size - self.read).unwrap_or(usize::MAX));
		if buf.is_empty() {
			return Ok(0);
		}
		if want == 0 {
			if !self.checked {
				self.checked = true;
				if let Some(expected) = self.expected_crc
					&& self.crc.clone().finalize() != expected
				{
					return Err(io::Error::new(
						io::ErrorKind::InvalidData,
						SevenZError::Corrupt("a 7z entry's CRC-32 does not match"),
					));
				}
			}
			return Ok(0);
		}
		let n = self.reader.read(&mut buf[..want])?;
		if n == 0 {
			return Err(io::Error::new(
				io::ErrorKind::InvalidData,
				SevenZError::Corrupt(FOLDER_ENDS_EARLY),
			));
		}
		self.crc.update(&buf[..n]);
		self.read += n as u64;
		*self.at += n as u64;
		Ok(n)
	}
}

/// What decoding under a key failed with, as far as the key is concerned: a wrong key decrypts to
/// noise, which fails to decode or to match its CRC. The source's own errors stay.
pub(crate) fn wrong_key(error: SevenZError) -> SevenZError {
	match error {
		SevenZError::Corrupt(_) => SevenZError::WrongPassword,
		SevenZError::Read(error) if !from_source(&error) => SevenZError::WrongPassword,
		error => error,
	}
}

/// The error of reading past what a folder decodes to.
pub(crate) const FOLDER_ENDS_EARLY: &str = "a 7z folder ends early";

/// A decoder's read error: the source's own passes through, anything else is damaged data.
pub(crate) fn read_error(error: io::Error) -> SevenZError {
	if error
		.get_ref()
		.is_some_and(|inner| inner.is::<SevenZError>())
	{
		return *error
			.into_inner()
			.expect("checked above")
			.downcast::<SevenZError>()
			.expect("checked above");
	}
	SevenZError::Read(error)
}

#[cfg(test)]
mod tests;
