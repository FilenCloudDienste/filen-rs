use std::{
	path::Path,
	time::{Duration, UNIX_EPOCH},
};

use filen_types::fs::Uuid;
use tokio::sync::watch;

use super::*;
use crate::{
	helper::protocol::RootDir,
	identity::PlaceholderIdentity,
	test_support::{FakeEntry, FakeLocal, FakePlatform, Gated, LocalOp, PlatformCall, root_path},
};

/// The user whose root the tests unregister.
fn user() -> UserId {
	UserId::from(42)
}

const ACCOUNT_ROOT: Uuid = Uuid::from_u128(0x100);

fn file(n: u128) -> PlaceholderIdentity {
	PlaceholderIdentity::File {
		stable: Uuid::from_u128(n),
		version: Uuid::from_u128(n + 1000),
	}
}

fn dir(n: u128) -> PlaceholderIdentity {
	PlaceholderIdentity::Dir {
		uuid: Uuid::from_u128(n),
	}
}

/// A root holding one item of every kind the unregister tells apart.
fn every_kind() -> FakeLocal {
	let local = FakeLocal::new(PlaceholderIdentity::Root { root: ACCOUNT_ROOT });
	local.place_file(&root_path("clean.txt"), file(1), b"clean");
	local.place_file_with(&root_path("cloud.txt"), file(2), b"cloud", false);
	local.place_file(&root_path("edited.txt"), file(3), b"old");
	local.user_write(&root_path("edited.txt"), b"new").unwrap();
	local.user_create(&root_path("new.txt"), b"never uploaded");
	local.place_file(
		&root_path("conflict.txt"),
		PlaceholderIdentity::Detached {
			nonce: Uuid::from_u128(9),
		},
		b"mine",
	);
	local.place_file(&root_path("odd.txt"), dir(4), b"odd");
	local.user_create(&root_path("link"), b"");
	local.make_symlink(&root_path("link"));
	local.place_dir_unpopulated(&root_path("d"), dir(5));
	local.user_create(&root_path("d\\inside.txt"), b"inside");
	local.place_dir(&root_path("empty"), dir(6));
	local
}

fn kept(walked: &Walked) -> Vec<(String, KeptItemReason, bool)> {
	let mut kept: Vec<_> = walked
		.kept
		.iter()
		.map(|kept| (kept.path.to_string(), kept.reason, kept.revert.is_some()))
		.collect();
	kept.sort_by(|a, b| a.0.cmp(&b.0));
	kept
}

#[test]
fn every_item_holding_data_the_drive_lacks_is_kept() {
	let walked = walk(&every_kind(), &Stop::never()).unwrap();
	assert_eq!(
		kept(&walked),
		[
			(
				"conflict.txt".to_owned(),
				KeptItemReason::ConflictCopy,
				true
			),
			(
				"d\\inside.txt".to_owned(),
				KeptItemReason::NotUploaded,
				false
			),
			("edited.txt".to_owned(), KeptItemReason::Modified, true),
			("link".to_owned(), KeptItemReason::Unsupported, false),
			("new.txt".to_owned(), KeptItemReason::NotUploaded, false),
			(
				"odd.txt".to_owned(),
				KeptItemReason::UnknownPlaceholder,
				true
			),
		]
	);
	let clean: Vec<String> = walked
		.clean
		.iter()
		.map(|(path, _)| path.to_string())
		.collect();
	assert_eq!(clean, ["clean.txt"]);
	assert_eq!(walked.partial().count(), 0);
}

fn platform(local: &FakeLocal) -> Arc<FakePlatform> {
	Arc::new(FakePlatform::new(local.clone()))
}

/// A roots index naming [`user`]'s root at `root`, in a temporary state directory.
fn index(root: &Path) -> (tempfile::TempDir, RootsIndex) {
	let temp = tempfile::tempdir().unwrap();
	let mut index = RootsIndex::load(&StateDir::new(temp.path().join("state")).unwrap()).unwrap();
	index
		.upsert(RootEntry {
			user: user(),
			root: RootDir::parse(root.to_str().unwrap().to_owned()).unwrap(),
			data_dir: temp.path().join("data"),
			account_root: Some(ACCOUNT_ROOT),
			kept: Vec::new(),
			unregistered: false,
			branding: None,
		})
		.unwrap();
	(temp, index)
}

