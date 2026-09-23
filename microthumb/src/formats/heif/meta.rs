//! The item index of a HEIF file's `meta` box, read here rather than through
//! libheif for the one thing libheif cannot hand over: this build's libheif
//! carries no JPEG decoder (`heif-decoder`'s build.rs), so a JPEG-coded item
//! is invisible through it. Fujifilm stores the camera's own rendering of
//! every HIF exactly so, as JPEG thumbnail items of the primary image.
//!
//! Only what locates those items is read: `pitm`, `iinf`, `iref`, `iloc`, and
//! the size and transforms of the items concerned from `iprp`. The `meta` box
//! is fetched whole in one bounded read — about a kilobyte on a Fujifilm HIF,
//! a few on a phone HEIC — and parsed as a slice, so nothing is read past what
//! was fetched, and anything malformed ends the walk with no answer.

use std::ops::Range;

use crate::{
	ByteSource,
	formats::{cr3, raw},
};

/// Top-level boxes looked at for `meta`, which every writer puts right after
/// `ftyp`.
const MAX_TOP_LEVEL_BOXES: usize = 16;
/// Most of a `meta` box read. Real ones run from one to a few tens of
/// kilobytes; anything past this is not worth a thumbnail's fetch.
const MAX_META_BYTES: u64 = 256 * 1024;
/// JPEG thumbnail items taken, each of which costs a small read of its head
/// to verify. Fujifilm writes three.
const MAX_JPEG_THUMBNAILS: usize = 8;
/// Most extents one `iloc` item may list — libheif's own default ceiling.
/// Only single-extent items are ever used, but the walk still steps over the
/// others, and with zero-width fields an extent costs no bytes: without a cap
/// a forged box of 65535-extent items fills the meta read with billions of
/// steps.
const MAX_EXTENTS_PER_ITEM: u16 = 32;

/// What the walk found.
pub(super) struct Meta {
	/// The primary image's coded size, before its transforms.
	pub primary_size: (u32, u32),
	/// The JPEG items that are thumbnails of the primary image — claims, not
	/// yet verified as JPEGs.
	pub jpeg_thumbnails: Vec<JpegThumbnail>,
}

/// One JPEG thumbnail item: where its bytes are, and which way up it shows.
pub(super) struct JpegThumbnail {
	pub offset: u64,
	pub len: u64,
	/// As an EXIF orientation: the item's own rotation and mirror when it
	/// declares any, else the primary image's. Fujifilm declares none on its
	/// thumbnails and stores them as coded, so a portrait shot's thumbnail is
	/// sideways until the primary's rotation is applied to it.
	pub orientation: u8,
}

/// The primary image's JPEG thumbnails, or `None` when the file has none this
/// walk can vouch for.
pub(super) fn read(src: &mut dyn ByteSource) -> Option<Meta> {
	let end = src.len();
	let mut at = 0;
	for _ in 0..MAX_TOP_LEVEL_BOXES {
		let (kind, body, body_end) = cr3::read_box(src, at, end)?;
		if &kind == b"meta" {
			if body_end - body > MAX_META_BYTES {
				return None;
			}
			return parse(&raw::read_exact_at(src, body, body_end - body)?);
		}
		at = body_end;
	}
	None
}

fn parse(meta: &[u8]) -> Option<Meta> {
	let (0, _, children) = full_box(meta)? else {
		return None;
	};
	let (mut pitm, mut iinf, mut iref, mut iprp, mut iloc) = (None, None, None, None, None);
	for (kind, body) in boxes(children) {
		let slot = match &kind {
			b"pitm" => &mut pitm,
			b"iinf" => &mut iinf,
			b"iref" => &mut iref,
			b"iprp" => &mut iprp,
			b"iloc" => &mut iloc,
			_ => continue,
		};
		slot.get_or_insert(body);
	}

	let primary = primary_item(pitm?)?;
	let jpegs = jpeg_items(iinf?)?;
	// Every phone HEIC ends here: none of its items is a JPEG.
	if jpegs.is_empty() {
		return None;
	}
	let thumbnails = thumbnails_of(iref?, primary, &jpegs)?;
	if thumbnails.is_empty() {
		return None;
	}
	let properties = item_properties(iprp?)?;
	let primary_size = properties
		.of(primary)
		.find_map(|property| match *property {
			Property::Size(width, height) => Some((width, height)),
			_ => None,
		})?;
	let primary_orientation = orientation(properties.of(primary)).unwrap_or(1);
	let locations = locations(iloc?, &thumbnails)?;
	Some(Meta {
		primary_size,
		jpeg_thumbnails: thumbnails
			.iter()
			.filter_map(|&item| {
				let (_, range) = locations.iter().find(|(id, _)| *id == item)?;
				Some(JpegThumbnail {
					offset: range.start,
					len: range.end - range.start,
					orientation: orientation(properties.of(item)).unwrap_or(primary_orientation),
				})
			})
			.collect(),
	})
}

