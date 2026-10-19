use std::{
	borrow::Cow,
	ffi::OsStr,
	path::{Path, PathBuf},
};

use chrono::{DateTime, Utc};
use filen_types::{api::v3::dir::color::DirColor, auth::FileEncryptionVersion, fs::StableUuid};
use notify::event::{
	CreateKind, DataChange, Flag, MetadataKind, ModifyKind, RemoveKind, RenameMode,
};

use super::{FullPassReason as R, *};
use crate::{
	crypto::file::FileKey,
	fs::{
		dir::{DecryptedDirectoryMeta, cache::CacheableDir},
		file::{cache::CacheableFile, meta::DecryptedFileMeta},
	},
};

const ROOT: &str = "/sync/root";

fn fs_event(kind: EventKind, paths: &[&str]) -> Event {
	Event {
		kind,
		paths: paths.iter().map(PathBuf::from).collect(),
		attrs: Default::default(),
	}
}

/// A kernel-dropped notice: pathless, and flagged for a rescan.
fn rescan() -> Event {
	let mut event = fs_event(EventKind::Other, &[]);
	event.attrs.set_flag(Flag::Rescan);
	event
}

/// The FSEvents notice that the watch root itself was renamed or removed.
fn root_changed() -> Event {
	let mut event = fs_event(
		EventKind::Modify(ModifyKind::Name(RenameMode::From)),
		&[ROOT],
	);
	event.attrs.set_info(ROOT_CHANGED_INFO);
	event
}

/// A WATCHED pair whose changelists are sized for a tree big enough not to collapse.
fn sized_pair() -> PairChanges {
	let changes = PairChanges::new();
	changes.cover_local();
	changes.note_tree_size(4_000);
	changes
}

fn note(changes: &PairChanges, event: &Event) {
	changes.note_local_event(Path::new(ROOT), event, |_| false);
}

fn create(name: &str) -> Event {
	fs_event(
		EventKind::Create(CreateKind::File),
		&[&format!("{ROOT}/{name}")],
	)
}

/// The carry-over set is not a changelist entry, and the cap must not touch it: [`dirty_cap`]
/// scales with the tree as the pair last RECORDED it, which is nothing before a first sync, so
/// a plan of any size would otherwise collapse the next pass to a whole-tree read and report
/// work as owing that the pass had in fact applied.
#[test]
fn the_carry_over_set_joins_the_next_pass_outside_the_cap() {
	let changes = PairChanges::new();
	changes.cover_local();
	// Nothing recorded yet, so `dirty_cap` sits at its floor of one path.
	let owed: BTreeSet<String> = (0..50).map(|i| format!("owed{i:02}.txt")).collect();
	changes.note_owed(owed.iter().map(String::as_str));

	let (paths, full) = taken(&changes);
	assert_eq!(
		paths.len(),
		owed.len(),
		"the cap swallowed the carry-over set"
	);
	assert_eq!(
		full, None,
		"paths the last plan owes are something to look at, not a reason to read everything"
	);
	// Taken with the lists, like everything else a pass consumes.
	assert_eq!(taken(&changes), (Vec::<String>::new(), None));
}

/// A pass rebuilds the set from its own plan, so recording one REPLACES the last.
#[test]
fn each_pass_replaces_what_the_last_one_left_owing() {
	let changes = sized_pair();
	changes.note_owed(["first.txt"]);
	changes.note_owed(["second.txt"]);
	assert_eq!(taken(&changes).0, vec!["second.txt".to_string()]);
}