/// Unregisters the root at `root`, registered on `platform`, for [`user`].
async fn unregister(
	platform: &Arc<FakePlatform>,
	index: &mut RootsIndex,
	root: &Path,
	delete_hydrated: bool,
	date: &str,
) -> Result<RootOutcome, Stopped<crate::test_support::FakeRegistered>> {
	let registered = platform.register(root, ACCOUNT_ROOT).unwrap();
	unregister_root(
		platform,
		index,
		registered,
		Unregistering {
			user: user(),
			root,
			delete_hydrated,
			date: date.to_owned(),
			stop: &Stop::never(),
		},
	)
	.await
}

fn entry<'a>(entries: &'a [FakeEntry], path: &str) -> Option<&'a FakeEntry> {
	entries.iter().find(|entry| entry.path.to_string() == path)
}

#[tokio::test(start_paused = true)]
async fn unregistering_keeps_every_changed_item_as_an_ordinary_one_and_moves_the_root_aside() {
	let local = every_kind();
	let platform = platform(&local);
	let root = Path::new("/home/u/Filen");
	let (_temp, mut index) = index(root);
	let outcome = unregister(&platform, &mut index, root, true, "2026-11-12")
		.await
		.unwrap_or_else(|stopped| panic!("{:?}", stopped.error));
	assert_eq!(outcome.deleted_hydrated, 1);
	let aside = PathBuf::from("/home/u/Filen (not synced 2026-11-12)");
	let record = outcome.record.unwrap();
	assert_eq!(record.kept_count, 6);
	assert_eq!(
		record.place,
		KeptPlace::Settled {
			kept_in: Some(aside.clone())
		}
	);
	assert_eq!(index.entry(user()).unwrap().kept, [record]);
	assert_eq!(
		outcome.tidied,
		Tidied {
			root_removed: false,
			kept_in: Some(aside.clone()),
		}
	);
	assert!(local.entries().is_empty(), "moved with the root");
	let entries = platform.aside(&aside).unwrap();
	assert!(entries.iter().all(|entry| !entry.placeholder));
	for (path, bytes) in [
		("edited.txt", &b"new"[..]),
		("new.txt", b"never uploaded"),
		("conflict.txt", b"mine"),
		("odd.txt", b"odd"),
		("d\\inside.txt", b"inside"),
	] {
		assert_eq!(
			entry(&entries, path).map(|entry| &entry.bytes[..]),
			Some(bytes),
			"{path}"
		);
	}
	for gone in ["clean.txt", "cloud.txt", "empty"] {
		assert!(entry(&entries, gone).is_none(), "{gone}");
	}
	assert_eq!(
		platform.calls()[1..],
		[
			PlatformCall::ConnectRefusing(root.to_path_buf()),
			PlatformCall::Disconnect(root.to_path_buf()),
			PlatformCall::Unregister(root.to_path_buf()),
			PlatformCall::SetAside(aside),
		]
	);
}

#[tokio::test(start_paused = true)]
async fn clean_downloaded_files_stay_as_ordinary_files_unless_asked_to_go() {
	let local = FakeLocal::new(PlaceholderIdentity::Root { root: ACCOUNT_ROOT });
	local.place_file(&root_path("clean.txt"), file(1), b"clean");
	let platform = platform(&local);
	let root = Path::new("/r");
	let (_temp, mut index) = index(root);
	let outcome = unregister(&platform, &mut index, root, false, "d")
		.await
		.unwrap_or_else(|stopped| panic!("{:?}", stopped.error));
	assert_eq!(outcome.deleted_hydrated, 0);
	assert_eq!(outcome.record.unwrap().kept_count, 0);
	let aside = outcome.tidied.kept_in.unwrap();
	let entries = platform.aside(&aside).unwrap();
	assert_eq!(entry(&entries, "clean.txt").unwrap().bytes, b"clean");
	assert!(
		index.entry(user()).unwrap().kept.is_empty(),
		"nothing kept, nothing to report"
	);
}

