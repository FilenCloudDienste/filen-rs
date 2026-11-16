use std::{ops::Range, time::Duration};

use filen_mobile_native_cache::{CacheError, abort::FfiAbortController, range::RangeReadError};
use filen_types::fs::Uuid;
use tokio::{join, time::timeout};

use super::{
	HydrateError, HydrateFailed, HydrateReport, SinkError, Verification, hydrate,
	plan::{ChunkError, PlanError, TRANSFER_ALIGN, TransferWindow, plan},
};
use crate::{
	failure::Failure,
	identity::PlaceholderIdentity,
	os::{OptionalRange, OsError, RequestedRange, TransferKey},
	placeholder::NegativeSize,
	test_support::{FakeDrive, FakeEvent, FakeOs, pattern, place_files},
};

/// The 5 MiB file the measured random and partial reads were made on.
const FIVE_MIB: i64 = 5_242_880;

/// The window planned for a request with only a required range.
fn window(size: i64, offset: i64, len: i64) -> TransferWindow {
	plan(RequestedRange::new(size, offset, len, None)).unwrap()
}

fn bounds(window: TransferWindow) -> (u64, u64) {
	(window.byte_range().offset(), window.byte_range().end())
}

/// Whether `plan`'s window is the whole file, which the cache's read checks against the file's
/// declared hash.
fn whole_file(plan: TransferWindow) -> bool {
	bounds(plan) == (0, plan.file_size().get())
}

/// The `(start, end, whole_file)` planned for `required` and `optional` (offset, length, as cfapi
/// sends them: length 0 for no optional range) of a file of `size` bytes.
fn planned(size: i64, required: (i64, i64), optional: (i64, i64)) -> (u64, u64, bool) {
	let plan = plan(RequestedRange::new(
		size,
		required.0,
		required.1,
		OptionalRange::new(optional.0, optional.1),
	))
	.unwrap();
	(
		plan.byte_range().offset(),
		plan.byte_range().end(),
		whole_file(plan),
	)
}

#[test]
fn random_read_at_3_000_000_widens_to_its_chunk_end() {
	let plan = plan(RequestedRange::new(
		FIVE_MIB,
		2_998_272,
		4096,
		OptionalRange::new(3_002_368, 2_240_512),
	))
	.unwrap();
	assert_eq!(bounds(plan), (2_998_272, 3_145_728));
	assert!(!whole_file(plan));
}

#[test]
fn the_next_random_read_starts_where_the_first_stopped() {
	let plan = plan(RequestedRange::new(
		FIVE_MIB,
		3_002_368,
		4096,
		OptionalRange::new(3_006_464, 2_236_416),
	))
	.unwrap();
	assert_eq!(bounds(plan), (3_002_368, 3_145_728));
}

#[test]
fn a_powershell_read_without_optional_is_kept_as_asked() {
	assert_eq!(
		bounds(window(FIVE_MIB, 999_424, 8192)),
		(999_424, 1_007_616)
	);
}

#[test]
fn a_powershell_follow_up_is_kept_as_asked() {
	assert_eq!(
		bounds(window(FIVE_MIB, 1_007_616, 4096)),
		(1_007_616, 1_011_712)
	);
}

#[test]
fn a_full_read_after_partial_reads_fetches_the_first_gap() {
	let plan = plan(RequestedRange::new(FIVE_MIB, 0, 999_424, None)).unwrap();
	assert_eq!(bounds(plan), (0, 999_424));
	assert!(!whole_file(plan));
}

#[test]
fn a_full_read_after_partial_reads_fetches_the_tail_to_eof() {
	assert_eq!(
		bounds(window(FIVE_MIB, 1_011_712, 4_231_168)),
		(1_011_712, 5_242_880)
	);
}

#[test]
fn a_single_reader_full_read_is_the_whole_file() {
	let plan = plan(RequestedRange::new(300_000, 0, 300_000, None)).unwrap();
	assert_eq!(bounds(plan), (0, 300_000));
	assert!(whole_file(plan));
}

