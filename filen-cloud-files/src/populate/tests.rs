use filen_mobile_native_cache::ffi::FfiNonRootObject;
use filen_sdk_rs::io::DeviceRejection;

use super::*;
use crate::{
	identity::IdentityBlob,
	placeholder::SpecKind,
	sealed,
	test_support::{FakeDrive, FakeEvent, FakeOs, TIMESTAMP_MILLIS, ffi_dir, ffi_file},
};

const ROOT_UUID: Uuid = Uuid::from_u128(0x100);
const ROOT: PlaceholderIdentity = PlaceholderIdentity::Root { root: ROOT_UUID };
const ROOT_ID: &str = "00000000-0000-0000-0000-000000000100";
const SUB: PlaceholderIdentity = PlaceholderIdentity::Dir {
	uuid: Uuid::from_u128(0x300),
};

/// The FILETIME ticks of the helpers' dates.
const CREATED_TICKS: i64 = 132_444_736_000_000_000;
const MODIFIED_TICKS: i64 = 133_444_736_000_000_000;
const TIMESTAMP_TICKS: i64 = 132_944_736_000_000_000;

/// The listed file `name` of `size` bytes, stable id `stable` and version `stable + 1`.
fn file(stable: u128, name: &str, size: i64) -> FfiNonRootObject {
	FfiNonRootObject::File(ffi_file(
		Uuid::from_u128(stable),
		Uuid::from_u128(stable + 1),
		name,
		size,
	))
}

fn dir(uuid: u128, name: &str) -> FfiNonRootObject {
	FfiNonRootObject::Dir(ffi_dir(Uuid::from_u128(uuid), name))
}

fn file_identity(stable: u128) -> PlaceholderIdentity {
	PlaceholderIdentity::File {
		stable: Uuid::from_u128(stable),
		version: Uuid::from_u128(stable + 1),
	}
}

/// A drive of the root account listing `children` under the root.
fn drive_listing(children: Vec<FfiNonRootObject>) -> FakeDrive {
	let drive = FakeDrive::new(ROOT_UUID);
	drive.list(ROOT_ID, children);
	drive
}

async fn populate_dir(
	drive: &FakeDrive,
	os: &FakeOs,
	dir: &PlaceholderIdentity,
) -> Result<PopulateReport, PopulateFailed> {
	let (request, responder) = os.fetch_placeholders(dir);
	populate(drive, request, responder).await
}

/// Populates the root, expecting no failure, and returns its report.
async fn populate_root(drive: &FakeDrive, os: &FakeOs) -> PopulateReport {
	populate_dir(drive, os, &ROOT).await.unwrap()
}

fn names(os: &FakeOs, dir: &PlaceholderIdentity) -> Vec<String> {
	os.children(dir)
		.iter()
		.map(|spec| spec.name().as_str().to_owned())
		.collect()
}

fn skip(id: &str, name: Option<&str>, reason: SkipReason) -> SkippedItem {
	SkippedItem {
		id: id.to_owned(),
		name: name.map(str::to_owned),
		reason,
	}
}

/// A population responder answering with no outcome at all.
struct NoOutcomes;

impl sealed::Sealed for NoOutcomes {}

impl PopulateResponder for NoOutcomes {
	fn populate(self, _: &[PlaceholderSpec]) -> Result<Vec<EntryOutcome>, OsError> {
		Ok(Vec::new())
	}

	fn fail(self, failure: Failure) {
		panic!("a population that was answered was failed with {failure:?}");
	}
}

#[tokio::test(start_paused = true)]
async fn the_root_is_listed_under_the_account_root_uuid() {
	let drive = drive_listing(vec![file(0x200, "a.txt", 10)]);
	let os = FakeOs::with_root(ROOT);
	assert_eq!(
		populate_root(&drive, &os).await,
		PopulateReport {
			created: 1,
			skipped: Vec::new()
		}
	);
	assert_eq!(names(&os, &ROOT), ["a.txt"]);
	os.assert_clean();
}

