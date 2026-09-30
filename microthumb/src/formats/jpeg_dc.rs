//! DC-only decoding of progressive JPEG — the escape hatch for progressive
//! sources whose full decode cannot fit the budget (whole-image coefficient
//! storage, ~3 B/px, is structural to progressive).
//!
//! A progressive file's first scans carry every block's DC coefficient, and
//! the DC term of the IDCT is the block's mean — so the DC scans alone are a
//! native 1/8-scale preview, sitting physically at the START of the bitstream.
//! This module Huffman-decodes exactly those scans into per-component
//! block-resolution planes (~0.05 B/px of the source image) and never reads
//! past them: over a lazy [`ByteSource`] the bulk of the file is never
//! fetched at all.
//!
//! Everything here parses untrusted bytes: no indexing without a bounds
//! check, checked arithmetic on anything header-derived, malformed input is
//! [`ThumbError::Decode`], never a panic.

use crate::{ByteSource, PixelSink, PreparedDecode, SmallImage, ThumbError};

use super::jpeg::exif_preview;

/// Ceiling on total DC blocks across all components: 2^21 blocks, which the
/// planes store as i32, so 8 MiB of allocation — most of a decode budget on
/// its own. In source terms that is roughly 89 MP at 4:2:0, 67 MP at 4:2:2 and
/// 45 MP at 4:4:4 (a component contributes mcus_x*h * mcus_y*v blocks, so the
/// limit moves with the sampling factors).
///
/// Checked before the planes are allocated, so header-forged dimensions cannot
/// make us allocate first and discover the problem afterwards. It fires on real
/// files too — a 188 MP 8466x22207 progressive library scan needs 4.4M blocks,
/// twice this — and those legitimately have no affordable decode path here.
const MAX_TOTAL_BLOCKS: u64 = 1 << 21;

fn err(msg: &str) -> ThumbError {
	ThumbError::Decode(format!("jpeg dc-scan: {msg}"))
}

/// Buffered sequential reads over a [`ByteSource`] — the marker walk and the
/// bit reader both consume through this, so "how far did we read" is one
/// number (`pos`), which is also the whole prefix-only story.
struct ByteCursor<'a> {
	src: &'a mut dyn ByteSource,
	pos: u64,
	buf: [u8; 4096],
	buf_start: u64,
	buf_len: usize,
}

impl<'a> ByteCursor<'a> {
	fn new(src: &'a mut dyn ByteSource, pos: u64) -> Self {
		ByteCursor {
			src,
			pos,
			buf: [0; 4096],
			buf_start: 0,
			buf_len: 0,
		}
	}

	fn next_u8(&mut self) -> Result<u8, ThumbError> {
		if self.pos < self.buf_start || self.pos >= self.buf_start + self.buf_len as u64 {
			let n = self.src.read_at(self.pos, &mut self.buf)?;
			if n == 0 {
				return Err(err("truncated"));
			}
			self.buf_start = self.pos;
			self.buf_len = n;
		}
		let byte = self.buf[(self.pos - self.buf_start) as usize];
		self.pos += 1;
		Ok(byte)
	}

	fn next_u16(&mut self) -> Result<u16, ThumbError> {
		Ok(u16::from_be_bytes([self.next_u8()?, self.next_u8()?]))
	}

	fn skip(&mut self, n: u64) {
		self.pos += n;
	}
}

#[derive(Clone)]
struct CompInfo {
	h: u8,
	v: u8,
	tq: u8,
}

struct FrameInfo {
	width: u32,
	height: u32,
	comps: Vec<(u8, CompInfo)>,
	hmax: u8,
	vmax: u8,
}

/// Canonical Huffman table, decoded bit by bit via the classic
/// mincode/maxcode/valptr walk (JPEG F.16) — slow-and-simple beats a lookup
/// table for a preview path fed hostile input.
struct HuffTable {
	mincode: [i32; 17],
	maxcode: [i32; 17],
	valptr: [usize; 17],
	symbols: Vec<u8>,
}

