//! Seekable entries: a deflated zip entry (plain, ZipCrypto or WinZip AES), a stored one under
//! ZipCrypto or AES, and a member of a gzip tar, read by range through a decoder cursor.
//!
//! A cursor is the decoder's whole state at a call boundary: miniz_oxide's core decompressor
//! (which can be cloned), its 32 KiB window, where it is in the stored and the decoded bytes, and
//! the ZipCrypto keys there. A read goes on from the cursor the read before it parked when that
//! is at or before its start, and otherwise from the nearest checkpoint at or before it: a copy
//! of a cursor, laid every few MiB of output as reads decode past it, never ahead of them. AES
//! needs no state of its own: its keystream is derived from the offset.
//!
//! Each decode step makes at most [`STEP_BYTES`] of output and runs on the CPU pool, never on the
//! thread that drives the SDK's tasks; between steps the reader fetches the stored bytes, through
//! the chunks it keeps. Reads of one stream run one at a time. A range is not checked against the
//! entry's checksum, which covers it whole; a whole read goes through the checked download.

#![warn(unreachable_pub, unused_qualifications)]

use std::{ops::Range, sync::Arc};

use futures::{AsyncWrite, AsyncWriteExt};
use miniz_oxide::inflate::{
	TINFLStatus,
	core::{DecompressorOxide, decompress, inflate_flags::TINFL_FLAG_HAS_MORE_INPUT},
};

use crate::{
	Error, ErrorKind,
	consts::CHUNK_SIZE_U64,
	fs::archive::zip::crypto::{
		AES_AUTH_CODE_LEN_U64, AES_VERIFIER_LEN, AesStream, AesStrength, ZIP_CRYPTO_HEADER_LEN,
		ZIP_CRYPTO_HEADER_LEN_U64, ZipCryptoState,
	},
	runtime::do_cpu_intensive,
	util::MaybeSend,
};

use super::Reader;

/// Most output one decode step makes, before the reader writes it and fetches more.
pub(crate) const STEP_BYTES: u64 = 1 << 20;
/// Output between two checkpoints, at first.
pub(crate) const CHECKPOINT_SPACING: u64 = 4 << 20;
/// Most a stream's checkpoints take: past it every other one goes, and the spacing doubles.
pub(crate) const MAX_CHECKPOINT_BYTES: u64 = 16 << 20;
/// Deflate's window, which the decompressor writes around.
const WINDOW: usize = 32 << 10;
/// Most stored bytes one decompressor call is given.
const PIECE: usize = 32 << 10;
/// Most a gzip member's header may take.
const MAX_GZIP_HEADER: usize = 64 << 10;
/// Bytes of a gzip member's trailer: its CRC-32 and size.
const GZIP_TRAILER: usize = 8;

/// Which stream of a reader a cursor decodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum StreamKey {
	/// A zip entry's data, by ordinal.
	Entry(u64),
	/// A gzip tar's whole stream, which every member is a range of.
	Gzip,
}

/// How a stream's stored bytes decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SeekCodec {
	Deflate,
	/// Stored, under a cipher.
	Copy,
}

/// How a stream is framed around its data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Framing {
	/// The data alone: a zip entry's.
	Raw,
	/// gzip members, each a header, deflate data and a trailer, one after another.
	Gzip,
}

/// What a stream's data is encrypted with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CipherSpec {
	None,
	/// A 12-byte header comes first, whose last byte decrypts to `check`.
	ZipCrypto {
		check: u8,
	},
	/// The salt and verifier come first, the authentication code last.
	Aes {
		strength: AesStrength,
	},
}

/// A stream an entry is a range of, as resolving it found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SeekStream {
	pub(crate) key: StreamKey,
	/// Where its stored bytes start in the archive, and how many there are, a cipher's header
	/// and authentication code included.
	pub(crate) start: u64,
	pub(crate) len: u64,
	pub(crate) codec: SeekCodec,
	pub(crate) framing: Framing,
	pub(crate) cipher: CipherSpec,
}