/// The carry-over set is bounded all the same, by the absolute cap rather than the tree-scaled
/// one: kept, it sits in memory for as long as the pair idles, and after a first sync it names
/// every item in the tree. Up to the cap it is carried path by path; one past it, the next pass
/// reads both sides whole instead and no set is kept.
#[test]
fn a_carry_over_set_past_the_cap_is_a_whole_read_and_keeps_nothing() {
	let paths: Vec<String> = (0..=DIRTY_CAP).map(|i| format!("owed{i:06}.txt")).collect();

	// A plan of uploads names every path twice, as both endpoints of its action: the cap counts
	// paths, not mentions.
	let at_cap = sized_pair();
	at_cap.note_owed(
		paths
			.iter()
			.take(DIRTY_CAP)
			.flat_map(|path| [path.as_str(), path.as_str()]),
	);
	let (carried, full) = taken(&at_cap);
	assert_eq!(
		(carried.len(), full),
		(7_700, None),
		"at the cap the set is carried"
	);

	let past = sized_pair();
	past.note_owed(paths.iter().map(String::as_str));
	assert_eq!(
		past.state().owed.len(),
		0,
		"one past the cap, no set is kept"
	);
	assert!(
		!past.is_idle(),
		"the whole read owed instead is a pass to run"
	);
	let (carried, full) = taken(&past);
	assert_eq!((carried.len(), full), (0, Some(R::CarriedOver)));
}

/// A removal of `uuid`, as the cache announces one.
fn removed(uuid: u128) -> CacheEvent<'static> {
	cache_event(
		Some(u64::try_from(uuid).expect("a test uuid fits an event id")),
		CacheEventType::File(FileEvent::Removed(Uuid::from_u128(uuid))),
	)
}

/// The delta entries a pass takes for those removals — minted the one way a `Gone` can be.
fn minted_delta(uuids: &[u128]) -> Vec<RemoteDeltaEntry> {
	let changes = sized_pair();
	let events: Vec<CacheEvent<'static>> = uuids.iter().copied().map(removed).collect();
	changes.note_remote_batch(&mut events.iter());
	changes.take().take_remote()
}

/// An interrupted pass hands BOTH lists back, and its announced changes lead the ones that
/// arrived while it ran — the order the cache dispatched them in, which is the order the next
/// pass applies them to its derived view.
#[test]
fn an_interrupted_passs_announced_changes_come_back_ahead_of_the_newer_ones() {
	let handed = minted_delta(&[100, 101]);
	let newer = [removed(102)];
	let changes = sized_pair();
	changes.note_owed(["gone.txt"]);
	changes.note_owed_remote(handed.clone());
	changes.note_remote_batch(&mut newer.iter());

	let mut scope = changes.take();
	assert_eq!(
		scope.full_pass_reason(10),
		None,
		"a hand-back is something to look at, not a reason to read everything"
	);
	assert_eq!(
		scope.take_remote(),
		[handed, minted_delta(&[102])].concat(),
		"the replayed changes must lead the ones that arrived while the pass ran"
	);
	assert_eq!(
		scope.take_local(),
		BTreeSet::from(["gone.txt".to_string()]),
		"and the paths that pass's plan owed come with them"
	);
	// Taken with the lists: they belong to the pass that took them now.
	assert!(
		changes.take().take_remote().is_empty(),
		"a handed-back change was replayed to two passes"
	);
}

/// And the cap does not touch them, for the reason it does not touch the owed paths: the
/// hand-back is the previous pass's own list, so collapsing on it would make an interrupted
/// first sync — where `dirty_cap` sits at its floor — read everything for ever.
#[test]
fn the_handed_back_changes_join_the_next_pass_outside_the_cap() {
	let handed = minted_delta(&(200..250).collect::<Vec<u128>>());
	let changes = PairChanges::new();
	changes.cover_local();

	changes.note_owed_remote(handed.clone());
	let mut scope = changes.take();
	assert_eq!(
		scope.full_pass_reason(10),
		None,
		"the cap collapsed a pass onto its own hand-back"
	);
	assert_eq!(scope.take_remote().len(), handed.len());
}

/// A run of cut-short passes hands the same changes on with whatever arrived between them, so
/// the hand-back is bounded like the lists are: past the cap it collapses to a whole read
/// rather than growing a queue nothing drains.
#[test]
fn a_hand_back_past_the_cap_collapses_to_a_whole_read() {
	let one = minted_delta(&[300]).remove(0);
	let changes = sized_pair();

	changes.note_owed_remote(vec![one; DIRTY_CAP + 1]);
	let scope = changes.take();
	assert_eq!(
		scope.full_pass_reason(10),
		Some(R::RemoteOverflow),
		"a hand-back past the cap must collapse, not queue"
	);
	assert!(
		scope.remote.is_empty(),
		"a collapsed list hands the pass nothing to narrow itself with"
	);
}