impl HuffTable {
	fn build(counts: &[u8; 16], symbols: Vec<u8>) -> Result<Self, ThumbError> {
		let total: usize = counts.iter().map(|&c| c as usize).sum();
		if total != symbols.len() || total == 0 || total > 256 {
			return Err(err("huffman table with inconsistent symbol count"));
		}
		let mut mincode = [0i32; 17];
		let mut maxcode = [-1i32; 17];
		let mut valptr = [0usize; 17];
		let mut code = 0i32;
		let mut k = 0usize;
		for len in 1..=16usize {
			let n = counts[len - 1] as i32;
			if n > 0 {
				valptr[len] = k;
				mincode[len] = code;
				code += n;
				maxcode[len] = code - 1;
				k += n as usize;
			}
			code <<= 1;
			// A canonical code of length `len` cannot exceed 2^len - 1; a
			// table claiming more is malformed and would otherwise let the
			// decode loop below run away.
			if code > (1 << (len + 1)) {
				return Err(err("huffman table overfull"));
			}
		}
		Ok(HuffTable {
			mincode,
			maxcode,
			valptr,
			symbols,
		})
	}
}

/// Bit-level reads over the entropy-coded segment, honoring byte stuffing
/// (FF 00) and stopping cleanly at markers.
struct BitReader<'a, 'b> {
	cursor: &'a mut ByteCursor<'b>,
	bits: u32,
	nbits: u8,
	/// A marker (RSTn or scan-terminating) was hit; no more data bits exist
	/// until the walk handles it.
	pending_marker: Option<u8>,
}

impl<'a, 'b> BitReader<'a, 'b> {
	fn new(cursor: &'a mut ByteCursor<'b>) -> Self {
		BitReader {
			cursor,
			bits: 0,
			nbits: 0,
			pending_marker: None,
		}
	}

	fn fill(&mut self) -> Result<(), ThumbError> {
		while self.nbits <= 24 {
			if self.pending_marker.is_some() {
				return Ok(());
			}
			let byte = self.cursor.next_u8()?;
			let byte = if byte == 0xFF {
				let next = self.cursor.next_u8()?;
				if next == 0x00 {
					0xFF
				} else {
					self.pending_marker = Some(next);
					return Ok(());
				}
			} else {
				byte
			};
			self.bits |= u32::from(byte) << (24 - self.nbits);
			self.nbits += 8;
		}
		Ok(())
	}

	fn take_bits(&mut self, n: u8) -> Result<u32, ThumbError> {
		debug_assert!(n <= 16);
		if n == 0 {
			return Ok(0);
		}
		self.fill()?;
		if self.nbits < n {
			return Err(err("entropy data ended inside a code"));
		}
		let out = self.bits >> (32 - n);
		self.bits <<= n;
		self.nbits -= n;
		Ok(out)
	}

	fn decode_symbol(&mut self, table: &HuffTable) -> Result<u8, ThumbError> {
		let mut code = 0i32;
		for len in 1..=16usize {
			code = (code << 1) | self.take_bits(1)? as i32;
			if table.maxcode[len] >= code && code >= table.mincode[len] {
				let idx = table.valptr[len] + (code - table.mincode[len]) as usize;
				return table
					.symbols
					.get(idx)
					.copied()
					.ok_or_else(|| err("huffman code indexes past its symbols"));
			}
		}
		Err(err("huffman code longer than 16 bits"))
	}

	/// JPEG EXTEND: an `n`-bit magnitude to a signed value.
	fn receive_extend(&mut self, n: u8) -> Result<i32, ThumbError> {
		if n == 0 {
			return Ok(0);
		}
		let v = self.take_bits(n)? as i32;
		Ok(if v < (1 << (n - 1)) {
			v - (1 << n) + 1
		} else {
			v
		})
	}
}

pub(super) struct PreparedDcScan {
	src: Box<dyn ByteSource>,
	dims: (u32, u32),
	out_dims: (u32, u32),
	exif: Option<Vec<u8>>,
	orientation: u8,
	plane_bytes: usize,
}

