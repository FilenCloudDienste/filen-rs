use crate::ThumbError;

/// A decoded image small enough to hold whole — an embedded preview or the
/// finished accumulator canvas. Tightly packed RGBA8.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SmallImage {
	pub width: u32,
	pub height: u32,
	pub rgba: Vec<u8>,
}

/// Where decoders deliver pixels, in whatever geometry their format produces:
/// scanline rows, tiles, or one small whole image. Regions may arrive in any
/// order; pushing outside the dimensions the sink was constructed with is an
/// error — the one geometry failure mode left. Every source pixel must be
/// pushed exactly once: the box filter derives its weights from the
/// source→canvas mapping instead of storing a per-cell counter.
pub trait PixelSink {
	/// `rgba` is one or more complete rows of a `w`-wide region whose top-left
	/// corner is at (`x`, `y`) in the decoder's output coordinate space.
	fn push(&mut self, x: u32, y: u32, w: u32, rgba: &[u8]) -> Result<(), ThumbError>;
}

/// Box filter accumulating source pixels into a canvas at target resolution —
/// the ONLY full-image buffer in the pipeline, 16 bytes per canvas pixel
/// (u32 × RGBA sums; the divisor is recomputed from the mapping in
/// [`finish`](Self::finish), so no weight plane is stored). Source dims are
/// constructor state on purpose: there is no "dims not set yet" state to
/// misuse.
pub struct BoxAccumulator {
	src_w: u32,
	src_h: u32,
	dst_w: u32,
	dst_h: u32,
	/// dst_w × dst_h × 4 channel sums.
	acc: Vec<u32>,
}

/// What a canvas of `dims` costs, for the orchestrator's budget check: 20
/// bytes per canvas pixel.
///
/// 16 of those are the live accumulator (u32 × RGBA sums); the other 4 are the
/// packed RGBA that [`BoxAccumulator::finish`] allocates *while the
/// accumulator is still alive*. Charging only the accumulator understated the
/// real high-water mark by a quarter of the canvas, which is exactly the
/// margin a budget check exists to protect.
pub(crate) fn accumulator_bytes(dims: (u32, u32)) -> usize {
	dims.0 as usize * dims.1 as usize * 20
}

/// How many source coordinates in `0..src` map onto `cell` under the integer
/// mapping `s * dst / src`.
fn contributions(src: u32, dst: u32, cell: u32) -> u32 {
	// s maps to cell  ⇔  cell*src/dst ≤ s (rounded up) and s < (cell+1)*src/dst.
	let start = (u64::from(cell) * u64::from(src)).div_ceil(u64::from(dst));
	let end = ((u64::from(cell) + 1) * u64::from(src)).div_ceil(u64::from(dst));
	u32::try_from(end - start).unwrap_or(u32::MAX)
}

impl BoxAccumulator {
	/// Panics if either dimension is zero — both are derived from validated
	/// decoder headers before construction.
	pub fn new(source_dims: (u32, u32), target_dims: (u32, u32)) -> Self {
		let (src_w, src_h) = source_dims;
		let (dst_w, dst_h) = target_dims;
		assert!(src_w > 0 && src_h > 0 && dst_w > 0 && dst_h > 0);
		let px = dst_w as usize * dst_h as usize;
		BoxAccumulator {
			src_w,
			src_h,
			dst_w,
			dst_h,
			acc: vec![0; px * 4],
		}
	}

	/// Consumes the accumulator — there is no push-after-finish. Cells whose
	/// mapped source region was never pushed come out black; in-crate
	/// decoders always push full coverage.
	pub fn finish(self) -> SmallImage {
		let mut col_weights = Vec::with_capacity(self.dst_w as usize);
		for tx in 0..self.dst_w {
			col_weights.push(contributions(self.src_w, self.dst_w, tx));
		}
		let px = self.dst_w as usize * self.dst_h as usize;
		let mut rgba = vec![0u8; px * 4];
		for ty in 0..self.dst_h {
			let row_weight = u64::from(contributions(self.src_h, self.dst_h, ty));
			for (tx, col_weight) in col_weights.iter().enumerate() {
				let weight = row_weight * u64::from(*col_weight);
				if weight == 0 {
					continue;
				}
				let i = ty as usize * self.dst_w as usize + tx;
				for c in 0..4 {
					rgba[i * 4 + c] = (u64::from(self.acc[i * 4 + c]) / weight).min(255) as u8;
				}
			}
		}
		SmallImage {
			width: self.dst_w,
			height: self.dst_h,
			rgba,
		}
	}
}

