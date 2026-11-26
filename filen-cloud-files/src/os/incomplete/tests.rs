use std::cell::RefCell;

use super::*;
use crate::os::OsError;

/// What a test copy came to: what it copied, the copy's bytes, and every read's offset and length.
type Outcome = (Result<Copied, CopyFailure>, Vec<u8>, Vec<(u64, u64)>);

/// The HRESULT of `ERROR_CLOUD_FILE_REQUEST_TIMEOUT` (426), what a read waiting on a fetch the
/// provider never serves fails with at the platform's limit.
const TIMED_OUT: i32 = 0x8007_01AA_u32.cast_signed();

fn byte(offset: u64) -> u8 {
	u8::try_from(offset % 251).unwrap()
}

fn range(offset: u64, len: u64) -> ByteRange {
	ByteRange { offset, len }
}

fn never() -> bool {
	false
}

/// A file of `size` bytes, each byte its offset's low byte, of which `held` ranges are on disk,
/// copied by the ranges `reported` into a buffer, with `on_disk` the count the platform gives:
/// a read of a byte not on disk waits out the platform's limit and fails, as every read of a file
/// left with a fetch nobody serves does. What it copied, the buffer and every read's range.
fn copy_held(
	size: u64,
	held: &[ByteRange],
	reported: &[ByteRange],
	on_disk: u64,
	stop: &dyn Fn() -> bool,
) -> Outcome {
	let reads = RefCell::new(Vec::new());
	let out = RefCell::new(vec![0_u8; usize::try_from(size).unwrap()]);
	let is_held = |offset: u64| {
		held.iter()
			.any(|range| offset >= range.offset && offset < range.offset + range.len)
	};
	let copied = copy_ranges(
		size,
		reported,
		on_disk,
		|offset, buffer: &mut [u8]| {
			let len = u64::try_from(buffer.len()).unwrap();
			reads.borrow_mut().push((offset, len));
			if !(offset..offset + len).all(is_held) {
				return Err(LocalError::Os(OsError::Rejected { code: TIMED_OUT }));
			}
			for (at, slot) in (offset..).zip(buffer.iter_mut()) {
				*slot = byte(at);
			}
			Ok(())
		},
		|offset, bytes| {
			let start = usize::try_from(offset).unwrap();
			out.borrow_mut()[start..start + bytes.len()].copy_from_slice(bytes);
			Ok(())
		},
		stop,
	);
	(copied, out.into_inner(), reads.into_inner())
}

/// [`copy_held`] of a file whose ranges are reported as they are.
fn copy(size: u64, on_disk: &[ByteRange]) -> Outcome {
	let total = on_disk.iter().map(|range| range.len).sum();
	copy_held(size, on_disk, on_disk, total, &never)
}

#[test]
fn a_file_all_on_disk_is_copied_whole_one_window_at_a_time() {
	let size = 2 * WINDOW + 10;
	let (copied, out, reads) = copy(size, &[range(0, size)]);
	let copied = copied.unwrap();
	assert_eq!(copied.on_disk, size);
	assert!(copied.zeroed.is_empty());
	assert_eq!(reads, [(0, WINDOW), (WINDOW, WINDOW), (2 * WINDOW, 10)]);
	assert!((0..size).all(|at| out[usize::try_from(at).unwrap()] == byte(at)));
}

#[test]
fn only_the_ranges_on_disk_are_read_and_every_other_byte_is_zero() {
	let size = 3 * WINDOW;
	let on_disk = [range(0, 69_632), range(WINDOW + 8192, 4096)];
	let (copied, out, reads) = copy(size, &on_disk);
	let copied = copied.unwrap();
	assert_eq!(copied.on_disk, 69_632 + 4096);
	assert_eq!(
		copied.zeroed,
		[
			range(69_632, WINDOW + 8192 - 69_632),
			range(WINDOW + 12_288, 2 * WINDOW - 12_288)
		]
	);
	assert_eq!(reads, [(0, 69_632), (WINDOW + 8192, 4096)]);
	for at in 0..size {
		let want = if on_disk
			.iter()
			.any(|range| at >= range.offset && at < range.offset + range.len)
		{
			byte(at)
		} else {
			0
		};
		assert_eq!(out[usize::try_from(at).unwrap()], want, "byte {at}");
	}
}

#[test]
fn a_file_whose_every_read_of_a_missing_range_waits_out_the_platform_is_copied() {
	// The tail is not on disk, and a read of it would wait for a fetch nobody serves.
	let size = 64 * WINDOW;
	let on_disk = [range(0, 32 * WINDOW)];
	let (copied, out, reads) = copy(size, &on_disk);
	let copied = copied.unwrap();
	assert_eq!(copied.zeroed, [range(32 * WINDOW, 32 * WINDOW)]);
	assert!(
		reads
			.iter()
			.all(|(offset, len)| offset + len <= 32 * WINDOW)
	);
	assert_eq!(out[usize::try_from(32 * WINDOW).unwrap()], 0);
}

