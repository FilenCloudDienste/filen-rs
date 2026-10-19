use std::borrow::Cow;

use chrono::TimeZone;
use filen_sdk_rs::{
	cache::open_sqlite_in_memory,
	fs::{dir::RemoteDirectory, dir::meta::DirectoryMeta, file::RemoteFile, file::meta::FileMeta},
	user::events::{
		DecryptedUserEvent, DecryptedUserEventKind, UserEventBaseInfo, UserEventFileInfo,
		UserEventFilePairInfo, UserEventFolderInfo, UserEventItemFavoriteInfo,
	},
};
use filen_types::{
	api::v3::dir::color::DirColor, crypto::EncryptedString, fs::StableUuid, fs::UuidStr,
};

use super::*;
use crate::sql::{dir::DBDir, file::DBFile};

const ROOT: u8 = 9;

fn uuid(byte: u8) -> Uuid {
	Uuid::from_bytes([byte; 16])
}

fn db() -> rusqlite::Connection {
	let conn = open_sqlite_in_memory().unwrap();
	crate::auth::configure_conn(&conn).unwrap();
	conn.execute_batch(crate::sql::statements::INIT).unwrap();
	conn.execute(
		"INSERT INTO items (uuid, parent, type) VALUES (?1, NULL, 0);",
		[uuid(ROOT)],
	)
	.unwrap();
	conn
}

fn when() -> DateTime<Utc> {
	Utc.timestamp_opt(1_700_000_000, 0).unwrap()
}

/// A directory row under `parent`, so events naming it resolve.
fn hold_dir(conn: &mut rusqlite::Connection, uuid_: u8, parent: u8) {
	DBDir::upsert_from_remote(
		conn,
		RemoteDirectory {
			uuid: uuid(uuid_),
			parent: ParentUuid::Uuid(uuid(parent)),
			color: DirColor::Default,
			favorited: false,
			timestamp: when(),
			meta: DirectoryMeta::Encrypted(EncryptedString(Cow::Borrowed("enc-dir"))),
		},
	)
	.unwrap();
}

/// A file row under `parent`, carrying `stable` as its whole-life id.
fn hold_file(conn: &mut rusqlite::Connection, uuid_: u8, stable: u8, parent: u8) {
	DBFile::upsert_from_remote(
		conn,
		RemoteFile {
			uuid: uuid(uuid_),
			stable_uuid: StableUuid::new_for_test(uuid(stable)),
			meta: FileMeta::Encrypted(EncryptedString(Cow::Borrowed("enc-file"))),
			parent: ParentUuid::Uuid(uuid(parent)),
			size: 3,
			favorited: false,
			region: "de-1".into(),
			bucket: "b".into(),
			timestamp: when(),
			chunks: 1,
		},
	)
	.unwrap();
}

/// Re-files a held file as trashed, exactly as a trash listing delivers it: the row stays,
/// the original parent is preserved for the restore.
fn trash_file(conn: &mut rusqlite::Connection, uuid_: u8, stable: u8, original_parent: u8) {
	DBFile::upsert_from_remote(
		conn,
		RemoteFile {
			uuid: uuid(uuid_),
			stable_uuid: StableUuid::new_for_test(uuid(stable)),
			meta: FileMeta::Encrypted(EncryptedString(Cow::Borrowed("enc-file"))),
			parent: ParentUuid::Trash(uuid(original_parent)),
			size: 3,
			favorited: false,
			region: "de-1".into(),
			bucket: "b".into(),
			timestamp: when(),
			chunks: 1,
		},
	)
	.unwrap();
}

fn event(kind: DecryptedUserEventKind) -> DecryptedUserEvent {
	DecryptedUserEvent {
		id: 1,
		timestamp: when(),
		uuid: uuid(200),
		kind,
	}
}

fn base_info() -> UserEventBaseInfo {
	UserEventBaseInfo {
		ip: "1.2.3.4".to_owned(),
		user_agent: "ua".to_owned(),
	}
}