/// What a wake has to look at, and what the pass may therefore skip entirely — asked without
/// taking any of it.
#[test]
fn a_pair_with_nothing_announced_is_idle() {
	let changes = sized_pair();
	assert!(
		changes.is_idle(),
		"a quiet watched pair has nothing to look at"
	);

	note(&changes, &create("a.txt"));
	assert!(!changes.is_idle(), "a changed path is something to look at");
	assert!(!changes.is_idle(), "and asking took nothing");
	let _ = changes.take();
	assert!(changes.is_idle(), "the pass that ran took it");

	changes.note_remote_batch(&mut [removed(1)].iter());
	assert!(
		!changes.is_idle(),
		"an announced remote change is something to look at"
	);
	let _ = changes.take();

	changes.note_owed(["owed.txt"]);
	assert!(
		!changes.is_idle(),
		"a path the last plan left owing is something to look at"
	);
	let _ = changes.take();

	changes.note_owed_remote(minted_delta(&[2]));
	assert!(
		!changes.is_idle(),
		"a change a cut-short pass handed back is something to look at"
	);
	let _ = changes.take();

	changes.force(FullPassReason::SafetyNet);
	assert!(!changes.is_idle(), "a forced whole read is not idle");
	let _ = changes.take();
	assert!(changes.is_idle());

	let unwatched = PairChanges::new();
	assert!(
		!unwatched.is_idle(),
		"with no watcher an empty list is no evidence, so there is always something to read"
	);
}

/// Re-reading two trees nothing has changed in only holds the identical batch again, so a
/// hold by itself gives a wake nothing to do.
#[test]
fn a_held_batch_alone_leaves_the_pair_idle() {
	let changes = sized_pair();
	changes.note_deletion_hold();
	assert!(changes.is_idle());
}

/// A wake that skips its pass leaves the hold for the next pass that runs, which must read
/// whole to offer the same batch again.
#[test]
fn an_idle_check_leaves_the_hold_for_the_next_pass() {
	let changes = sized_pair();
	changes.note_deletion_hold();
	assert!(changes.is_idle());
	assert!(changes.is_idle());
	assert_eq!(taken(&changes).1, Some(R::DeletionHold));
}

/// A change on either side while a batch is held makes the pair due, and the pass it gets reads
/// whole for the hold — a narrowed read could not reproduce the batch.
#[test]
fn a_held_batch_with_an_announced_change_reads_whole() {
	let local = sized_pair();
	local.note_deletion_hold();
	note(&local, &create("a.txt"));
	assert!(!local.is_idle(), "a local change");
	assert_eq!(local.take().full_pass_reason(10), Some(R::DeletionHold));

	let remote = sized_pair();
	remote.note_deletion_hold();
	remote.note_remote_batch(&mut [removed(1)].iter());
	assert!(!remote.is_idle(), "a remote change");
	assert_eq!(remote.take().full_pass_reason(10), Some(R::DeletionHold));
}

/// The safety net still comes due with a hold pending, and is the reason its pass reports: kept
/// in `forced`, the hold would have swallowed it.
#[test]
fn the_safety_net_with_a_hold_pending_reads_whole_for_itself() {
	let changes = sized_pair();
	changes.note_deletion_hold();
	changes.force(R::SafetyNet);
	assert!(!changes.is_idle());
	assert_eq!(taken(&changes).1, Some(R::SafetyNet));
}

/// The pass that runs takes the hold with everything else; only a pass that holds again
/// records it again.
#[test]
fn a_pass_takes_the_hold() {
	let changes = sized_pair();
	changes.note_deletion_hold();
	note(&changes, &create("a.txt"));
	assert_eq!(taken(&changes).1, Some(R::DeletionHold));
	assert_eq!(taken(&changes), (Vec::new(), None));
}

