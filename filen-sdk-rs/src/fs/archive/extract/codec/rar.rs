//! Reading a RAR (1.3 to 7) through rars: its headers parsed from the archive read through the
//! driver, then its members decoded in archive order, each checked against its CRC-32 or BLAKE2sp
//! hash. A multi-volume archive is refused. The members' table is handed back to be held, with
//! rars' own parse: a later listing is made from the table, and a later job that decodes points
//! the held parse at its own input instead of reading every header again, which in an archive of
//! small members means every chunk of it.

#![warn(unreachable_pub, unused_qualifications)]

use std::{
	cell::RefCell,
	io::{self, Write},
	mem,
	ops::Range,
	rc::Rc,
	sync::{Arc, Mutex, PoisonError},
};

use chrono::{DateTime, Utc};
use microthumb::{ByteSource, SeqReader};
use rars::{Archive, ArchiveReadOptions, ExtractionDecision};

use crate::{
	Error, ErrorKind,
	consts::CHUNK_SIZE,
	fs::{
		archive::{
			error::read_failure,
			extract::{
				SolidSkipExceeded,
				list::{ArchiveEntryKind, EntryAccess, PasswordCheck},
				storage_exceeded,
			},
			format::{ArchiveFormat, RarVersion},
			worker::{CachedInput, EntryKind, WorkerEvent, WorkerPort, from_source},
		},
		drive_job::exceeds_limit,
	},
};

use super::{
	ArchiveEnd, HeldIndex, IndexUse, StreamJob, check_stated_size,
	entries::{Found, Verdict, Walk, found_path, symlink},
};

/// What a RAR's headers say of its members, in archive order.
#[derive(Debug)]
pub(crate) struct RarTable {
	pub(crate) version: RarVersion,
	/// Its headers are encrypted, so the password opened them.
	pub(crate) headers_encrypted: bool,
	/// Some member is compressed on from the history of those before it.
	pub(crate) solid: bool,
	pub(crate) members: Vec<RarMember>,
	/// rars' parse of the archive, which the table was made from.
	parsed: Option<HeldRar>,
}

/// rars' parse of a RAR, kept for the jobs after the one that made it. rars reads the archive
/// through `input`, which a job fills with its own input while it holds `busy`, and empties
/// before it ends; a job that finds it busy parses the archive again.
struct HeldRar {
	archive: Archive,
	/// The heap the parse holds, measured once it was made.
	heap: u64,
	input: Arc<Mutex<Option<CachedInput<'static>>>>,
	busy: Mutex<()>,
}

impl std::fmt::Debug for HeldRar {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("HeldRar").finish_non_exhaustive()
	}
}

/// The share of a codec job's decoder memory a RAR's parse may take: the rest is left to the
/// decoder.
const PARSE_SHARE: u64 = 2;

/// Inline bytes of the largest header block rars keeps, of any RAR version.
const BLOCK_BYTES: u64 = {
	let rar50 = size_of::<rars::rar50::Block>();
	let rar15 = size_of::<rars::rar15_40::Block>();
	let rar13 = size_of::<rars::rar13::Entry>();
	let most = if rar50 > rar15 { rar50 } else { rar15 };
	(if most > rar13 { most } else { rar13 }) as u64
};

/// Heap one header can cost before its variable fields: rars' block, twice over for the slack
/// of the Vec it grows in, and the member the table makes of it.
const HEADER_HEAP: u64 = 2 * BLOCK_BYTES + size_of::<RarMember>() as u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RarKind {
	File,
	Dir,
	/// A RAR 5 redirection: a symbolic or hard link, or a junction.
	Link,
}

/// How a RAR member's data is packed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RarPacking {
	/// As it is.
	Stored,
	/// Compressed on its own.
	Compressed,
	/// Compressed on from the history of the members before it.
	Solid,
}