fn file_info(uuid_: Option<u8>, stable: Option<u8>, parent: Option<u8>) -> UserEventFileInfo {
	UserEventFileInfo {
		ip: "1.2.3.4".to_owned(),
		user_agent: "ua".to_owned(),
		metadata: FileMeta::Encrypted(EncryptedString(Cow::Borrowed("enc-file"))),
		uuid: uuid_.map(uuid),
		stable_uuid: stable.map(|b| StableUuid::new_for_test(uuid(b))),
		new_uuid: None,
		parent: parent.map(uuid),
		bucket: None,
		region: None,
		rm: None,
		chunks: None,
		version: None,
		favorited: None,
		timestamp: None,
		current_uuid: None,
	}
}

fn folder_info(uuid_: Option<u8>, parent: Option<u8>) -> UserEventFolderInfo {
	UserEventFolderInfo {
		ip: "1.2.3.4".to_owned(),
		user_agent: "ua".to_owned(),
		name: DirectoryMeta::Encrypted(EncryptedString(Cow::Borrowed("enc-dir"))),
		uuid: uuid_.map(uuid),
		parent: parent.map(uuid),
		timestamp: None,
	}
}

fn targets(conn: &rusqlite::Connection, kind: DecryptedUserEventKind) -> EventTarget {
	target_for(conn, &event(kind))
}

/// The containers-only shape most events resolve to.
fn containers(uuids: Vec<Uuid>) -> EventTarget {
	EventTarget::Refresh {
		containers: uuids,
		trash: false,
	}
}

/// A move has to re-list BOTH ends: the destination gains the row, the origin loses it.
/// Re-listing only the container the event names would leave the file showing in its old
/// folder until something unrelated listed it.
#[test]
fn a_move_targets_both_ends() {
	let mut conn = db();
	hold_dir(&mut conn, 8, ROOT);
	hold_dir(&mut conn, 7, ROOT);
	hold_file(&mut conn, 1, 2, 8);

	let EventTarget::Refresh {
		containers: found, ..
	} = targets(
		&conn,
		DecryptedUserEventKind::FileMoved(file_info(Some(1), Some(2), Some(7))),
	)
	else {
		panic!("a move must name containers");
	};
	assert!(found.contains(&uuid(7)), "the destination");
	assert!(
		found.contains(&uuid(8)),
		"the origin we still hold it under"
	);
}

/// The whole point of the stable id on rename events: an edit re-mints the file's uuid, so
/// a later rename names a uuid this cache has never seen while the lineage is right here.
#[test]
fn a_rename_for_a_reminted_uuid_resolves_through_the_stable_id() {
	let mut conn = db();
	hold_dir(&mut conn, 8, ROOT);
	hold_file(&mut conn, 1, 2, 8);

	let renamed = DecryptedUserEventKind::FileRenamed(UserEventFilePairInfo {
		ip: "1.2.3.4".to_owned(),
		user_agent: "ua".to_owned(),
		metadata: FileMeta::Encrypted(EncryptedString(Cow::Borrowed("new"))),
		old_metadata: FileMeta::Encrypted(EncryptedString(Cow::Borrowed("old"))),
		// The uuid the edit minted — never stored here.
		uuid: Some(uuid(11)),
		stable_uuid: Some(StableUuid::new_for_test(uuid(2))),
	});
	assert_eq!(targets(&conn, renamed), containers(vec![uuid(8)]));
}

/// Without the stable id the same event is unattributable — and must be dropped as
/// irrelevant rather than forcing a pass, since an unknown uuid is overwhelmingly a part of
/// the drive this device never listed.
#[test]
fn an_identified_event_we_do_not_hold_is_dropped_not_swept() {
	let conn = db();
	assert_eq!(
		targets(
			&conn,
			DecryptedUserEventKind::FileUploaded(file_info(Some(1), Some(2), Some(8)))
		),
		EventTarget::Irrelevant
	);
}