impl PreparedDcScan {
	/// Walks the headers far enough to size everything; the scans themselves
	/// are decoded in `decode_into`.
	pub(super) fn open(
		mut src: Box<dyn ByteSource>,
		exif: Option<Vec<u8>>,
		orientation: u8,
	) -> Result<Self, ThumbError> {
		let frame = {
			let mut cursor = ByteCursor::new(&mut *src, 0);
			let mut walk = Walk::new(&mut cursor);
			walk.until_frame()?
		};
		let dims = (frame.width, frame.height);
		let out_dims = (frame.width.div_ceil(8), frame.height.div_ceil(8));
		let plane_bytes = total_blocks(&frame)? as usize * 4;
		Ok(PreparedDcScan {
			src,
			dims,
			out_dims,
			exif,
			orientation,
			plane_bytes,
		})
	}
}

fn mcu_grid(frame: &FrameInfo) -> (u32, u32) {
	(
		frame.width.div_ceil(8 * u32::from(frame.hmax)),
		frame.height.div_ceil(8 * u32::from(frame.vmax)),
	)
}

fn total_blocks(frame: &FrameInfo) -> Result<u64, ThumbError> {
	let (mcus_x, mcus_y) = mcu_grid(frame);
	let mut total = 0u64;
	for (_, comp) in &frame.comps {
		total += u64::from(mcus_x) * u64::from(comp.h) * u64::from(mcus_y) * u64::from(comp.v);
	}
	if total == 0 || total > MAX_TOTAL_BLOCKS {
		return Err(err("implausible dc plane size"));
	}
	Ok(total)
}

impl PreparedDecode for PreparedDcScan {
	fn dims(&self) -> (u32, u32) {
		self.dims
	}

	fn output_dims(&self) -> (u32, u32) {
		self.out_dims
	}

	fn orientation(&self) -> u8 {
		self.orientation
	}

	fn embedded_preview(&mut self, mem_budget: usize) -> Result<Option<SmallImage>, ThumbError> {
		Ok(exif_preview(self.exif.as_deref(), mem_budget))
	}

	fn peak_estimate(&self) -> usize {
		// The per-component DC planes, one output row of RGBA, and the
		// cursor/bit-reader working set.
		self.plane_bytes + self.out_dims.0 as usize * 4 + 8 * 1024
	}

	fn decode_into(mut self: Box<Self>, sink: &mut dyn PixelSink) -> Result<(), ThumbError> {
		let mut cursor = ByteCursor::new(&mut *self.src, 0);
		let mut walk = Walk::new(&mut cursor);
		let planes = walk.decode_dc_scans()?;
		planes.emit(sink)
	}
}

/// One component's decoded state: its DC plane at padded MCU-grid block
/// resolution, and where the next block lands.
struct CompState {
	info: CompInfo,
	/// Dequantized, level-shifted-later DC values, padded to the MCU grid.
	plane: Vec<i32>,
	plane_w: u32,
	plane_h: u32,
	pred: i32,
	/// Next block index for a non-interleaved scan (row-major over the
	/// UNPADDED block grid of this component).
	seq: u64,
	done: bool,
	q_dc: i32,
}

struct Planes {
	width: u32,
	height: u32,
	hmax: u8,
	vmax: u8,
	comps: Vec<CompState>,
}

impl Planes {
	/// One output pixel per luma-resolution 8×8 block: sample every plane at
	/// its sampling-scaled position, reconstruct, convert, push row by row.
	fn emit(&self, sink: &mut dyn PixelSink) -> Result<(), ThumbError> {
		let out_w = self.width.div_ceil(8);
		let out_h = self.height.div_ceil(8);
		let mut row = vec![0u8; out_w as usize * 4];
		for y in 0..out_h {
			for x in 0..out_w {
				// i64 throughout: the DC value carries the successive-
				// approximation shift (`Al`, up to 13) and the quantizer comes
				// from a 16-bit DQT, so a hostile pair overflows i32 — which
				// this module promises never to panic on, and `[profile.test]`
				// keeps overflow-checks on. Clamped to the byte range before
				// narrowing, so the chroma arithmetic below stays small too.
				let sample = |c: &CompState| -> i64 {
					let cx = (x * u32::from(c.info.h) / u32::from(self.hmax)).min(c.plane_w - 1);
					let cy = (y * u32::from(c.info.v) / u32::from(self.vmax)).min(c.plane_h - 1);
					// DC term of the IDCT: block mean = dequantized DC / 8,
					// plus the JPEG level shift.
					let dc = i64::from(c.plane[(cy * c.plane_w + cx) as usize]);
					(dc * i64::from(c.q_dc) / 8 + 128).clamp(0, 255)
				};
				let (r, g, b) = match self.comps.len() {
					1 => {
						let l = sample(&self.comps[0]);
						(l, l, l)
					}
					3 => {
						let y_ = sample(&self.comps[0]);
						let cb = sample(&self.comps[1]) - 128;
						let cr = sample(&self.comps[2]) - 128;
						(
							(y_ + (1402 * cr) / 1000).clamp(0, 255),
							(y_ - (344 * cb + 714 * cr) / 1000).clamp(0, 255),
							(y_ + (1772 * cb) / 1000).clamp(0, 255),
						)
					}
					_ => return Err(err("unsupported component count for emission")),
				};
				let o = x as usize * 4;
				row[o..o + 4].copy_from_slice(&[r as u8, g as u8, b as u8, 255]);
			}
			sink.push(0, y, out_w, &row)?;
		}
		Ok(())
	}
}