#[tokio::test(start_paused = true)]
async fn an_empty_root_is_removed() {
	let local = FakeLocal::new(PlaceholderIdentity::Root { root: ACCOUNT_ROOT });
	local.place_file_with(&root_path("cloud.txt"), file(2), b"cloud", false);
	local.place_dir(&root_path("empty"), dir(6));
	let platform = platform(&local);
	let root = Path::new("/r");
	let (_temp, mut index) = index(root);
	let outcome = unregister(&platform, &mut index, root, true, "d")
		.await
		.unwrap_or_else(|stopped| panic!("{:?}", stopped.error));
	assert_eq!(
		outcome.tidied,
		Tidied {
			root_removed: true,
			kept_in: None,
		}
	);
}

#[tokio::test(start_paused = true)]
async fn a_changed_file_not_all_on_disk_stops_the_unregister_before_anything_is_deleted() {
	let local = FakeLocal::new(PlaceholderIdentity::Root { root: ACCOUNT_ROOT });
	local.place_file(&root_path("clean.txt"), file(1), b"clean");
	local.place_file(&root_path("edited.txt"), file(3), b"old data");
	local
		.user_write(&root_path("edited.txt"), b"new data")
		.unwrap();
	local.set_on_disk(&root_path("edited.txt"), 3);
	let platform = platform(&local);
	let root = Path::new("/r");
	let (_temp, mut index) = index(root);
	let Err(stopped) = unregister(&platform, &mut index, root, true, "d").await else {
		panic!("the unregister went ahead");
	};
	assert_eq!(stopped.error.code, ErrorCode::UnsyncedChanges);
	assert!(matches!(stopped.held, Held::Registered(_)));
	assert_eq!(platform.registration(root), Some(Some(ACCOUNT_ROOT)));
	let entries = local.entries();
	assert!(entry(&entries, "clean.txt").unwrap().placeholder);
	assert!(entry(&entries, "edited.txt").unwrap().placeholder);
	assert!(
		!platform
			.calls()
			.iter()
			.any(|call| matches!(call, PlatformCall::Unregister(_)))
	);
}

#[test]
fn a_file_edited_after_the_walk_is_kept_not_deleted() {
	let local = FakeLocal::new(PlaceholderIdentity::Root { root: ACCOUNT_ROOT });
	local.place_file(&root_path("clean.txt"), file(1), b"clean");
	let walked = walk(&local, &Stop::never()).unwrap();
	local
		.user_write(&root_path("clean.txt"), b"edited")
		.unwrap();
	let settled = settle(&local, walked, true).unwrap();
	assert_eq!(settled.deleted_hydrated, 0);
	assert_eq!(
		settled
			.kept
			.iter()
			.map(|kept| (kept.path.to_string(), kept.reason))
			.collect::<Vec<_>>(),
		[("clean.txt".to_owned(), KeptItemReason::Modified)]
	);
	let entries = local.entries();
	let kept = entry(&entries, "clean.txt").unwrap();
	assert_eq!(kept.bytes, b"edited");
	assert!(!kept.placeholder);
}

#[test]
fn the_date_is_utc_and_zero_padded() {
	assert_eq!(utc_date(UNIX_EPOCH), "1970-01-01");
	assert_eq!(
		utc_date(UNIX_EPOCH + Duration::from_secs(951_782_400)),
		"2000-02-29"
	);
	assert_eq!(
		utc_date(UNIX_EPOCH + Duration::from_secs(1_794_441_600 + 86_399)),
		"2026-11-12"
	);
}

