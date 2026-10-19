//! Canon CR3, via the preview image in its ISO-BMFF box tree.
//!
//! CR3 broke with the TIFF lineage every other Canon RAW follows: it is an
//! MP4-shaped container, so none of the directory machinery in [`super::raw`]
//! applies. What survives is the principle — find the JPEG Canon stored
//! alongside the mosaic and decode that — so all this module does is walk to
//! the box holding it.
//!
//! Three JPEGs are in there. Two sit in boxes nested inside `uuid` boxes
//! Canon defines: `PRVW` (1620x1080 across every pinned body, from the EOS R
//! to the PowerShot V1) and `THMB` (160x120). The third is the whole picture
//! at sensor resolution, stored as the single sample of the first track —
//! `moov` > `trak` > `mdia` > `minf` > `stbl`, where `stsd` names the codec
//! (a `JPEG` box inside the `CRAW` sample entry; the sensor tracks carry
//! `CMP1` there instead), `stsz` the sample's size and `co64` its offset in
//! `mdat`. All three are bare JPEGs — SOI straight into the quantisation
//! tables, no APP1 — so the shot's orientation is not in any stream. It is in
//! `CMT1`, the box beside them that carries the file's EXIF IFD0 as a plain
//! TIFF.
//!
//! Which JPEG serves depends on the caller. A thumbnail takes the largest
//! box: an IDCT-scaled decode still reads every byte of the entropy stream,
//! so decoding the full picture would cost twenty times PRVW's bytes for the
//! same 256 px tile. A preview takes the largest of all, the full picture
//! included — it is the only rendering of the shot a viewer will ever get.
//!
//! The walk is its own bounded parser rather than a media library: box sizes
//! are attacker-controlled, so every one is checked against its parent's
//! extent, recursion is depth-capped, the total number of boxes visited is
//! capped, and no read is ever sized from a header without that check.

use crate::{
	ByteSource, FormatDecoder, LocatedPreview, PreparedDecode, ThumbError, ThumbSpec, exif,
};

use super::raw::{self, Index, Preview};

pub struct Cr3;

/// Boxes visited across the whole tree.
const MAX_BOXES: usize = 256;
/// How deep the containers nest before the walk stops. `moov` -> `trak` ->
/// `mdia` -> `minf` is the deepest real path; `stbl`'s own children are read
/// flat by [`Walk::sample_table`].
const MAX_DEPTH: u8 = 4;
/// A box header is 8 bytes, or 16 when the size word is the `1` escape.
const BOX_HEADER: u64 = 8;
/// Bytes of a preview box's payload searched for the SOI. Both box types put
/// their JPEG at offset 16, behind a small fixed descriptor.
const PREVIEW_PROBE: u64 = 32;
/// Most of `CMT1` read for the orientation tag. The box is IFD0 alone — make,
/// model, timestamps, a few hundred bytes on every pinned body — so this caps
/// a forged box size rather than budgeting a real one.
const CMT1_MAX_BYTES: u64 = 64 * 1024;
/// Most of an `stsd` read to name a track's codec. A `CRAW` sample entry is
/// ~120–240 bytes on every pinned body.
const STSD_MAX_BYTES: u64 = 4096;

/// The JPEGs the file carries, and its orientation.
///
/// `with_tracks` says whether the full picture in the first track is looked
/// at as well: its head is a read at an `mdat` offset, which over the network
/// is a chunk the thumbnail path never needs.
///
/// The JPEGs carry no EXIF of their own, so a portrait shot would come out
/// sideways without the file-level tag. `CMT1` is a TIFF, which is exactly the
/// payload the EXIF walker takes.
fn index(src: &mut dyn ByteSource, with_tracks: bool) -> Index {
	let mut walk = Walk {
		boxes: 0,
		previews: Vec::new(),
		tracks: Vec::new(),
		cmt1: None,
	};
	let end = src.len();
	walk.boxes_in(src, 0, end, 0);
	let mut believe = |claims: Vec<(u64, u64)>| {
		claims
			.into_iter()
			.filter_map(|(off, len)| raw::jpeg_window(src, off, len))
			.max_by_key(Preview::area)
	};
	let boxed = believe(walk.previews);
	let track = if with_tracks {
		believe(walk.tracks)
	} else {
		None
	};
	let orientation = walk
		.cmt1
		.and_then(|(body, body_end)| {
			raw::read_exact_at(src, body, (body_end - body).min(CMT1_MAX_BYTES))
		})
		.map_or(1, |tiff| exif::orientation(&tiff));
	Index {
		orientation,
		// The box first: see the module doc for why a thumbnail never wants
		// the full picture. The track only when there is no box at all —
		// which `open` asks for only once it has seen that there is none.
		preview: boxed.or(track),
		jpeg: [boxed, track]
			.into_iter()
			.flatten()
			.max_by_key(Preview::area),
	}
}