#[derive(Debug)]
pub(crate) struct RarMember {
	pub(crate) name: String,
	/// The stored name was not valid UTF-8, and was decoded lossily.
	pub(crate) name_rewritten: bool,
	pub(crate) kind: RarKind,
	pub(crate) size: u64,
	pub(crate) packed: u64,
	pub(crate) modified: Option<DateTime<Utc>>,
	pub(crate) encrypted: bool,
	pub(crate) packing: RarPacking,
	/// Where its packed data lies in the archive, when the parse says.
	pub(crate) data: Option<Range<u64>>,
}

impl RarTable {
	/// The heap it takes, in bytes.
	pub(crate) fn heap_bytes(&self) -> u64 {
		let names: usize = self
			.members
			.iter()
			.map(|member| member.name.capacity())
			.sum();
		let table = (self.members.capacity() * size_of::<RarMember>() + names) as u64;
		table.saturating_add(self.parsed.as_ref().map_or(0, |parsed| parsed.heap))
	}
}

/// The error a rars error is reported as: the job's end when the driver went away or a fetch
/// failed, which the driver knows the cause of.
fn rar_error(error: &rars::Error) -> Error {
	if let rars::Error::Io(io_error) = error.root_cause()
		&& io_error
			.source()
			.downcast_ref::<io::Error>()
			.is_some_and(from_source)
	{
		return Error::custom(ErrorKind::Cancelled, "the archive job ended");
	}
	let kind = match error.kind() {
		rars::ErrorKind::PasswordRequired => ErrorKind::ArchivePasswordRequired,
		rars::ErrorKind::BadPassword => ErrorKind::ArchiveWrongPassword,
		rars::ErrorKind::UnsupportedFormat | rars::ErrorKind::UnsupportedFeature => {
			ErrorKind::ArchiveUnsupported
		}
		rars::ErrorKind::ResourceLimit => ErrorKind::ArchiveTooLarge,
		rars::ErrorKind::Cancelled => ErrorKind::Cancelled,
		_ => ErrorKind::ArchiveCorrupt,
	};
	Error::custom(
		kind,
		format!("the RAR could not be read: {}", error.kind().code()),
	)
}

/// What parsing the RAR's headers may spend: its share of the job's decoder memory, every
/// header counted (members and service headers alike) before rars keeps it, and its bytes, whose
/// names and fields the parse and the table copy.
fn parse_options<'a>(job: &'a StreamJob) -> ArchiveReadOptions<'a> {
	let share = job.limits.decoder_memory / PARSE_SHARE;
	let headers = (share / 2 / HEADER_HEAP).min(job.limits.max_members.saturating_add(2));
	ArchiveReadOptions::with_optional_password(
		job.password.as_ref().map(|password| password.as_bytes()),
	)
	.with_max_header_count(headers)
	.with_max_header_bytes(job.limits.max_index_bytes.min(share / 2 / 3))
	.with_rar50_dictionary_size_limit(job.limits.decoder_memory)
	.with_max_reader_workspace_bytes(job.limits.decoder_memory)
}

/// What decoding the RAR's members may spend: the job's decoder memory, less what a parse this
/// job made holds.
fn decode_options<'a>(job: &'a StreamJob, parsed: u64) -> ArchiveReadOptions<'a> {
	let left = job.limits.decoder_memory.saturating_sub(parsed);
	ArchiveReadOptions::with_optional_password(
		job.password.as_ref().map(|password| password.as_bytes()),
	)
	.with_rar50_dictionary_size_limit(left)
	.with_max_reader_workspace_bytes(left)
}