#[test]
fn an_unregistered_root_holding_nothing_but_empty_directories_is_removed() {
	let temp = tempfile::tempdir().unwrap();
	let root = temp.path().join("Filen");
	std::fs::create_dir_all(root.join("a").join("b")).unwrap();
	assert_eq!(clear_plain_root(&root).unwrap(), Cleared::Gone);
	assert!(!root.exists());
}

#[test]
fn an_unregistered_root_that_is_not_there_is_gone_already() {
	let temp = tempfile::tempdir().unwrap();
	assert_eq!(
		clear_plain_root(&temp.path().join("Filen")).unwrap(),
		Cleared::Gone
	);
}

#[test]
fn an_unregistered_root_holding_files_is_renamed_aside_next_to_any_earlier_one() {
	let temp = tempfile::tempdir().unwrap();
	let root = temp.path().join("Filen");
	for _ in 0..2 {
		std::fs::create_dir_all(root.join("a").join("empty")).unwrap();
		std::fs::write(root.join("a").join("kept.txt"), b"kept").unwrap();
		std::fs::write(root.join("top.txt"), b"top").unwrap();
		assert_eq!(clear_plain_root(&root).unwrap(), Cleared::Left(2));
		let aside = aside_path(&root, "2026-11-12", path_taken).unwrap();
		set_plain_root_aside(&root, &aside).unwrap();
	}
	let first = temp.path().join("Filen (not synced 2026-11-12)");
	let second = temp.path().join("Filen (not synced 2026-11-12) 2");
	for aside in [&first, &second] {
		assert_eq!(
			std::fs::read(aside.join("a").join("kept.txt")).unwrap(),
			b"kept"
		);
		assert_eq!(std::fs::read(aside.join("top.txt")).unwrap(), b"top");
		assert!(!aside.join("a").join("empty").exists());
	}
	assert!(!root.exists());
}

#[test]
fn a_root_is_never_renamed_onto_something_already_there() {
	let temp = tempfile::tempdir().unwrap();
	let root = temp.path().join("Filen");
	std::fs::create_dir_all(&root).unwrap();
	std::fs::write(root.join("kept.txt"), b"kept").unwrap();
	let taken = temp.path().join("Filen (not synced d)");
	std::fs::create_dir_all(&taken).unwrap();
	let error = set_plain_root_aside(&root, &taken).unwrap_err();
	assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
	assert_eq!(std::fs::read(root.join("kept.txt")).unwrap(), b"kept");
	assert_eq!(std::fs::read_dir(&taken).unwrap().count(), 0);
}

#[tokio::test(start_paused = true)]
async fn a_directory_holding_a_name_the_core_cannot_spell_is_made_ordinary_with_it() {
	let local = FakeLocal::new(PlaceholderIdentity::Root { root: ACCOUNT_ROOT });
	local.place_dir(&root_path("d"), dir(5));
	local.add_unrepresentable(&root_path("d"));
	let platform = platform(&local);
	let root = Path::new("/r");
	let (_temp, mut index) = index(root);
	let outcome = unregister(&platform, &mut index, root, true, "d")
		.await
		.unwrap_or_else(|stopped| panic!("{:?}", stopped.error));
	assert_eq!(
		outcome.record.unwrap().kept,
		[KeptItem {
			path: "d".to_owned(),
			reason: KeptItemReason::UnrepresentableName,
		}]
	);
	assert!(local.ops().contains(&LocalOp::Reverted(root_path("d"))));
}

/// The error a revert of an item in a state the platform cannot revert was seen failing with.
fn tag_mismatch() -> LocalError {
	LocalError::Os(crate::os::OsError::Rejected {
		code: crate::os::cfapi::Hresult::from_win32(crate::os::cfapi::ERROR_REPARSE_TAG_MISMATCH)
			.get(),
	})
}

/// Whether the platform's unregister ran on `root`.
fn unregistered(platform: &FakePlatform, root: &Path) -> bool {
	platform
		.calls()
		.contains(&PlatformCall::Unregister(root.to_path_buf()))
}