impl FormatDecoder for Cr3 {
	fn detect(&self, prefix: &[u8]) -> bool {
		// `ftyp` with Canon's `crx ` brand. Checking the brand as well as the
		// box keeps ordinary MP4s — which nothing here decodes — from being
		// claimed and then refused.
		prefix.get(4..12) == Some(b"ftypcrx ")
	}

	fn locate_preview(
		&self,
		src: &mut dyn ByteSource,
	) -> Result<Option<LocatedPreview>, ThumbError> {
		Ok(raw::locate(index(src, true)))
	}

	fn open(
		&self,
		mut src: Box<dyn ByteSource>,
		spec: &ThumbSpec,
	) -> Result<Box<dyn PreparedDecode>, ThumbError> {
		let mut found = index(&mut *src, false);
		if found.preview.is_none() {
			// No preview box at all: the picture track is the only JPEG
			// left, and its head is worth the chunk when the alternative is
			// no thumbnail.
			found = index(&mut *src, true);
		}
		let Index {
			orientation,
			preview: Some(preview),
			..
		} = found
		else {
			return Err(ThumbError::Decode(
				"cr3: no preview box; the sensor data is not a decodable image".into(),
			));
		};
		raw::prepare(src, preview, orientation, spec)
	}
}

struct Walk {
	boxes: usize,
	/// Byte ranges the preview boxes claim hold a JPEG, unverified.
	previews: Vec<(u64, u64)>,
	/// Byte ranges the sample tables claim hold a JPEG-coded track's one
	/// sample, unverified.
	tracks: Vec<(u64, u64)>,
	/// Payload range of the first `CMT1` box: the file's EXIF IFD0.
	cmt1: Option<(u64, u64)>,
}

/// One box header at `at`, as `(kind, body, body_end)`, or `None` when it
/// does not fit inside `end` — a tree that is forged or truncated, where the
/// caller stops rather than guessing where the next box starts.
pub(super) fn read_box(src: &mut dyn ByteSource, at: u64, end: u64) -> Option<([u8; 4], u64, u64)> {
	let header = raw::read_exact_at(src, at, BOX_HEADER)?;
	let kind: [u8; 4] = header[4..8].try_into().expect("four bytes");
	let mut size = u64::from(u32::from_be_bytes(
		header[0..4].try_into().expect("four bytes"),
	));
	let mut header_len = BOX_HEADER;
	match size {
		// The 64-bit escape.
		1 => {
			let large = raw::read_exact_at(src, at + BOX_HEADER, 8)?;
			size = u64::from_be_bytes(large.try_into().expect("eight bytes"));
			header_len = 16;
		}
		// "To the end of the enclosing box."
		0 => size = end - at,
		_ => {}
	}
	if size < header_len || at.checked_add(size).is_none_or(|box_end| box_end > end) {
		return None;
	}
	Some((kind, at + header_len, at + size))
}

impl Walk {
	fn boxes_in(&mut self, src: &mut dyn ByteSource, start: u64, end: u64, depth: u8) {
		if depth > MAX_DEPTH {
			return;
		}
		let mut at = start;
		while at + BOX_HEADER <= end && self.boxes < MAX_BOXES {
			self.boxes += 1;
			let Some((kind, body, body_end)) = read_box(src, at, end) else {
				return;
			};
			match &kind {
				b"PRVW" | b"THMB" => self.preview_box(src, body, body_end),
				b"CMT1" => {
					if self.cmt1.is_none() {
						self.cmt1 = Some((body, body_end));
					}
				}
				b"moov" | b"trak" | b"mdia" | b"minf" => {
					self.boxes_in(src, body, body_end, depth + 1)
				}
				b"stbl" => self.sample_table(src, body, body_end),
				// A `uuid` box names itself with 16 bytes and then, in Canon's
				// case, either starts its children immediately or first writes
				// a version word. Rather than hard-code which UUID does which,
				// look at whether a box header is actually there.
				b"uuid" => {
					if let Some(child) = child_start(src, body + 16, body_end) {
						self.boxes_in(src, child, body_end, depth + 1);
					}
				}
				_ => {}
			}
			at = body_end;
		}
	}

