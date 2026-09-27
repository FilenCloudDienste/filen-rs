//! Writing a zip front to back: each entry's local header, its data and a data descriptor with
//! the sizes and CRC-32 known only after it, then the central directory. Nothing is ever written
//! twice, so the archive can be uploaded as it is written. Names are UTF-8; encryption is WinZip
//! AES in its AE-2 form (no CRC stored: the authentication code covers the data), with a fresh
//! salt per entry; zip64 records are written wherever sizes, offsets or the entry count need
//! them.

use std::io::{self, Read, Write};

use chrono::{DateTime, Datelike, Local, Timelike, Utc};

use super::{
	crypto::{AesStrength, AesWriter},
	read::{CENTRAL_HEADER_SIG, EOCD_SIG, EOCD64_LOCATOR_SIG, EOCD64_SIG, LOCAL_HEADER_SIG},
};

const DATA_DESCRIPTOR_SIG: u32 = 0x0807_4b50;
const FLAG_ENCRYPTED: u16 = 0x0001;
const FLAG_DATA_DESCRIPTOR: u16 = 0x0008;
const FLAG_UTF8: u16 = 0x0800;
const METHOD_STORED: u16 = 0;
const METHOD_DEFLATE: u16 = 8;
const METHOD_BZIP2: u16 = 12;
const METHOD_AES: u16 = 99;
/// Unix (3), zip specification 6.3.
const VERSION_MADE_BY: u16 = 0x033F;
/// An entry this large, or larger, is written with zip64 sizes: its compressed size may pass
/// 4 GiB even though its data does not, since stored and deflated data can grow a little.
const ZIP64_ENTRY_THRESHOLD: u64 = 0xF000_0000;

/// How a zip entry's data is compressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(
	feature = "wasm-full",
	derive(serde::Serialize, serde::Deserialize, tsify::Tsify),
	tsify(into_wasm_abi, from_wasm_abi),
	serde(
		tag = "type",
		rename_all = "camelCase",
		rename_all_fields = "camelCase"
	)
)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
pub enum ZipMethod {
	/// The data as it is. Every entry ends in a data descriptor (its CRC-32 is only known once
	/// the data is written), which readers that go through a zip front to back without its
	/// central directory refuse for stored entries, such as `java.util.zip.ZipInputStream`;
	/// `ZipFile` and every other central-directory reader take it.
	Stored,
	/// Levels 1 to 9; zip has no level 0 deflate (use [`ZipMethod::Stored`]).
	Deflate { level: u32 },
	/// Levels 1 to 9.
	Bzip2 { level: u32 },
}

impl ZipMethod {
	/// The method's level, checked against its range.
	pub(crate) fn check(self) -> Result<(), &'static str> {
		match self {
			Self::Stored => Ok(()),
			Self::Deflate { level } | Self::Bzip2 { level } if (1..=9).contains(&level) => Ok(()),
			Self::Deflate { .. } | Self::Bzip2 { .. } => Err("zip compression takes levels 1 to 9"),
		}
	}

	fn code(self) -> u16 {
		match self {
			Self::Stored => METHOD_STORED,
			Self::Deflate { .. } => METHOD_DEFLATE,
			Self::Bzip2 { .. } => METHOD_BZIP2,
		}
	}
}

/// How an entry is encrypted, with its salt.
pub(crate) struct Encryption<'p> {
	pub(crate) password: &'p [u8],
	pub(crate) strength: AesStrength,
	pub(crate) salt: Vec<u8>,
}

struct CentralEntry {
	name: Vec<u8>,
	flags: u16,
	method: u16,
	aes: Option<(AesStrength, u16)>,
	dos: (u16, u16),
	unix_time: Option<i32>,
	crc: u32,
	compressed_size: u64,
	size: u64,
	offset: u64,
	dir: bool,
}

/// Counts what goes through to `inner`.
struct Counting<W> {
	inner: W,
	written: u64,
}

impl<W: Write> Write for Counting<W> {
	fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
		let n = self.inner.write(buf)?;
		self.written += n as u64;
		Ok(n)
	}

	fn flush(&mut self) -> io::Result<()> {
		self.inner.flush()
	}
}