/// The marker walk: headers, tables, and the DC scans, stopping the moment
/// every component's first DC scan has been decoded.
struct Walk<'a, 'b> {
	cursor: &'a mut ByteCursor<'b>,
	q_dc: [Option<i32>; 4],
	dc_tables: [Option<HuffTable>; 4],
	restart_interval: u32,
	frame: Option<FrameInfo>,
	planes: Option<Planes>,
}

impl<'a, 'b> Walk<'a, 'b> {
	fn new(cursor: &'a mut ByteCursor<'b>) -> Self {
		Walk {
			cursor,
			q_dc: [None, None, None, None],
			dc_tables: [None, None, None, None],
			restart_interval: 0,
			frame: None,
			planes: None,
		}
	}

	fn expect_soi(&mut self) -> Result<(), ThumbError> {
		if self.cursor.next_u8()? != 0xFF || self.cursor.next_u8()? != 0xD8 {
			return Err(err("missing SOI"));
		}
		Ok(())
	}

	fn next_marker(&mut self) -> Result<u8, ThumbError> {
		if self.cursor.next_u8()? != 0xFF {
			return Err(err("expected a marker"));
		}
		// Fill bytes (FF FF ... marker) are legal padding.
		loop {
			match self.cursor.next_u8()? {
				0xFF => continue,
				0x00 => return Err(err("stuffed byte outside entropy data")),
				m => return Ok(m),
			}
		}
	}

	fn segment_len(&mut self) -> Result<u64, ThumbError> {
		let len = self.cursor.next_u16()?;
		if len < 2 {
			return Err(err("segment length below 2"));
		}
		Ok(u64::from(len) - 2)
	}

	fn parse_dqt(&mut self) -> Result<(), ThumbError> {
		let mut remaining = self.segment_len()?;
		while remaining > 0 {
			let pq_tq = self.cursor.next_u8()?;
			let (pq, tq) = (pq_tq >> 4, (pq_tq & 0x0F) as usize);
			if pq > 1 || tq > 3 {
				return Err(err("malformed quantization table header"));
			}
			// Tables are stored in zigzag order: the FIRST element is the DC
			// quantizer — all this preview needs; the other 63 are skipped.
			let dc = if pq == 0 {
				i32::from(self.cursor.next_u8()?)
			} else {
				i32::from(self.cursor.next_u16()?)
			};
			if dc == 0 {
				return Err(err("zero DC quantizer"));
			}
			self.q_dc[tq] = Some(dc);
			let value_bytes = 64 * (1 + u64::from(pq));
			self.cursor.skip(value_bytes - (1 + u64::from(pq)));
			remaining = remaining
				.checked_sub(1 + value_bytes)
				.ok_or_else(|| err("quantization tables overrun their segment"))?;
		}
		Ok(())
	}

