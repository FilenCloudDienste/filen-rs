//! The pieces 7z headers are made of: property ids, variable-length numbers and bit vectors.

use super::SevenZError;

pub(crate) const SIGNATURE: [u8; 6] = [b'7', b'z', 0xBC, 0xAF, 0x27, 0x1C];
/// The signature, the format version (0.4) and the start header after it.
pub(crate) const START_HEADER_LEN: u64 = 32;

pub(crate) const K_END: u8 = 0x00;
pub(crate) const K_HEADER: u8 = 0x01;
pub(crate) const K_ARCHIVE_PROPERTIES: u8 = 0x02;
pub(crate) const K_ADDITIONAL_STREAMS_INFO: u8 = 0x03;
pub(crate) const K_MAIN_STREAMS_INFO: u8 = 0x04;
pub(crate) const K_FILES_INFO: u8 = 0x05;
pub(crate) const K_PACK_INFO: u8 = 0x06;
pub(crate) const K_UNPACK_INFO: u8 = 0x07;
pub(crate) const K_SUBSTREAMS_INFO: u8 = 0x08;
pub(crate) const K_SIZE: u8 = 0x09;
pub(crate) const K_CRC: u8 = 0x0A;
pub(crate) const K_FOLDER: u8 = 0x0B;
pub(crate) const K_CODERS_UNPACK_SIZE: u8 = 0x0C;
pub(crate) const K_NUM_UNPACK_STREAM: u8 = 0x0D;
pub(crate) const K_EMPTY_STREAM: u8 = 0x0E;
pub(crate) const K_EMPTY_FILE: u8 = 0x0F;
pub(crate) const K_ANTI: u8 = 0x10;
pub(crate) const K_NAME: u8 = 0x11;
pub(crate) const K_MTIME: u8 = 0x14;
pub(crate) const K_WIN_ATTRIBUTES: u8 = 0x15;
pub(crate) const K_ENCODED_HEADER: u8 = 0x17;

/// Windows' directory attribute.
pub(crate) const ATTRIBUTE_DIRECTORY: u32 = 0x10;
/// A reparse point, which is how Windows stores a symbolic link.
pub(crate) const ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
/// Set when the high 16 bits carry a Unix mode (p7zip's extension, which 7-Zip reads too).
pub(crate) const ATTRIBUTE_UNIX_EXTENSION: u32 = 0x8000;
pub(crate) const UNIX_TYPE_MASK: u32 = 0o170_000;
pub(crate) const UNIX_SYMLINK: u32 = 0o120_000;
pub(crate) const UNIX_DIR: u32 = 0o040_000;

/// Seconds from 1601-01-01, where FILETIME counts from, to the Unix epoch.
pub(crate) const FILETIME_UNIX_OFFSET_SECS: i64 = 11_644_473_600;

/// A read position in a decoded header.
pub(crate) struct HeaderReader<'a> {
	bytes: &'a [u8],
	at: usize,
}

impl<'a> HeaderReader<'a> {
	pub(crate) fn new(bytes: &'a [u8]) -> Self {
		Self { bytes, at: 0 }
	}

	pub(crate) fn remaining(&self) -> usize {
		self.bytes.len() - self.at
	}

	pub(crate) fn byte(&mut self) -> Result<u8, SevenZError> {
		Ok(self.bytes(1)?[0])
	}