/// The heap rars' parse of a RAR holds: its header blocks inline, at the capacity of the Vec
/// they are in, and the names and fields each one keeps.
fn parse_heap(archive: &Archive) -> u64 {
	let heap = match archive {
		Archive::Rar13(rar) => {
			rar.entries.capacity() * size_of::<rars::rar13::Entry>()
				+ rar
					.entries
					.iter()
					.map(|entry| entry.name.capacity() + entry.extra.capacity())
					.sum::<usize>()
		}
		Archive::Rar15To40(rar) => {
			rar.blocks.capacity() * size_of::<rars::rar15_40::Block>()
				+ rar
					.blocks
					.iter()
					.map(|block| match block {
						rars::rar15_40::Block::File(file)
						| rars::rar15_40::Block::NewSub(rars::rar15_40::NewSubHeader {
							file,
							..
						}) => rar15_file_heap(file),
						_ => 0,
					})
					.sum::<usize>()
		}
		Archive::Rar50Plus(rar) => {
			rar.blocks.capacity() * size_of::<rars::rar50::Block>()
				+ rar
					.blocks
					.iter()
					.map(|block| match block {
						rars::rar50::Block::File(file) | rars::rar50::Block::Service(file) => {
							file.name.capacity()
								+ file.service_data.as_ref().map_or(0, Vec::capacity)
								+ file
									.redirection
									.as_ref()
									.map_or(0, |redirection| redirection.target_name.capacity())
						}
						_ => 0,
					})
					.sum::<usize>()
		}
		_ => 0,
	};
	heap as u64
}

fn rar15_file_heap(file: &rars::rar15_40::FileHeader) -> usize {
	file.name.capacity()
		+ file.unicode_name.as_ref().map_or(0, Vec::capacity)
		+ file.file_comment.capacity()
		+ file.ext_time.capacity()
}

/// The RAR in the input a job puts in `input`, past any self-extracting stub, as a source of its
/// own: rars reads a seekable source as if the archive started at its start.
struct Shifted {
	input: Arc<Mutex<Option<CachedInput<'static>>>>,
	/// The archive's file's length.
	len: u64,
	/// Where the archive starts in it.
	offset: u64,
}

impl ByteSource for Shifted {
	fn len(&self) -> u64 {
		self.len - self.offset
	}

	fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
		let Some(at) = offset.checked_add(self.offset) else {
			return Ok(0);
		};
		let mut input = self.input.lock().unwrap_or_else(PoisonError::into_inner);
		match input.as_mut() {
			Some(input) => input.read_at(at, buf),
			None => Err(io::Error::other("no archive job reads the RAR")),
		}
	}
}

/// Longest self-extracting stub a RAR may follow: what a stub tool writes, and no more than one
/// chunk of the archive is scanned for it.
const SFX_SCAN_BYTES: usize = 1 << 20;

/// Where the RAR in `input` starts: at its start, or after a self-extracting stub of at most
/// [`SFX_SCAN_BYTES`], which only the archive's name made it look for.
fn archive_start(input: &mut CachedInput<'static>) -> Result<u64, Error> {
	let scan = usize::try_from(input.len())
		.unwrap_or(usize::MAX)
		.min(SFX_SCAN_BYTES);
	let mut head = vec![0u8; scan];
	let mut filled = 0;
	while filled < scan {
		let read = input
			.read_at(filled as u64, &mut head[filled..])
			.map_err(read_failure)?;
		if read == 0 {
			break;
		}
		filled += read;
	}
	head.truncate(filled);
	rars::find_archive_start(&head, SFX_SCAN_BYTES)
		.map(|signature| signature.offset as u64)
		.ok_or_else(|| {
			Error::custom(
				ErrorKind::ArchiveUnsupported,
				"the file is not an archive the SDK can extract",
			)
		})
}

/// A parse of the RAR `input` reads, through the driver.
struct Parsed {
	archive: Archive,
	/// Where the archive starts in its file.
	offset: u64,
	/// What rars reads it through, holding `input` until the job empties it.
	input: Arc<Mutex<Option<CachedInput<'static>>>>,
	/// The heap the parse holds.
	heap: u64,
}

impl Parsed {
	/// The parse, to be held once its job is done with it.
	fn held(self) -> HeldRar {
		*self.input.lock().unwrap_or_else(PoisonError::into_inner) = None;
		HeldRar {
			archive: self.archive,
			heap: self.heap,
			input: self.input,
			busy: Mutex::new(()),
		}
	}
}