	fn parse_dht(&mut self) -> Result<(), ThumbError> {
		let mut remaining = self.segment_len()?;
		while remaining > 0 {
			let tc_th = self.cursor.next_u8()?;
			let (tc, th) = (tc_th >> 4, (tc_th & 0x0F) as usize);
			if tc > 1 || th > 3 {
				return Err(err("malformed huffman table header"));
			}
			let mut counts = [0u8; 16];
			for c in &mut counts {
				*c = self.cursor.next_u8()?;
			}
			let total: u64 = counts.iter().map(|&c| u64::from(c)).sum();
			if tc == 0 {
				let mut symbols = Vec::with_capacity(total as usize);
				for _ in 0..total {
					symbols.push(self.cursor.next_u8()?);
				}
				self.dc_tables[th] = Some(HuffTable::build(&counts, symbols)?);
			} else {
				// AC tables are irrelevant to a DC preview.
				self.cursor.skip(total);
			}
			remaining = remaining
				.checked_sub(1 + 16 + total)
				.ok_or_else(|| err("huffman tables overrun their segment"))?;
		}
		Ok(())
	}

	fn parse_sof2(&mut self) -> Result<(), ThumbError> {
		let len = self.segment_len()?;
		let precision = self.cursor.next_u8()?;
		let height = u32::from(self.cursor.next_u16()?);
		let width = u32::from(self.cursor.next_u16()?);
		let ncomp = self.cursor.next_u8()?;
		if precision != 8 {
			return Err(err("only 8-bit precision is supported"));
		}
		if width == 0 || height == 0 {
			return Err(err("zero dimensions"));
		}
		if !(ncomp == 1 || ncomp == 3) {
			return Err(err("unsupported component count"));
		}
		if len != 6 + 3 * u64::from(ncomp) {
			return Err(err("SOF2 length mismatch"));
		}
		let mut comps = Vec::with_capacity(ncomp as usize);
		let (mut hmax, mut vmax) = (0u8, 0u8);
		for _ in 0..ncomp {
			let id = self.cursor.next_u8()?;
			let hv = self.cursor.next_u8()?;
			let (h, v) = (hv >> 4, hv & 0x0F);
			let tq = self.cursor.next_u8()?;
			if !(1..=4).contains(&h) || !(1..=4).contains(&v) || tq > 3 {
				return Err(err("malformed component in SOF2"));
			}
			hmax = hmax.max(h);
			vmax = vmax.max(v);
			comps.push((id, CompInfo { h, v, tq }));
		}
		self.frame = Some(FrameInfo {
			width,
			height,
			comps,
			hmax,
			vmax,
		});
		Ok(())
	}

	/// Headers up to and including SOF2 — everything `open` needs.
	fn until_frame(&mut self) -> Result<FrameInfo, ThumbError> {
		self.expect_soi()?;
		loop {
			match self.next_marker()? {
				0xC2 => {
					self.parse_sof2()?;
					return Ok(self.frame.take().expect("just parsed"));
				}
				m if (0xC0..=0xCF).contains(&m) && m != 0xC4 && m != 0xC8 && m != 0xCC => {
					return Err(err("not a progressive frame"));
				}
				0xD9 => return Err(err("EOI before SOF2")),
				_ => {
					let len = self.segment_len()?;
					self.cursor.skip(len);
				}
			}
		}
	}

	fn decode_dc_scans(&mut self) -> Result<Planes, ThumbError> {
		self.expect_soi()?;
		loop {
			match self.next_marker()? {
				0xC2 => self.parse_sof2()?,
				m if (0xC0..=0xCF).contains(&m) && m != 0xC4 && m != 0xC8 && m != 0xCC => {
					return Err(err("not a progressive frame"));
				}
				0xC4 => self.parse_dht()?,
				0xDB => self.parse_dqt()?,
				0xDD => {
					if self.segment_len()? != 2 {
						return Err(err("DRI length mismatch"));
					}
					self.restart_interval = u32::from(self.cursor.next_u16()?);
				}
				0xDA => {
					if let Some(planes) = self.parse_scan()? {
						return Ok(planes);
					}
				}
				0xD9 => return Err(err("EOI before every DC scan arrived")),
				_ => {
					let len = self.segment_len()?;
					self.cursor.skip(len);
				}
			}
		}
	}