#[test]
fn cmd_type_reads_4096_4096_1808_without_optional() {
	let plans = [(0, 4096), (4096, 4096), (8192, 1808)]
		.map(|(offset, len)| plan(RequestedRange::new(10_000, offset, len, None)).unwrap());
	assert_eq!(
		plans.map(|plan| (bounds(plan), whole_file(plan))),
		[
			((0, 4096), false),
			((4096, 8192), false),
			((8192, 10_000), false)
		]
	);
}

#[test]
fn cmd_type_first_read_with_optional_widens_to_the_whole_file() {
	let plan = plan(RequestedRange::new(
		10_000,
		0,
		4096,
		OptionalRange::new(4096, 5904),
	))
	.unwrap();
	assert_eq!(bounds(plan), (0, 10_000));
	assert!(whole_file(plan));
}

#[test]
fn the_self_access_anomaly_is_clamped_to_the_file() {
	let plan = plan(RequestedRange::new(10_000, -4096, 8192, None)).unwrap();
	assert_eq!(bounds(plan), (0, 4096));
	assert!(!whole_file(plan));
}

#[test]
fn a_negative_file_size_is_refused() {
	assert_eq!(
		plan(RequestedRange::new(-1, 0, 4096, None)),
		Err(PlanError::Size(NegativeSize { size: -1 }))
	);
}

#[test]
fn an_empty_file_is_refused() {
	assert_eq!(
		plan(RequestedRange::new(0, 0, 4096, None)),
		Err(PlanError::EmptyFile)
	);
}

#[test]
fn a_zero_or_negative_required_length_is_refused() {
	assert_eq!(
		plan(RequestedRange::new(10_000, 0, 0, None)),
		Err(PlanError::NothingRequired { offset: 0, len: 0 })
	);
	assert_eq!(
		plan(RequestedRange::new(10_000, 4096, -1, None)),
		Err(PlanError::NothingRequired {
			offset: 4096,
			len: -1
		})
	);
}

#[test]
fn a_range_entirely_before_zero_is_refused() {
	assert_eq!(
		plan(RequestedRange::new(10_000, -8192, 8192, None)),
		Err(PlanError::NothingRequired {
			offset: -8192,
			len: 8192
		})
	);
}

#[test]
fn an_overflowing_required_end_is_refused() {
	assert_eq!(
		plan(RequestedRange::new(i64::MAX, i64::MAX, 1, None)),
		Err(PlanError::Overflow {
			offset: i64::MAX,
			len: 1
		})
	);
}

#[test]
fn a_request_starting_at_eof_is_past_end() {
	assert_eq!(
		plan(RequestedRange::new(10_000, 10_000, 4096, None)),
		Err(PlanError::PastEnd {
			offset: 10_000,
			size: 10_000
		})
	);
}

#[test]
fn an_unaligned_start_rounds_down() {
	assert_eq!(bounds(window(FIVE_MIB, 5000, 3192)), (4096, 8192));
}

#[test]
fn an_unaligned_end_mid_file_rounds_up() {
	assert_eq!(bounds(window(FIVE_MIB, 0, 5000)), (0, 8192));
}

#[test]
fn an_end_rounding_past_eof_stops_at_eof() {
	assert_eq!(bounds(window(10_000, 8192, 100)), (8192, 10_000));
}

#[test]
fn a_required_range_past_eof_is_clamped_to_eof() {
	assert_eq!(bounds(window(10_000, 8192, 8192)), (8192, 10_000));
}

#[test]
fn an_optional_range_not_touching_the_required_one_is_ignored() {
	let plan = plan(RequestedRange::new(
		FIVE_MIB,
		0,
		4096,
		OptionalRange::new(8192, 4096),
	))
	.unwrap();
	assert_eq!(bounds(plan), (0, 4096));
}