/// Parses the RAR `input` reads, through the driver.
fn parse(input: CachedInput<'_>, job: &StreamJob) -> Result<Parsed, Error> {
	let mut input = input.into_owned();
	let offset = archive_start(&mut input)?;
	let len = input.len();
	let input = Arc::new(Mutex::new(Some(input)));
	let source = SeqReader::new(Box::new(Shifted {
		input: Arc::clone(&input),
		len,
		offset,
	}));
	let archive = rars::ArchiveReader::read_reader_with_options(source, parse_options(job))
		.map_err(|error| rar_error(&error))?;
	let heap = parse_heap(&archive);
	if heap > job.limits.decoder_memory / PARSE_SHARE {
		return Err(Error::custom(
			ErrorKind::ArchiveTooLarge,
			format!("the RAR's headers take {heap} bytes, more than its parse may"),
		));
	}
	Ok(Parsed {
		archive,
		offset,
		input,
		heap,
	})
}

/// What `archive`'s headers say of its members, `offset` bytes into its file; a multi-volume
/// archive is refused.
fn table_of(archive: &Archive, offset: u64) -> Result<RarTable, Error> {
	let (version, headers_encrypted, data) = match archive {
		Archive::Rar13(_) => (RarVersion::Rar13, false, None),
		Archive::Rar15To40(rar) => (
			RarVersion::Rar15To40,
			rar.main.encrypt_version.is_some() && rar.main.flags & 0x80 != 0,
			Some(
				rar.blocks
					.iter()
					.filter_map(|block| match block {
						rars::rar15_40::Block::File(file) => Some(file.packed_range.clone()),
						_ => None,
					})
					.collect::<Vec<_>>(),
			),
		),
		Archive::Rar50Plus(rar) => (
			RarVersion::Rar50Plus,
			rar.main.encrypted_headers,
			Some(
				rar.blocks
					.iter()
					.filter_map(|block| match block {
						rars::rar50::Block::File(file) => Some(file.block.data_range.clone()),
						_ => None,
					})
					.collect::<Vec<_>>(),
			),
		),
		_ => {
			return Err(Error::custom(
				ErrorKind::ArchiveUnsupported,
				"a RAR of a family the SDK does not know",
			));
		}
	};
	// one member made at a time: the parse holds them all already
	let count = archive.members().count();
	// offsets only when every member has its own, as the members list them
	let data = data.filter(|data| data.len() == count);
	let mut table = Vec::with_capacity(count);
	for (at, member) in archive.members().enumerate() {
		let meta = &member.meta;
		if meta.is_split_before || meta.is_split_after {
			return Err(Error::custom(
				ErrorKind::ArchiveUnsupported,
				"a RAR split across several volumes",
			));
		}
		let decoded = member
			.decoded_name(None)
			.map_err(|error| rar_error(&error))?;
		let (name, name_rewritten) = match String::from_utf8(decoded.into_owned()) {
			Ok(name) => (name, false),
			Err(error) => (String::from_utf8_lossy(error.as_bytes()).into_owned(), true),
		};
		let kind = if meta.is_directory {
			RarKind::Dir
		} else if meta.is_redirection {
			RarKind::Link
		} else {
			RarKind::File
		};
		let data = data
			.as_ref()
			.and_then(|data| data.get(at))
			.map(|range| range.start as u64 + offset..range.end as u64 + offset)
			.filter(|range| range.end - range.start == meta.packed_size);
		table.push(RarMember {
			name,
			name_rewritten,
			kind,
			size: meta.unpacked_size,
			packed: meta.packed_size,
			modified: meta.modification_time().map(DateTime::<Utc>::from),
			encrypted: meta.is_encrypted,
			packing: match (meta.is_stored, member.is_solid()) {
				(true, _) => RarPacking::Stored,
				(false, false) => RarPacking::Compressed,
				(false, true) => RarPacking::Solid,
			},
			data,
		});
	}
	Ok(RarTable {
		parsed: None,
		version,
		headers_encrypted,
		solid: table
			.iter()
			.any(|member| member.packing == RarPacking::Solid),
		members: table,
	})
}