/// What a change-scoped pass asks before it trusts an absence: has anything already made the
/// next pass a whole-tree one, which would say its own narrowed read may be missing something.
#[test]
fn a_pending_whole_read_is_visible_while_a_pass_is_still_reading() {
	let changes = sized_pair();
	assert!(!changes.full_pending());

	changes.force(FullPassReason::ConflictResolved);
	assert!(changes.full_pending());
	// Taken with the lists, so it belongs to the pass that took it and not to the one after.
	let _ = changes.take();
	assert!(!changes.full_pending());

	changes.uncover_local();
	assert!(
		changes.full_pending(),
		"with no watcher recording them, every pass reads the local side whole"
	);
}

/// The local paths a pass would take, and the reason it cannot narrow down.
fn taken(changes: &PairChanges) -> (Vec<String>, Option<FullPassReason>) {
	let scope = changes.take();
	(scope.local.iter().cloned().collect(), scope.full)
}

pub(crate) fn dt(ms: i64) -> DateTime<Utc> {
	DateTime::from_timestamp_millis(ms).expect("a valid timestamp")
}

pub(crate) fn file_key() -> FileKey {
	FileKey::from_str_with_version(&"a".repeat(64), FileEncryptionVersion::V3).expect("a valid key")
}

pub(crate) fn cacheable_file(uuid: Uuid, parent: Uuid, name: &str) -> CacheableFile<'static> {
	CacheableFile {
		uuid,
		stable_uuid: StableUuid::new_for_test(uuid),
		parent,
		chunks_size: 1024,
		chunks: 1,
		favorited: false,
		region: Cow::Borrowed("de-1"),
		bucket: Cow::Borrowed("bucket-a"),
		timestamp: dt(0),
		name: Cow::Owned(name.to_string()),
		size: 7,
		mime: Cow::Borrowed("text/plain"),
		key: file_key(),
		last_modified: dt(1_234),
		created: None,
		hash: None,
	}
}

pub(crate) fn cacheable_dir(uuid: Uuid, parent: Uuid, name: &str) -> CacheableDir<'static> {
	CacheableDir {
		uuid,
		parent,
		color: DirColor::Default,
		favorited: false,
		timestamp: dt(0),
		name: Cow::Owned(name.to_string()),
		created: Some(dt(99)),
	}
}