#[tokio::test(start_paused = true)]
async fn a_changed_file_the_platform_cannot_make_ordinary_is_kept_as_a_copy_beside_it() {
	let local = FakeLocal::new(PlaceholderIdentity::Root { root: ACCOUNT_ROOT });
	local.place_file(&root_path("clean.txt"), file(1), b"clean");
	local.place_file(&root_path("edited.txt"), file(3), b"old");
	local.user_write(&root_path("edited.txt"), b"new").unwrap();
	local.place_dir(&root_path("d"), dir(5));
	local.place_file(&root_path("d\\inner.txt"), file(7), b"old");
	local
		.user_write(&root_path("d\\inner.txt"), b"inner")
		.unwrap();
	local.fail_revert(&root_path("edited.txt"), Some(tag_mismatch()));
	local.fail_revert(&root_path("d\\inner.txt"), Some(tag_mismatch()));
	let platform = platform(&local);
	let root = Path::new("/r");
	let (_temp, mut index) = index(root);
	let outcome = unregister(&platform, &mut index, root, false, "d")
		.await
		.unwrap_or_else(|stopped| panic!("{:?}", stopped.error));
	assert!(unregistered(&platform, root));
	let mut kept = outcome.record.unwrap().kept;
	kept.sort_by(|a, b| a.path.cmp(&b.path));
	assert_eq!(
		kept,
		[
			KeptItem {
				path: "d\\inner.old.txt".to_owned(),
				reason: KeptItemReason::Modified,
			},
			KeptItem {
				path: "edited.old.txt".to_owned(),
				reason: KeptItemReason::Modified,
			},
		]
	);
	let aside = outcome.tidied.kept_in.unwrap();
	let entries = platform.aside(&aside).unwrap();
	assert_eq!(entry(&entries, "edited.old.txt").unwrap().bytes, b"new");
	assert_eq!(entry(&entries, "d\\inner.old.txt").unwrap().bytes, b"inner");
	assert_eq!(entry(&entries, "clean.txt").unwrap().bytes, b"clean");
	assert!(entries.iter().all(|entry| !entry.placeholder));
}

/// A root holding `edited.txt`, changed, which the platform refuses to make ordinary.
fn unrevertable_edit() -> FakeLocal {
	let local = FakeLocal::new(PlaceholderIdentity::Root { root: ACCOUNT_ROOT });
	local.place_file(&root_path("edited.txt"), file(3), b"old");
	local.user_write(&root_path("edited.txt"), b"new").unwrap();
	local.fail_revert(&root_path("edited.txt"), Some(tag_mismatch()));
	local
}

/// Whether `path` is a name a copy is written under before it takes its own.
fn is_copy_name(path: &RootPath) -> bool {
	crate::names::is_upload_name(path.name().unwrap().as_str())
}

#[tokio::test(start_paused = true)]
async fn a_kept_files_copy_takes_its_old_name_only_once_whole() {
	let local = unrevertable_edit();
	local.user_create(&root_path("edited.old.txt"), b"the user's own");
	let platform = platform(&local);
	let root = Path::new("/r");
	let (_temp, mut index) = index(root);
	let outcome = unregister(&platform, &mut index, root, false, "d")
		.await
		.unwrap_or_else(|stopped| panic!("{:?}", stopped.error));
	let copies: Vec<LocalOp> = local
		.ops()
		.into_iter()
		.filter(|op| matches!(op, LocalOp::CopiedToPlain { .. } | LocalOp::Renamed { .. }))
		.collect();
	let [
		LocalOp::CopiedToPlain { from, to: copy },
		LocalOp::Renamed {
			from: renamed,
			to: named,
		},
	] = copies.as_slice()
	else {
		panic!("{copies:?}");
	};
	assert_eq!(from, &root_path("edited.txt"));
	assert!(is_copy_name(copy), "{copy}");
	assert_eq!(renamed, copy);
	assert_eq!(named, &root_path("edited.old.1.txt"));
	let entries = platform.aside(&outcome.tidied.kept_in.unwrap()).unwrap();
	assert_eq!(entry(&entries, "edited.old.1.txt").unwrap().bytes, b"new");
	assert_eq!(
		entry(&entries, "edited.old.txt").unwrap().bytes,
		b"the user's own"
	);
}