/// `pitm`: the primary item's id.
fn primary_item(pitm: &[u8]) -> Option<u32> {
	let (version, _, body) = full_box(pitm)?;
	Fields(body).id(version >= 1)
}

/// `iinf`: the ids of the items whose type is `jpeg`. The entry count is not
/// needed — the `infe` boxes that follow are the truth — and versions 0 and 1
/// of `infe` carry no item type at all, so they never name a JPEG.
fn jpeg_items(iinf: &[u8]) -> Option<Vec<u32>> {
	let (version, _, body) = full_box(iinf)?;
	let mut fields = Fields(body);
	fields.take(if version == 0 { 2 } else { 4 })?;
	Some(
		boxes(fields.0)
			.filter(|(kind, _)| kind == b"infe")
			.filter_map(|(_, infe)| {
				let (version, _, body) = full_box(infe)?;
				if version < 2 {
					return None;
				}
				let mut fields = Fields(body);
				let id = fields.id(version >= 3)?;
				// item_protection_index
				fields.take(2)?;
				(fields.take(4)? == b"jpeg").then_some(id)
			})
			.collect(),
	)
}

/// `iref`: the JPEG items with a `thmb` reference to the primary item, in
/// file order, at most [`MAX_JPEG_THUMBNAILS`].
fn thumbnails_of(iref: &[u8], primary: u32, jpegs: &[u32]) -> Option<Vec<u32>> {
	let (version, _, body) = full_box(iref)?;
	let wide = version >= 1;
	Some(
		boxes(body)
			.filter(|(kind, _)| kind == b"thmb")
			.filter_map(|(_, reference)| {
				let mut fields = Fields(reference);
				let from = fields.id(wide)?;
				let count = fields.u16()?;
				let of_primary = (0..count)
					.map_while(|_| fields.id(wide))
					.any(|to| to == primary);
				(of_primary && jpegs.contains(&from)).then_some(from)
			})
			.take(MAX_JPEG_THUMBNAILS)
			.collect(),
	)
}

/// The properties this walk reads from `ipco`; everything else is `Other`,
/// kept only so the 1-based indices `ipma` uses still line up.
enum Property {
	/// `ispe`: the coded width and height.
	Size(u32, u32),
	/// `irot`: counter-clockwise quarter turns.
	Rotation(u8),
	/// `imir`: 0 swaps top and bottom, 1 swaps left and right — libheif's
	/// reading, which this follows.
	Mirror(u8),
	Other,
}

/// What `iprp` says: every property in `ipco`, and each item's property
/// indices from its `ipma` boxes — all of them, as libheif merges them, the
/// first entry for an item winning.
struct ItemProperties {
	properties: Vec<Property>,
	/// 1-based, 0 meaning none, in the order `ipma` lists them — the order
	/// transforms apply in.
	associations: Vec<(u32, Vec<u16>)>,
}

impl ItemProperties {
	/// An item's properties, in order.
	fn of(&self, item: u32) -> impl Iterator<Item = &Property> {
		self.associations
			.iter()
			.find(|(id, _)| *id == item)
			.into_iter()
			.flat_map(|(_, indices)| indices)
			.filter_map(|&index| self.properties.get(usize::from(index).checked_sub(1)?))
	}
}