	/// One SOS: decodes it when it is a first DC scan, skips it otherwise.
	/// Returns the finished planes once every component has its DC data.
	fn parse_scan(&mut self) -> Result<Option<Planes>, ThumbError> {
		let _len = self.segment_len()?;
		let ns = self.cursor.next_u8()?;
		if ns == 0 || ns > 4 {
			return Err(err("malformed scan header"));
		}
		let mut raw_comps = Vec::with_capacity(ns as usize);
		for _ in 0..ns {
			let cs = self.cursor.next_u8()?;
			let td_ta = self.cursor.next_u8()?;
			raw_comps.push((cs, (td_ta >> 4) as usize));
		}
		let frame = self.frame.as_ref().ok_or_else(|| err("SOS before SOF2"))?;
		let mut scan_comps = Vec::with_capacity(raw_comps.len());
		for (cs, td) in raw_comps {
			let idx = frame
				.comps
				.iter()
				.position(|(id, _)| *id == cs)
				.ok_or_else(|| err("scan references an unknown component"))?;
			scan_comps.push((idx, td));
		}
		let ss = self.cursor.next_u8()?;
		let se = self.cursor.next_u8()?;
		let ah_al = self.cursor.next_u8()?;
		let (ah, al) = (ah_al >> 4, ah_al & 0x0F);

		if !(ss == 0 && se == 0 && ah == 0) {
			// AC scans and DC refinements: irrelevant to the preview. (Each
			// component's FIRST DC scan precedes its refinements, so exiting
			// once every component is covered never depends on these.)
			self.skip_entropy()?;
			return Ok(None);
		}
		if al > 13 {
			return Err(err("implausible successive-approximation shift"));
		}
		self.decode_dc_entropy(&scan_comps, al)?;

		if self
			.planes
			.as_ref()
			.is_some_and(|p| p.comps.iter().all(|c| c.done))
		{
			return Ok(self.planes.take());
		}
		Ok(None)
	}

	fn ensure_planes(&mut self) -> Result<(), ThumbError> {
		if self.planes.is_some() {
			return Ok(());
		}
		let frame = self.frame.as_ref().ok_or_else(|| err("no frame"))?;
		total_blocks(frame)?;
		let (mcus_x, mcus_y) = mcu_grid(frame);
		let mut comps = Vec::with_capacity(frame.comps.len());
		for (_, info) in &frame.comps {
			let plane_w = mcus_x * u32::from(info.h);
			let plane_h = mcus_y * u32::from(info.v);
			let q_dc = self.q_dc[info.tq as usize].ok_or_else(|| err("missing quant table"))?;
			comps.push(CompState {
				info: info.clone(),
				plane: vec![0; plane_w as usize * plane_h as usize],
				plane_w,
				plane_h,
				pred: 0,
				seq: 0,
				done: false,
				q_dc,
			});
		}
		self.planes = Some(Planes {
			width: frame.width,
			height: frame.height,
			hmax: frame.hmax,
			vmax: frame.vmax,
			comps,
		});
		Ok(())
	}

	fn skip_entropy(&mut self) -> Result<(), ThumbError> {
		// Entropy data ends at the first marker that is not a restart.
		loop {
			let byte = self.cursor.next_u8()?;
			if byte != 0xFF {
				continue;
			}
			match self.cursor.next_u8()? {
				0x00 | 0xD0..=0xD7 => {}
				0xFF => self.cursor.pos -= 1,
				_ => {
					self.cursor.pos -= 2;
					return Ok(());
				}
			}
		}
	}

