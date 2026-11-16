use filen_types::fs::Uuid;

use super::*;
use crate::{
	identity::PlaceholderIdentity,
	names::PlaceholderName,
	placeholder::{FileSize, FileTime},
	test_support::{FakeEvent, FakeOs},
};

const ROOT: PlaceholderIdentity = PlaceholderIdentity::Root {
	root: Uuid::from_u128(0x100),
};
const FILE: PlaceholderIdentity = PlaceholderIdentity::File {
	stable: Uuid::from_u128(0x200),
	version: Uuid::from_u128(0x201),
};
const REFUSED: OsError = OsError::Rejected { code: -1 };

fn time() -> FileTime {
	FileTime::from_unix_millis(1_700_000_000_000).unwrap()
}

fn file_spec(name: &str, stable: u128, size: i64) -> PlaceholderSpec {
	PlaceholderSpec::file(
		PlaceholderName::parse(name).unwrap(),
		Uuid::from_u128(stable),
		Uuid::from_u128(stable + 1),
		FileSize::from_i64(size).unwrap(),
		time(),
		time(),
	)
}

/// A fake whose root holds `FILE`, `size` bytes long.
fn fake_with_file(size: i64) -> FakeOs {
	let os = FakeOs::with_root(ROOT);
	let (_, responder) = os.fetch_placeholders(&ROOT);
	assert_eq!(
		responder.populate(&[file_spec("f.bin", 0x200, size)]),
		Ok(vec![EntryOutcome::Created])
	);
	os
}

fn chunk(offset: u64, bytes: &[u8]) -> AlignedChunk<'_> {
	AlignedChunk::new(offset, bytes)
}

fn last_event(os: &FakeOs) -> FakeEvent {
	os.events().pop().unwrap()
}

#[test]
fn the_fake_refuses_an_unaligned_offset() {
	let os = fake_with_file(10_000);
	let (request, mut data) = os.fetch_data(&FILE, (0, 10_000), (0, 0));
	assert_eq!(data.transfer(chunk(100, &[1; 4096])), Err(REFUSED));
	assert_eq!(
		last_event(&os),
		FakeEvent::RejectedTransfer {
			key: request.transfer(),
			offset: 100,
			len: 4096,
			why: "an unaligned offset",
		}
	);
	data.fail(Failure::Internal);
}

#[test]
fn the_fake_refuses_an_unaligned_length_mid_file() {
	let os = fake_with_file(10_000);
	let (request, mut data) = os.fetch_data(&FILE, (0, 10_000), (0, 0));
	assert_eq!(data.transfer(chunk(0, &[1; 100])), Err(REFUSED));
	assert_eq!(
		last_event(&os),
		FakeEvent::RejectedTransfer {
			key: request.transfer(),
			offset: 0,
			len: 100,
			why: "an unaligned length before the end of the file",
		}
	);
	data.fail(Failure::Internal);
}

#[test]
fn the_fake_accepts_an_unaligned_tail_at_eof() {
	let os = fake_with_file(10_000);
	let (request, mut data) = os.fetch_data(&FILE, (8192, 1808), (0, 0));
	assert_eq!(data.transfer(chunk(8192, &[7; 1808])), Ok(()));
	data.complete();
	let key = request.transfer();
	assert_eq!(
		os.events()[1..],
		[
			FakeEvent::Transferred {
				key,
				offset: 8192,
				len: 1808,
			},
			FakeEvent::Completed(key),
		]
	);
	let (bytes, hydrated) = os.contents(&FILE);
	assert_eq!(bytes.len(), 10_000);
	assert_eq!(bytes[8191..8194], [0, 7, 7]);
	assert_eq!(hydrated, vec![8192..10_000]);
	os.assert_clean();
}

#[test]
fn the_fake_refuses_a_transfer_past_eof() {
	let os = fake_with_file(10_000);
	let (request, mut data) = os.fetch_data(&FILE, (8192, 1808), (0, 0));
	assert_eq!(data.transfer(chunk(8192, &[7; 4096])), Err(REFUSED));
	assert_eq!(
		last_event(&os),
		FakeEvent::RejectedTransfer {
			key: request.transfer(),
			offset: 8192,
			len: 4096,
			why: "a transfer past the end of the file",
		}
	);
	data.fail(Failure::Internal);
}

#[test]
fn the_fake_refuses_a_transfer_outside_the_request() {
	let os = fake_with_file(20_480);
	let (request, mut data) = os.fetch_data(&FILE, (0, 4096), (0, 0));
	assert_eq!(data.transfer(chunk(8192, &[7; 4096])), Err(REFUSED));
	assert_eq!(
		last_event(&os),
		FakeEvent::RejectedTransfer {
			key: request.transfer(),
			offset: 8192,
			len: 4096,
			why: "a transfer outside the request",
		}
	);
	data.fail(Failure::Internal);
}

#[test]
fn a_transfer_inside_the_optional_range_is_accepted() {
	let os = fake_with_file(20_480);
	let (_, mut data) = os.fetch_data(&FILE, (0, 4096), (4096, 6000));
	assert_eq!(data.transfer(chunk(0, &[7; 12_288])), Ok(()));
	data.complete();
	assert_eq!(os.contents(&FILE).1, vec![0..12_288]);
	os.assert_clean();
}