pub(crate) fn cache_event(id: Option<u64>, event: CacheEventType<'static>) -> CacheEvent<'static> {
	CacheEvent { id, event }
}

/// Every kind that names a changed path is collected, under the same keys the scan produces;
/// non-mutating access is noise, except the close of a WRITE, which is how inotify announces a
/// finished write.
#[test]
fn every_mutating_event_names_paths_and_reads_are_dropped() {
	let changes = sized_pair();
	let mutating = [
		("create", EventKind::Create(CreateKind::File)),
		(
			"data",
			EventKind::Modify(ModifyKind::Data(DataChange::Content)),
		),
		(
			"meta",
			EventKind::Modify(ModifyKind::Metadata(MetadataKind::WriteTime)),
		),
		(
			"rename",
			EventKind::Modify(ModifyKind::Name(RenameMode::Any)),
		),
		("modify-any", EventKind::Modify(ModifyKind::Any)),
		("remove", EventKind::Remove(RemoveKind::File)),
		("any", EventKind::Any),
		("other", EventKind::Other),
		(
			"close-write",
			EventKind::Access(AccessKind::Close(AccessMode::Write)),
		),
	];
	for (name, kind) in mutating {
		note(&changes, &fs_event(kind, &[&format!("{ROOT}/{name}")]));
	}
	for kind in [
		EventKind::Access(AccessKind::Read),
		EventKind::Access(AccessKind::Open(AccessMode::Read)),
		EventKind::Access(AccessKind::Close(AccessMode::Read)),
		EventKind::Access(AccessKind::Any),
	] {
		note(&changes, &fs_event(kind, &[&format!("{ROOT}/noise")]));
	}

	let (paths, full) = taken(&changes);
	assert_eq!(full, None, "nothing was lost, so nothing may collapse");
	let mut expected: Vec<String> = mutating.iter().map(|(name, _)| name.to_string()).collect();
	expected.sort();
	assert_eq!(paths, expected, "a read must not dirty a path");
}

/// A directory event names the directory, and the root itself is the empty key the pair's own
/// row uses — both are paths to LOOK at, which is all this type can say.
#[test]
fn a_directory_and_the_root_itself_are_ordinary_keys() {
	let changes = sized_pair();
	note(
		&changes,
		&fs_event(
			EventKind::Create(CreateKind::Folder),
			&[&format!("{ROOT}/sub")],
		),
	);
	note(
		&changes,
		&fs_event(
			EventKind::Modify(ModifyKind::Metadata(MetadataKind::WriteTime)),
			&[ROOT],
		),
	);
	assert_eq!(
		taken(&changes),
		(vec![String::new(), "sub".to_string()], None)
	);
}

/// The collapse rows of the local table: a kernel drop, the root changing, and a path that
/// cannot be keyed. Each is sticky until a pass takes it, and each drops the paths it can no
/// longer narrow.
#[test]
fn a_lost_local_event_collapses_the_set_and_stays_collapsed() {
	for (label, lost, reason) in [
		("rescan", rescan(), R::KernelDropped),
		("root changed", root_changed(), R::RootChanged),
		(
			"outside the root",
			fs_event(EventKind::Create(CreateKind::File), &["/elsewhere/x.txt"]),
			R::UnmappedLocalPath,
		),
	] {
		let changes = sized_pair();
		note(&changes, &create("a.txt"));
		note(&changes, &lost);
		// A later ordinary event cannot un-collapse it.
		note(&changes, &create("b.txt"));

		let (paths, full) = taken(&changes);
		assert_eq!(full, Some(reason), "{label}");
		assert!(paths.is_empty(), "{label}: no path can narrow this pass");
		// Taken means taken: the next pass starts clean.
		assert_eq!(taken(&changes), (Vec::new(), None), "{label}");
	}
}

/// The cap scales with the tree the last full pass saw, is never zero, and never exceeds the
/// absolute ceiling.
#[test]
fn the_dirty_cap_is_a_quarter_of_the_tree_within_bounds() {
	assert_eq!(
		dirty_cap(0),
		1,
		"an unknown size must still narrow one path"
	);
	assert_eq!(dirty_cap(4), 1);
	assert_eq!(dirty_cap(400), 100);
	assert_eq!(dirty_cap(usize::MAX), 7_700);
}

/// Past the cap the local set collapses rather than growing without bound.
#[test]
fn too_many_local_paths_collapse_to_a_full_pass() {
	let changes = PairChanges::new();
	changes.cover_local();
	changes.note_tree_size(40); // cap: 10
	for i in 0..10 {
		note(&changes, &create(&format!("f{i}")));
	}
	let scope = changes.take();
	assert_eq!(scope.full, None, "the cap itself is not an overflow");
	assert_eq!(scope.sizes().0, 10);

	for i in 0..11 {
		note(&changes, &create(&format!("f{i}")));
	}
	assert_eq!(taken(&changes).1, Some(R::LocalOverflow));
}

/// The engine's own staging writes are dropped from the set, as they are from the wake-up.
#[test]
fn staging_writes_do_not_dirty_a_path() {
	let changes = sized_pair();
	let staged = format!("{ROOT}/sub/x.filendl");
	let real = format!("{ROOT}/sub/x.txt");
	changes.note_local_event(
		Path::new(ROOT),
		&fs_event(EventKind::Create(CreateKind::File), &[&staged, &real]),
		|path| path.extension() == Some(OsStr::new("filendl")),
	);
	assert_eq!(taken(&changes), (vec!["sub/x.txt".to_string()], None));
}

/// Each remote event shape becomes the change the remote view is built from — and a removal is
/// the only shape that can say an item is gone.
#[test]
fn a_remote_batch_becomes_the_projection_the_view_reads() {
	let changes = sized_pair();
	let file = Uuid::from_u128(1);
	let dir = Uuid::from_u128(2);
	let parent = Uuid::from_u128(3);
	let successor = Uuid::from_u128(4);
	let events = [
		cache_event(
			Some(10),
			CacheEventType::File(FileEvent::New(cacheable_file(file, parent, "a.txt"))),
		),
		// A resync synthetic carries no drive message id.
		cache_event(
			None,
			CacheEventType::Dir(DirEvent::New(cacheable_dir(dir, parent, "sub"))),
		),
		cache_event(
			Some(11),
			CacheEventType::File(FileEvent::Trashed {
				uuid: file,
				stable_uuid: StableUuid::new_for_test(file),
				new_uuid: Some(successor),
			}),
		),
		cache_event(Some(12), CacheEventType::Dir(DirEvent::Removed(dir))),
		// Nothing the engine reads.
		cache_event(
			Some(13),
			CacheEventType::Dir(DirEvent::ColorChanged {
				uuid: dir,
				color: DirColor::Blue,
			}),
		),
		cache_event(Some(14), CacheEventType::NoOp),
		cache_event(Some(15), CacheEventType::Global(GlobalEvent::TrashEmpty)),
	];
	changes.note_remote_batch(&mut events.iter());

	let scope = changes.take();
	assert_eq!(scope.full, None);
	assert_eq!(
		scope.sizes().1,
		4,
		"a colour change, a no-op and an emptied trash are not changes to this tree"
	);

	let RemoteChange::Upsert(item) = &scope.remote[0].change else {
		panic!("a new file must upsert the view");
	};
	assert_eq!(scope.remote[0].id, Some(10));
	assert_eq!(
		(item.uuid, item.parent, item.name.as_str(), item.size),
		(file, parent, "a.txt", 7)
	);
	assert!(
		item.stable_uuid.is_some(),
		"a file carries its whole-life id"
	);
	assert_eq!(item.modified_millis, 1_234);

	let RemoteChange::Upsert(item) = &scope.remote[1].change else {
		panic!("a new directory must upsert the view");
	};
	assert_eq!(
		scope.remote[1].id, None,
		"a synthetic has no id to align by"
	);
	assert_eq!(item.stable_uuid, None, "a directory has no whole-life id");
	assert_eq!((item.size, item.modified_millis), (0, 99));

	let RemoteChange::Gone(gone) = &scope.remote[2].change else {
		panic!("a trash must be an absence");
	};
	assert_eq!(gone.uuid(), file);
	assert_eq!(
		gone.successor(),
		Some(successor),
		"a versioning edit re-mints the file rather than deleting it"
	);

	let RemoteChange::Gone(gone) = &scope.remote[3].change else {
		panic!("a removed directory must be an absence");
	};
	assert_eq!((gone.uuid(), gone.successor()), (dir, None));
}

/// A metadata patch is a rename, with the content stamps that ride along for a file and none
/// for a directory — and it names no parent, because the item did not move.
#[test]
fn a_metadata_patch_renames_without_moving() {
	let changes = sized_pair();
	let file = Uuid::from_u128(1);
	let dir = Uuid::from_u128(2);
	let events = [
		cache_event(
			Some(1),
			CacheEventType::File(FileEvent::MetadataChanged {
				uuid: file,
				meta: DecryptedFileMeta {
					name: Cow::Borrowed("new.txt"),
					size: 12,
					mime: Cow::Borrowed("text/plain"),
					key: file_key(),
					last_modified: dt(500),
					created: None,
					hash: None,
				},
			}),
		),
		cache_event(
			Some(2),
			CacheEventType::Dir(DirEvent::MetadataChanged {
				uuid: dir,
				meta: DecryptedDirectoryMeta {
					name: Cow::Borrowed("renamed"),
					created: None,
				},
			}),
		),
	];
	changes.note_remote_batch(&mut events.iter());

	let scope = changes.take();
	assert_eq!(
		scope.remote[0].change,
		RemoteChange::Renamed {
			uuid: file,
			name: "new.txt".to_string(),
			content: Some(FileContent {
				size: 12,
				hash: None,
				modified_millis: 500,
			}),
		}
	);
	assert_eq!(
		scope.remote[1].change,
		RemoteChange::Renamed {
			uuid: dir,
			name: "renamed".to_string(),
			content: None,
		}
	);
}

/// An account-wide wipe cannot be described as a delta, and past the cap the delta collapses.
#[test]
fn a_wipe_or_an_overflow_collapses_the_delta() {
	let changes = sized_pair();
	let wipe = [cache_event(
		Some(1),
		CacheEventType::Global(GlobalEvent::DeleteAll),
	)];
	changes.note_remote_batch(&mut wipe.iter());
	assert_eq!(changes.take().full, Some(R::RemoteWiped));

	let changes = PairChanges::new();
	changes.cover_local();
	changes.note_tree_size(40); // cap: 10
	let batch: Vec<CacheEvent<'static>> = (0..11)
		.map(|i| {
			cache_event(
				Some(i),
				CacheEventType::Dir(DirEvent::Removed(Uuid::from_u128(u128::from(i) + 1))),
			)
		})
		.collect();
	changes.note_remote_batch(&mut batch.iter());
	assert_eq!(changes.take().full, Some(R::RemoteOverflow));
}

/// With no filesystem watcher attached, nothing is recording local changes, so an empty local
/// list means "nobody looked" rather than "nothing changed" — every pass reads the local side
/// whole. A watcher that starts clears it; one that ends brings it back.
#[test]
fn an_unwatched_pair_never_narrows_its_local_half() {
	let changes = PairChanges::new();
	changes.note_tree_size(4_000);
	assert_eq!(
		changes.take().full_pass_reason(10),
		Some(R::LocalEventsUnwatched),
		"a pair nothing is watching cannot narrow a pass down"
	);

	changes.cover_local();
	note(&changes, &create("a.txt"));
	assert_eq!(
		changes.take().full_pass_reason(10),
		None,
		"a watcher that covers the tree is what makes the list evidence"
	);

	changes.uncover_local();
	assert_eq!(
		changes.take().full_pass_reason(10),
		Some(R::LocalEventsUnwatched),
		"a watch that ended stops being evidence for what did NOT change"
	);
}

/// A rule file is not a path to look at: its contents decide what is hidden for its whole
/// directory and below, on both sides, so a change to one on either side collapses that side
/// to a whole-tree read. What the notification cannot show — a removal, which names a uuid and
/// no name — is left to the consuming pass, which holds the baseline.
#[test]
fn a_rule_file_change_collapses_its_side() {
	// Local: at the root, and nested.
	for rel in [FILENIGNORE, "proj/sub/.filenignore"] {
		let changes = sized_pair();
		note(&changes, &create("a.txt"));
		note(
			&changes,
			&fs_event(
				EventKind::Modify(ModifyKind::Data(DataChange::Content)),
				&[&format!("{ROOT}/{rel}")],
			),
		);
		let (paths, full) = taken(&changes);
		assert_eq!(full, Some(R::RulesChanged), "{rel}");
		assert!(
			paths.is_empty(),
			"{rel}: no path set describes a rules change"
		);
	}

	// A file that merely LOOKS like one is an ordinary path.
	let changes = sized_pair();
	note(&changes, &create("sub/.filenignore.bak"));
	note(&changes, &create("sub/filenignore"));
	assert_eq!(taken(&changes).1, None);

	// Remote: a rule file arriving, and one being renamed into that name.
	let dir = Uuid::from_u128(9);
	let rule = Uuid::from_u128(10);
	let arriving = [cache_event(
		Some(1),
		CacheEventType::File(FileEvent::New(cacheable_file(rule, dir, FILENIGNORE))),
	)];
	let changes = sized_pair();
	changes.note_remote_batch(&mut arriving.iter());
	assert_eq!(changes.take().full, Some(R::RulesChanged));

	let renamed = [cache_event(
		Some(2),
		CacheEventType::File(FileEvent::MetadataChanged {
			uuid: rule,
			meta: DecryptedFileMeta {
				name: Cow::Borrowed(FILENIGNORE),
				size: 3,
				mime: Cow::Borrowed("text/plain"),
				key: file_key(),
				last_modified: dt(1),
				created: None,
				hash: None,
			},
		}),
	)];
	let changes = sized_pair();
	changes.note_remote_batch(&mut renamed.iter());
	assert_eq!(changes.take().full, Some(R::RulesChanged));

	// A removal names a uuid and no name, so the event cannot say it was a rule file: it is
	// recorded as the ordinary absence it looks like, and the consuming pass checks the
	// baseline. Pinned so that obligation cannot be forgotten silently.
	let removed = [cache_event(
		Some(3),
		CacheEventType::File(FileEvent::Removed(rule)),
	)];
	let changes = sized_pair();
	changes.note_remote_batch(&mut removed.iter());
	let scope = changes.take();
	assert_eq!(scope.full, None);
	assert_eq!(scope.sizes().1, 1);
}

/// A degraded trigger source forces every pass full for the life of the watch: a pass does not
/// clear it, and it outranks every other reason.
#[test]
fn a_degraded_source_keeps_every_pass_full() {
	for reason in [R::LocalEventsDegraded, R::RemoteEventsDegraded] {
		let changes = sized_pair();
		changes.degrade(reason);
		changes.force(R::SafetyNet);
		assert_eq!(changes.take().full, Some(reason));
		changes.note_tree_size(4_000);
		assert_eq!(
			changes.take().full,
			Some(reason),
			"lost coverage does not come back"
		);
	}
}

/// A forced reason goes to exactly one pass — the next one to START — keeps the first reason
/// recorded, and is outranked by evidence that events were actually lost.
#[test]
fn a_forced_reason_goes_to_the_next_pass_to_start() {
	let changes = sized_pair();
	changes.force(R::PreviousRefusal);
	changes.force(R::SafetyNet);
	assert_eq!(
		changes.take().full,
		Some(R::PreviousRefusal),
		"the first reason is the one that made the pass mandatory"
	);
	// That pass took it, so the pass after it is free again.
	changes.note_tree_size(4_000);
	assert_eq!(changes.take().full, None);

	let lossy = sized_pair();
	lossy.force(R::RulesChanged);
	note(&lossy, &rescan());
	assert_eq!(
		lossy.take().full,
		Some(R::KernelDropped),
		"lost events are the more specific reason"
	);
}

/// A reason recorded while a pass is already running belongs to the pass AFTER it: the running
/// one took its lists before that demand existed and cannot have covered it. Clearing the
/// reason when the pass ended instead dropped it — the lost-trigger hole, silent by
/// construction.
#[test]
fn a_reason_forced_after_the_take_survives_that_pass() {
	let changes = sized_pair();
	note(&changes, &create("a.txt"));
	let scope = changes.take();
	assert_eq!(scope.full_pass_reason(10), None, "nothing forced it yet");

	// ... and now, mid-pass, the rules change under it.
	changes.force(R::RulesChanged);
	// The pass ends and records the tree it read.
	changes.note_tree_size(4_000);

	assert_eq!(
		changes.take().full,
		Some(R::RulesChanged),
		"a demand that arrived mid-pass must outlive that pass"
	);
}

/// The one row only the pass itself knows: an empty baseline is a first sync whatever the
/// changelists say — and a collapsed list outranks even that.
#[test]
fn the_pass_itself_contributes_the_last_row() {
	let changes = sized_pair();
	note(&changes, &create("a.txt"));
	let scope = changes.take();
	assert_eq!(scope.full_pass_reason(10), None);
	assert_eq!(scope.full_pass_reason(0), Some(R::EmptyBaseline));

	let lossy = sized_pair();
	note(&lossy, &rescan());
	assert_eq!(lossy.take().full_pass_reason(0), Some(R::KernelDropped));
}

/// Every reason renders as a sentence a caller can show.
#[test]
fn every_reason_says_why() {
	for reason in [
		R::FirstPass,
		R::EmptyBaseline,
		R::KernelDropped,
		R::RootChanged,
		R::UnmappedLocalPath,
		R::LocalOverflow,
		R::RemoteOverflow,
		R::RemoteWiped,
		R::LocalEventsDegraded,
		R::RemoteEventsDegraded,
		R::SafetyNet,
		R::Requested,
		R::PreviousRefusal,
		R::DeletionHold,
		R::InterruptedPass,
		R::RulesChanged,
		R::UnappliedWork,
		R::ConflictResolved,
		R::LocalEventsUnwatched,
	] {
		assert!(!reason.to_string().is_empty(), "{reason:?}");
	}
}