impl SeekStream {
	/// The stored bytes of its data: a cipher's header and code left out.
	fn data(&self) -> Range<u64> {
		let (head, tail) = match self.cipher {
			CipherSpec::None => (0, 0),
			CipherSpec::ZipCrypto { .. } => (ZIP_CRYPTO_HEADER_LEN_U64, 0),
			CipherSpec::Aes { strength } => (
				strength.salt_len() as u64 + AES_VERIFIER_LEN,
				AES_AUTH_CODE_LEN_U64,
			),
		};
		head..self.len.saturating_sub(tail).max(head)
	}
}

/// The decoder's state: a cursor, and a checkpoint is a copy of one.
#[derive(Clone)]
pub(crate) struct Cursor {
	/// Bytes of the stream decoded.
	decoded: u64,
	/// Stored bytes of the stream consumed, from its start.
	consumed: u64,
	phase: Phase,
	inflater: Option<Box<Inflater>>,
	/// The keys after the bytes consumed, for a ZipCrypto stream.
	zip_crypto: Option<ZipCryptoState>,
}

#[derive(Clone)]
struct Inflater {
	decompressor: DecompressorOxide,
	window: Box<[u8]>,
	/// Where in `window` the next output goes.
	at: usize,
	/// Output it made, up to the window's size: what of the output before the cursor `window`
	/// still holds.
	filled: usize,
}

impl Inflater {
	fn new() -> Box<Self> {
		Box::new(Self {
			decompressor: DecompressorOxide::new(),
			window: vec![0u8; WINDOW].into_boxed_slice(),
			at: 0,
			filled: 0,
		})
	}
}

impl Cursor {
	/// The first output the cursor can still give: what its window holds is before it.
	fn holds_from(&self) -> u64 {
		let held = self
			.inflater
			.as_ref()
			.filter(|_| self.phase == Phase::Data || self.phase == Phase::Ended)
			.map_or(0, |inflater| inflater.filled as u64);
		self.decoded - held
	}

	/// Appends what of `want` lies in the window, before the cursor.
	fn replay(&self, want: &Range<u64>, out: &mut Vec<u8>) {
		let Some(inflater) = &self.inflater else {
			return;
		};
		let from = want.start.max(self.holds_from());
		let to = want.end.min(self.decoded);
		for at in from..to {
			let back = usize::try_from(self.decoded - at).expect("within the window");
			out.push(inflater.window[(inflater.at + WINDOW - back) & (WINDOW - 1)]);
		}
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
	Data,
	GzipHeader,
	GzipTrailer,
	Ended,
}

/// What one copy of a cursor takes.
pub(crate) const CURSOR_BYTES: u64 = (size_of::<Cursor>() + size_of::<Inflater>() + WINDOW) as u64;

/// What a reader keeps of a stream between its reads: the cursor the last one parked, the
/// checkpoints laid, and an AES entry's keystream.
pub(crate) struct Seeks {
	parked: Option<Cursor>,
	/// By the output they are at.
	checkpoints: Vec<Cursor>,
	spacing: u64,
	max_bytes: u64,
	aes: Option<AesStream>,
	/// Output the stream's reads decoded in all, to tell a seek that went on from where the
	/// read before stopped from one that decoded from far back.
	decoded: u64,
}

impl Seeks {
	pub(crate) fn new() -> Self {
		Self::with_limits(CHECKPOINT_SPACING, MAX_CHECKPOINT_BYTES)
	}

	pub(crate) fn with_limits(spacing: u64, max_bytes: u64) -> Self {
		Self {
			parked: None,
			checkpoints: Vec::new(),
			spacing,
			max_bytes,
			aes: None,
			decoded: 0,
		}
	}

	/// What it takes.
	pub(crate) fn bytes(&self) -> u64 {
		let cursors = self.checkpoints.len() as u64 + u64::from(self.parked.is_some());
		cursors * CURSOR_BYTES + size_of::<Self>() as u64
	}