#[test]
fn many_small_ranges_are_each_read_whole_and_the_gaps_listed() {
	let size = 64 * 4096;
	let on_disk: Vec<ByteRange> = (0..32).map(|n| range(n * 8192 + 4096, 4096)).collect();
	let (copied, _, reads) = copy(size, &on_disk);
	let copied = copied.unwrap();
	assert_eq!(copied.on_disk, 32 * 4096);
	assert_eq!(reads.len(), 32);
	let zeroed: Vec<ByteRange> = (0..32).map(|n| range(n * 8192, 4096)).collect();
	assert_eq!(copied.zeroed, zeroed);
}

#[test]
fn a_range_reported_on_disk_that_cannot_be_read_ends_the_copy() {
	let size = 4 * WINDOW;
	let held = [range(0, WINDOW)];
	let reported = [range(0, 2 * WINDOW)];
	let (copied, _, _) = copy_held(size, &held, &reported, 2 * WINDOW, &never);
	assert_eq!(
		copied,
		Err(CopyFailure::Local(LocalError::Os(OsError::Rejected {
			code: TIMED_OUT
		})))
	);
}

#[test]
fn ranges_adding_up_to_another_count_than_on_disk_are_refused_before_any_read() {
	let on_disk = [range(0, 8192)];
	let (copied, _, reads) = copy_held(WINDOW, &on_disk, &on_disk, 4096, &never);
	assert_eq!(
		copied,
		Err(CopyFailure::Mismatch {
			copied: 8192,
			on_disk: 4096
		})
	);
	assert!(reads.is_empty());
}

#[test]
fn ranges_out_of_order_overlapping_empty_or_past_the_end_are_refused_before_any_read() {
	for bad in [
		vec![range(8192, 4096), range(0, 4096)],
		vec![range(0, 8192), range(4096, 4096)],
		vec![range(0, 0)],
		vec![range(WINDOW - 4096, 8192)],
		vec![range(u64::MAX - 1, 2)],
	] {
		let total = bad
			.iter()
			.fold(0_u64, |sum, range| sum.saturating_add(range.len));
		let (copied, _, reads) = copy_held(WINDOW, &bad, &bad, total, &never);
		assert_eq!(copied, Err(CopyFailure::Ranges), "{bad:?}");
		assert!(reads.is_empty(), "{bad:?}");
	}
}

#[test]
fn a_file_with_nothing_on_disk_is_all_zeros_with_no_read() {
	let (copied, out, reads) = copy(3 * 4096, &[]);
	let copied = copied.unwrap();
	assert_eq!(copied.on_disk, 0);
	assert_eq!(copied.zeroed, [range(0, 3 * 4096)]);
	assert!(reads.is_empty());
	assert!(out.iter().all(|byte| *byte == 0));
}

#[test]
fn a_stop_ends_the_copy_before_the_next_window() {
	let asked = RefCell::new(0);
	let stop = || {
		*asked.borrow_mut() += 1;
		*asked.borrow() > 1
	};
	let on_disk = [range(0, 3 * WINDOW)];
	let (copied, _, reads) = copy_held(3 * WINDOW, &on_disk, &on_disk, 3 * WINDOW, &stop);
	assert_eq!(copied, Err(CopyFailure::Stopped));
	assert_eq!(reads, [(0, WINDOW)]);
}

/// Reads `out` as the copy.
fn reader(out: &[u8]) -> impl FnMut(u64, &mut [u8]) -> Result<(), LocalError> + '_ {
	move |offset, buffer| {
		let start = usize::try_from(offset).unwrap();
		buffer.copy_from_slice(&out[start..start + buffer.len()]);
		Ok(())
	}
}

#[test]
fn the_copy_reads_back_only_when_its_ranges_hold_the_bytes_read() {
	let size = 2 * WINDOW;
	let on_disk = [range(4096, 8192), range(WINDOW, 4096)];
	let (copied, out, _) = copy(size, &on_disk);
	let copied = copied.unwrap();
	assert_eq!(check_read_back(&on_disk, &copied, reader(&out)), Ok(()));
	let mut damaged = out.clone();
	damaged[usize::try_from(WINDOW).unwrap()] ^= 1;
	assert_eq!(
		check_read_back(&on_disk, &copied, reader(&damaged)),
		Err(CopyFailure::ReadBack)
	);
}

#[test]
fn the_zeroed_ranges_are_what_the_ranges_on_disk_leave() {
	assert_eq!(zeroed_ranges(10, &[]), [range(0, 10)]);
	assert_eq!(zeroed_ranges(10, &[range(0, 10)]), []);
	assert_eq!(
		zeroed_ranges(10, &[range(2, 3), range(7, 1)]),
		[range(0, 2), range(5, 2), range(8, 2)]
	);
}