/// An event carrying no identity at all cannot be told apart from one we simply failed to
/// resolve, so it is the pass's problem. This is the pre-rollout `deleteFilePermanently`.
#[test]
fn an_anonymous_event_forces_the_pass() {
	let conn = db();
	assert_eq!(
		targets(
			&conn,
			DecryptedUserEventKind::DeleteFilePermanently(file_info(None, None, None))
		),
		EventTarget::Unknown
	);
}

/// Every file kind has carried its lineage id since the server began logging it. An event
/// without one predates that, or is a server anomaly, and its uuid alone cannot tell a
/// lineage this cache holds under a re-minted uuid from a part of the drive it never listed
/// — dropping it as irrelevant would close the gap over a stale container. So it is the
/// pass's problem, even when the uuid or the parent IS held.
#[test]
fn a_file_event_without_a_stable_id_is_handed_to_the_pass() {
	let mut conn = db();
	hold_dir(&mut conn, 8, ROOT);
	hold_file(&mut conn, 1, 2, 8);

	assert_eq!(
		targets(
			&conn,
			DecryptedUserEventKind::FileUploaded(file_info(Some(1), None, Some(8)))
		),
		EventTarget::Unknown
	);
	assert_eq!(
		targets(
			&conn,
			DecryptedUserEventKind::FileTrash(file_info(Some(1), None, None))
		),
		EventTarget::Unknown
	);
	let renamed = DecryptedUserEventKind::FileRenamed(UserEventFilePairInfo {
		ip: "1.2.3.4".to_owned(),
		user_agent: "ua".to_owned(),
		metadata: FileMeta::Encrypted(EncryptedString(Cow::Borrowed("new"))),
		old_metadata: FileMeta::Encrypted(EncryptedString(Cow::Borrowed("old"))),
		uuid: Some(uuid(1)),
		stable_uuid: None,
	});
	assert_eq!(targets(&conn, renamed), EventTarget::Unknown);
	let favorited = DecryptedUserEventKind::ItemFavorite(UserEventItemFavoriteInfo {
		ip: "1.2.3.4".to_owned(),
		user_agent: "ua".to_owned(),
		value: true,
		metadata: FileMeta::Encrypted(EncryptedString(Cow::Borrowed("enc-file"))),
		uuid: Some(uuid(1)),
		stable_uuid: None,
		item_type: Some(filen_types::fs::ObjectType::File),
	});
	assert_eq!(targets(&conn, favorited), EventTarget::Unknown);
}

/// The two absences that are legitimate today, and must keep resolving: a
/// `deleteFilePermanently` of archived versions only (the live head stands, and the event
/// names its uuid), and a folder favourite, which never carries a stable id.
#[test]
fn the_kinds_that_legitimately_lack_a_stable_id_still_resolve() {
	let mut conn = db();
	hold_dir(&mut conn, 8, ROOT);
	hold_file(&mut conn, 1, 2, 8);

	assert_eq!(
		targets(
			&conn,
			DecryptedUserEventKind::DeleteFilePermanently(file_info(Some(1), None, None))
		),
		containers(vec![uuid(8)])
	);
	let folder_favorite = DecryptedUserEventKind::ItemFavorite(UserEventItemFavoriteInfo {
		ip: "1.2.3.4".to_owned(),
		user_agent: "ua".to_owned(),
		value: true,
		metadata: FileMeta::Encrypted(EncryptedString(Cow::Borrowed("enc-dir"))),
		uuid: Some(uuid(8)),
		stable_uuid: None,
		item_type: Some(filen_types::fs::ObjectType::Dir),
	});
	assert_eq!(
		targets(&conn, folder_favorite),
		containers(vec![uuid(ROOT)])
	);
}