#[tokio::test(start_paused = true)]
async fn a_directory_is_listed_under_its_stable_id() {
	let drive = drive_listing(vec![dir(0x300, "sub")]);
	drive.list(
		"stable/00000000-0000-0000-0000-000000000300",
		vec![file(0x400, "inner.txt", 5)],
	);
	let os = FakeOs::with_root(ROOT);
	populate_root(&drive, &os).await;
	assert_eq!(populate_dir(&drive, &os, &SUB).await.unwrap().created, 1);
	assert_eq!(names(&os, &SUB), ["inner.txt"]);
	os.assert_clean();
}

#[tokio::test(start_paused = true)]
async fn a_listing_becomes_literal_specs() {
	let drive = drive_listing(vec![file(0x200, "a.txt", 10), dir(0x300, "sub")]);
	let os = FakeOs::with_root(ROOT);
	populate_root(&drive, &os).await;
	let specs: Vec<_> = os
		.children(&ROOT)
		.iter()
		.map(|spec| {
			(
				spec.name().as_str().to_owned(),
				spec.identity(),
				spec.kind(),
				spec.created().ticks(),
				spec.modified().ticks(),
			)
		})
		.collect();
	assert_eq!(
		specs,
		[
			(
				"a.txt".to_owned(),
				file_identity(0x200),
				SpecKind::File {
					size: FileSize::from_i64(10).unwrap()
				},
				CREATED_TICKS,
				MODIFIED_TICKS
			),
			(
				"sub".to_owned(),
				SUB,
				SpecKind::Dir,
				CREATED_TICKS,
				CREATED_TICKS
			),
		]
	);
	os.assert_clean();
}

#[tokio::test(start_paused = true)]
async fn a_file_without_a_creation_date_takes_the_server_timestamp() {
	let mut undated = ffi_file(Uuid::from_u128(0x200), Uuid::from_u128(0x201), "a.txt", 10);
	if let Some(meta) = &mut undated.meta {
		meta.created = None;
	}
	assert_eq!(undated.timestamp, TIMESTAMP_MILLIS);
	let drive = drive_listing(vec![FfiNonRootObject::File(undated)]);
	let os = FakeOs::with_root(ROOT);
	populate_root(&drive, &os).await;
	let children = os.children(&ROOT);
	let [spec] = children.as_slice() else {
		panic!("one child expected: {children:?}");
	};
	assert_eq!(
		(spec.created().ticks(), spec.modified().ticks()),
		(TIMESTAMP_TICKS, MODIFIED_TICKS)
	);
	os.assert_clean();
}

#[tokio::test(start_paused = true)]
async fn an_undecryptable_child_is_skipped_and_reported() {
	let mut undecryptable = ffi_file(Uuid::from_u128(0x200), Uuid::from_u128(0x201), "a.txt", 10);
	undecryptable.meta = None;
	let drive = drive_listing(vec![
		FfiNonRootObject::File(undecryptable),
		file(0x300, "b.txt", 1),
	]);
	let os = FakeOs::with_root(ROOT);
	assert_eq!(
		populate_root(&drive, &os).await,
		PopulateReport {
			created: 1,
			skipped: vec![skip(
				"00000000-0000-0000-0000-000000000200",
				None,
				SkipReason::Undecryptable
			)]
		}
	);
	assert_eq!(names(&os, &ROOT), ["b.txt"]);
	os.assert_clean();
}

#[tokio::test(start_paused = true)]
async fn an_illegal_name_is_skipped_and_reported() {
	let drive = drive_listing(vec![file(0x200, "a:b", 10)]);
	let os = FakeOs::with_root(ROOT);
	assert_eq!(
		populate_root(&drive, &os).await.skipped,
		[skip(
			"00000000-0000-0000-0000-000000000200",
			Some("a:b"),
			SkipReason::Name(NameRejection::Windows(DeviceRejection::ForbiddenChar(':')))
		)]
	);
	assert_eq!(names(&os, &ROOT), Vec::<String>::new());
	os.assert_clean();
}

#[tokio::test(start_paused = true)]
async fn a_negative_size_is_skipped_and_reported() {
	let drive = drive_listing(vec![file(0x200, "a.txt", -1)]);
	let os = FakeOs::with_root(ROOT);
	assert_eq!(
		populate_root(&drive, &os).await.skipped,
		[skip(
			"00000000-0000-0000-0000-000000000200",
			Some("a.txt"),
			SkipReason::NegativeSize(NegativeSize { size: -1 })
		)]
	);
	os.assert_clean();
}