#[tokio::test(start_paused = true)]
async fn a_copy_that_cannot_take_its_old_name_stays_under_its_own_and_stops_the_unregister() {
	let local = unrevertable_edit();
	local.fail_rename(LocalError::Busy);
	let platform = platform(&local);
	let root = Path::new("/r");
	let (_temp, mut index) = index(root);
	let Err(stopped) = unregister(&platform, &mut index, root, false, "d").await else {
		panic!("the unregister went ahead");
	};
	assert_eq!(stopped.error.code, ErrorCode::Os);
	assert!(matches!(stopped.held, Held::Registered(_)));
	assert!(!unregistered(&platform, root));
	let entries = local.entries();
	assert!(entry(&entries, "edited.old.txt").is_none());
	let copy = entries
		.iter()
		.find(|entry| is_copy_name(&entry.path))
		.expect("the copy is under its own name");
	assert_eq!(copy.bytes, b"new");
	assert!(!copy.placeholder);
	assert_eq!(entry(&entries, "edited.txt").unwrap().bytes, b"new");
}

#[tokio::test(start_paused = true)]
async fn a_changed_file_that_can_be_neither_made_ordinary_nor_copied_stops_the_unregister() {
	let local = FakeLocal::new(PlaceholderIdentity::Root { root: ACCOUNT_ROOT });
	local.place_file(&root_path("edited.txt"), file(3), b"old");
	local.user_write(&root_path("edited.txt"), b"new").unwrap();
	local.fail_revert(&root_path("edited.txt"), Some(tag_mismatch()));
	local.fail_copy(&root_path("edited.txt"), Some(LocalError::Busy));
	let platform = platform(&local);
	let root = Path::new("/r");
	let (_temp, mut index) = index(root);
	let Err(stopped) = unregister(&platform, &mut index, root, true, "d").await else {
		panic!("the unregister went ahead");
	};
	assert_eq!(stopped.error.code, ErrorCode::Os);
	assert!(
		stopped.error.message.contains("edited.txt"),
		"{}",
		stopped.error.message
	);
	assert!(matches!(stopped.held, Held::Registered(_)));
	assert!(!unregistered(&platform, root));
	assert_eq!(local.entry(&root_path("edited.txt")).unwrap().bytes, b"new");
}

#[tokio::test(start_paused = true)]
async fn an_item_whose_state_cannot_be_read_is_left_as_it_is_and_reported_kept() {
	let local = FakeLocal::new(PlaceholderIdentity::Root { root: ACCOUNT_ROOT });
	local.place_file(&root_path("odd.txt"), file(3), b"odd");
	local.place_file(&root_path("edited.txt"), file(4), b"old");
	local.user_write(&root_path("edited.txt"), b"new").unwrap();
	local.place_dir(&root_path("d"), dir(5));
	local.user_create(&root_path("d\\new.txt"), b"never uploaded");
	local.fail_inspect(&root_path("odd.txt"), Some(tag_mismatch()));
	local.fail_list_dir(&root_path("d"), Some(tag_mismatch()));
	let platform = platform(&local);
	let root = Path::new("/r");
	let (_temp, mut index) = index(root);
	let outcome = unregister(&platform, &mut index, root, true, "d")
		.await
		.unwrap_or_else(|stopped| panic!("{:?}", stopped.error));
	assert!(unregistered(&platform, root));
	let mut kept = outcome.record.unwrap().kept;
	kept.sort_by(|a, b| a.path.cmp(&b.path));
	assert_eq!(
		kept,
		[
			KeptItem {
				path: "d".to_owned(),
				reason: KeptItemReason::UnknownPlaceholder,
			},
			KeptItem {
				path: "edited.txt".to_owned(),
				reason: KeptItemReason::Modified,
			},
			KeptItem {
				path: "odd.txt".to_owned(),
				reason: KeptItemReason::UnknownPlaceholder,
			},
		]
	);
	let entries = platform.aside(&outcome.tidied.kept_in.unwrap()).unwrap();
	for (path, bytes) in [
		("odd.txt", &b"odd"[..]),
		("edited.txt", b"new"),
		("d\\new.txt", b"never uploaded"),
	] {
		assert_eq!(
			entry(&entries, path).map(|entry| &entry.bytes[..]),
			Some(bytes),
			"{path}"
		);
	}
	assert!(local.ops().contains(&LocalOp::Reverted(root_path("d"))));
	assert!(
		!local
			.ops()
			.contains(&LocalOp::Reverted(root_path("odd.txt")))
	);
}