/// What a RAR member is, by its header.
fn rar_found(ordinal: u64, member: &RarMember) -> Found<'_> {
	let (kind, unreadable) = match member.kind {
		RarKind::File => (ArchiveEntryKind::File, None),
		RarKind::Dir => (ArchiveEntryKind::Dir, None),
		// rars gives a redirection's target to extraction alone
		RarKind::Link => symlink(None),
	};
	Found {
		ordinal,
		stored: &member.name,
		path: found_path(&member.name, member.name_rewritten),
		kind,
		unreadable,
		size: member.size,
		modified: member.modified,
		encrypted: member.encrypted,
		method: (member.kind == RarKind::File).then(|| {
			match member.packing {
				RarPacking::Stored => "Stored",
				RarPacking::Compressed | RarPacking::Solid => "RAR",
			}
			.to_owned()
		}),
	}
}

/// A RAR read from `input`: listed, or its members decoded and sent, as `walk` asks. `held` is
/// the table an earlier job read, which a listing is made from.
pub(super) fn extract_rar(
	walk: &mut Walk,
	input: CachedInput<'_>,
	job: &StreamJob,
	held: Option<&RarTable>,
) -> Result<ArchiveEnd, Error> {
	if let Some(table) = held {
		if walk.listing() {
			return list_rar(walk, table, job);
		}
		if let Some(parsed) = &table.parsed
			&& let Ok(_busy) = parsed.busy.try_lock()
		{
			*parsed.input.lock().unwrap_or_else(PoisonError::into_inner) = Some(input.into_owned());
			// the held parse is charged to the reader that holds it
			let end = decode_rar(walk, &parsed.archive, table, job, 0);
			// the held parse keeps nothing of this job
			*parsed.input.lock().unwrap_or_else(PoisonError::into_inner) = None;
			return end;
		}
	}
	let parsed = parse(input, job)?;
	let mut table = table_of(&parsed.archive, parsed.offset)?;
	let end = if walk.listing() {
		list_rar(walk, &table, job)?
	} else {
		decode_rar(walk, &parsed.archive, &table, job, parsed.heap)?
	};
	if held.is_none() && matches!(job.index, IndexUse::ReadAndReturn) {
		table.parsed = Some(parsed.held());
		walk.port
			.send(WorkerEvent::Indexed(Box::new(HeldIndex::Rar(table))))
			.map_err(read_failure)?;
	}
	Ok(end)
}

/// Opens a RAR for a reader: its table, entries unread, with the parse it was made from.
pub(super) fn open_rar(input: CachedInput<'_>, job: &StreamJob) -> Result<RarTable, Error> {
	let parsed = parse(input, job)?;
	let mut table = table_of(&parsed.archive, parsed.offset)?;
	table.parsed = Some(parsed.held());
	Ok(table)
}

/// The RAR listed from its table.
fn list_rar(walk: &mut Walk, table: &RarTable, job: &StreamJob) -> Result<ArchiveEnd, Error> {
	walk.port
		.send(WorkerEvent::Opened(ArchiveFormat::Rar {
			version: table.version,
		}))
		.map_err(read_failure)?;
	for (ordinal, member) in (0u64..).zip(&table.members) {
		let access = if member.packing == RarPacking::Solid {
			EntryAccess::Sequential
		} else {
			EntryAccess::Direct {
				packed_bytes: member.packed,
			}
		};
		walk.list(rar_found(ordinal, member), None, access)?;
	}
	walk.send_mac_folders().map_err(read_failure)?;
	let encrypted = table.members.iter().any(|member| member.encrypted);
	let password = match (table.headers_encrypted, encrypted, &job.password) {
		(true, ..) => PasswordCheck::Right,
		(false, false, _) => PasswordCheck::NotNeeded,
		(false, true, None) => PasswordCheck::Required,
		(false, true, Some(_)) => PasswordCheck::Unchecked,
	};
	Ok(ArchiveEnd {
		unaccounted_bytes: 0,
		duplicates: None,
		unchecked_entries: 0,
		password,
	})
}