	/// The cursor to go on from to reach output `at`: the one parked or the nearest checkpoint,
	/// whichever is further and at or before it.
	fn start_at(&mut self, at: u64) -> Option<Cursor> {
		let checkpoint = self
			.checkpoints
			.partition_point(|checkpoint| checkpoint.decoded <= at)
			.checked_sub(1)
			.map(|index| &self.checkpoints[index]);
		let parked = self
			.parked
			.take()
			.filter(|parked| parked.holds_from() <= at);
		match (parked, checkpoint) {
			(Some(parked), Some(checkpoint)) if checkpoint.decoded > parked.holds_from() => {
				Some(checkpoint.clone())
			}
			(Some(parked), _) => Some(parked),
			(None, checkpoint) => checkpoint.cloned(),
		}
	}

	/// Where the next checkpoint goes.
	fn next_checkpoint(&self) -> u64 {
		self.checkpoints
			.last()
			.map_or(0, |last| last.decoded)
			.saturating_add(self.spacing)
	}

	/// Lays `cursor` as a checkpoint, thinning them when they take too much.
	fn lay(&mut self, cursor: &Cursor) {
		if self
			.checkpoints
			.last()
			.is_some_and(|last| last.decoded >= cursor.decoded)
		{
			return;
		}
		self.checkpoints.push(cursor.clone());
		if self.checkpoints.len() as u64 * CURSOR_BYTES > self.max_bytes {
			let mut keep = false;
			self.checkpoints.retain(|_| {
				keep = !keep;
				!keep
			});
			self.spacing = self.spacing.saturating_mul(2);
		}
	}
}

#[cfg(test)]
impl Seeks {
	pub(crate) fn decoded_for_test(&self) -> u64 {
		self.decoded
	}

	/// Where the checkpoints are, in the output.
	pub(crate) fn checkpoints_for_test(&self) -> Vec<u64> {
		self.checkpoints
			.iter()
			.map(|checkpoint| checkpoint.decoded)
			.collect()
	}
}

/// Why decoding a stream failed.
fn damaged(what: &'static str) -> Error {
	Error::custom(ErrorKind::ArchiveCorrupt, what)
}

/// What a decode step stopped for.
enum Stopped {
	/// It reached the end of what was asked for, or made a step's output.
	Output,
	/// It used up the stored bytes it was given.
	NeedInput,
	/// It needs this many stored bytes at once, more than reach the end of the chunk.
	Contiguous(usize),
}

/// The stored bytes a step decodes from: `bytes`, `at` bytes into the stream; `last` when they
/// reach the stream's end.
struct Input<'a> {
	bytes: &'a [u8],
	at: u64,
	last: bool,
}