pub(crate) struct ZipWriter<W> {
	out: Counting<W>,
	entries: Vec<CentralEntry>,
	/// [`ZIP64_ENTRY_THRESHOLD`], but for the tests, which cannot write 4 GiB to reach it.
	zip64_entry_threshold: u64,
}

impl<W: Write> ZipWriter<W> {
	pub(crate) fn new(out: W) -> Self {
		Self {
			out: Counting {
				inner: out,
				written: 0,
			},
			entries: Vec::new(),
			zip64_entry_threshold: ZIP64_ENTRY_THRESHOLD,
		}
	}

	/// A writer whose archive already holds `written` bytes that `out` never sees, and which
	/// writes an entry of `zip64_entry_threshold` bytes or more with zip64 sizes: the zip64
	/// records of an archive past 4 GiB, without writing 4 GiB.
	#[cfg(test)]
	pub(crate) fn past(out: W, written: u64, zip64_entry_threshold: u64) -> Self {
		Self {
			out: Counting {
				inner: out,
				written,
			},
			entries: Vec::new(),
			zip64_entry_threshold,
		}
	}

	/// Adds a directory; `path` without its trailing `/`.
	pub(crate) fn add_dir(
		&mut self,
		path: &str,
		modified: Option<DateTime<Utc>>,
	) -> io::Result<()> {
		let entry = CentralEntry {
			name: format!("{path}/").into_bytes(),
			flags: FLAG_UTF8,
			method: METHOD_STORED,
			aes: None,
			dos: dos_datetime(modified),
			unix_time: unix_time(modified),
			crc: 0,
			compressed_size: 0,
			size: 0,
			offset: self.out.written,
			dir: true,
		};
		write_local_header(&mut self.out, &entry, false)?;
		self.entries.push(entry);
		Ok(())
	}