#[test]
fn an_unaligned_optional_end_mid_file_is_aligned_down() {
	let plan = plan(RequestedRange::new(
		FIVE_MIB,
		0,
		4096,
		OptionalRange::new(4096, 10_000),
	))
	.unwrap();
	assert_eq!(bounds(plan), (0, 12_288));
}

#[test]
fn a_required_end_on_a_chunk_boundary_is_not_widened() {
	let plan = plan(RequestedRange::new(
		FIVE_MIB,
		1_044_480,
		4096,
		OptionalRange::new(1_048_576, 4_194_304),
	))
	.unwrap();
	assert_eq!(bounds(plan), (1_044_480, 1_048_576));
}

#[test]
fn the_window_is_what_the_cache_is_asked_to_read() {
	let range = window(FIVE_MIB, 999_424, 8192).byte_range();
	assert_eq!(
		(range.offset(), range.len().get(), range.end()),
		(999_424, 8192, 1_007_616)
	);
}

#[test]
fn the_window_carries_the_file_size() {
	assert_eq!(window(10_000, 0, 4096).file_size().get(), 10_000);
}

#[test]
fn a_chunk_at_the_window_end_is_accepted_and_one_byte_past_refused() {
	let bytes = pattern(1809);
	let window = window(10_000, 8192, 1808);
	let chunk = window.chunk(8192, &bytes[..1808]).unwrap();
	assert_eq!((chunk.offset(), chunk.bytes().len()), (8192, 1808));
	assert_eq!(
		window.chunk(8192, &bytes).unwrap_err(),
		ChunkError::OutsideWindow {
			offset: 8192,
			len: 1809
		}
	);
}

#[test]
fn a_chunk_before_the_window_start_is_refused() {
	let window = window(FIVE_MIB, 999_424, 8192);
	assert_eq!(
		window.chunk(995_328, &[0; 4096]).unwrap_err(),
		ChunkError::OutsideWindow {
			offset: 995_328,
			len: 4096
		}
	);
}

#[test]
fn a_misaligned_chunk_is_refused() {
	let window = window(FIVE_MIB, 0, 8192);
	assert_eq!(
		window.chunk(100, &[0; 4096]).unwrap_err(),
		ChunkError::Misaligned { offset: 100 }
	);
}

#[test]
fn an_unaligned_tail_before_eof_is_refused() {
	let window = window(FIVE_MIB, 0, 8192);
	assert_eq!(
		window.chunk(4096, &[0; 100]).unwrap_err(),
		ChunkError::UnalignedTail { end: 4196 }
	);
}

#[test]
fn an_empty_chunk_is_refused() {
	let window = window(FIVE_MIB, 0, 8192);
	assert_eq!(window.chunk(0, &[]).unwrap_err(), ChunkError::Empty);
}

#[test]
fn the_transfer_alignment_is_4096_bytes() {
	assert_eq!(TRANSFER_ALIGN, 4096);
}

#[test]
fn files_around_one_alignment_unit_plan_literal_windows() {
	let cases = [
		(1, (0, 1), (0, 1, true)),
		(4095, (0, 4095), (0, 4095, true)),
		(4096, (0, 4096), (0, 4096, true)),
		(4097, (0, 4096), (0, 4096, false)),
		(4097, (0, 4097), (0, 4097, true)),
		(4097, (4096, 1), (4096, 4097, false)),
		(4097, (4095, 2), (0, 4097, true)),
	];
	for (size, required, expected) in cases {
		assert_eq!(
			planned(size, required, (0, 0)),
			expected,
			"{size} bytes, {required:?}"
		);
	}
}