/// Folder events land on the folder's PARENT: a rename, a colour or a trash changes how the
/// folder appears in the listing above it, not what is inside it.
#[test]
fn folder_events_target_the_parent_listing() {
	let mut conn = db();
	hold_dir(&mut conn, 8, ROOT);

	assert_eq!(
		targets(
			&conn,
			DecryptedUserEventKind::SubFolderCreated(folder_info(Some(5), Some(8)))
		),
		containers(vec![uuid(8)]),
		"a new folder shows up in its parent's listing"
	);
	assert_eq!(
		targets(
			&conn,
			DecryptedUserEventKind::FolderTrash(folder_info(Some(8), None))
		),
		containers(vec![uuid(ROOT)]),
		"a trashed folder disappears from the listing above it"
	);
}

/// `deleteAll` is never replayed — the pass tells "gone" from "unreachable" per container —
/// and an emptied trash is the trash listing's business, not a drive sweep.
#[test]
fn account_wide_events_route_to_the_pass_or_the_trash() {
	let conn = db();
	assert_eq!(
		targets(&conn, DecryptedUserEventKind::DeleteAll(base_info())),
		EventTarget::Unknown
	);
	assert_eq!(
		targets(&conn, DecryptedUserEventKind::TrashEmptied(base_info())),
		EventTarget::Refresh {
			containers: Vec::new(),
			trash: true,
		}
	);
}

/// Ordinary login/share/link traffic must not drag a full pass behind it; the live socket
/// path does not even subscribe to these.
#[test]
fn out_of_scope_kinds_are_skipped() {
	let conn = db();
	for kind in [
		DecryptedUserEventKind::Login(base_info()),
		DecryptedUserEventKind::PasswordChanged(base_info()),
		DecryptedUserEventKind::DeleteVersioned(base_info()),
	] {
		assert_eq!(targets(&conn, kind), EventTarget::Irrelevant);
	}
}

/// The truncation detector. Re-asking with the same cutoff returns the same page, which is
/// what a second holding more events than one page looks like from here.
#[test]
fn a_repeated_page_is_no_progress() {
	let page = vec![event(DecryptedUserEventKind::Login(base_info()))];
	let mut seen = HashSet::new();
	assert!(
		made_progress(&mut seen, &page),
		"the first page is progress"
	);
	assert!(
		!made_progress(&mut seen, &page),
		"the same page again means the cutoff cannot go lower"
	);
}

/// The feed answers strictly BEFORE the cutoff, and a page can end partway through a
/// second. Asking next for exactly the oldest second seen would skip the rest of it, so the
/// next cutoff is one second past it — and the walk may only stop once a page reached a
/// second strictly older than the cursor's, which is when the cursor second is known whole.
#[test]
fn the_next_cutoff_refetches_the_boundary_second() {
	let oldest = when();
	let at = |seconds_after: i64| DecryptedUserEvent {
		timestamp: oldest + chrono::TimeDelta::seconds(seconds_after),
		..event(DecryptedUserEventKind::Login(base_info()))
	};
	let page = [at(5), at(0), at(2)];
	let next = next_cutoff(&page).expect("a non-empty page has a next cutoff");
	assert_eq!(next, oldest + chrono::TimeDelta::seconds(1));
	assert!(
		!reached(next, oldest),
		"a page whose oldest second IS the cursor second may have split it; ask once more"
	);
	assert!(
		reached(next, oldest + chrono::TimeDelta::seconds(1)),
		"a page reaching strictly past the cursor second has fetched that second whole"
	);
	assert_eq!(
		next_cutoff(&[]),
		None,
		"an empty page is the end of the feed"
	);
}

/// One unattributable event poisons the whole range: the pass is the only thing that can
/// close a gap we cannot describe.
#[test]
fn one_unattributable_event_gives_up_the_whole_range() {
	let mut conn = db();
	hold_dir(&mut conn, 8, ROOT);
	let events = vec![
		event(DecryptedUserEventKind::SubFolderCreated(folder_info(
			Some(5),
			Some(8),
		))),
		event(DecryptedUserEventKind::DeleteAll(base_info())),
	];
	let mut containers = HashSet::new();
	let mut relist_trash = false;
	assert!(!fold_targets(
		&conn,
		&events,
		when(),
		&mut containers,
		&mut relist_trash
	));
}