#[tokio::test(start_paused = true)]
async fn a_malformed_uuid_is_skipped_and_reported() {
	let mut malformed = ffi_file(Uuid::from_u128(0x200), Uuid::from_u128(0x201), "a.txt", 10);
	malformed.stable_uuid = "not-a-uuid".to_owned();
	let drive = drive_listing(vec![FfiNonRootObject::File(malformed)]);
	let os = FakeOs::with_root(ROOT);
	assert_eq!(
		populate_root(&drive, &os).await.skipped,
		[skip("not-a-uuid", Some("a.txt"), SkipReason::MalformedId)]
	);
	os.assert_clean();
}

#[tokio::test(start_paused = true)]
async fn a_case_collision_keeps_the_lowest_uuid() {
	let drive = drive_listing(vec![
		file(0x300, "A.txt", 1),
		file(0x200, "a.txt", 2),
		file(0x400, "A.TXT", 3),
	]);
	let os = FakeOs::with_root(ROOT);
	let report = populate_root(&drive, &os).await;
	assert_eq!(names(&os, &ROOT), ["a.txt"]);
	assert_eq!(
		report,
		PopulateReport {
			created: 1,
			skipped: vec![
				skip(
					"00000000-0000-0000-0000-000000000300",
					Some("A.txt"),
					SkipReason::CaseCollision {
						kept: file_identity(0x200)
					}
				),
				skip(
					"00000000-0000-0000-0000-000000000400",
					Some("A.TXT"),
					SkipReason::CaseCollision {
						kept: file_identity(0x200)
					}
				),
			]
		}
	);
	os.assert_clean();
}

#[tokio::test(start_paused = true)]
async fn a_per_entry_refusal_is_reported() {
	let drive = drive_listing(vec![file(0x200, "a.txt", 1), file(0x300, "b.txt", 2)]);
	let os = FakeOs::with_root(ROOT);
	os.refuse_entry("b.txt");
	assert_eq!(
		populate_root(&drive, &os).await,
		PopulateReport {
			created: 1,
			skipped: vec![skip(
				"00000000-0000-0000-0000-000000000300",
				Some("b.txt"),
				SkipReason::Refused(OsError::Rejected { code: -1 })
			)]
		}
	);
	os.assert_clean();
}

#[tokio::test(start_paused = true)]
async fn an_entry_a_local_item_holds_is_not_skipped() {
	let drive = drive_listing(vec![file(0x200, "a.txt", 1), file(0x300, "b.txt", 2)]);
	let os = FakeOs::with_root(ROOT);
	os.hold_locally("b.txt");
	assert_eq!(
		populate_root(&drive, &os).await,
		PopulateReport {
			created: 1,
			skipped: Vec::new()
		}
	);
	os.assert_clean();
}

#[tokio::test(start_paused = true)]
async fn a_wrong_outcome_count_is_an_internal_failure() {
	let drive = drive_listing(vec![file(0x200, "a.txt", 1)]);
	let failed = populate(
		&drive,
		PopulateRequest::new(ROOT.encode(), false),
		NoOutcomes,
	)
	.await
	.unwrap_err();
	assert!(
		matches!(
			failed.error,
			PopulateError::OutcomeCount {
				sent: 1,
				answered: 0
			}
		),
		"{failed:?}"
	);
	assert_eq!(failed.error.failure(), Failure::Internal);
	assert_eq!(failed.report, PopulateReport::default());
}

#[tokio::test(start_paused = true)]
async fn another_accounts_root_is_not_authenticated() {
	let drive = drive_listing(vec![file(0x200, "a.txt", 1)]);
	let other = PlaceholderIdentity::Root {
		root: Uuid::from_u128(0x999),
	};
	let os = FakeOs::with_root(other);
	let failed = populate_dir(&drive, &os, &other).await.unwrap_err();
	assert!(
		matches!(
			failed.error,
			PopulateError::OtherAccount { placeholder, account }
				if placeholder == Uuid::from_u128(0x999) && account == ROOT_UUID
		),
		"{failed:?}"
	);
	assert_eq!(
		os.events(),
		[FakeEvent::PopulateFailed(Failure::NotAuthenticated)]
	);
	os.assert_clean();
}

