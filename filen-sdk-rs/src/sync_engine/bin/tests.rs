use std::fs;

use chrono::NaiveDate;
use tempfile::TempDir;

use super::{quarantine_local, test_support::day};
use crate::sync_engine::scan::QUARANTINE_DIR;

fn root() -> TempDir {
	tempfile::tempdir().expect("a temp dir")
}

#[test]
fn a_binned_file_lands_under_its_days_bucket_at_its_original_path() {
	let root = root();
	fs::create_dir(root.path().join("docs")).unwrap();
	fs::write(root.path().join("docs").join("a.txt"), b"bytes").unwrap();

	let binned = quarantine_local(root.path(), "docs/a.txt", day()).unwrap();

	let expected = root
		.path()
		.join(".filen-sync-trash")
		.join("2026-10-26")
		.join("docs")
		.join("a.txt");
	assert_eq!(binned, Some(expected.clone()));
	assert_eq!(fs::read(&expected).unwrap(), b"bytes");
	assert!(!root.path().join("docs").join("a.txt").exists());
}

#[test]
fn a_bucket_name_pads_month_and_day_to_two_digits() {
	let root = root();
	fs::write(root.path().join("a.txt"), b"x").unwrap();
	let day = NaiveDate::from_ymd_opt(2027, 1, 5).unwrap();

	assert_eq!(
		quarantine_local(root.path(), "a.txt", day).unwrap(),
		Some(
			root.path()
				.join(QUARANTINE_DIR)
				.join("2027-01-05")
				.join("a.txt")
		)
	);
}

#[test]
fn one_path_binned_on_two_days_lands_in_two_buckets_without_a_suffix() {
	let root = root();
	let next_day = NaiveDate::from_ymd_opt(2026, 10, 27).unwrap();

	fs::write(root.path().join("x.txt"), b"first").unwrap();
	let first = quarantine_local(root.path(), "x.txt", day()).unwrap();
	fs::write(root.path().join("x.txt"), b"second").unwrap();
	let second = quarantine_local(root.path(), "x.txt", next_day).unwrap();

	let bin = root.path().join(QUARANTINE_DIR);
	assert_eq!(first, Some(bin.join("2026-10-26").join("x.txt")));
	assert_eq!(second, Some(bin.join("2026-10-27").join("x.txt")));
	assert_eq!(
		fs::read(bin.join("2026-10-26").join("x.txt")).unwrap(),
		b"first"
	);
	assert_eq!(
		fs::read(bin.join("2026-10-27").join("x.txt")).unwrap(),
		b"second"
	);
}

/// Two deletions at the same path on one day must not overwrite each other.
#[test]
fn one_path_binned_twice_on_one_day_gets_a_suffix_never_overwriting() {
	let root = root();
	let bucket = root.path().join(QUARANTINE_DIR).join("2026-10-26");

	fs::write(root.path().join("x.txt"), b"first").unwrap();
	assert_eq!(
		quarantine_local(root.path(), "x.txt", day()).unwrap(),
		Some(bucket.join("x.txt"))
	);
	fs::write(root.path().join("x.txt"), b"second").unwrap();
	assert_eq!(
		quarantine_local(root.path(), "x.txt", day()).unwrap(),
		Some(bucket.join("x.txt (1)"))
	);

	assert_eq!(
		fs::read(bucket.join("x.txt")).unwrap(),
		b"first",
		"first copy preserved"
	);
	assert_eq!(
		fs::read(bucket.join("x.txt (1)")).unwrap(),
		b"second",
		"second copy under a unique name"
	);
}

#[test]
fn binning_a_missing_source_is_a_noop_and_creates_no_bin() {
	let root = root();
	// e.g. the item already moved as part of an ancestor's quarantine.
	assert_eq!(
		quarantine_local(root.path(), "already/gone.txt", day())
			.expect("missing source is a no-op"),
		None
	);
	assert!(!root.path().join(QUARANTINE_DIR).exists());
}