fn item_properties(iprp: &[u8]) -> Option<ItemProperties> {
	let (mut ipco, mut ipmas) = (None, Vec::new());
	for (kind, body) in boxes(iprp) {
		match &kind {
			b"ipco" => {
				ipco.get_or_insert(body);
			}
			b"ipma" => ipmas.push(body),
			_ => {}
		}
	}
	let properties = boxes(ipco?)
		.map(|(kind, body)| match &kind {
			b"ispe" => full_box(body)
				.and_then(|(_, _, body)| {
					let mut fields = Fields(body);
					Some(Property::Size(fields.u32()?, fields.u32()?))
				})
				.unwrap_or(Property::Other),
			b"irot" => body
				.first()
				.map_or(Property::Other, |byte| Property::Rotation(byte & 3)),
			b"imir" => body
				.first()
				.map_or(Property::Other, |byte| Property::Mirror(byte & 1)),
			_ => Property::Other,
		})
		.collect();

	let mut associations = Vec::new();
	for ipma in ipmas {
		let (version, flags, body) = full_box(ipma)?;
		let mut fields = Fields(body);
		let count = fields.u32()?;
		for _ in 0..count {
			let item = fields.id(version >= 1)?;
			let indices = (0..fields.u8()?)
				.map(|_| {
					// The top bit of each is the `essential` flag.
					if flags & 1 == 1 {
						fields.u16().map(|index| index & 0x7FFF)
					} else {
						fields.u8().map(|index| u16::from(index & 0x7F))
					}
				})
				.collect::<Option<Vec<_>>>()?;
			associations.push((item, indices));
		}
	}
	Some(ItemProperties {
		properties,
		associations,
	})
}

/// `iloc`: where each of `items` lies in the file. Only an item stored as one
/// extent of the file itself is kept — construction method 0, no external
/// data reference, a non-zero length — which is how every JPEG thumbnail item
/// seen in the wild is stored; anything else would need reassembly and is
/// left alone.
fn locations(iloc: &[u8], items: &[u32]) -> Option<Vec<(u32, Range<u64>)>> {
	let (version, _, body) = full_box(iloc)?;
	if version > 2 {
		return None;
	}
	let mut fields = Fields(body);
	let sizes = fields.u16()?;
	let (offset_size, length_size, base_offset_size) = (
		usize::from(sizes >> 12),
		usize::from((sizes >> 8) & 0xF),
		usize::from((sizes >> 4) & 0xF),
	);
	let index_size = if version >= 1 {
		usize::from(sizes & 0xF)
	} else {
		0
	};
	let count = if version < 2 {
		u32::from(fields.u16()?)
	} else {
		fields.u32()?
	};
	let mut found = Vec::new();
	for _ in 0..count {
		let item = fields.id(version >= 2)?;
		let construction_method = if version >= 1 { fields.u16()? & 0xF } else { 0 };
		let data_reference = fields.u16()?;
		let base_offset = fields.uint(base_offset_size)?;
		let extents = fields.u16()?;
		if extents > MAX_EXTENTS_PER_ITEM {
			return None;
		}
		let mut first = None;
		for _ in 0..extents {
			fields.uint(index_size)?;
			let offset = fields.uint(offset_size)?;
			let length = fields.uint(length_size)?;
			first.get_or_insert((offset, length));
		}
		if let (0, 0, 1, Some((offset, length))) =
			(construction_method, data_reference, extents, first)
			&& length > 0
			&& items.contains(&item)
		{
			let start = base_offset.checked_add(offset)?;
			found.push((item, start..start.checked_add(length)?));
		}
	}
	Some(found)
}

/// The EXIF orientation that `irot` and `imir` properties, applied in order,
/// add up to — `None` when there are none.
fn orientation<'a>(properties: impl Iterator<Item = &'a Property>) -> Option<u8> {
	properties.fold(None, |so_far, property| {
		let step = match *property {
			// Each quarter turn counter-clockwise is EXIF 8.
			Property::Rotation(turns) => (0..turns).fold(1, |o, _| compose(o, 8)),
			Property::Mirror(0) => 4,
			Property::Mirror(_) => 2,
			Property::Size(..) | Property::Other => return so_far,
		};
		Some(compose(so_far.unwrap_or(1), step))
	})
}