#[tokio::test(start_paused = true)]
async fn a_file_identity_is_an_invalid_request() {
	let drive = drive_listing(Vec::new());
	let os = FakeOs::with_root(ROOT);
	let (_, responder) = os.fetch_placeholders(&ROOT);
	let request = PopulateRequest::new(file_identity(0x200).encode(), false);
	let failed = populate(&drive, request, responder).await.unwrap_err();
	assert!(
		matches!(failed.error, PopulateError::NotADirectory),
		"{failed:?}"
	);
	assert_eq!(
		os.events(),
		[FakeEvent::PopulateFailed(Failure::InvalidRequest)]
	);
	os.assert_clean();
}

#[tokio::test(start_paused = true)]
async fn a_missing_directory_is_not_found() {
	let drive = FakeDrive::new(ROOT_UUID);
	let os = FakeOs::with_root(ROOT);
	let failed = populate_dir(&drive, &os, &ROOT).await.unwrap_err();
	assert!(matches!(failed.error, PopulateError::Missing), "{failed:?}");
	assert_eq!(os.events(), [FakeEvent::PopulateFailed(Failure::NotFound)]);
	os.assert_clean();
}

#[tokio::test(start_paused = true)]
async fn offline_population_fails_unreachable_without_creating_anything() {
	let drive = drive_listing(vec![file(0x200, "a.txt", 1)]);
	drive.fail_list(CacheError::Remote("offline".into()));
	let os = FakeOs::with_root(ROOT);
	let failed = populate_dir(&drive, &os, &ROOT).await.unwrap_err();
	assert!(
		matches!(failed.error, PopulateError::Cache(CacheError::Remote(_))),
		"{failed:?}"
	);
	assert_eq!(
		os.events(),
		[FakeEvent::PopulateFailed(Failure::Unreachable)]
	);
	assert_eq!(os.children(&ROOT), []);
	os.assert_clean();
}

#[tokio::test(start_paused = true)]
async fn a_corrupt_identity_is_an_invalid_request() {
	let drive = drive_listing(Vec::new());
	let os = FakeOs::with_root(ROOT);
	let (_, responder) = os.fetch_placeholders(&ROOT);
	let request = PopulateRequest::new(IdentityBlob::try_from(&[9_u8][..]).unwrap(), false);
	let failed = populate(&drive, request, responder).await.unwrap_err();
	assert!(
		matches!(
			failed.error,
			PopulateError::Identity(IdentityError::UnknownFormat { format: 9 })
		),
		"{failed:?}"
	);
	assert_eq!(
		os.events(),
		[FakeEvent::PopulateFailed(Failure::InvalidRequest)]
	);
	os.assert_clean();
}

#[tokio::test(start_paused = true)]
async fn a_failed_root_lookup_fails_the_population() {
	let drive = drive_listing(vec![file(0x200, "a.txt", 1)]);
	drive.fail_root(CacheError::Unauthenticated("signed out".into()));
	let os = FakeOs::with_root(ROOT);
	let failed = populate_dir(&drive, &os, &ROOT).await.unwrap_err();
	assert!(
		matches!(
			failed.error,
			PopulateError::Cache(CacheError::Unauthenticated(_))
		),
		"{failed:?}"
	);
	assert_eq!(
		os.events(),
		[FakeEvent::PopulateFailed(Failure::NotAuthenticated)]
	);
	os.assert_clean();
}