impl PixelSink for BoxAccumulator {
	fn push(&mut self, x: u32, y: u32, w: u32, rgba: &[u8]) -> Result<(), ThumbError> {
		if w == 0 {
			return Ok(());
		}
		let row_bytes = w as usize * 4;
		if !rgba.len().is_multiple_of(row_bytes) {
			return Err(ThumbError::Geometry);
		}
		let rows = u32::try_from(rgba.len() / row_bytes).map_err(|_| ThumbError::Geometry)?;
		if x.checked_add(w).is_none_or(|end| end > self.src_w)
			|| y.checked_add(rows).is_none_or(|end| end > self.src_h)
		{
			return Err(ThumbError::Geometry);
		}
		for row in 0..rows {
			// Integer source→target mapping; u64 keeps 4-gigapixel inputs exact. The
			// bounds check above keeps every mapped cell inside dst_w × dst_h, so the
			// conversions to usize cannot fail.
			let ty = usize::try_from((y + row) as u64 * self.dst_h as u64 / self.src_h as u64)
				.map_err(|_| ThumbError::Geometry)?;
			let src_row = &rgba[row as usize * row_bytes..][..row_bytes];
			for col in 0..w as usize {
				let tx = usize::try_from(
					(x as u64 + col as u64) * self.dst_w as u64 / self.src_w as u64,
				)
				.map_err(|_| ThumbError::Geometry)?;
				let t = ty * self.dst_w as usize + tx;
				let s = col * 4;
				// Saturating: a pathological aspect ratio could route millions
				// of source pixels through one cell; clipping beats wrapping.
				for c in 0..4 {
					self.acc[t * 4 + c] =
						self.acc[t * 4 + c].saturating_add(u32::from(src_row[s + c]));
				}
			}
		}
		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use super::{BoxAccumulator, PixelSink, contributions};

	#[test]
	fn a_checkerboard_averages_to_grey() {
		let mut acc = BoxAccumulator::new((8, 8), (1, 1));
		for y in 0..8u32 {
			let mut row = Vec::new();
			for x in 0..8u32 {
				let v = if (x + y) % 2 == 0 { 0u8 } else { 255 };
				row.extend_from_slice(&[v, v, v, 255]);
			}
			acc.push(0, y, 8, &row).unwrap();
		}
		let img = acc.finish();
		assert_eq!(img.width, 1);
		assert_eq!(img.height, 1);
		// 32 black + 32 white pixels → 127 (integer division).
		assert_eq!(&img.rgba, &[127, 127, 127, 255]);
	}

	#[test]
	fn tiles_pushed_out_of_order_land_where_rows_would() {
		let solid = |v: u8, px: usize| vec![v; px * 4];
		// 4×4 source → 2×2 target; four 2×2 tiles pushed in reverse order.
		let mut acc = BoxAccumulator::new((4, 4), (2, 2));
		acc.push(2, 2, 2, &solid(40, 4)).unwrap();
		acc.push(0, 2, 2, &solid(30, 4)).unwrap();
		acc.push(2, 0, 2, &solid(20, 4)).unwrap();
		acc.push(0, 0, 2, &solid(10, 4)).unwrap();
		let img = acc.finish();
		assert_eq!(img.rgba[0], 10);
		assert_eq!(img.rgba[4], 20);
		assert_eq!(img.rgba[8], 30);
		assert_eq!(img.rgba[12], 40);
	}

	#[test]
	fn pushes_outside_the_source_bounds_error() {
		let mut acc = BoxAccumulator::new((4, 4), (2, 2));
		assert!(acc.push(3, 0, 2, &[0u8; 8]).is_err());
		assert!(acc.push(0, 4, 1, &[0u8; 4]).is_err());
		// A byte count that is not whole rows is rejected too.
		assert!(acc.push(0, 0, 2, &[0u8; 7]).is_err());
	}

	#[test]
	fn derived_weights_partition_the_source_axis() {
		// Every source coordinate lands in exactly one cell, for awkward
		// ratios too — the divisor derivation depends on it.
		for (src, dst) in [(7u32, 3u32), (8, 8), (100, 7), (5, 4), (4096, 1600)] {
			let total: u32 = (0..dst).map(|cell| contributions(src, dst, cell)).sum();
			assert_eq!(total, src, "src={src} dst={dst}");
		}
	}

	#[test]
	fn uneven_ratios_average_with_true_per_cell_weights() {
		// 3 source columns → 2 target columns: cell 0 gets 2 columns, cell 1
		// gets 1. A wrong uniform divisor would darken cell 1.
		let mut acc = BoxAccumulator::new((3, 1), (2, 1));
		acc.push(
			0,
			0,
			3,
			&[100, 100, 100, 255, 200, 200, 200, 255, 60, 60, 60, 255],
		)
		.unwrap();
		let img = acc.finish();
		assert_eq!(img.rgba[0], 150); // (100+200)/2
		assert_eq!(img.rgba[4], 60); // 60/1
	}
}