	/// Adds a file whose data is `data`, which must be `size` bytes long; the bytes it held.
	pub(crate) fn add_file(
		&mut self,
		path: &str,
		modified: Option<DateTime<Utc>>,
		size: u64,
		method: ZipMethod,
		encryption: Option<Encryption<'_>>,
		data: &mut dyn Read,
	) -> io::Result<u64> {
		let zip64 = size >= self.zip64_entry_threshold;
		let mut entry = CentralEntry {
			name: path.as_bytes().to_vec(),
			flags: FLAG_UTF8
				| FLAG_DATA_DESCRIPTOR
				| if encryption.is_some() {
					FLAG_ENCRYPTED
				} else {
					0
				},
			method: if encryption.is_some() {
				METHOD_AES
			} else {
				method.code()
			},
			aes: encryption
				.as_ref()
				.map(|encryption| (encryption.strength, method.code())),
			dos: dos_datetime(modified),
			unix_time: unix_time(modified),
			crc: 0,
			compressed_size: 0,
			size: 0,
			offset: self.out.written,
			dir: false,
		};
		write_local_header(&mut self.out, &entry, zip64)?;

		let start = self.out.written;
		let mut crc = crc32fast::Hasher::new();
		let mut read = 0u64;
		{
			// data → compression → encryption → the archive
			let encrypted: Box<dyn Finish + '_> = match &encryption {
				None => Box::new(Plain(&mut self.out)),
				Some(encryption) => Box::new(AesWriter::new(
					&mut self.out,
					encryption.password,
					encryption.strength,
					&encryption.salt,
				)?),
			};
			let mut compressed: Box<dyn FinishInto + '_> = match method {
				ZipMethod::Stored => Box::new(Stored(encrypted)),
				ZipMethod::Deflate { level } => Box::new(flate2::write::DeflateEncoder::new(
					encrypted,
					flate2::Compression::new(level),
				)),
				ZipMethod::Bzip2 { level } => Box::new(bzip2::write::BzEncoder::new(
					encrypted,
					bzip2::Compression::new(level),
				)),
			};
			let mut buf = vec![0u8; 64 * 1024];
			loop {
				let n = match data.read(&mut buf) {
					Ok(0) => break,
					Ok(n) => n,
					Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
					Err(e) => return Err(e),
				};
				crc.update(&buf[..n]);
				read += n as u64;
				compressed.write_all(&buf[..n])?;
			}
			compressed.finish_into()?.finish()?;
		}
		entry.compressed_size = self.out.written - start;
		entry.size = read;
		// AE-2 stores no CRC: the authentication code covers the data instead
		entry.crc = if encryption.is_some() {
			0
		} else {
			crc.finalize()
		};
		let out = &mut self.out;
		out.write_all(&DATA_DESCRIPTOR_SIG.to_le_bytes())?;
		out.write_all(&entry.crc.to_le_bytes())?;
		if zip64 {
			out.write_all(&entry.compressed_size.to_le_bytes())?;
			out.write_all(&entry.size.to_le_bytes())?;
		} else {
			let too_large = || io::Error::other("an entry grew past 4 GiB without zip64");
			out.write_all(
				&u32::try_from(entry.compressed_size)
					.map_err(|_| too_large())?
					.to_le_bytes(),
			)?;
			out.write_all(
				&u32::try_from(entry.size)
					.map_err(|_| too_large())?
					.to_le_bytes(),
			)?;
		}
		self.entries.push(entry);
		Ok(read)
	}

	/// Writes the central directory and the end records.
	pub(crate) fn finish(mut self) -> io::Result<W> {
		let cd_start = self.out.written;
		for entry in &self.entries {
			write_central_header(&mut self.out, entry)?;
		}
		let cd_size = self.out.written - cd_start;
		let count = self.entries.len() as u64;
		let needs_zip64 = count >= 0xFFFF || cd_size >= 0xFFFF_FFFF || cd_start >= 0xFFFF_FFFF;
		let out = &mut self.out;
		if needs_zip64 {
			let record_at = out.written;
			out.write_all(&EOCD64_SIG.to_le_bytes())?;
			// the record's size, less its first 12 bytes
			out.write_all(&44u64.to_le_bytes())?;
			out.write_all(&VERSION_MADE_BY.to_le_bytes())?;
			out.write_all(&VERSION_ZIP64.to_le_bytes())?;
			out.write_all(&0u32.to_le_bytes())?;
			out.write_all(&0u32.to_le_bytes())?;
			out.write_all(&count.to_le_bytes())?;
			out.write_all(&count.to_le_bytes())?;
			out.write_all(&cd_size.to_le_bytes())?;
			out.write_all(&cd_start.to_le_bytes())?;
			out.write_all(&EOCD64_LOCATOR_SIG.to_le_bytes())?;
			out.write_all(&0u32.to_le_bytes())?;
			out.write_all(&record_at.to_le_bytes())?;
			out.write_all(&1u32.to_le_bytes())?;
		}
		out.write_all(&EOCD_SIG.to_le_bytes())?;
		out.write_all(&0u16.to_le_bytes())?;
		out.write_all(&0u16.to_le_bytes())?;
		let count16 = u16::try_from(count).unwrap_or(0xFFFF);
		out.write_all(&count16.to_le_bytes())?;
		out.write_all(&count16.to_le_bytes())?;
		out.write_all(&clamp32(cd_size).to_le_bytes())?;
		out.write_all(&clamp32(cd_start).to_le_bytes())?;
		out.write_all(&0u16.to_le_bytes())?;
		Ok(self.out.inner)
	}
}

fn clamp32(value: u64) -> u32 {
	u32::try_from(value).unwrap_or(0xFFFF_FFFF)
}

fn extras(entry: &CentralEntry, zip64: Option<Vec<u64>>) -> Vec<u8> {
	let mut extra = Vec::new();
	if let Some(values) = zip64 {
		extra.extend_from_slice(&0x0001u16.to_le_bytes());
		extra.extend_from_slice(&((values.len() * 8) as u16).to_le_bytes());
		for value in values {
			extra.extend_from_slice(&value.to_le_bytes());
		}
	}
	if let Some(time) = entry.unix_time {
		extra.extend_from_slice(&0x5455u16.to_le_bytes());
		extra.extend_from_slice(&5u16.to_le_bytes());
		extra.push(1);
		extra.extend_from_slice(&time.to_le_bytes());
	}
	if let Some((strength, actual)) = entry.aes {
		extra.extend_from_slice(&0x9901u16.to_le_bytes());
		extra.extend_from_slice(&7u16.to_le_bytes());
		// AE-2
		extra.extend_from_slice(&2u16.to_le_bytes());
		extra.extend_from_slice(b"AE");
		extra.push(strength.byte());
		extra.extend_from_slice(&actual.to_le_bytes());
	}
	extra
}