/// One decode step: `cursor` goes on decoding `input` until its output reaches `want.end`, or it
/// made [`STEP_BYTES`] of output, or `input` is used up; the output within `want` is appended to
/// `out`, and checkpoints are laid in `seeks` as the output passes them.
fn step(
	cursor: &mut Cursor,
	stream: &SeekStream,
	seeks: &mut Seeks,
	input: &Input<'_>,
	want: &Range<u64>,
	out: &mut Vec<u8>,
) -> Result<Stopped, Error> {
	let data = stream.data();
	let made_from = cursor.decoded;
	let mut piece = Vec::new();
	loop {
		if cursor.decoded >= want.end || cursor.decoded - made_from >= STEP_BYTES {
			return Ok(Stopped::Output);
		}
		// what of `input` is ahead of the cursor
		let ahead = cursor
			.consumed
			.checked_sub(input.at)
			.and_then(|skip| usize::try_from(skip).ok())
			.and_then(|skip| input.bytes.get(skip..))
			.unwrap_or_default();
		match cursor.phase {
			Phase::Ended => return Err(damaged("an entry's data ends before its stated size")),
			Phase::GzipHeader => {
				if ahead.is_empty() && input.last {
					cursor.phase = Phase::Ended;
					continue;
				}
				match gzip_header(ahead)? {
					Some(len) => {
						cursor.consumed += len as u64;
						cursor.inflater = Some(Inflater::new());
						cursor.phase = Phase::Data;
					}
					// trailing zeros after the last member are padding
					None if input.last && ahead.iter().all(|&byte| byte == 0) => {
						cursor.consumed += ahead.len() as u64;
						cursor.phase = Phase::Ended;
					}
					None if input.last || ahead.len() >= MAX_GZIP_HEADER => {
						return Err(damaged("a gzip member's header is damaged"));
					}
					None => return Ok(Stopped::Contiguous(MAX_GZIP_HEADER)),
				}
			}
			Phase::GzipTrailer => {
				if ahead.len() < GZIP_TRAILER {
					return if input.last {
						Err(damaged("a gzip member ends inside its trailer"))
					} else {
						Ok(Stopped::Contiguous(GZIP_TRAILER))
					};
				}
				cursor.consumed += GZIP_TRAILER as u64;
				cursor.phase = Phase::GzipHeader;
			}
			Phase::Data => {
				let left = data.end.saturating_sub(cursor.consumed);
				let mut len = ahead
					.len()
					.min(PIECE)
					.min(usize::try_from(left).unwrap_or(PIECE));
				// stored data stops where the read does: a cursor past it would be of no use to
				// the read after it, as nothing keeps what it went past
				if cursor.inflater.is_none() {
					len = len.min(usize::try_from(want.end - cursor.decoded).unwrap_or(len));
				}
				let ends = input.last && len as u64 == left;
				if len == 0 && !ends {
					return Ok(Stopped::NeedInput);
				}
				piece.clear();
				piece.extend_from_slice(&ahead[..len]);
				match (&stream.cipher, &mut cursor.zip_crypto, &mut seeks.aes) {
					(CipherSpec::ZipCrypto { .. }, Some(keys), _) => keys.decrypted(&mut piece),
					(CipherSpec::Aes { .. }, _, Some(aes)) => {
						aes.decrypt_at(cursor.consumed - data.start, &mut piece);
					}
					(CipherSpec::None, ..) => {}
					_ => return Err(damaged("an entry's cipher was not set up")),
				}
				let (consumed, made, done) = match &mut cursor.inflater {
					None => {
						emit(cursor.decoded, &piece, want, out);
						(len, len, ends)
					}
					Some(inflater) => {
						let flags = if ends { 0 } else { TINFL_FLAG_HAS_MORE_INPUT };
						let (status, consumed, made) = decompress(
							&mut inflater.decompressor,
							&piece,
							&mut inflater.window,
							inflater.at,
							flags,
						);
						emit(
							cursor.decoded,
							&inflater.window[inflater.at..inflater.at + made],
							want,
							out,
						);
						inflater.at = (inflater.at + made) & (WINDOW - 1);
						inflater.filled = (inflater.filled + made).min(WINDOW);
						let done = match status {
							TINFLStatus::Done => true,
							TINFLStatus::NeedsMoreInput if ends => {
								return Err(damaged("an entry's deflate data ends early"));
							}
							TINFLStatus::NeedsMoreInput | TINFLStatus::HasMoreOutput => false,
							_ => return Err(damaged("an entry's deflate data is damaged")),
						};
						(consumed, made, done)
					}
				};
				if let Some(keys) = &mut cursor.zip_crypto {
					keys.advance(&piece[..consumed]);
				}
				cursor.consumed += consumed as u64;
				cursor.decoded += made as u64;
				if done {
					cursor.phase = match stream.framing {
						Framing::Raw => Phase::Ended,
						Framing::Gzip => Phase::GzipTrailer,
					};
				}
				if cursor.phase == Phase::Data && cursor.decoded >= seeks.next_checkpoint() {
					seeks.lay(cursor);
				}
			}
		}
	}
}