	/// A `PRVW` / `THMB` payload: a short fixed descriptor, then the JPEG. The
	/// descriptor's own width and height fields differ in layout between the
	/// two box types, so the SOI is found by looking rather than by trusting
	/// either, and the dimensions come from the JPEG itself.
	fn preview_box(&mut self, src: &mut dyn ByteSource, body: u64, body_end: u64) {
		let probe_len = PREVIEW_PROBE.min(body_end.saturating_sub(body));
		let Some(probe) = raw::read_exact_at(src, body, probe_len) else {
			return;
		};
		let Some(soi) = probe.windows(3).position(|w| w == [0xFF, 0xD8, 0xFF]) else {
			return;
		};
		let off = body + soi as u64;
		self.previews.push((off, body_end - off));
	}

	/// A track's sample table, read flat: the codec its sample entry names,
	/// the size of its first sample and where that sample sits. Only a track
	/// whose entry carries a `JPEG` box becomes a candidate — the sensor
	/// tracks (`CMP1`) would cost a read at their `mdat` offset just to be
	/// refused for having no SOI, and over the network that read is a chunk.
	fn sample_table(&mut self, src: &mut dyn ByteSource, body: u64, end: u64) {
		let (mut jpeg, mut size, mut offset) = (false, None, None);
		let mut at = body;
		while at + BOX_HEADER <= end && self.boxes < MAX_BOXES {
			self.boxes += 1;
			let Some((kind, body, body_end)) = read_box(src, at, end) else {
				return;
			};
			let len = body_end - body;
			match &kind {
				b"stsd" => {
					// Version and flags, an entry count, then the `CRAW`
					// sample entry: the standard 78-byte visual entry, a few
					// bytes Canon adds, then the codec box. The tail makes a
					// fixed offset a guess, so the codec box is found by its
					// name; the stream is verified by its own SOI and SOF
					// either way.
					if let Some(stsd) = raw::read_exact_at(src, body, len.min(STSD_MAX_BYTES)) {
						jpeg = stsd
							.get(8..)
							.is_some_and(|entries| entries.windows(4).any(|w| w == b"JPEG"));
					}
				}
				// Version and flags, a sample size that applies to every
				// sample when non-zero, a count, then per-sample sizes.
				b"stsz" => {
					if let Some(stsz) = raw::read_exact_at(src, body, len.min(16)) {
						let shared = be_u32(&stsz, 4);
						let count = be_u32(&stsz, 8);
						size = match (shared, count) {
							(Some(0), Some(n)) if n >= 1 => be_u32(&stsz, 12).map(u64::from),
							(Some(shared), _) if shared > 0 => Some(u64::from(shared)),
							_ => None,
						};
					}
				}
				// Version and flags, a count, then chunk offsets — 64-bit in
				// `co64`, 32-bit in `stco`. One sample per chunk here.
				b"co64" => {
					if let Some(co64) = raw::read_exact_at(src, body, len.min(16))
						&& be_u32(&co64, 4).is_some_and(|n| n >= 1)
						&& let Some(bytes) = co64.get(8..16)
					{
						offset = Some(u64::from_be_bytes(bytes.try_into().expect("eight bytes")));
					}
				}
				b"stco" => {
					if let Some(stco) = raw::read_exact_at(src, body, len.min(12))
						&& be_u32(&stco, 4).is_some_and(|n| n >= 1)
					{
						offset = be_u32(&stco, 8).map(u64::from);
					}
				}
				_ => {}
			}
			at = body_end;
		}
		if jpeg && let (Some(offset), Some(size)) = (offset, size) {
			self.tracks.push((offset, size));
		}
	}
}

fn be_u32(data: &[u8], at: usize) -> Option<u32> {
	data.get(at..at + 4)
		.map(|b| u32::from_be_bytes(b.try_into().expect("four bytes")))
}

/// Where a `uuid` box's children begin: right after the UUID, or 8 bytes
/// further on when what sits there is a version word rather than a box.
fn child_start(src: &mut dyn ByteSource, at: u64, end: u64) -> Option<u64> {
	for candidate in [at, at + 8] {
		if candidate + BOX_HEADER > end {
			continue;
		}
		let header = raw::read_exact_at(src, candidate, BOX_HEADER)?;
		// Every box type in this container is four printable characters; a
		// version word is not.
		if header[4..8].iter().all(u8::is_ascii_graphic) {
			return Some(candidate);
		}
	}
	None
}

#[cfg(test)]
mod tests;