/// Where the member being sent stands.
struct Sending {
	port: WorkerPort,
	chunk: Vec<u8>,
	/// A file was announced, and its end not sent yet.
	open: bool,
	/// Handing a chunk over failed: the job ended.
	failed: bool,
}

impl Sending {
	/// Ends the file announced last, its data checked: what is left of it, then its end.
	fn end_file(&mut self) -> io::Result<()> {
		if !mem::take(&mut self.open) {
			return Ok(());
		}
		if !self.chunk.is_empty() {
			let last = mem::take(&mut self.chunk);
			self.port.send(WorkerEvent::Data(last))?;
		}
		self.port.send(WorkerEvent::FileEnd)
	}
}

/// What rars writes a member's data to: handed to the driver in whole chunks.
struct MemberWriter(Rc<RefCell<Sending>>);

impl Write for MemberWriter {
	fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
		let mut sending = self.0.borrow_mut();
		if sending.failed {
			return Err(io::Error::other("the archive job ended"));
		}
		let n = buf.len().min(CHUNK_SIZE - sending.chunk.len());
		sending.chunk.extend_from_slice(&buf[..n]);
		if sending.chunk.len() == CHUNK_SIZE {
			let full = mem::replace(&mut sending.chunk, Vec::with_capacity(CHUNK_SIZE));
			if let Err(error) = sending.port.send(WorkerEvent::Data(full)) {
				sending.failed = true;
				return Err(error);
			}
		}
		Ok(n)
	}

	fn flush(&mut self) -> io::Result<()> {
		Ok(())
	}
}

/// The RAR's members decoded in archive order, those `walk` takes sent.
fn decode_rar(
	walk: &mut Walk,
	archive: &Archive,
	table: &RarTable,
	job: &StreamJob,
	parsed: u64,
) -> Result<ArchiveEnd, Error> {
	let ordinals = 0u64..;
	walk.check_selection(
		ordinals
			.clone()
			.zip(&table.members)
			.map(|(ordinal, member)| (ordinal, member.name.as_str(), member.kind == RarKind::Dir)),
	)?;
	check_stated_size(job, table.members.iter().map(|member| member.size))?;
	let extracted = walk.extracted_bytes(
		ordinals
			.clone()
			.zip(&table.members)
			.filter(|(_, member)| member.kind == RarKind::File)
			.map(|(ordinal, member)| (ordinal, member.name.as_str(), member.size)),
	);
	if let Some(error) = storage_exceeded(job.limits.max_bytes, extracted) {
		return Err(error);
	}
	let history = solid_history(table, |ordinal| walk.wants(ordinal));
	if let Some(limit) = walk.max_solid_skip() {
		check_solid_skip(table, &history, walk, limit)?;
	}
	let port = walk.port;
	port.send(WorkerEvent::Opened(ArchiveFormat::Rar {
		version: table.version,
	}))
	.map_err(read_failure)?;
	let sending = Rc::new(RefCell::new(Sending {
		port: port.clone(),
		chunk: Vec::with_capacity(CHUNK_SIZE),
		open: false,
		failed: false,
	}));
	let mut ordinal = 0u64;
	// the walk's own errors, which rars only carries as its own
	let mut failed: Option<Error> = None;
	let decided = archive.extract_with_control(decode_options(job, parsed), |_| {
		let decision = decide(walk, table, &history, ordinal, &sending);
		ordinal += 1;
		decision.map_err(|error| {
			failed = Some(error);
			rars::Error::Cancelled
		})
	});
	if let Some(error) = failed {
		return Err(error);
	}
	decided.map_err(|error| rar_error(&error))?;
	sending.borrow_mut().end_file().map_err(read_failure)?;
	walk.send_mac_folders().map_err(read_failure)?;
	Ok(ArchiveEnd::plain(0, 0))
}