/// The blind spot that shipped once: a row this cache holds TRASHED is in no directory
/// listing, so reading only its directory parent resolves the event to nothing and drops it.
/// Half the kinds carry no parent of their own — favourites, renames, metadata changes — so
/// the row is the only thing that can place them, and the trash listing is where they show.
#[test]
fn an_event_for_a_row_we_hold_trashed_refreshes_the_trash() {
	let mut conn = db();
	hold_dir(&mut conn, 8, ROOT);
	hold_file(&mut conn, 1, 2, 8);
	// Trashed the way a trash listing delivers it: parent kept for the restore.
	trash_file(&mut conn, 1, 2, 8);

	let favorited = DecryptedUserEventKind::ItemFavorite(UserEventItemFavoriteInfo {
		ip: "1.2.3.4".to_owned(),
		user_agent: "ua".to_owned(),
		value: true,
		metadata: FileMeta::Encrypted(EncryptedString(Cow::Borrowed("enc-file"))),
		uuid: Some(uuid(1)),
		stable_uuid: Some(StableUuid::new_for_test(uuid(2))),
		item_type: Some(filen_types::fs::ObjectType::File),
	});
	assert_eq!(
		targets(&conn, favorited),
		EventTarget::Refresh {
			containers: Vec::new(),
			trash: true,
		},
		"a trashed row's listing is the trash; dropping it leaves the trash view stale"
	);
}

/// Restoring one is the case that needs BOTH: the directory it lands in gains a row, and the
/// trash it left has to stop showing it.
#[test]
fn a_restore_refreshes_the_destination_and_the_trash() {
	let mut conn = db();
	hold_dir(&mut conn, 8, ROOT);
	hold_file(&mut conn, 1, 2, 8);
	trash_file(&mut conn, 1, 2, 8);

	assert_eq!(
		targets(
			&conn,
			DecryptedUserEventKind::FileRestored(file_info(Some(1), Some(2), Some(8)))
		),
		EventTarget::Refresh {
			containers: vec![uuid(8)],
			trash: true,
		}
	);
}

/// An unverified payload shape must not be treated as attributable: if `baseFolderCreated`
/// turns out to carry no parent, classifying it would drop it silently. It fires about once
/// per account, so the pass costs nothing.
#[test]
fn an_unobserved_kind_is_handed_to_the_pass() {
	let mut conn = db();
	hold_dir(&mut conn, 8, ROOT);
	assert_eq!(
		targets(
			&conn,
			DecryptedUserEventKind::BaseFolderCreated(folder_info(Some(5), Some(8)))
		),
		EventTarget::Unknown
	);
}

/// The debt rule, which is the one thing here that can silently reintroduce the
/// stale-forever bug. A recorded `pending_reconcile` is set for containers no event will
/// ever name, so the sweep must run even though the log could describe the gap.
#[test]
fn a_recorded_debt_is_never_answered_by_a_replay() {
	let cursor = Some(when());
	assert!(replay_allowed(false, cursor), "the ordinary case replays");
	assert!(
		!replay_allowed(true, cursor),
		"a debt is the full pass's to answer; replaying it clears nothing and leaves the \
			 container it was recorded for stale forever"
	);
	assert!(
		!replay_allowed(false, None),
		"with no cursor there is no lower bound to page back to"
	);
	assert!(!replay_allowed(true, None));
}