#[test]
fn required_ranges_around_eof_plan_literal_windows() {
	let cases = [
		// Ending one byte short of EOF, at EOF and one byte past it.
		((8192, 1807), (8192, 10_000, false)),
		((8192, 1808), (8192, 10_000, false)),
		((8192, 1809), (8192, 10_000, false)),
		// Starting one byte before EOF.
		((9999, 1), (8192, 10_000, false)),
		((0, 9999), (0, 10_000, true)),
		((0, 10_001), (0, 10_000, true)),
	];
	for (required, expected) in cases {
		assert_eq!(planned(10_000, required, (0, 0)), expected, "{required:?}");
	}
}

#[test]
fn the_last_byte_of_the_largest_file_plans_without_overflow() {
	let expected = (9_223_372_036_854_771_712, 9_223_372_036_854_775_807, false);
	assert_eq!(planned(i64::MAX, (i64::MAX - 1, 1), (0, 0)), expected);
	assert_eq!(
		planned(i64::MAX, (i64::MAX - 1, 1), (i64::MAX - 1, 1)),
		expected
	);
}

#[test]
fn optional_ranges_plan_literal_windows() {
	let cases = [
		// The optional range's end overflows.
		((0, 4096), (4096, i64::MAX), (0, 4096, false)),
		// A negative start, ending past the window.
		((0, 4096), (-4096, 20_480), (0, 16_384, false)),
		// A negative start, ending before 0.
		((0, 4096), (-8192, 4096), (0, 4096, false)),
		// Starting exactly at the window's end, and one byte after it.
		((0, 4096), (4096, 8192), (0, 12_288, false)),
		((0, 4096), (4097, 8192), (0, 4096, false)),
		// An unaligned end past the chunk the required range ends in stops at that chunk's end.
		(
			(1_040_384, 4096),
			(1_044_480, 10_000),
			(1_040_384, 1_048_576, false),
		),
	];
	for (required, optional, expected) in cases {
		assert_eq!(
			planned(FIVE_MIB, required, optional),
			expected,
			"{required:?} {optional:?}"
		);
	}
}

#[test]
fn an_optional_range_past_eof_stops_at_eof() {
	assert_eq!(
		planned(10_000, (0, 4096), (4096, 100_000)),
		(0, 10_000, true)
	);
}

const ROOT_UUID: Uuid = Uuid::from_u128(0x100);
const ROOT: PlaceholderIdentity = PlaceholderIdentity::Root { root: ROOT_UUID };
const STABLE: Uuid = Uuid::from_u128(0x200);
const VERSION: Uuid = Uuid::from_u128(0x201);
const FILE: PlaceholderIdentity = PlaceholderIdentity::File {
	stable: STABLE,
	version: VERSION,
};

/// A drive holding `FILE` as `size` bytes of the pattern, and an OS holding its placeholder.
fn placed(size: usize) -> (FakeDrive, FakeOs, Vec<u8>) {
	let bytes = pattern(size);
	let drive = FakeDrive::new(ROOT_UUID);
	drive.add_file(STABLE, VERSION, &bytes);
	let os = FakeOs::with_root(ROOT);
	place_files(
		&os,
		&ROOT,
		&[(STABLE, VERSION, i64::try_from(size).unwrap())],
	);
	(drive, os, bytes)
}

/// Hydrates `required` and `optional` (offset, length) of `FILE`.
async fn run(
	drive: &FakeDrive,
	os: &FakeOs,
	required: (i64, i64),
	optional: (i64, i64),
) -> (TransferKey, Result<HydrateReport, HydrateFailed>) {
	let (request, responder) = os.fetch_data(&FILE, required, optional);
	let key = request.transfer();
	let controller = FfiAbortController::new();
	let result = hydrate(drive, request, responder, &controller.signal()).await;
	(key, result)
}

/// The events after the population that placed the file.
fn events(os: &FakeOs) -> Vec<FakeEvent> {
	os.events().split_off(1)
}

/// `[start, end)`.
fn span(start: u64, end: u64) -> Range<u64> {
	start..end
}

fn transferred(key: TransferKey, offset: u64, len: u64) -> FakeEvent {
	FakeEvent::Transferred { key, offset, len }
}