	pub(crate) fn bytes(&mut self, len: usize) -> Result<&'a [u8], SevenZError> {
		if len > self.remaining() {
			return Err(truncated());
		}
		let bytes = &self.bytes[self.at..self.at + len];
		self.at += len;
		Ok(bytes)
	}

	pub(crate) fn u32(&mut self) -> Result<u32, SevenZError> {
		Ok(u32::from_le_bytes(
			self.bytes(4)?.try_into().expect("4 bytes"),
		))
	}

	pub(crate) fn u64(&mut self) -> Result<u64, SevenZError> {
		Ok(u64::from_le_bytes(
			self.bytes(8)?.try_into().expect("8 bytes"),
		))
	}

	/// 7z's variable-length number: the first byte's leading one bits count the bytes that
	/// follow (least significant first), its remaining bits are the number's top bits.
	pub(crate) fn number(&mut self) -> Result<u64, SevenZError> {
		let first = self.byte()?;
		let mut value = 0u64;
		let mut mask = 0x80u8;
		for i in 0..8 {
			if first & mask == 0 {
				let high = u64::from(first & mask.wrapping_sub(1));
				return Ok(value | (high << (8 * i)));
			}
			value |= u64::from(self.byte()?) << (8 * i);
			mask >>= 1;
		}
		Ok(value)
	}

	/// A count of items, each of which takes at least a byte of what is left of the header, so
	/// no count can make a reader allocate more than the header's size in items.
	pub(crate) fn count(&mut self, max: u64) -> Result<usize, SevenZError> {
		let count = self.number()?;
		if count > max {
			return Err(SevenZError::TooLarge("a 7z header lists too many items"));
		}
		if count > self.remaining() as u64 {
			return Err(truncated());
		}
		Ok(count as usize)
	}

	/// A number that has to fit a `usize` and the header left, for a length of bytes that
	/// follow.
	pub(crate) fn length(&mut self) -> Result<usize, SevenZError> {
		let len = self.number()?;
		if len > self.remaining() as u64 {
			return Err(truncated());
		}
		Ok(len as usize)
	}

	/// `len` flags, most significant bit first.
	pub(crate) fn bits(&mut self, len: usize) -> Result<Vec<bool>, SevenZError> {
		let bytes = self.bytes(len.div_ceil(8))?;
		Ok((0..len)
			.map(|i| bytes[i / 8] & (0x80 >> (i % 8)) != 0)
			.collect())
	}

	/// A byte saying "all set", else `len` flags.
	pub(crate) fn defined(&mut self, len: usize) -> Result<Vec<bool>, SevenZError> {
		if self.byte()? != 0 {
			return Ok(vec![true; len]);
		}
		self.bits(len)
	}

	/// The "external" byte in front of file properties, which says the data is elsewhere; no
	/// writer does that.
	pub(crate) fn not_external(&mut self) -> Result<(), SevenZError> {
		match self.byte()? {
			0 => Ok(()),
			_ => Err(SevenZError::Unsupported("external 7z header data")),
		}
	}

	/// CRC-32s for the `len` items the flags before them mark as having one.
	pub(crate) fn digests(&mut self, len: usize) -> Result<Vec<Option<u32>>, SevenZError> {
		let defined = self.defined(len)?;
		defined
			.into_iter()
			.map(|defined| defined.then(|| self.u32()).transpose())
			.collect()
	}

	/// Skips a property's data, whose length comes first.
	pub(crate) fn skip_data(&mut self) -> Result<(), SevenZError> {
		let len = self.length()?;
		self.bytes(len)?;
		Ok(())
	}

	/// Skips properties up to `id`, which has to come before the end of the section.
	pub(crate) fn wait_for(&mut self, id: u8) -> Result<(), SevenZError> {
		loop {
			match self.property()? {
				found if found == id => return Ok(()),
				K_END => return Err(SevenZError::Corrupt("a 7z header section is incomplete")),
				_ => self.skip_data()?,
			}
		}
	}

	/// A property id, which is a number that fits a byte.
	pub(crate) fn property(&mut self) -> Result<u8, SevenZError> {
		u8::try_from(self.number()?)
			.map_err(|_| SevenZError::Corrupt("a 7z header property id is out of range"))
	}
}

fn truncated() -> SevenZError {
	SevenZError::Corrupt("a 7z header ends early")
}

/// Appends 7z's variable-length encoding of `value`.
pub(crate) fn write_number(out: &mut Vec<u8>, value: u64) {
	let mut first = 0u8;
	let mut mask = 0x80u8;
	let mut extra = 0;
	while extra < 8 {
		if value < 1u64 << (7 * (extra + 1)) {
			first |= (value >> (8 * extra)) as u8;
			break;
		}
		first |= mask;
		mask >>= 1;
		extra += 1;
	}
	out.push(first);
	out.extend_from_slice(&value.to_le_bytes()[..extra]);
}

/// Appends `flags`, most significant bit first.
pub(crate) fn write_bits(out: &mut Vec<u8>, flags: &[bool]) {
	for byte in flags.chunks(8) {
		out.push(
			byte.iter()
				.enumerate()
				.fold(0u8, |acc, (i, &set)| acc | (u8::from(set) << (7 - i))),
		);
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn numbers_round_trip_at_every_width() {
		let mut values = vec![0, 1, 0x7F, 0x80, 0x3FFF, 0x4000, u64::MAX, u64::MAX - 1];
		for bits in 1..64 {
			values.extend([(1u64 << bits) - 1, 1u64 << bits, (1u64 << bits) + 1]);
		}
		for value in values {
			let mut out = Vec::new();
			write_number(&mut out, value);
			let mut reader = HeaderReader::new(&out);
			assert_eq!(reader.number().unwrap(), value);
			assert_eq!(reader.remaining(), 0, "{value:#x}");
		}
		// the encodings 7-Zip's own writer produces
		let encoded = |value| {
			let mut out = Vec::new();
			write_number(&mut out, value);
			out
		};
		assert_eq!(encoded(0x7F), [0x7F]);
		assert_eq!(encoded(0x80), [0x80, 0x80]);
		assert_eq!(encoded(0x1234), [0x92, 0x34]);
		assert_eq!(encoded(u64::MAX), [0xFF; 9]);
	}

	#[test]
	fn counts_are_bounded_by_the_header_left() {
		let mut out = Vec::new();
		write_number(&mut out, 5);
		out.extend([0; 4]);
		assert!(HeaderReader::new(&out).count(100).is_err());
		out.push(0);
		assert_eq!(HeaderReader::new(&out).count(100).unwrap(), 5);
		assert!(matches!(
			HeaderReader::new(&out).count(4),
			Err(SevenZError::TooLarge(_))
		));
	}

	#[test]
	fn bit_vectors_are_most_significant_first() {
		let flags = [true, false, false, true, true, false, false, false, true];
		let mut out = Vec::new();
		write_bits(&mut out, &flags);
		assert_eq!(out, [0b1001_1000, 0b1000_0000]);
		assert_eq!(HeaderReader::new(&out).bits(9).unwrap(), flags);
		assert_eq!(HeaderReader::new(&[1]).defined(3).unwrap(), [true; 3]);
	}
}