/// Each EXIF orientation as the matrix taking a stored pixel's position to
/// its displayed one, `[a, b, c, d]` for `(x, y) -> (ax + by, cx + dy)`, with
/// y pointing down.
const ORIENTATIONS: [[i8; 4]; 8] = [
	[1, 0, 0, 1],
	[-1, 0, 0, 1],
	[-1, 0, 0, -1],
	[1, 0, 0, -1],
	[0, 1, 1, 0],
	[0, -1, 1, 0],
	[0, -1, -1, 0],
	[0, 1, -1, 0],
];

/// Orientation `first`, then `then` applied to its result.
fn compose(first: u8, then: u8) -> u8 {
	let [a, b, c, d] = ORIENTATIONS[usize::from(first - 1)];
	let [e, f, g, h] = ORIENTATIONS[usize::from(then - 1)];
	let product = [e * a + f * c, e * b + f * d, g * a + h * c, g * b + h * d];
	let index = ORIENTATIONS
		.iter()
		.position(|m| *m == product)
		.expect("the eight orientations are closed under composition");
	index as u8 + 1
}

/// A FullBox's version, flags and body.
fn full_box(data: &[u8]) -> Option<(u8, u32, &[u8])> {
	let mut fields = Fields(data);
	let version = fields.u8()?;
	let flags = u32::try_from(fields.uint(3)?).ok()?;
	Some((version, flags, fields.0))
}

/// The boxes laid end to end in `data`, stopping at the first that does not
/// fit.
fn boxes(data: &[u8]) -> impl Iterator<Item = ([u8; 4], &[u8])> {
	let mut rest = data;
	std::iter::from_fn(move || {
		let mut fields = Fields(rest);
		let size = fields.u32()?;
		let kind: [u8; 4] = fields.take(4)?.try_into().ok()?;
		let body = match size {
			// To the end of the enclosing box.
			0 => fields.0,
			1 => {
				let size = usize::try_from(fields.u64()?).ok()?;
				fields.0.get(..size.checked_sub(16)?)?
			}
			size => fields
				.0
				.get(..usize::try_from(size).ok()?.checked_sub(8)?)?,
		};
		let consumed = rest.len() - fields.0.len() + body.len();
		rest = &rest[consumed..];
		Some((kind, body))
	})
}

/// Big-endian fields read front to back, `None` once the bytes run out.
struct Fields<'a>(&'a [u8]);

impl<'a> Fields<'a> {
	fn take(&mut self, len: usize) -> Option<&'a [u8]> {
		let (head, rest) = self.0.split_at_checked(len)?;
		self.0 = rest;
		Some(head)
	}

	/// An unsigned field of `len` bytes, 0 to 8; a zero-length one is 0.
	fn uint(&mut self, len: usize) -> Option<u64> {
		if len > 8 {
			return None;
		}
		Some(
			self.take(len)?
				.iter()
				.fold(0, |value, &byte| value << 8 | u64::from(byte)),
		)
	}

	fn u8(&mut self) -> Option<u8> {
		self.take(1).map(|bytes| bytes[0])
	}

	fn u16(&mut self) -> Option<u16> {
		Some(u16::from_be_bytes(self.take(2)?.try_into().ok()?))
	}

	fn u32(&mut self) -> Option<u32> {
		Some(u32::from_be_bytes(self.take(4)?.try_into().ok()?))
	}

	fn u64(&mut self) -> Option<u64> {
		Some(u64::from_be_bytes(self.take(8)?.try_into().ok()?))
	}

	/// An item id: 32-bit in the wide box versions, 16-bit otherwise.
	fn id(&mut self, wide: bool) -> Option<u32> {
		if wide {
			self.u32()
		} else {
			self.u16().map(u32::from)
		}
	}
}

#[cfg(test)]
mod tests {
	use super::{Property, item_properties, locations, orientation, parse, read};
	use crate::{MemSource, formats::cr3};