#[tokio::test(start_paused = true)]
async fn a_whole_small_file_is_hash_checked_and_byte_equal() {
	let (drive, os, bytes) = placed(300_000);
	let (key, result) = run(&drive, &os, (0, 300_000), (0, 0)).await;
	assert_eq!(result.unwrap().verification, Verification::HashChecked);
	assert_eq!(
		events(&os),
		[transferred(key, 0, 300_000), FakeEvent::Completed(key)]
	);
	assert_eq!(os.contents(&FILE), (bytes, vec![span(0, 300_000)]));
	os.assert_clean();
}

#[tokio::test(start_paused = true)]
async fn a_read_at_3_000_000_hydrates_exactly_its_window() {
	let (drive, os, bytes) = placed(5_242_880);
	let (key, result) = run(&drive, &os, (2_998_272, 4096), (3_002_368, 2_240_512)).await;
	assert_eq!(result.unwrap().verification, Verification::Unchecked);
	assert_eq!(drive.reads(), [(2_998_272, 3_145_728)]);
	assert_eq!(
		events(&os),
		[
			transferred(key, 2_998_272, 147_456),
			FakeEvent::Completed(key)
		]
	);
	let (data, hydrated) = os.contents(&FILE);
	assert_eq!(hydrated, [span(2_998_272, 3_145_728)]);
	assert_eq!(
		data.get(2_998_272..3_145_728),
		bytes.get(2_998_272..3_145_728)
	);
	os.assert_clean();
}

#[tokio::test(start_paused = true)]
async fn a_three_chunk_window_transfers_three_aligned_pieces() {
	let (drive, os, _) = placed(5_242_880);
	let (key, result) = run(&drive, &os, (1_044_480, 2_101_248), (0, 0)).await;
	result.unwrap();
	assert_eq!(
		events(&os),
		[
			transferred(key, 1_044_480, 4096),
			transferred(key, 1_048_576, 1_048_576),
			transferred(key, 2_097_152, 1_048_576),
			FakeEvent::Completed(key)
		]
	);
	os.assert_clean();
}

#[tokio::test(start_paused = true)]
async fn a_whole_file_mismatch_fails_damaged_and_keeps_what_was_transferred() {
	let (drive, os, _) = placed(2_100_000);
	drive.declare(STABLE, Some(*blake3::hash(b"other").as_bytes()));
	let (key, result) = run(&drive, &os, (0, 2_100_000), (0, 0)).await;
	let failed = result.unwrap_err();
	assert!(
		matches!(
			failed.error,
			HydrateError::Read(RangeReadError::Cache(CacheError::ContentHashMismatch(_)))
		),
		"{failed:?}"
	);
	assert_eq!(failed.transferred, 2_097_152);
	assert_eq!(
		events(&os),
		[
			transferred(key, 0, 1_048_576),
			transferred(key, 1_048_576, 1_048_576),
			FakeEvent::Failed(key, Failure::ContentDamaged),
		]
	);
	// The hydration job dehydrates the file once the dispatcher records the version damaged.
	assert_eq!(os.contents(&FILE).1, vec![span(0, 2_097_152)]);
	os.assert_clean();
}

#[tokio::test(start_paused = true)]
async fn a_new_version_is_stale_with_nothing_transferred() {
	let (drive, os, _) = placed(10_000);
	drive.bump_version(STABLE, Uuid::from_u128(0x202));
	let (key, result) = run(&drive, &os, (0, 4096), (0, 0)).await;
	let failed = result.unwrap_err();
	assert!(
		matches!(
			failed.error,
			HydrateError::Read(RangeReadError::Sink(SinkError::VersionChanged {
				placeholder: VERSION,
				current
			})) if current == Uuid::from_u128(0x202)
		),
		"{failed:?}"
	);
	assert_eq!(failed.transferred, 0);
	assert_eq!(
		events(&os),
		[FakeEvent::Failed(key, Failure::StalePlaceholder)]
	);
	os.assert_clean();
}