/// The zip specification version (4.4.3.2) that deflate and directories need.
const VERSION_DEFLATE: u16 = 20;
const VERSION_ZIP64: u16 = 45;
const VERSION_BZIP2: u16 = 46;
const VERSION_AES: u16 = 51;

/// The highest version any of the entry's features needs: a bzip2 entry with zip64 sizes needs
/// bzip2's, not zip64's.
fn version_needed(entry: &CentralEntry, zip64: bool) -> u16 {
	[
		(zip64, VERSION_ZIP64),
		(entry.method == METHOD_BZIP2, VERSION_BZIP2),
		(entry.aes.is_some(), VERSION_AES),
	]
	.into_iter()
	.filter(|&(needed, _)| needed)
	.fold(VERSION_DEFLATE, |version, (_, needed)| version.max(needed))
}

fn write_local_header<W: Write>(out: &mut W, entry: &CentralEntry, zip64: bool) -> io::Result<()> {
	let extra = extras(entry, zip64.then(|| vec![0, 0]));
	out.write_all(&LOCAL_HEADER_SIG.to_le_bytes())?;
	out.write_all(&version_needed(entry, zip64).to_le_bytes())?;
	out.write_all(&entry.flags.to_le_bytes())?;
	out.write_all(&entry.method.to_le_bytes())?;
	out.write_all(&entry.dos.1.to_le_bytes())?;
	out.write_all(&entry.dos.0.to_le_bytes())?;
	// CRC-32 and sizes follow the data in its descriptor (or are zero, for a directory)
	out.write_all(&0u32.to_le_bytes())?;
	let size_field: u32 = if zip64 { 0xFFFF_FFFF } else { 0 };
	out.write_all(&size_field.to_le_bytes())?;
	out.write_all(&size_field.to_le_bytes())?;
	out.write_all(&(entry.name.len() as u16).to_le_bytes())?;
	out.write_all(&(extra.len() as u16).to_le_bytes())?;
	out.write_all(&entry.name)?;
	out.write_all(&extra)
}

fn write_central_header<W: Write>(out: &mut W, entry: &CentralEntry) -> io::Result<()> {
	let mut zip64 = Vec::new();
	let size = if entry.size >= 0xFFFF_FFFF {
		zip64.push(entry.size);
		0xFFFF_FFFF
	} else {
		entry.size as u32
	};
	let compressed_size = if entry.compressed_size >= 0xFFFF_FFFF {
		zip64.push(entry.compressed_size);
		0xFFFF_FFFF
	} else {
		entry.compressed_size as u32
	};
	let offset = if entry.offset >= 0xFFFF_FFFF {
		zip64.push(entry.offset);
		0xFFFF_FFFF
	} else {
		entry.offset as u32
	};
	let has_zip64 = !zip64.is_empty();
	let extra = extras(entry, has_zip64.then_some(zip64));
	let mode: u32 = if entry.dir { 0o040_755 } else { 0o100_644 };
	let external = (mode << 16) | if entry.dir { 0x10 } else { 0 };
	out.write_all(&CENTRAL_HEADER_SIG.to_le_bytes())?;
	out.write_all(&VERSION_MADE_BY.to_le_bytes())?;
	out.write_all(&version_needed(entry, has_zip64).to_le_bytes())?;
	out.write_all(&entry.flags.to_le_bytes())?;
	out.write_all(&entry.method.to_le_bytes())?;
	out.write_all(&entry.dos.1.to_le_bytes())?;
	out.write_all(&entry.dos.0.to_le_bytes())?;
	out.write_all(&entry.crc.to_le_bytes())?;
	out.write_all(&compressed_size.to_le_bytes())?;
	out.write_all(&size.to_le_bytes())?;
	out.write_all(&(entry.name.len() as u16).to_le_bytes())?;
	out.write_all(&(extra.len() as u16).to_le_bytes())?;
	// comment length, disk number, internal attributes
	out.write_all(&[0; 6])?;
	out.write_all(&external.to_le_bytes())?;
	out.write_all(&offset.to_le_bytes())?;
	out.write_all(&entry.name)?;
	out.write_all(&extra)
}