	const FUJI: &[u8] = include_bytes!("../../../tests/fixtures/heif/fuji.heic");
	const FUJI_PORTRAIT: &[u8] = include_bytes!("../../../tests/fixtures/heif/fuji-irot1.heic");

	/// The `meta` box's body, as `read` fetches it.
	fn meta_body(file: &[u8]) -> &[u8] {
		let mut src = MemSource(file.to_vec());
		let end = file.len() as u64;
		let mut at = 0;
		loop {
			let (kind, body, body_end) = cr3::read_box(&mut src, at, end).unwrap();
			if &kind == b"meta" {
				return &file[body as usize..body_end as usize];
			}
			at = body_end;
		}
	}

	#[test]
	fn every_jpeg_thumbnail_of_the_primary_is_found() {
		let meta = read(&mut MemSource(FUJI.to_vec())).expect("three JPEG thumbnails");
		assert_eq!(meta.primary_size, (120, 80));
		assert_eq!(meta.jpeg_thumbnails.len(), 3);
		for thumbnail in &meta.jpeg_thumbnails {
			let start = thumbnail.offset as usize;
			let end = start + thumbnail.len as usize;
			assert_eq!(&FUJI[start..start + 2], [0xFF, 0xD8], "SOI at {start}");
			assert_eq!(&FUJI[end - 2..end], [0xFF, 0xD9], "EOI before {end}");
			assert_eq!(thumbnail.orientation, 1);
		}
	}

	/// Fujifilm's thumbnails carry no transform of their own, so a portrait
	/// shot's are shown with the primary's quarter turn.
	#[test]
	fn a_thumbnail_without_transforms_takes_the_primarys() {
		let meta = read(&mut MemSource(FUJI_PORTRAIT.to_vec())).expect("three JPEG thumbnails");
		// Coded size: the transform is not applied to it.
		assert_eq!(meta.primary_size, (120, 80));
		assert!(
			meta.jpeg_thumbnails
				.iter()
				.all(|thumbnail| thumbnail.orientation == 8)
		);
	}

	#[test]
	fn a_heif_without_jpeg_items_has_nothing_to_offer() {
		for file in [
			&include_bytes!("../../../tests/fixtures/heif/hevc-thumb.heic")[..],
			include_bytes!("../../../tests/fixtures/heif/grid-irot1.heic"),
		] {
			assert!(read(&mut MemSource(file.to_vec())).is_none());
		}
	}

	#[test]
	fn transforms_compose_in_the_order_they_are_listed() {
		let of = |properties: &[Property]| orientation(properties.iter());
		assert_eq!(of(&[]), None);
		assert_eq!(of(&[Property::Size(1, 1), Property::Other]), None);
		// A quarter turn counter-clockwise at a time: 90, 180, 270.
		assert_eq!(of(&[Property::Rotation(0)]), Some(1));
		assert_eq!(of(&[Property::Rotation(1)]), Some(8));
		assert_eq!(of(&[Property::Rotation(2)]), Some(3));
		assert_eq!(of(&[Property::Rotation(3)]), Some(6));
		// Axis 0 swaps top and bottom, axis 1 left and right.
		assert_eq!(of(&[Property::Mirror(0)]), Some(4));
		assert_eq!(of(&[Property::Mirror(1)]), Some(2));
		// Turned, then flipped left to right: transverse. The other way
		// round: transposed.
		assert_eq!(of(&[Property::Rotation(1), Property::Mirror(1)]), Some(7));
		assert_eq!(of(&[Property::Mirror(1), Property::Rotation(1)]), Some(5));
		// Two quarter turns listed apart are one half turn.
		assert_eq!(of(&[Property::Rotation(1), Property::Rotation(1)]), Some(3));
	}

	/// Every prefix of the `meta` box and every single-byte corruption of it
	/// either parses or answers `None` — never a panic, never a read outside
	/// what was fetched.
	#[test]
	fn a_truncated_or_corrupted_meta_box_never_panics() {
		let body = meta_body(FUJI_PORTRAIT);
		for len in 0..body.len() {
			parse(&body[..len]);
		}
		let mut corrupted = body.to_vec();
		for at in 0..body.len() {
			for flip in [0x01, 0x80, 0xFF] {
				corrupted[at] ^= flip;
				parse(&corrupted);
				corrupted[at] ^= flip;
			}
		}
		// And through the file: every truncation of it.
		for len in 0..FUJI_PORTRAIT.len() {
			read(&mut MemSource(FUJI_PORTRAIT[..len].to_vec()));
		}
	}

