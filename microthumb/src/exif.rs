//! A minimal, bounds-checked TIFF/EXIF walker: just enough to read the
//! orientation tag and locate the IFD1 embedded thumbnail. Input is the raw
//! EXIF payload (TIFF header first — any "Exif\0\0" prefix already stripped,
//! which is how `jpeg_decoder::Decoder::exif_data` hands it over). Everything
//! here is hostile-input territory: no indexing without a bounds check, no
//! panics, `None` on any structural nonsense.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Endian {
	Little,
	Big,
}

pub(crate) fn u16_at(data: &[u8], off: usize, endian: Endian) -> Option<u16> {
	let bytes: [u8; 2] = data.get(off..off + 2)?.try_into().ok()?;
	Some(match endian {
		Endian::Little => u16::from_le_bytes(bytes),
		Endian::Big => u16::from_be_bytes(bytes),
	})
}

pub(crate) fn u32_at(data: &[u8], off: usize, endian: Endian) -> Option<u32> {
	let bytes: [u8; 4] = data.get(off..off + 4)?.try_into().ok()?;
	Some(match endian {
		Endian::Little => u32::from_le_bytes(bytes),
		Endian::Big => u32::from_be_bytes(bytes),
	})
}

struct Ifd<'a> {
	data: &'a [u8],
	endian: Endian,
	offset: usize,
	entries: u16,
}

impl<'a> Ifd<'a> {
	fn at(data: &'a [u8], endian: Endian, offset: usize) -> Option<Self> {
		let entries = u16_at(data, offset, endian)?;
		Some(Ifd {
			data,
			endian,
			offset,
			entries,
		})
	}

	/// A tag's value as an unsigned integer, for SHORT and LONG entries whose
	/// value fits inline (count 1) — which covers every tag walked here.
	fn uint_value(&self, tag: u16) -> Option<u32> {
		for i in 0..self.entries as usize {
			let entry = self.offset + 2 + i * 12;
			if u16_at(self.data, entry, self.endian)? != tag {
				continue;
			}
			let kind = u16_at(self.data, entry + 2, self.endian)?;
			let count = u32_at(self.data, entry + 4, self.endian)?;
			if count != 1 {
				return None;
			}
			return match kind {
				3 => u16_at(self.data, entry + 8, self.endian).map(u32::from),
				4 => u32_at(self.data, entry + 8, self.endian),
				_ => None,
			};
		}
		None
	}

	/// Offset of the next IFD, or `None` when this is the last one.
	fn next_ifd(&self) -> Option<usize> {
		let end = self.offset + 2 + self.entries as usize * 12;
		let next = u32_at(self.data, end, self.endian)?;
		if next == 0 { None } else { Some(next as usize) }
	}
}

fn tiff_ifd0(exif: &[u8]) -> Option<Ifd<'_>> {
	let endian = match exif.get(0..2)? {
		b"II" => Endian::Little,
		b"MM" => Endian::Big,
		_ => return None,
	};
	if u16_at(exif, 2, endian)? != 42 {
		return None;
	}
	let ifd0 = u32_at(exif, 4, endian)? as usize;
	Ifd::at(exif, endian, ifd0)
}

/// The EXIF orientation (1–8), defaulting to 1 (upright) when absent or
/// malformed — a wrong default only costs a rotated thumbnail, never an error.
pub fn orientation(exif: &[u8]) -> u8 {
	fn walk(exif: &[u8]) -> Option<u32> {
		tiff_ifd0(exif)?.uint_value(0x0112)
	}
	match walk(exif).map(u8::try_from) {
		Some(Ok(v @ 1..=8)) => v,
		_ => 1,
	}
}

/// Where IFD0 lives per the 8-byte TIFF header — for callers probing a
/// bounded prefix of a larger file, who may need a second read at that offset
/// when the directory sits beyond their window (IFD-last files: the layout
/// most TIFF writers, including the `tiff` crate's encoder, produce).
pub fn ifd0_offset(exif: &[u8]) -> Option<u64> {
	let endian = match exif.get(0..2)? {
		b"II" => Endian::Little,
		b"MM" => Endian::Big,
		_ => return None,
	};
	if u16_at(exif, 2, endian)? != 42 {
		return None;
	}
	Some(u64::from(u32_at(exif, 4, endian)?))
}

/// [`orientation`] for a separately-fetched IFD0: `header` establishes the
/// endianness, `window` holds the directory table rebased to offset 0. Only
/// inline (SHORT, count 1) values resolve — which the orientation tag is.
pub fn orientation_in_window(header: &[u8], window: &[u8]) -> u8 {
	fn walk(header: &[u8], window: &[u8]) -> Option<u32> {
		let endian = match header.get(0..2)? {
			b"II" => Endian::Little,
			b"MM" => Endian::Big,
			_ => return None,
		};
		Ifd::at(window, endian, 0)?.uint_value(0x0112)
	}
	match walk(header, window).map(u8::try_from) {
		Some(Ok(v @ 1..=8)) => v,
		_ => 1,
	}
}