#[tokio::test(start_paused = true)]
async fn a_directory_above_a_kept_item_that_cannot_be_made_ordinary_stops_the_unregister() {
	let local = FakeLocal::new(PlaceholderIdentity::Root { root: ACCOUNT_ROOT });
	local.place_dir(&root_path("d"), dir(5));
	local.user_create(&root_path("d\\new.txt"), b"never uploaded");
	local.fail_revert(&root_path("d"), Some(tag_mismatch()));
	let platform = platform(&local);
	let root = Path::new("/r");
	let (_temp, mut index) = index(root);
	let Err(stopped) = unregister(&platform, &mut index, root, true, "d").await else {
		panic!("the unregister went ahead");
	};
	assert_eq!(stopped.error.code, ErrorCode::Os);
	assert!(
		stopped.error.message.contains('d'),
		"{}",
		stopped.error.message
	);
	assert!(!unregistered(&platform, root));
	assert_eq!(
		local.entry(&root_path("d\\new.txt")).unwrap().bytes,
		b"never uploaded"
	);
}

#[test]
fn a_file_edited_after_the_walk_whose_state_then_cannot_be_read_is_left_and_kept() {
	let local = FakeLocal::new(PlaceholderIdentity::Root { root: ACCOUNT_ROOT });
	local.place_file(&root_path("clean.txt"), file(1), b"clean");
	let walked = walk(&local, &Stop::never()).unwrap();
	local
		.user_write(&root_path("clean.txt"), b"edited")
		.unwrap();
	local.fail_inspect(&root_path("clean.txt"), Some(tag_mismatch()));
	let settled = settle(&local, walked, true).unwrap();
	assert_eq!(
		settled
			.kept
			.iter()
			.map(|kept| (kept.path.to_string(), kept.reason, kept.revert))
			.collect::<Vec<_>>(),
		[("clean.txt".to_owned(), KeptItemReason::Modified, None)]
	);
	assert_eq!(
		local.entry(&root_path("clean.txt")).unwrap().bytes,
		b"edited"
	);
}

/// A root holding two changed files whose data is not all on disk.
fn two_partial_edits() -> FakeLocal {
	let local = FakeLocal::new(PlaceholderIdentity::Root { root: ACCOUNT_ROOT });
	for (n, name) in [(1, "a.txt"), (2, "b.txt")] {
		local.place_file(&root_path(name), file(n), b"old data");
		local.user_write(&root_path(name), b"new data").unwrap();
		local.set_on_disk(&root_path(name), 3);
	}
	local
}

/// A stop and what asks for it.
fn stop() -> (watch::Sender<bool>, Stop) {
	let (ask, stopping) = watch::channel(false);
	(ask, Stop::new(stopping))
}

fn hydrated(local: &FakeLocal) -> usize {
	local
		.ops()
		.iter()
		.filter(|op| matches!(op, LocalOp::Hydrated(_)))
		.count()
}

#[test]
fn a_walk_asked_to_stop_lists_nothing() {
	let local = every_kind();
	// A listing would fail the walk otherwise.
	local.fail_list_dir(&RootPath::root(), Some(LocalError::Busy));
	let (ask, stop) = stop();
	ask.send_replace(true);
	assert!(matches!(
		walk(&local, &stop),
		Err(WalkError::Stopping(Stopping))
	));
}