	fn boxed(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
		let mut out = ((body.len() + 8) as u32).to_be_bytes().to_vec();
		out.extend_from_slice(kind);
		out.extend_from_slice(body);
		out
	}

	/// The `iloc` every real Fujifilm HIF writes — version 1, 32-bit offsets
	/// and lengths, a construction method on each item — which the MP4Box
	/// fixtures (version 0) never exercise. The grid item lives in `idat`
	/// (method 1) and is left alone; the JPEG is a plain file extent.
	#[test]
	fn a_version_1_iloc_is_read_as_fujifilm_writes_it() {
		let mut iloc = vec![1, 0, 0, 0];
		// Offset and length 4 bytes each, no base offset, no extent index.
		iloc.extend_from_slice(&0x4400u16.to_be_bytes());
		iloc.extend_from_slice(&2u16.to_be_bytes());
		for (item, method, offset, length) in [(512u16, 0u16, 20_480u32, 690_102u32), (1, 1, 0, 8)]
		{
			iloc.extend_from_slice(&item.to_be_bytes());
			iloc.extend_from_slice(&method.to_be_bytes());
			// data_reference_index, then one extent.
			iloc.extend_from_slice(&0u16.to_be_bytes());
			iloc.extend_from_slice(&1u16.to_be_bytes());
			iloc.extend_from_slice(&offset.to_be_bytes());
			iloc.extend_from_slice(&length.to_be_bytes());
		}
		assert_eq!(
			locations(&iloc, &[512, 1]),
			Some(vec![(512, 20_480..20_480 + 690_102)])
		);
	}

	/// Zero-width fields make an extent cost no bytes, so an item may list
	/// 65535 of them for free; past libheif's own 32 the walk gives up at once
	/// instead of stepping through them.
	#[test]
	fn an_item_with_too_many_extents_ends_the_walk() {
		let mut iloc = vec![0, 0, 0, 0];
		iloc.extend_from_slice(&0u16.to_be_bytes());
		iloc.extend_from_slice(&1u16.to_be_bytes());
		// Item 1, data_reference_index 0, 65535 extents of nothing.
		iloc.extend_from_slice(&1u16.to_be_bytes());
		iloc.extend_from_slice(&0u16.to_be_bytes());
		iloc.extend_from_slice(&u16::MAX.to_be_bytes());
		assert_eq!(locations(&iloc, &[1]), None);
	}

	/// An item's properties may sit in a second `ipma`, which libheif merges
	/// into the first: a rotation declared there still counts.
	#[test]
	fn every_ipma_box_is_read() {
		let mut ispe = vec![0, 0, 0, 0];
		ispe.extend_from_slice(&1000u32.to_be_bytes());
		ispe.extend_from_slice(&500u32.to_be_bytes());
		let mut ipco = boxed(b"ispe", &ispe);
		ipco.extend_from_slice(&boxed(b"irot", &[1]));
		// Version 0 and no flags: 16-bit item ids, 7-bit property indices.
		let ipma = |item: u16, property: u8| {
			let mut body = vec![0, 0, 0, 0];
			body.extend_from_slice(&1u32.to_be_bytes());
			body.extend_from_slice(&item.to_be_bytes());
			body.extend_from_slice(&[1, property]);
			boxed(b"ipma", &body)
		};
		let mut iprp = boxed(b"ipco", &ipco);
		iprp.extend_from_slice(&ipma(1, 1));
		iprp.extend_from_slice(&ipma(2, 2));
		let properties = item_properties(&iprp).expect("parses");
		assert_eq!(orientation(properties.of(2)), Some(8));
		assert!(matches!(
			properties.of(1).next(),
			Some(Property::Size(1000, 500))
		));
	}
}