#[tokio::test(start_paused = true)]
async fn a_changed_size_is_stale_with_nothing_transferred() {
	let (drive, os, _) = placed(10_000);
	drive.set_size(STABLE, 9000);
	let (key, result) = run(&drive, &os, (0, 4096), (0, 0)).await;
	let failed = result.unwrap_err();
	assert!(
		matches!(
			failed.error,
			HydrateError::Read(RangeReadError::Sink(SinkError::SizeChanged {
				placeholder: 10_000,
				current: 9000
			}))
		),
		"{failed:?}"
	);
	assert_eq!(
		events(&os),
		[FakeEvent::Failed(key, Failure::StalePlaceholder)]
	);
	os.assert_clean();
}

#[tokio::test(start_paused = true)]
async fn a_truncated_file_is_damaged_without_dehydrating() {
	let (drive, os, bytes) = placed(2_100_000);
	drive.truncate_at(STABLE, 1_048_576);
	let (key, result) = run(&drive, &os, (0, 2_100_000), (0, 0)).await;
	let failed = result.unwrap_err();
	assert!(
		matches!(
			failed.error,
			HydrateError::Read(RangeReadError::Truncated {
				reached: 1_048_576,
				end: 2_100_000,
				..
			})
		),
		"{failed:?}"
	);
	assert_eq!(
		events(&os),
		[
			transferred(key, 0, 1_048_576),
			FakeEvent::Failed(key, Failure::ContentDamaged)
		]
	);
	let (data, hydrated) = os.contents(&FILE);
	assert_eq!(hydrated, [span(0, 1_048_576)]);
	assert_eq!(data.get(..1_048_576), bytes.get(..1_048_576));
	os.assert_clean();
}

#[tokio::test(start_paused = true)]
async fn an_os_cancel_mid_read_stops_the_read() {
	let (drive, os, _) = placed(10_000);
	let gate = drive.gate_reads();
	let (request, responder) = os.fetch_data(&FILE, (0, 10_000), (0, 0));
	let key = request.transfer();
	let controller = FfiAbortController::new();
	let abort = controller.signal();
	let (result, ()) = join!(hydrate(&drive, request, responder, &abort), async {
		drive.reads_started(1).await;
		os.cancel(key, 0, 10_000);
		gate.add_permits(1);
	});
	let failed = result.unwrap_err();
	assert!(
		matches!(
			failed.error,
			HydrateError::Read(RangeReadError::Sink(SinkError::Os(OsError::Cancelled)))
		),
		"{failed:?}"
	);
	assert_eq!(events(&os), [FakeEvent::Cancelled(key)]);
	os.assert_clean();
}

#[tokio::test(start_paused = true)]
async fn an_aborted_read_is_cancelled() {
	let (drive, os, _) = placed(10_000);
	let (request, responder) = os.fetch_data(&FILE, (0, 10_000), (0, 0));
	let key = request.transfer();
	let controller = FfiAbortController::new();
	controller.abort();
	let result = hydrate(&drive, request, responder, &controller.signal()).await;
	assert!(matches!(
		result.unwrap_err().error,
		HydrateError::Read(RangeReadError::Cache(CacheError::Aborted(_)))
	),);
	assert_eq!(events(&os), [FakeEvent::Cancelled(key)]);
	os.assert_clean();
}

#[tokio::test(start_paused = true)]
async fn an_os_rejection_is_transient() {
	let (drive, os, _) = placed(10_000);
	os.fail_next_transfer(OsError::Rejected { code: 7 });
	let (key, result) = run(&drive, &os, (0, 10_000), (0, 0)).await;
	assert!(matches!(
		result.unwrap_err().error,
		HydrateError::Read(RangeReadError::Sink(SinkError::Os(OsError::Rejected {
			code: 7
		})))
	));
	assert_eq!(
		events(&os),
		[
			FakeEvent::RejectedTransfer {
				key,
				offset: 0,
				len: 10_000,
				why: "refused by a switch"
			},
			FakeEvent::Failed(key, Failure::Transient)
		]
	);
}