#[test]
fn fetching_asked_to_stop_fetches_nothing() {
	let local = two_partial_edits();
	let walked = walk(&local, &Stop::never()).unwrap();
	let (ask, stop) = stop();
	ask.send_replace(true);
	assert_eq!(hydrate_kept(&local, &walked, &stop), Err(Stopping));
	assert_eq!(hydrated(&local), 0);
}

#[test]
fn a_stop_during_a_fetch_fetches_no_further_file() {
	let local = two_partial_edits();
	let walked = walk(&local, &Stop::never()).unwrap();
	assert_eq!(walked.partial().count(), 2);
	let gate = local.gate(Gated::Hydrate);
	let (ask, stop) = stop();
	let fetching = {
		let local = local.clone();
		std::thread::spawn(move || hydrate_kept(&local, &walked, &stop))
	};
	gate.wait_entered();
	ask.send_replace(true);
	gate.open();
	assert_eq!(fetching.join().unwrap(), Err(Stopping));
	assert_eq!(hydrated(&local), 1);
}

#[tokio::test(start_paused = true)]
async fn an_unregister_asked_to_stop_before_it_starts_leaves_the_root_as_it_was() {
	let local = two_partial_edits();
	let platform = platform(&local);
	let root = Path::new("/home/u/Filen");
	let (_temp, mut index) = index(root);
	let registered = platform.register(root, ACCOUNT_ROOT).unwrap();
	let (ask, stop) = stop();
	ask.send_replace(true);
	let Err(stopped) = unregister_root(
		&platform,
		&mut index,
		registered,
		Unregistering {
			user: user(),
			root,
			delete_hydrated: true,
			date: "2026-11-26".to_owned(),
			stop: &stop,
		},
	)
	.await
	else {
		panic!("the unregister went ahead");
	};
	assert_eq!(stopped.error.code, ErrorCode::ShuttingDown);
	assert!(matches!(stopped.held, Held::Registered(_)));
	assert_eq!(
		platform.calls(),
		[PlatformCall::Register {
			root: root.to_path_buf(),
			account_root: ACCOUNT_ROOT
		}]
	);
}

#[tokio::test(start_paused = true)]
async fn a_stop_during_the_walk_of_the_refusing_root_disconnects_it_and_changes_nothing() {
	let local = every_kind();
	let platform = platform(&local);
	let root = Path::new("/home/u/Filen");
	let (_temp, mut index) = index(root);
	let before = local.entries();
	let registered = platform.register(root, ACCOUNT_ROOT).unwrap();
	let gate = local.gate(Gated::ListDir);
	let (ask, stop) = stop();
	let unregistering = unregister_root(
		&platform,
		&mut index,
		registered,
		Unregistering {
			user: user(),
			root,
			delete_hydrated: true,
			date: "2026-11-26".to_owned(),
			stop: &stop,
		},
	);
	let stopping = async {
		{
			let gate = gate.clone();
			blocking(move || gate.wait_entered()).await;
		}
		ask.send_replace(true);
		gate.open();
	};
	let (unregistered, ()) = tokio::join!(unregistering, stopping);
	let Err(stopped) = unregistered else {
		panic!("the unregister went ahead");
	};
	assert_eq!(stopped.error.code, ErrorCode::ShuttingDown);
	assert!(matches!(stopped.held, Held::Registered(_)));
	assert_eq!(
		platform.calls(),
		[
			PlatformCall::Register {
				root: root.to_path_buf(),
				account_root: ACCOUNT_ROOT
			},
			PlatformCall::ConnectRefusing(root.to_path_buf()),
			PlatformCall::Disconnect(root.to_path_buf()),
		]
	);
	assert_eq!(local.entries(), before);
	assert!(index.entry(user()).unwrap().kept.is_empty());
	assert!(
		!local
			.ops()
			.iter()
			.any(|op| matches!(op, LocalOp::Reverted(_) | LocalOp::Hydrated(_)))
	);
}