/// Appends what of `bytes`, decoded from output `at` on, lies within `want`.
fn emit(at: u64, bytes: &[u8], want: &Range<u64>, out: &mut Vec<u8>) {
	let end = at + bytes.len() as u64;
	if end <= want.start || at >= want.end {
		return;
	}
	let from = usize::try_from(want.start.saturating_sub(at)).expect("within the bytes");
	let to = usize::try_from(want.end.min(end) - at).expect("within the bytes");
	out.extend_from_slice(&bytes[from..to]);
}

/// The length of the gzip member header `bytes` starts with (RFC 1952), when they hold all of
/// it; `None` when they hold less.
fn gzip_header(bytes: &[u8]) -> Result<Option<usize>, Error> {
	const FHCRC: u8 = 2;
	const FEXTRA: u8 = 4;
	const FNAME: u8 = 8;
	const FCOMMENT: u8 = 16;
	let Some(fixed) = bytes.get(..10) else {
		return Ok(None);
	};
	if fixed[..3] != [0x1F, 0x8B, 8] {
		return Err(damaged("a gzip member's header is damaged"));
	}
	let flags = fixed[3];
	let mut at = 10;
	if flags & FEXTRA != 0 {
		let Some(len) = bytes.get(at..at + 2) else {
			return Ok(None);
		};
		at += 2 + usize::from(u16::from_le_bytes([len[0], len[1]]));
	}
	for flag in [FNAME, FCOMMENT] {
		if flags & flag != 0 {
			let Some(nul) = bytes
				.get(at..)
				.and_then(|rest| rest.iter().position(|&b| b == 0))
			else {
				return Ok(None);
			};
			at += nul + 1;
		}
	}
	if flags & FHCRC != 0 {
		at += 2;
	}
	Ok((at <= bytes.len()).then_some(at))
}

impl Reader {
	/// Writes output `want` of `stream` to `writer`, then closes it.
	pub(crate) async fn read_seekable<W: AsyncWrite + Unpin + MaybeSend>(
		&self,
		stream: SeekStream,
		want: Range<u64>,
		writer: &mut W,
	) -> Result<(), Arc<Error>> {
		let flight = self.flight(stream.key);
		let _flight = flight.lock().await;
		let mut seeks = self
			.retained
			.as_ref()
			.and_then(|retained| retained.take_seeks(stream.key))
			.unwrap_or_else(Seeks::new);
		let read = self
			.read_stream(&stream, &mut seeks, want.clone(), writer)
			.await;
		if let Some(retained) = &self.retained {
			retained.keep_seeks(stream.key, seeks);
		}
		read?;
		writer.close().await.map_err(|error| Arc::new(error.into()))
	}