#[tokio::test(start_paused = true)]
async fn every_class_of_name_windows_cannot_hold_is_skipped_and_reported() {
	let long_ascii = "a".repeat(256);
	// 128 characters outside the BMP: 256 UTF-16 units, one past the limit.
	let long_emoji = "\u{1F600}".repeat(128);
	let fits_emoji = "\u{1F600}".repeat(127);
	let rows: [(&str, NameRejection); 9] = [
		("CON", NameRejection::Windows(DeviceRejection::ReservedName)),
		(
			"com\u{b9}.txt",
			NameRejection::Windows(DeviceRejection::ReservedName),
		),
		(
			"nul.txt",
			NameRejection::Windows(DeviceRejection::ReservedName),
		),
		(
			"trailing.",
			NameRejection::Windows(DeviceRejection::TrailingDotOrSpace),
		),
		(
			"trailing ",
			NameRejection::Windows(DeviceRejection::TrailingDotOrSpace),
		),
		(
			".",
			NameRejection::Windows(DeviceRejection::TrailingDotOrSpace),
		),
		(
			"..",
			NameRejection::Windows(DeviceRejection::TrailingDotOrSpace),
		),
		(
			&long_ascii,
			NameRejection::Windows(DeviceRejection::TooLong { len: 256, max: 255 }),
		),
		(
			&long_emoji,
			NameRejection::Windows(DeviceRejection::TooLong { len: 256, max: 255 }),
		),
	];
	let mut children: Vec<_> = (0_u128..)
		.zip(&rows)
		.map(|(n, (name, _))| file(0x1000 + n * 0x10, name, 1))
		.collect();
	children.push(file(0x2000, "", 1));
	children.push(file(0x3000, &fits_emoji, 1));
	let drive = drive_listing(children);
	let os = FakeOs::with_root(ROOT);
	let report = populate_root(&drive, &os).await;
	let mut expected: Vec<_> = (0_u128..)
		.zip(&rows)
		.map(|(n, (name, rejection))| {
			skip(
				&Uuid::from_u128(0x1000 + n * 0x10).to_string(),
				Some(*name),
				SkipReason::Name(*rejection),
			)
		})
		.collect();
	expected.push(skip(
		"00000000-0000-0000-0000-000000002000",
		Some(""),
		SkipReason::Name(NameRejection::Empty),
	));
	assert_eq!(
		report,
		PopulateReport {
			created: 1,
			skipped: expected
		}
	);
	assert_eq!(names(&os, &ROOT), [fits_emoji]);
	os.assert_clean();
}

#[tokio::test(start_paused = true)]
async fn an_empty_directory_is_populated_with_no_entries() {
	let drive = drive_listing(Vec::new());
	let os = FakeOs::with_root(ROOT);
	assert_eq!(populate_root(&drive, &os).await, PopulateReport::default());
	// Populated with nothing, so the OS stops asking for the directory.
	assert_eq!(
		os.events(),
		[FakeEvent::Populated {
			dir: ROOT,
			entries: Vec::new()
		}]
	);
	os.assert_clean();
}

#[tokio::test(start_paused = true)]
async fn a_case_collision_between_a_file_and_a_directory_keeps_the_lowest_uuid() {
	let drive = drive_listing(vec![
		file(0x400, "A", 1),
		dir(0x300, "a"),
		dir(0x600, "x"),
		dir(0x500, "X"),
	]);
	let os = FakeOs::with_root(ROOT);
	let report = populate_root(&drive, &os).await;
	assert_eq!(names(&os, &ROOT), ["a", "X"]);
	let kept_x = PlaceholderIdentity::Dir {
		uuid: Uuid::from_u128(0x500),
	};
	assert_eq!(
		report,
		PopulateReport {
			created: 2,
			skipped: vec![
				skip(
					"00000000-0000-0000-0000-000000000400",
					Some("A"),
					SkipReason::CaseCollision { kept: SUB }
				),
				skip(
					"00000000-0000-0000-0000-000000000600",
					Some("x"),
					SkipReason::CaseCollision { kept: kept_x }
				),
			]
		}
	);
	os.assert_clean();
}

#[tokio::test(start_paused = true)]
async fn an_undecryptable_directory_is_skipped_and_reported() {
	let mut undecryptable = ffi_dir(Uuid::from_u128(0x300), "sub");
	undecryptable.meta = None;
	let drive = drive_listing(vec![
		FfiNonRootObject::Dir(undecryptable),
		file(0x400, "b.txt", 1),
	]);
	let os = FakeOs::with_root(ROOT);
	assert_eq!(
		populate_root(&drive, &os).await,
		PopulateReport {
			created: 1,
			skipped: vec![skip(
				"00000000-0000-0000-0000-000000000300",
				None,
				SkipReason::Undecryptable
			)]
		}
	);
	assert_eq!(names(&os, &ROOT), ["b.txt"]);
	os.assert_clean();
}