	fn decode_dc_entropy(
		&mut self,
		scan_comps: &[(usize, usize)],
		al: u8,
	) -> Result<(), ThumbError> {
		self.ensure_planes()?;
		// Split borrows: tables and frame stay shared, planes and cursor are
		// exclusive — going through `self` for each would alias.
		let Walk {
			cursor,
			dc_tables,
			restart_interval,
			frame,
			planes,
			..
		} = self;
		let frame = frame.as_ref().ok_or_else(|| err("no frame"))?;
		let planes = planes.as_mut().ok_or_else(|| err("no planes"))?;
		let (mcus_x, mcus_y) = mcu_grid(frame);

		let mut tables = Vec::with_capacity(scan_comps.len());
		for (_, td) in scan_comps {
			tables.push(
				dc_tables
					.get(*td)
					.and_then(Option::as_ref)
					.ok_or_else(|| err("scan references a missing DC table"))?,
			);
		}
		for (idx, _) in scan_comps {
			planes.comps[*idx].pred = 0;
		}

		let interleaved = scan_comps.len() > 1;
		let (units_x, units_y) = if interleaved {
			(u64::from(mcus_x), u64::from(mcus_y))
		} else {
			// A non-interleaved scan covers the component's UNPADDED block
			// grid: ceil(dim * sampling / max_sampling / 8), in one step to
			// dodge nested-ceil pitfalls with sampling factors like 3-of-4.
			let info = &planes.comps[scan_comps[0].0].info;
			let bw =
				(u64::from(frame.width) * u64::from(info.h)).div_ceil(8 * u64::from(frame.hmax));
			let bh =
				(u64::from(frame.height) * u64::from(info.v)).div_ceil(8 * u64::from(frame.vmax));
			(bw.max(1), bh.max(1))
		};
		let total_units = units_x * units_y;

		let mut reader = BitReader::new(cursor);
		let mut since_restart = 0u32;
		for unit in 0..total_units {
			if *restart_interval > 0 && since_restart == *restart_interval {
				reader.expect_restart()?;
				for (idx, _) in scan_comps {
					planes.comps[*idx].pred = 0;
				}
				since_restart = 0;
			}
			for (slot, (idx, _)) in scan_comps.iter().enumerate() {
				let comp = &mut planes.comps[*idx];
				let blocks = if interleaved {
					u32::from(comp.info.h) * u32::from(comp.info.v)
				} else {
					1
				};
				for b in 0..blocks {
					let t = reader.decode_symbol(tables[slot])?;
					if t > 15 {
						return Err(err("DC category out of range"));
					}
					let diff = reader.receive_extend(t)?;
					comp.pred = comp
						.pred
						.checked_add(diff)
						.ok_or_else(|| err("DC predictor overflow"))?;
					// Widened before the shift: `pred` spans the full i32 range
					// and `al` reaches 13, so shifting in place silently drops
					// the high bits and emits garbage pixels. Saturating keeps
					// a hostile file's DC absurd-but-bounded, which the sample
					// clamp then flattens.
					let value = (i64::from(comp.pred) << al)
						.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32;
					let (bx, by) = if interleaved {
						let mcu_x = (unit % u64::from(mcus_x)) as u32;
						let mcu_y = (unit / u64::from(mcus_x)) as u32;
						(
							mcu_x * u32::from(comp.info.h) + b % u32::from(comp.info.h),
							mcu_y * u32::from(comp.info.v) + b / u32::from(comp.info.h),
						)
					} else {
						// seq keeps counting across repeated scans of one component, so a
						// hostile file can push the row past u32::MAX: saturating leaves
						// the block off the grid instead of wrapping it onto an earlier row.
						(
							(comp.seq % units_x) as u32,
							u32::try_from(comp.seq / units_x).unwrap_or(u32::MAX),
						)
					};
					if bx < comp.plane_w && by < comp.plane_h {
						comp.plane[(by * comp.plane_w + bx) as usize] = value;
					}
					if !interleaved {
						comp.seq += 1;
					}
				}
			}
			since_restart += 1;
		}
		for (idx, _) in scan_comps {
			planes.comps[*idx].done = true;
		}
		// If the reader already swallowed the scan-terminating marker while
		// filling, put it back for the walk; trailing pad bits just drop.
		if reader.pending_marker.take().is_some() {
			cursor.pos -= 2;
		}
		Ok(())
	}
}

impl BitReader<'_, '_> {
	/// At a restart boundary: drop pad bits, consume the RSTn marker whether
	/// the fill already swallowed it or it is still in the stream.
	fn expect_restart(&mut self) -> Result<(), ThumbError> {
		self.bits = 0;
		self.nbits = 0;
		let marker = match self.pending_marker.take() {
			Some(m) => m,
			None => {
				if self.cursor.next_u8()? != 0xFF {
					return Err(err("expected a restart marker"));
				}
				let mut m = self.cursor.next_u8()?;
				while m == 0xFF {
					m = self.cursor.next_u8()?;
				}
				m
			}
		};
		if !(0xD0..=0xD7).contains(&marker) {
			return Err(err("expected a restart marker"));
		}
		Ok(())
	}
}