/// `(date, time)` as DOS stores them, in local time (what zip tools show), clamped to the years
/// DOS can hold; 1980-01-01 when unknown.
fn dos_datetime(modified: Option<DateTime<Utc>>) -> (u16, u16) {
	let Some(time) = modified else {
		return ((1 << 5) | 1, 0);
	};
	let local = time.with_timezone(&Local);
	let year = local.year().clamp(1980, 2107);
	if year != local.year() {
		// out of range: the nearest end of it
		return if year == 1980 {
			((1 << 5) | 1, 0)
		} else {
			((127 << 9) | (12 << 5) | 31, (23 << 11) | (59 << 5) | 29)
		};
	}
	let date = (((year - 1980) as u16) << 9) | ((local.month() as u16) << 5) | local.day() as u16;
	let clock = ((local.hour() as u16) << 11)
		| ((local.minute() as u16) << 5)
		| (local.second() as u16 / 2);
	(date, clock)
}

/// The time as the Unix extended timestamp field holds it, when it fits.
fn unix_time(modified: Option<DateTime<Utc>>) -> Option<i32> {
	modified.and_then(|time| i32::try_from(time.timestamp()).ok())
}

/// The end of an entry's data: flushes a compressor or writes an auth code.
trait Finish: Write {
	fn finish(self: Box<Self>) -> io::Result<()>;
}

struct Plain<W>(W);

impl<W: Write> Write for Plain<W> {
	fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
		self.0.write(buf)
	}

	fn flush(&mut self) -> io::Result<()> {
		self.0.flush()
	}
}

impl<W: Write> Finish for Plain<W> {
	fn finish(self: Box<Self>) -> io::Result<()> {
		Ok(())
	}
}

impl<W: Write> Finish for AesWriter<W> {
	fn finish(self: Box<Self>) -> io::Result<()> {
		AesWriter::finish(*self).map(drop)
	}
}

/// A compressor over an encryption layer, finished into it.
trait FinishInto: Write {
	fn finish_into<'a>(self: Box<Self>) -> io::Result<Box<dyn Finish + 'a>>
	where
		Self: 'a;
}

struct Stored<'a>(Box<dyn Finish + 'a>);

impl Write for Stored<'_> {
	fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
		self.0.write(buf)
	}

	fn flush(&mut self) -> io::Result<()> {
		self.0.flush()
	}
}

impl<'b> FinishInto for Stored<'b> {
	fn finish_into<'a>(self: Box<Self>) -> io::Result<Box<dyn Finish + 'a>>
	where
		Self: 'a,
	{
		Ok(self.0)
	}
}

impl<'b> FinishInto for flate2::write::DeflateEncoder<Box<dyn Finish + 'b>> {
	fn finish_into<'a>(self: Box<Self>) -> io::Result<Box<dyn Finish + 'a>>
	where
		Self: 'a,
	{
		(*self).finish()
	}
}

impl<'b> FinishInto for bzip2::write::BzEncoder<Box<dyn Finish + 'b>> {
	fn finish_into<'a>(self: Box<Self>) -> io::Result<Box<dyn Finish + 'a>>
	where
		Self: 'a,
	{
		(*self).finish()
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn the_version_needed_meets_every_requirement() {
		let entry = |method: ZipMethod, aes: bool| CentralEntry {
			name: b"a".to_vec(),
			flags: 0,
			method: if aes { METHOD_AES } else { method.code() },
			aes: aes.then_some((AesStrength::Aes256, method.code())),
			dos: dos_datetime(None),
			unix_time: None,
			crc: 0,
			compressed_size: 0,
			size: 0,
			offset: 0,
			dir: false,
		};
		// (method, AES, zip64, version): deflate 2.0, zip64 4.5, bzip2 4.6, AES 5.1
		for (method, aes, zip64, version) in [
			(ZipMethod::Stored, false, false, 20),
			(ZipMethod::Deflate { level: 6 }, false, true, 45),
			(ZipMethod::Bzip2 { level: 9 }, false, false, 46),
			(ZipMethod::Bzip2 { level: 9 }, false, true, 46),
			(ZipMethod::Bzip2 { level: 9 }, true, true, 51),
		] {
			assert_eq!(
				version_needed(&entry(method, aes), zip64),
				version,
				"{method:?} {aes} {zip64}"
			);
		}
	}
}
