use std::{cell::Cell, ffi::OsStr, fs, path::Path};

use chrono::NaiveDate;
use tempfile::TempDir;

use super::{
	DATED_MARKER, MigrateSummary, PruneSummary, migrate_legacy_trash, parse_bucket, prune_bin,
	quarantine_local,
	test_support::{dated_bin, day},
};
use crate::sync_engine::scan::{LEGACY_TRASH_DIR, QUARANTINE_DIR};

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
fn a_bin_binning_creates_is_marked_as_filed_by_day() {
	let root = root();
	fs::write(root.path().join("a.txt"), b"x").unwrap();

	quarantine_local(root.path(), "a.txt", day()).unwrap();

	assert!(root.path().join(QUARANTINE_DIR).join(DATED_MARKER).is_dir());
}

/// A bin that was there before may hold the flat layout, whose entries only a prune can file.
#[test]
fn binning_into_an_existing_unmarked_bin_does_not_mark_it() {
	let root = root();
	let bin = root.path().join(QUARANTINE_DIR);
	fs::create_dir_all(bin.join("2019-07-14")).unwrap();
	fs::write(root.path().join("a.txt"), b"x").unwrap();

	quarantine_local(root.path(), "a.txt", day()).unwrap();

	assert!(!bin.join(DATED_MARKER).exists());
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

fn date(y: i32, m: u32, d: u32) -> NaiveDate {
	NaiveDate::from_ymd_opt(y, m, d).unwrap()
}

/// A bucket named `name` under `root`'s bin, holding one file, in a bin marked as filed by day.
fn bucket_with_file(root: &Path, name: &str) {
	dated_bin(root);
	let bucket = root.join(QUARANTINE_DIR).join(name);
	fs::create_dir_all(bucket.join("docs")).unwrap();
	fs::write(bucket.join("docs").join("a.txt"), b"binned").unwrap();
}

fn prune(root: &Path, today: NaiveDate) -> PruneSummary {
	prune_bin(root, today, || true)
}

fn summary(removed: usize, adopted: usize) -> PruneSummary {
	PruneSummary {
		removed,
		adopted,
		errors: 0,
		stopped: false,
	}
}

#[test]
fn a_bucket_name_parses_back_to_its_day() {
	assert_eq!(
		parse_bucket(OsStr::new("2026-10-26")),
		Some(date(2026, 10, 26))
	);
	assert_eq!(
		parse_bucket(OsStr::new("2024-02-29")),
		Some(date(2024, 2, 29))
	);
}

#[test]
fn only_an_exact_yyyy_mm_dd_naming_a_real_day_is_a_bucket() {
	for name in [
		"2026-13-01",
		"2026-02-30",
		"2026-1-05",
		"2026-01-5",
		"+2026-01-05",
		"2026-01-05 (1)",
		" 2026-01-05",
		"20260105",
		"docs",
		"",
	] {
		assert_eq!(parse_bucket(OsStr::new(name)), None, "{name:?}");
	}
}

#[test]
fn a_bucket_31_days_old_is_deleted_with_everything_in_it() {
	let root = root();
	bucket_with_file(root.path(), "2026-09-25");

	assert_eq!(prune(root.path(), day()), summary(1, 0));

	assert!(!root.path().join(QUARANTINE_DIR).join("2026-09-25").exists());
	assert!(
		root.path().join(QUARANTINE_DIR).exists(),
		"the bin itself stays"
	);
}

#[test]
fn a_bucket_exactly_30_days_old_is_kept() {
	let root = root();
	bucket_with_file(root.path(), "2026-09-26");

	assert_eq!(prune(root.path(), day()), summary(0, 0));

	assert_eq!(
		fs::read(
			root.path()
				.join(QUARANTINE_DIR)
				.join("2026-09-26")
				.join("docs")
				.join("a.txt")
		)
		.unwrap(),
		b"binned"
	);
}

#[test]
fn a_bucket_dated_after_today_is_kept() {
	let root = root();
	bucket_with_file(root.path(), "2027-01-01");
	bucket_with_file(root.path(), "2026-10-26");

	assert_eq!(prune(root.path(), day()), summary(0, 0));

	assert!(root.path().join(QUARANTINE_DIR).join("2027-01-01").exists());
	assert!(root.path().join(QUARANTINE_DIR).join("2026-10-26").exists());
}

/// The bin as it was laid out before it was filed by day: its entries have no date to age by, so
/// they are filed under today's and age from now.
#[test]
fn an_undated_file_and_directory_are_adopted_into_todays_bucket() {
	let root = root();
	let bin = root.path().join(QUARANTINE_DIR);
	fs::create_dir_all(bin.join("docs")).unwrap();
	fs::write(bin.join("docs").join("a.txt"), b"legacy dir").unwrap();
	fs::write(bin.join("b.txt"), b"legacy file").unwrap();

	assert_eq!(prune(root.path(), day()), summary(0, 2));

	let today = bin.join("2026-10-26");
	assert_eq!(
		fs::read(today.join("docs").join("a.txt")).unwrap(),
		b"legacy dir"
	);
	assert_eq!(fs::read(today.join("b.txt")).unwrap(), b"legacy file");
	assert!(!bin.join("docs").exists());
	assert!(!bin.join("b.txt").exists());
}

/// Before the bin was filed by day, a deleted top-level folder named like a day sat in the bin
/// under that name. In a bin not marked as filed by day it is the user's item, not a bucket: it is
/// adopted, never deleted, however old its name reads, and the bin is marked once it is.
#[test]
fn an_unmarked_bins_entry_named_like_an_old_day_is_adopted_not_deleted() {
	let root = root();
	let bin = root.path().join(QUARANTINE_DIR);
	fs::create_dir_all(bin.join("2019-07-14")).unwrap();
	fs::write(bin.join("2019-07-14").join("a.txt"), b"photos").unwrap();

	assert_eq!(prune(root.path(), day()), summary(0, 1));

	assert_eq!(
		fs::read(bin.join("2026-10-26").join("2019-07-14").join("a.txt")).unwrap(),
		b"photos"
	);
	assert!(!bin.join("2019-07-14").exists());
	assert!(bin.join(DATED_MARKER).is_dir(), "the bin was not marked");

	// Marked now, so the next prune leaves the adopted entry in today's bucket alone.
	assert_eq!(prune(root.path(), day()), summary(0, 0));
	assert_eq!(
		fs::read(bin.join("2026-10-26").join("2019-07-14").join("a.txt")).unwrap(),
		b"photos"
	);
}

/// An unmarked bin whose prune stopped part of the way is not marked: the next prune still takes
/// its date-named entries for the user's.
#[test]
fn an_unmarked_bin_is_not_marked_by_a_prune_that_stopped() {
	let root = root();
	let bin = root.path().join(QUARANTINE_DIR);
	fs::create_dir_all(bin.join("2019-07-14")).unwrap();
	fs::create_dir_all(bin.join("2019-07-15")).unwrap();
	let asked = Cell::new(0);

	let pruned = prune_bin(root.path(), day(), || {
		asked.set(asked.get() + 1);
		asked.get() == 1
	});

	assert_eq!(
		pruned,
		PruneSummary {
			removed: 0,
			adopted: 1,
			errors: 0,
			stopped: true,
		}
	);
	assert!(!bin.join(DATED_MARKER).exists());

	assert_eq!(prune(root.path(), day()), summary(0, 1));
	for name in ["2019-07-14", "2019-07-15"] {
		assert!(bin.join("2026-10-26").join(name).is_dir(), "{name}");
	}
}

#[test]
fn an_adopted_entry_colliding_in_todays_bucket_gets_a_suffix() {
	let root = root();
	let bin = root.path().join(QUARANTINE_DIR);
	fs::create_dir_all(bin.join("2026-10-26")).unwrap();
	fs::write(bin.join("2026-10-26").join("b.txt"), b"binned today").unwrap();
	fs::write(bin.join("b.txt"), b"legacy").unwrap();

	assert_eq!(prune(root.path(), day()), summary(0, 1));

	let today = bin.join("2026-10-26");
	assert_eq!(fs::read(today.join("b.txt")).unwrap(), b"binned today");
	assert_eq!(fs::read(today.join("b.txt (1)")).unwrap(), b"legacy");
}

/// A name that only looks like a date is not a bucket, however old it reads: it is adopted, never
/// deleted.
#[test]
fn a_name_that_is_not_a_real_day_is_adopted_not_deleted() {
	let root = root();
	bucket_with_file(root.path(), "2026-13-01");
	bucket_with_file(root.path(), "2000-02-30");

	assert_eq!(prune(root.path(), day()), summary(0, 2));

	let today = root.path().join(QUARANTINE_DIR).join("2026-10-26");
	for name in ["2026-13-01", "2000-02-30"] {
		assert_eq!(
			fs::read(today.join(name).join("docs").join("a.txt")).unwrap(),
			b"binned",
			"{name}"
		);
	}
}

#[test]
fn a_missing_bin_is_not_created() {
	let root = root();

	assert_eq!(prune(root.path(), day()), summary(0, 0));

	assert!(!root.path().join(QUARANTINE_DIR).exists());
}

#[test]
fn a_missing_root_is_a_noop() {
	let root = root();
	let gone = root.path().join("unmounted");

	assert_eq!(prune(&gone, day()), summary(0, 0));

	assert!(!gone.exists());
}

/// A top-level folder whose name differs from the bin's only in case is the user's: the scanner
/// syncs it. A case-insensitive file system opens it for the bin's path, and the prune must not
/// take it for the bin; on a case-sensitive one the bin is simply missing.
#[test]
fn a_folder_named_like_the_bin_in_another_case_is_untouched() {
	let root = root();
	let lookalike = root.path().join(".Filen-Sync-Trash");
	fs::create_dir_all(lookalike.join("2020-01-01")).unwrap();
	fs::write(lookalike.join("2020-01-01").join("a.txt"), b"user").unwrap();
	fs::create_dir(lookalike.join(DATED_MARKER)).unwrap();
	fs::write(lookalike.join("loose.txt"), b"user").unwrap();

	let pruned = prune(root.path(), day());

	assert_eq!((pruned.removed, pruned.adopted), (0, 0), "{pruned:?}");
	assert_eq!(
		fs::read(lookalike.join("2020-01-01").join("a.txt")).unwrap(),
		b"user"
	);
	assert_eq!(fs::read(lookalike.join("loose.txt")).unwrap(), b"user");
	let names: Vec<_> = fs::read_dir(root.path())
		.unwrap()
		.map(|entry| entry.unwrap().file_name())
		.collect();
	assert_eq!(names, [".Filen-Sync-Trash"], "a bucket was created");
}

/// A `.filen-sync-trash` below the root is the user's content, synced like any other folder.
#[test]
fn a_nested_bin_named_folder_is_untouched() {
	let root = root();
	let nested = root.path().join("sub").join(QUARANTINE_DIR);
	fs::create_dir_all(nested.join("2020-01-01")).unwrap();
	fs::write(nested.join("2020-01-01").join("a.txt"), b"user").unwrap();
	fs::write(nested.join("loose.txt"), b"user").unwrap();

	assert_eq!(prune(root.path(), day()), summary(0, 0));

	assert_eq!(
		fs::read(nested.join("2020-01-01").join("a.txt")).unwrap(),
		b"user"
	);
	assert_eq!(fs::read(nested.join("loose.txt")).unwrap(), b"user");
	assert!(!root.path().join(QUARANTINE_DIR).exists());
}

/// An expired bucket that is a link to a directory has the link removed; what it points at is not
/// the bin's to delete.
#[cfg(unix)]
#[test]
fn an_expired_bucket_that_is_a_link_loses_the_link_not_its_target() {
	let root = root();
	let outside = tempfile::tempdir().unwrap();
	fs::write(outside.path().join("keep.txt"), b"outside").unwrap();
	let bin = root.path().join(QUARANTINE_DIR);
	dated_bin(root.path());
	std::os::unix::fs::symlink(outside.path(), bin.join("2020-01-01")).unwrap();

	assert_eq!(prune(root.path(), day()), summary(1, 0));

	assert!(bin.join("2020-01-01").symlink_metadata().is_err());
	assert_eq!(
		fs::read(outside.path().join("keep.txt")).unwrap(),
		b"outside"
	);
}

/// Deleting an expired bucket does not follow a link inside it either.
#[cfg(unix)]
#[test]
fn a_link_inside_an_expired_bucket_is_not_followed() {
	let root = root();
	let outside = tempfile::tempdir().unwrap();
	fs::write(outside.path().join("keep.txt"), b"outside").unwrap();
	bucket_with_file(root.path(), "2020-01-01");
	std::os::unix::fs::symlink(
		outside.path(),
		root.path()
			.join(QUARANTINE_DIR)
			.join("2020-01-01")
			.join("link"),
	)
	.unwrap();

	assert_eq!(prune(root.path(), day()), summary(1, 0));

	assert!(!root.path().join(QUARANTINE_DIR).join("2020-01-01").exists());
	assert_eq!(
		fs::read(outside.path().join("keep.txt")).unwrap(),
		b"outside"
	);
}

/// A bin that is itself a link would be listed outside the root: it is left alone.
#[cfg(unix)]
#[test]
fn a_bin_that_is_a_link_is_not_pruned() {
	let root = root();
	let outside = tempfile::tempdir().unwrap();
	fs::create_dir(outside.path().join("2020-01-01")).unwrap();
	std::os::unix::fs::symlink(outside.path(), root.path().join(QUARANTINE_DIR)).unwrap();

	assert_eq!(
		prune(root.path(), day()),
		PruneSummary {
			removed: 0,
			adopted: 0,
			errors: 1,
			stopped: false,
		}
	);

	assert!(outside.path().join("2020-01-01").exists());
}

/// A pause stops the prune between entries, leaving what it did not reach as it was.
#[test]
fn a_prune_told_to_stop_leaves_the_remaining_entries_alone() {
	let root = root();
	bucket_with_file(root.path(), "2020-01-01");
	bucket_with_file(root.path(), "2020-01-02");
	let asked = Cell::new(0);

	let pruned = prune_bin(root.path(), day(), || {
		asked.set(asked.get() + 1);
		asked.get() == 1
	});

	assert_eq!(
		pruned,
		PruneSummary {
			removed: 1,
			adopted: 0,
			errors: 0,
			stopped: true,
		}
	);
	let left = fs::read_dir(root.path().join(QUARANTINE_DIR))
		.unwrap()
		.filter(|entry| entry.as_ref().unwrap().file_name() != DATED_MARKER)
		.count();
	assert_eq!(left, 1, "one bucket was not reached and stays");
}

/// Every filen-sync trash folder, at the root and nested, moves into the day's bucket at its tree
/// position with what it holds; the rest of the tree stays where it is.
#[test]
fn filen_syncs_trash_folders_move_into_the_days_bucket_at_their_tree_positions() {
	let root = root();
	let top = root.path().join(LEGACY_TRASH_DIR);
	fs::create_dir(&top).unwrap();
	fs::write(top.join("old.txt"), b"old").unwrap();
	let nested = root.path().join("Photos").join(LEGACY_TRASH_DIR);
	fs::create_dir_all(nested.join("deep")).unwrap();
	fs::write(nested.join("deep").join("x.jpg"), b"x").unwrap();
	fs::write(root.path().join("Photos").join("keep.jpg"), b"keep").unwrap();
	fs::write(root.path().join(".filen.trash.local.txt"), b"mine").unwrap();

	let migrated = migrate_legacy_trash(root.path(), day(), || true);

	assert_eq!(
		migrated,
		MigrateSummary {
			moved: 2,
			errors: 0,
			stopped: false,
		}
	);
	let bucket = root.path().join(QUARANTINE_DIR).join("2026-10-26");
	assert_eq!(
		fs::read(bucket.join(LEGACY_TRASH_DIR).join("old.txt")).unwrap(),
		b"old"
	);
	assert_eq!(
		fs::read(
			bucket
				.join("Photos")
				.join(LEGACY_TRASH_DIR)
				.join("deep")
				.join("x.jpg")
		)
		.unwrap(),
		b"x"
	);
	assert!(!top.exists());
	assert!(!nested.exists());
	assert_eq!(
		fs::read(root.path().join("Photos").join("keep.jpg")).unwrap(),
		b"keep"
	);
	assert_eq!(
		fs::read(root.path().join(".filen.trash.local.txt")).unwrap(),
		b"mine"
	);
	assert!(
		root.path().join(QUARANTINE_DIR).join(DATED_MARKER).is_dir(),
		"the bin the migration created is marked as filed by day"
	);
}

/// A tree with no filen-sync trash folder is left exactly as it was: no bin is created for it.
#[test]
fn a_tree_without_filen_syncs_trash_gets_no_bin() {
	let root = root();
	fs::create_dir(root.path().join("Photos")).unwrap();
	fs::write(root.path().join("Photos").join("a.jpg"), b"a").unwrap();

	assert_eq!(
		migrate_legacy_trash(root.path(), day(), || true),
		MigrateSummary::default()
	);
	assert!(!root.path().join(QUARANTINE_DIR).exists());
}

/// A missing root (an unmounted volume) has nothing to migrate, and nothing is created at it.
#[test]
fn a_missing_root_migrates_nothing_and_creates_nothing() {
	let parent = root();
	let missing = parent.path().join("unmounted");

	assert_eq!(
		migrate_legacy_trash(&missing, day(), || true),
		MigrateSummary::default()
	);
	assert!(!missing.exists());
}

/// A filen-sync trash folder inside the bin is the bin's: a second migration does not move it
/// again, or the one inside it.
#[test]
fn the_migration_leaves_what_the_bin_holds_alone() {
	let root = root();
	let binned = root
		.path()
		.join(QUARANTINE_DIR)
		.join("2026-10-01")
		.join(LEGACY_TRASH_DIR);
	fs::create_dir_all(&binned).unwrap();
	fs::write(binned.join("old.txt"), b"old").unwrap();

	assert_eq!(
		migrate_legacy_trash(root.path(), day(), || true),
		MigrateSummary::default()
	);
	assert_eq!(fs::read(binned.join("old.txt")).unwrap(), b"old");
	assert!(!root.path().join(QUARANTINE_DIR).join("2026-10-26").exists());
}

/// A symlink named like filen-sync's trash folder is moved as the link: what it points at is not
/// moved, read or changed. A link to a directory elsewhere is not walked into either.
#[cfg(unix)]
#[test]
fn a_link_named_like_filen_syncs_trash_moves_as_the_link() {
	let root = root();
	let outside = tempfile::tempdir().unwrap();
	fs::write(outside.path().join("target.txt"), b"elsewhere").unwrap();
	let outside_trash = outside.path().join(LEGACY_TRASH_DIR);
	fs::create_dir(&outside_trash).unwrap();
	fs::write(outside_trash.join("theirs.txt"), b"theirs").unwrap();
	std::os::unix::fs::symlink(outside.path(), root.path().join(LEGACY_TRASH_DIR)).unwrap();
	// A link to a directory holding a trash folder of its own: that folder is outside the tree.
	std::os::unix::fs::symlink(outside.path(), root.path().join("linked")).unwrap();

	let migrated = migrate_legacy_trash(root.path(), day(), || true);

	assert_eq!(migrated.moved, 1, "{migrated:?}");
	let moved = root
		.path()
		.join(QUARANTINE_DIR)
		.join("2026-10-26")
		.join(LEGACY_TRASH_DIR);
	assert!(
		moved.symlink_metadata().unwrap().file_type().is_symlink(),
		"the link itself was moved"
	);
	assert_eq!(fs::read_link(&moved).unwrap(), outside.path());
	assert!(root.path().join("linked").symlink_metadata().is_ok());
	assert_eq!(
		fs::read(outside.path().join("target.txt")).unwrap(),
		b"elsewhere"
	);
	assert_eq!(
		fs::read(outside_trash.join("theirs.txt")).unwrap(),
		b"theirs",
		"the trash folder behind a link was moved"
	);
}

/// A migration told to stop moves nothing more, and says so.
#[test]
fn a_migration_told_to_stop_leaves_the_rest_in_place() {
	let root = root();
	fs::create_dir(root.path().join(LEGACY_TRASH_DIR)).unwrap();

	assert_eq!(
		migrate_legacy_trash(root.path(), day(), || false),
		MigrateSummary {
			moved: 0,
			errors: 0,
			stopped: true,
		}
	);
	assert!(root.path().join(LEGACY_TRASH_DIR).is_dir());
	assert!(!root.path().join(QUARANTINE_DIR).exists());
}