/// How many strips or tiles a TIFF's IFD0 DECLARES, for callers that must
/// bound work before handing the file to a decoder — the `tiff` crate reads
/// the whole per-chunk offset/bytecount tables inside `Decoder::new`, under
/// its own 256 MiB default, so by the time a caller could inspect anything
/// the allocation has already happened.
///
/// `header` establishes endianness and `window` holds the directory table:
/// pass the same buffer twice when IFD0 is inside the probe, or the rebased
/// second window when it is not (see [`ifd0_offset`]). `None` when the tags
/// are absent, out of line, or degenerate — the caller decides whether an
/// unreadable declaration is worth refusing.
pub fn tiff_chunk_count(header: &[u8], window: &[u8], ifd_at: usize) -> Option<u64> {
	let endian = match header.get(0..2)? {
		b"II" => Endian::Little,
		b"MM" => Endian::Big,
		_ => return None,
	};
	let ifd = Ifd::at(window, endian, ifd_at)?;
	// Tiled: ceil(width/tile_width) * ceil(height/tile_height).
	if let (Some(tw), Some(th)) = (ifd.uint_value(0x0142), ifd.uint_value(0x0143)) {
		let (w, h) = (ifd.uint_value(0x0100)?, ifd.uint_value(0x0101)?);
		if tw == 0 || th == 0 {
			return None;
		}
		let across = u64::from(w).div_ceil(u64::from(tw));
		let down = u64::from(h).div_ceil(u64::from(th));
		return Some(across.saturating_mul(down));
	}
	// Stripped: ceil(ImageLength / RowsPerStrip). A missing RowsPerStrip
	// means one strip for the whole image, per the TIFF default.
	let height = u64::from(ifd.uint_value(0x0101)?);
	let rows_per_strip = ifd.uint_value(0x0116).map_or(height, u64::from);
	if rows_per_strip == 0 {
		return None;
	}
	Some(height.div_ceil(rows_per_strip))
}

/// The IFD1 embedded thumbnail's JPEG bytes (JPEGInterchangeFormat /
/// -Length), when present and structurally sane.
pub fn embedded_thumbnail(exif: &[u8]) -> Option<&[u8]> {
	let ifd0 = tiff_ifd0(exif)?;
	let ifd1 = Ifd::at(exif, ifd0.endian, ifd0.next_ifd()?)?;
	let offset = ifd1.uint_value(0x0201)? as usize;
	let length = ifd1.uint_value(0x0202)? as usize;
	let bytes = exif.get(offset..offset.checked_add(length)?)?;
	// Only the JPEG-compressed form is worth handling; uncompressed IFD1
	// thumbnails (compression 1) are near-extinct.
	bytes.starts_with(&[0xFF, 0xD8]).then_some(bytes)
}

#[cfg(test)]
mod tests {
	use super::{embedded_thumbnail, orientation};

	/// A hand-built little-endian TIFF: IFD0 with orientation 6, chaining to
	/// an IFD1 whose thumbnail tags point at a fake JPEG payload at the end.
	fn fixture() -> Vec<u8> {
		let mut exif = Vec::new();
		exif.extend_from_slice(b"II");
		exif.extend_from_slice(&42u16.to_le_bytes());
		exif.extend_from_slice(&8u32.to_le_bytes()); // IFD0 at 8
		// IFD0: 1 entry (orientation), next-IFD pointer follows.
		exif.extend_from_slice(&1u16.to_le_bytes());
		exif.extend_from_slice(&0x0112u16.to_le_bytes());
		exif.extend_from_slice(&3u16.to_le_bytes()); // SHORT
		exif.extend_from_slice(&1u32.to_le_bytes());
		exif.extend_from_slice(&6u32.to_le_bytes()); // value 6, padded
		exif.extend_from_slice(&26u32.to_le_bytes()); // IFD1 at 26
		// IFD1: thumbnail offset + length entries.
		exif.extend_from_slice(&2u16.to_le_bytes());
		exif.extend_from_slice(&0x0201u16.to_le_bytes());
		exif.extend_from_slice(&4u16.to_le_bytes()); // LONG
		exif.extend_from_slice(&1u32.to_le_bytes());
		exif.extend_from_slice(&56u32.to_le_bytes()); // thumb at 56
		exif.extend_from_slice(&0x0202u16.to_le_bytes());
		exif.extend_from_slice(&4u16.to_le_bytes());
		exif.extend_from_slice(&1u32.to_le_bytes());
		exif.extend_from_slice(&4u32.to_le_bytes()); // 4 bytes long
		exif.extend_from_slice(&0u32.to_le_bytes()); // no next IFD
		assert_eq!(exif.len(), 56);
		exif.extend_from_slice(&[0xFF, 0xD8, 0xFF, 0xD9]);
		exif
	}

	#[test]
	fn walks_orientation_and_ifd1_thumbnail() {
		let exif = fixture();
		assert_eq!(orientation(&exif), 6);
		assert_eq!(
			embedded_thumbnail(&exif),
			Some(&[0xFF, 0xD8, 0xFF, 0xD9][..])
		);
	}

	#[test]
	fn hostile_or_truncated_input_never_panics() {
		assert_eq!(orientation(&[]), 1);
		assert_eq!(embedded_thumbnail(&[]), None);
		let mut exif = fixture();
		// Point the thumbnail past the end of the payload.
		exif.truncate(57);
		assert_eq!(embedded_thumbnail(&exif), None);
		// Not a TIFF at all.
		assert_eq!(orientation(b"garbage everywhere"), 1);
	}
}