#[tokio::test(start_paused = true)]
async fn an_offline_read_is_unreachable() {
	let (drive, os, _) = placed(10_000);
	drive.fail_read(CacheError::Remote("offline".into()));
	let (key, result) = run(&drive, &os, (0, 10_000), (0, 0)).await;
	assert!(matches!(
		result.unwrap_err().error,
		HydrateError::Read(RangeReadError::Cache(CacheError::Remote(_)))
	));
	assert_eq!(events(&os), [FakeEvent::Failed(key, Failure::Unreachable)]);
	os.assert_clean();
}

#[tokio::test(start_paused = true)]
async fn a_read_while_the_drive_is_offline_is_unreachable() {
	let (drive, os, _) = placed(10_000);
	drive.set_offline(true);
	let (key, result) = run(&drive, &os, (0, 10_000), (0, 0)).await;
	assert!(matches!(
		result.unwrap_err().error,
		HydrateError::Read(RangeReadError::Cache(CacheError::Remote(_)))
	));
	assert_eq!(events(&os), [FakeEvent::Failed(key, Failure::Unreachable)]);
	os.assert_clean();
}

#[tokio::test(start_paused = true)]
async fn a_directory_identity_is_an_invalid_request() {
	let (drive, os, _) = placed(10_000);
	let directory = PlaceholderIdentity::Dir {
		uuid: Uuid::from_u128(0x300),
	};
	let (request, responder) = os.fetch_data_raw(
		directory.encode(),
		RequestedRange::new(10_000, 0, 4096, None),
	);
	let key = request.transfer();
	let controller = FfiAbortController::new();
	let result = hydrate(&drive, request, responder, &controller.signal()).await;
	assert!(matches!(result.unwrap_err().error, HydrateError::NotAFile));
	assert_eq!(drive.reads(), []);
	assert_eq!(
		events(&os),
		[FakeEvent::Failed(key, Failure::InvalidRequest)]
	);
	os.assert_clean();
}

#[tokio::test(start_paused = true)]
async fn a_request_past_eof_is_stale() {
	let (drive, os, _) = placed(10_000);
	drive.add_file(STABLE, VERSION, &pattern(4096));
	let (key, result) = run(&drive, &os, (8192, 1808), (0, 0)).await;
	assert!(matches!(
		result.unwrap_err().error,
		HydrateError::Read(RangeReadError::PastEnd {
			offset: 8192,
			size: 4096,
			..
		})
	));
	assert_eq!(
		events(&os),
		[FakeEvent::Failed(key, Failure::StalePlaceholder)]
	);
	os.assert_clean();
}

#[tokio::test(start_paused = true)]
async fn an_abort_while_the_read_waits_at_the_gate_cancels_it() {
	let (drive, os, _) = placed(10_000);
	let _gate = drive.gate_reads();
	let (request, responder) = os.fetch_data(&FILE, (0, 4096), (0, 0));
	let key = request.transfer();
	let controller = FfiAbortController::new();
	let abort = controller.signal();
	let (result, ()) = timeout(Duration::from_secs(60), async {
		join!(hydrate(&drive, request, responder, &abort), async {
			drive.reads_started(1).await;
			controller.abort();
		})
	})
	.await
	.expect("an abort must end a read held at the gate");
	assert!(
		matches!(
			result.unwrap_err().error,
			HydrateError::Read(RangeReadError::Cache(CacheError::Aborted(_)))
		),
		"the read must end aborted"
	);
	assert_eq!(events(&os), [FakeEvent::Cancelled(key)]);
	os.assert_clean();
}