	async fn read_stream<W: AsyncWrite + Unpin + MaybeSend>(
		&self,
		stream: &SeekStream,
		seeks: &mut Seeks,
		want: Range<u64>,
		writer: &mut W,
	) -> Result<(), Arc<Error>> {
		if want.is_empty() {
			return Ok(());
		}
		let mut cursor = match seeks.start_at(want.start) {
			Some(cursor) => cursor,
			None => self.fresh_cursor(stream, seeks).await?,
		};
		let data = stream.data();
		let started = cursor.decoded;
		let mut out = Vec::new();
		// what of the range the cursor went past, in its window
		cursor.replay(&want, &mut out);
		if !out.is_empty() {
			writer
				.write_all(&out)
				.await
				.map_err(|error| Arc::new(error.into()))?;
		}
		let mut contiguous: Option<Vec<u8>> = None;
		while cursor.decoded < want.end {
			// the stored bytes from the cursor on: to the end of their chunk, or as many as a
			// header needs at once
			let at = stream.start + cursor.consumed;
			let end = stream.start + stream.len;
			let (bytes, from) = match contiguous.take() {
				Some(bytes) => (Arc::new(bytes), at),
				None => {
					let index = at / CHUNK_SIZE_U64;
					let (chunk, _permit) = self.chunk_for_range(index, None).await?;
					(chunk, index * CHUNK_SIZE_U64)
				}
			};
			let reach = (from + bytes.len() as u64).min(end);
			// from the cursor on: the chunk may start before the stream does
			let skip = usize::try_from(at - from).expect("within a chunk");
			let bytes_end = usize::try_from(reach - from).expect("within a chunk");
			let input = Input {
				bytes: &bytes[skip..bytes_end],
				at: cursor.consumed,
				last: reach - stream.start >= data.end,
			};
			out.clear();
			let stopped =
				do_cpu_intensive(|| step(&mut cursor, stream, seeks, &input, &want, &mut out))
					.await?;
			drop(bytes);
			if !out.is_empty() {
				writer
					.write_all(&out)
					.await
					.map_err(|error| Arc::new(error.into()))?;
			}
			self.ensure_open()?;
			if let Stopped::Contiguous(len) = stopped {
				let to = end.min(at + len as u64);
				let mut joined = futures::io::Cursor::new(Vec::new());
				self.read_archive(at, to, &mut joined).await?;
				contiguous = Some(joined.into_inner());
			}
		}
		let decoded = cursor.decoded - started;
		seeks.decoded = seeks.decoded.saturating_add(decoded);
		tracing::debug!(
			"archive {}: read {want:?} of {:?}, decoding {decoded} bytes",
			self.archive_uuid(),
			stream.key
		);
		seeks.parked = Some(cursor);
		Ok(())
	}

	/// A cursor at the start of `stream`, past a cipher's header, whose keys it sets up.
	async fn fresh_cursor(
		&self,
		stream: &SeekStream,
		seeks: &mut Seeks,
	) -> Result<Cursor, Arc<Error>> {
		let data = stream.data();
		let mut cursor = Cursor {
			decoded: 0,
			consumed: data.start,
			phase: match stream.framing {
				Framing::Raw => Phase::Data,
				Framing::Gzip => Phase::GzipHeader,
			},
			inflater: (stream.codec == SeekCodec::Deflate && stream.framing == Framing::Raw)
				.then(Inflater::new),
			zip_crypto: None,
		};
		if stream.cipher == CipherSpec::None {
			return Ok(cursor);
		}
		let password = self.settings.password.clone().ok_or_else(|| {
			Arc::new(Error::custom(
				ErrorKind::ArchivePasswordRequired,
				"the entry is encrypted, and the reader has no password",
			))
		})?;
		let mut head = futures::io::Cursor::new(Vec::new());
		self.read_archive(stream.start, stream.start + data.start, &mut head)
			.await?;
		let head = head.into_inner();
		match stream.cipher {
			CipherSpec::None => {}
			CipherSpec::ZipCrypto { check } => {
				let header: &[u8; ZIP_CRYPTO_HEADER_LEN] = head
					.as_slice()
					.try_into()
					.map_err(|_| Arc::new(damaged("an entry is too short for its cipher")))?;
				cursor.zip_crypto = Some(
					ZipCryptoState::open(&password, header, check)
						.map_err(|error| Arc::new(error.into()))?,
				);
			}
			CipherSpec::Aes { strength } => {
				if seeks.aes.is_none() {
					let salt_len = strength.salt_len();
					let (salt, verifier) = head.split_at(salt_len.min(head.len()));
					let verifier: [u8; 2] = verifier
						.try_into()
						.map_err(|_| Arc::new(damaged("an entry is too short for its cipher")))?;
					// a thousand rounds of PBKDF2: off the thread that drives the tasks
					let aes =
						do_cpu_intensive(|| AesStream::open(&password, strength, salt, verifier))
							.await
							.map_err(|error| Arc::new(error.into()))?;
					seeks.aes = Some(aes);
				}
			}
		}
		Ok(cursor)
	}
}