/// The cursor a pass claims is read back against SERVER timestamps, so it must lean the same
/// way the first cutoff does — behind, where being wrong costs a re-list of something already
/// current. Leaning the other way drops in-gap events with no signal at all, and replay then
/// reports the gap closed.
#[test]
fn a_pass_cursor_leans_behind_and_a_first_cutoff_leans_ahead() {
	let now = when();
	assert!(pass_cursor(now) < now, "a pass cursor must never run ahead");
	assert!(
		first_cutoff(now) > now,
		"a first cutoff must never fall short"
	);
	// Symmetric, so a clock wrong in either direction costs the same harmless re-fold.
	assert_eq!(now - pass_cursor(now), first_cutoff(now) - now);
	// And the slack must actually cover a plausible clock difference.
	assert!(now - pass_cursor(now) >= chrono::TimeDelta::seconds(30));
}

/// A page reaches back about a hundred events whatever the gap was, so most of what it
/// returns is older than the cursor and already accounted for. Folding those is not merely
/// wasteful: an event the cache settled with long ago — a `deleteAll`, an unreadable payload
/// — would refuse every gap for as long as it stayed on the first page.
#[test]
fn events_from_before_the_cursor_are_not_folded() {
	let mut conn = db();
	hold_dir(&mut conn, 8, ROOT);

	let old = DecryptedUserEvent {
		timestamp: when() - chrono::TimeDelta::seconds(60),
		..event(DecryptedUserEventKind::DeleteAll(base_info()))
	};
	let fresh = event(DecryptedUserEventKind::SubFolderCreated(folder_info(
		Some(5),
		Some(8),
	)));

	let mut containers = HashSet::new();
	let mut relist_trash = false;
	assert!(
		fold_targets(
			&conn,
			&[old, fresh],
			when(),
			&mut containers,
			&mut relist_trash
		),
		"a settled deleteAll from before the cursor must not refuse this gap"
	);
	assert_eq!(containers, HashSet::from([uuid(8)]));
}

/// The cost bound: past a point, re-listing the named containers is no cheaper than the
/// sweep, and the sweep also clears the debt. Pinned because the check reads as an
/// off-by-one and is not: it runs after each event is folded, so the LAST accepted set is
/// `MAX_TARGETED_CONTAINERS` and one more gives up.
#[test]
fn the_targeted_set_is_bounded() {
	let mut conn = db();
	let mut events = Vec::new();
	// Each event names a distinct held container, so each adds exactly one target.
	for byte in (0u8..).take(MAX_TARGETED_CONTAINERS + 1) {
		// Disjoint ranges, and neither may land on ROOT: a held row whose parent is NULL
		// is the account root, which resolves to `Unknown` and would end the fold early.
		let parent = byte.wrapping_add(100);
		let child = byte.wrapping_add(180);
		hold_dir(&mut conn, parent, ROOT);
		events.push(event(DecryptedUserEventKind::SubFolderCreated(
			folder_info(Some(child), Some(parent)),
		)));
	}

	let mut containers = HashSet::new();
	let mut relist_trash = false;
	assert!(
		fold_targets(
			&conn,
			&events[..MAX_TARGETED_CONTAINERS],
			when(),
			&mut containers,
			&mut relist_trash
		),
		"the bound itself is accepted"
	);
	assert_eq!(containers.len(), MAX_TARGETED_CONTAINERS);

	let mut containers = HashSet::new();
	assert!(
		!fold_targets(&conn, &events, when(), &mut containers, &mut relist_trash),
		"one past the bound hands the gap to the sweep"
	);
}

/// `UuidStr` round-trips are what the materialized-container set stores; keep the target set
/// in the same currency so the two can be compared without conversion surprises.
#[test]
fn targets_are_plain_uuids() {
	let mut conn = db();
	hold_dir(&mut conn, 8, ROOT);
	let EventTarget::Refresh {
		containers: found, ..
	} = targets(
		&conn,
		DecryptedUserEventKind::SubFolderCreated(folder_info(Some(5), Some(8))),
	)
	else {
		panic!("expected containers");
	};
	assert_eq!(UuidStr::from(found[0]), UuidStr::from(uuid(8)));
}