/// For each member, whether a member read after it is solid and decodes on from its history,
/// directly or through members between them: `wants` tells the members read.
fn solid_history(table: &RarTable, wants: impl Fn(u64) -> bool) -> Vec<bool> {
	let mut history = vec![false; table.members.len()];
	if !table.solid {
		return history;
	}
	let mut needed = false;
	for (at, member) in table.members.iter().enumerate().rev() {
		// directories and links hold no data, and stored data goes past the decoder
		if member.kind != RarKind::File || member.packing == RarPacking::Stored {
			continue;
		}
		history[at] = needed;
		let read = wants(at as u64) || needed;
		// a solid member read needs the one before it; a member that is not solid starts afresh
		needed = read && member.packing == RarPacking::Solid;
	}
	history
}

/// Whether `member`, the `ordinal`-th, is decoded though left out, for the history after it.
fn feeds(history: &[bool], ordinal: u64, member: &RarMember) -> bool {
	member.kind == RarKind::File
		&& member.packing != RarPacking::Stored
		&& usize::try_from(ordinal)
			.ok()
			.and_then(|at| history.get(at))
			.is_some_and(|&needed| needed)
}

/// Refuses a download that would decode and throw away more than `limit` bytes of a solid RAR to
/// reach its entries: the members left out that the ones read decode on from.
fn check_solid_skip(
	table: &RarTable,
	history: &[bool],
	walk: &Walk,
	limit: u64,
) -> Result<(), Error> {
	let (mut thrown, mut packed) = (0u64, 0u64);
	for (ordinal, member) in (0u64..).zip(&table.members) {
		let read = walk.wants(ordinal) && member.kind == RarKind::File;
		if read || feeds(history, ordinal, member) {
			packed = packed.saturating_add(member.packed);
		}
		if !read && feeds(history, ordinal, member) {
			thrown = thrown.saturating_add(member.size);
		}
	}
	if exceeds_limit(thrown, limit) {
		return Err(SolidSkipExceeded {
			skipped_bytes: thrown,
			limit,
			estimated_packed_bytes: packed,
		}
		.into());
	}
	Ok(())
}

/// What becomes of member `ordinal`: the one before it ended (rars verified it before coming
/// here), then this one judged.
fn decide(
	walk: &mut Walk,
	table: &RarTable,
	history: &[bool],
	ordinal: u64,
	sending: &Rc<RefCell<Sending>>,
) -> Result<ExtractionDecision, Error> {
	sending.borrow_mut().end_file().map_err(read_failure)?;
	if walk.met_all_chosen() {
		return Ok(ExtractionDecision::Stop);
	}
	let member = usize::try_from(ordinal)
		.ok()
		.and_then(|at| table.members.get(at))
		.ok_or_else(|| {
			Error::custom(
				ErrorKind::Internal,
				"rars visited a member its listing does not hold",
			)
		})?;
	let found = rar_found(ordinal, member);
	// a member left out that a solid one read after it decodes on from is decoded all the same
	let pass = || {
		if feeds(history, ordinal, member) {
			ExtractionDecision::Extract(Box::new(io::sink()))
		} else {
			ExtractionDecision::Skip
		}
	};
	let (path, apple_double) = match walk.judge(&found)? {
		Verdict::Ignore | Verdict::Root | Verdict::Held => return Ok(pass()),
		Verdict::Skip(reason) => {
			walk.port
				.send(found.skipped(reason))
				.map_err(read_failure)?;
			return Ok(pass());
		}
		Verdict::Take { path, apple_double } => (path, apple_double),
	};
	if member.kind == RarKind::Dir {
		walk.port
			.send(found.head(path, EntryKind::Dir))
			.map_err(read_failure)?;
		return Ok(ExtractionDecision::Skip);
	}
	if apple_double {
		// rars hands a member's data to its writer only once the member is decoded, so its
		// first bytes cannot tell it AppleDouble first: it is extracted as an ordinary file
		walk.taken_after_all(&found);
	}
	walk.port
		.send(found.head(
			path,
			EntryKind::File {
				size: Some(member.size),
			},
		))
		.map_err(read_failure)?;
	sending.borrow_mut().open = true;
	Ok(ExtractionDecision::Extract(Box::new(MemberWriter(
		Rc::clone(sending),
	))))
}