#[test]
fn a_transfer_after_a_cancel_of_an_overlapping_range_is_taken() {
	let os = fake_with_file(10_000);
	let (request, mut data) = os.fetch_data(&FILE, (0, 8192), (0, 0));
	os.cancel(request.transfer(), 4096, 5904);
	assert_eq!(data.transfer(chunk(0, &[7; 8192])), Ok(()));
	data.complete();
	os.assert_clean();
}

#[test]
fn a_transfer_after_cancel_is_cancelled() {
	let os = fake_with_file(10_000);
	let (request, mut data) = os.fetch_data(&FILE, (0, 10_000), (0, 0));
	let cancel = os.cancel(request.transfer(), 0, 10_000);
	let range = cancel.range().expect("a ranged cancel");
	assert_eq!((range.offset(), range.length()), (0, 10_000));
	assert_eq!(data.transfer(chunk(0, &[7; 4096])), Err(OsError::Cancelled));
	data.cancelled();
	assert_eq!(last_event(&os), FakeEvent::Cancelled(request.transfer()));
	assert_eq!(os.contents(&FILE).1, Vec::new());
	os.assert_clean();
}

#[test]
fn a_dropped_responder_is_recorded_abandoned() {
	let os = fake_with_file(10_000);
	let (request, data) = os.fetch_data(&FILE, (0, 10_000), (0, 0));
	drop(data);
	assert_eq!(last_event(&os), FakeEvent::Abandoned(request.transfer()));
	let dir = PlaceholderIdentity::Root {
		root: Uuid::from_u128(0x300),
	};
	let other = FakeOs::with_root(dir);
	drop(other.fetch_placeholders(&dir));
	assert_eq!(other.events(), [FakeEvent::PopulateAbandoned { dir }]);
}

#[test]
fn complete_without_the_required_range_is_incomplete() {
	let os = fake_with_file(10_000);
	let (request, mut data) = os.fetch_data(&FILE, (0, 8192), (0, 0));
	assert_eq!(data.transfer(chunk(0, &[7; 4096])), Ok(()));
	data.complete();
	assert_eq!(last_event(&os), FakeEvent::Incomplete(request.transfer()));
}

#[test]
fn the_fake_refuses_a_case_duplicate_entry() {
	let os = FakeOs::with_root(ROOT);
	let (_, responder) = os.fetch_placeholders(&ROOT);
	let first = file_spec("A.txt", 0x200, 1);
	assert_eq!(
		responder.populate(&[first.clone(), file_spec("a.txt", 0x300, 1)]),
		Ok(vec![EntryOutcome::Created, EntryOutcome::Rejected(REFUSED)])
	);
	assert_eq!(os.children(&ROOT), vec![first.clone()]);
	assert_eq!(
		os.events(),
		[FakeEvent::Populated {
			dir: ROOT,
			entries: vec![first],
		}]
	);
}

#[test]
#[should_panic(expected = "is already populated, so Windows does not ask again")]
fn a_directory_is_never_asked_twice() {
	let os = fake_with_file(1);
	let _ = os.fetch_placeholders(&ROOT);
}

#[test]
fn an_optional_range_of_no_byte_is_none() {
	assert_eq!(OptionalRange::new(4096, 0), None);
	assert_eq!(OptionalRange::new(4096, -1), None);
	let optional = OptionalRange::new(-4096, 1).unwrap();
	assert_eq!((optional.offset(), optional.length()), (-4096, 1));
}

#[test]
fn the_fake_offers_an_optional_range_only_when_it_holds_a_byte() {
	let os = fake_with_file(10_000);
	let (none, data) = os.fetch_data(&FILE, (0, 4096), (4096, 0));
	assert_eq!(none.range().optional(), None);
	data.fail(Failure::Internal);
	let (some, data) = os.fetch_data(&FILE, (0, 4096), (4096, 4096));
	assert_eq!(some.range().optional(), OptionalRange::new(4096, 4096));
	data.fail(Failure::Internal);
}

#[test]
fn a_cancel_refuses_only_the_requests_it_overlaps_under_its_key() {
	let os = fake_with_file(20_480);
	let (first, mut first_data) = os.fetch_data(&FILE, (0, 4096), (0, 0));
	let key = first.transfer();
	let (second, mut second_data) = os.fetch_data_under(key, &FILE, (8192, 4096), (0, 0));
	assert_eq!(second.transfer(), key);
	os.cancel(key, 0, 4096);
	assert_eq!(
		first_data.transfer(chunk(0, &[7; 4096])),
		Err(OsError::Cancelled)
	);
	assert_eq!(second_data.transfer(chunk(8192, &[7; 4096])), Ok(()));
	first_data.cancelled();
	second_data.complete();
	assert_eq!(os.contents(&FILE).1, vec![8192..12_288]);
	os.assert_clean();
}

#[test]
fn a_cancel_of_no_byte_refuses_every_request_under_its_key() {
	let os = fake_with_file(20_480);
	let (first, mut first_data) = os.fetch_data(&FILE, (0, 4096), (0, 0));
	let key = first.transfer();
	let (_, mut second_data) = os.fetch_data_under(key, &FILE, (8192, 4096), (0, 0));
	os.cancel(key, 0, 0);
	assert_eq!(
		first_data.transfer(chunk(0, &[7; 4096])),
		Err(OsError::Cancelled)
	);
	assert_eq!(
		second_data.transfer(chunk(8192, &[7; 4096])),
		Err(OsError::Cancelled)
	);
	first_data.cancelled();
	second_data.cancelled();
	os.assert_clean();
}
