use std::{
	io::Cursor,
	sync::Arc,
	time::{Duration, Instant},
};

use filen_macros::shared_test_runtime;
use filen_sdk_rs::{
	auth::Client,
	fs::{
		HasName, HasUUID,
		dir::meta::{DirectoryMeta, DirectoryMetaChanges},
		file::meta::{FileMeta, FileMetaChanges},
	},
	user::events::{DecryptedUserEvent, DecryptedUserEventKind},
};
use filen_types::{api::v3::dir::color::DirColor, fs::UuidStr};
use rand::Rng;

fn file_meta_name<'a>(meta: &'a FileMeta<'_>) -> Option<&'a str> {
	match meta {
		FileMeta::Decoded(decoded) => Some(decoded.name()),
		_ => None,
	}
}

fn dir_meta_name<'a>(meta: &'a DirectoryMeta<'_>) -> Option<&'a str> {
	match meta {
		DirectoryMeta::Decoded(_) => meta.name(),
		_ => None,
	}
}

#[shared_test_runtime]
async fn upload_avatar() {
	let client = test_utils::RESOURCES.client().await;
	let before = client.get_user_info().await.unwrap();

	let mut rng = rand::rng();

	let img = image::ImageBuffer::from_fn(128, 128, |_, _| {
		image::Rgb([rng.random::<u8>(), rng.random::<u8>(), rng.random::<u8>()])
	});

	let mut buf = Vec::new();
	img.write_to(&mut Cursor::new(&mut buf), image::ImageFormat::Png)
		.unwrap();

	let url = client.upload_avatar(&buf).await.unwrap();

	let after = client.get_user_info().await.unwrap();
	assert_eq!(
		after.avatar_url,
		Some(url.to_string()),
		"avatar_url should match after upload"
	);

	assert_ne!(
		before.avatar_url, after.avatar_url,
		"avatar_url should be different after upload"
	);
}

// Resource name shared by every test that mutates the versioning flag —
// concurrent runs would otherwise race on this account-wide setting.
const LOCK_VERSIONING: &str = "test:user-versioning";
const LOCK_LOGIN_ALERTS: &str = "test:user-login-alerts";
const LOCK_PERSONAL_INFO: &str = "test:user-personal-info";

#[shared_test_runtime]
async fn versioning_toggle_round_trip() {
	let client = test_utils::RESOURCES.client().await;
	let _lock = client
		.acquire_lock_with_default(LOCK_VERSIONING)
		.await
		.unwrap();
	let original = client.get_user_info().await.unwrap().versioning_enabled;

	client.set_versioning_enabled(!original).await.unwrap();
	let toggled = client.get_user_info().await.unwrap().versioning_enabled;
	assert_eq!(toggled, !original, "versioning flag should be inverted");

	client.set_versioning_enabled(original).await.unwrap();
	let restored = client.get_user_info().await.unwrap().versioning_enabled;
	assert_eq!(restored, original, "versioning flag should be restored");
}

#[shared_test_runtime]
async fn login_alerts_toggle_round_trip() {
	let client = test_utils::RESOURCES.client().await;
	let _lock = client
		.acquire_lock_with_default(LOCK_LOGIN_ALERTS)
		.await
		.unwrap();
	let original = client.get_user_info().await.unwrap().login_alerts_enabled;

	client.set_login_alerts_enabled(!original).await.unwrap();
	let toggled = client.get_user_info().await.unwrap().login_alerts_enabled;
	assert_eq!(toggled, !original, "login_alerts flag should be inverted");

	client.set_login_alerts_enabled(original).await.unwrap();
	let restored = client.get_user_info().await.unwrap().login_alerts_enabled;
	assert_eq!(restored, original, "login_alerts flag should be restored");
}

/// With versioning enabled, uploading a file with the same name + parent should
/// retain the previous version (not replace it). With versioning disabled, the
/// previous upload should not produce a version entry.
#[shared_test_runtime]
async fn versioning_creates_versions_on_duplicate_upload() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let _version_lock = client
		.acquire_lock_with_default("test:versions")
		.await
		.unwrap();

	let _lock = client
		.acquire_lock_with_default(LOCK_VERSIONING)
		.await
		.unwrap();
	let original_versioning = client.get_user_info().await.unwrap().versioning_enabled;

	if !original_versioning {
		client.set_versioning_enabled(true).await.unwrap();
	}

	let restore_versioning = |enabled: bool| async move {
		let client = test_utils::RESOURCES.client().await;
		client.set_versioning_enabled(enabled).await.unwrap();
	};

	let first = client
		.make_file_builder("versioning-test.txt", *test_dir.uuid())
		.unwrap();
	let first = client.upload_file(first, b"first content").await.unwrap();
	tokio::time::sleep(Duration::from_secs(2)).await;

	let second = client
		.make_file_builder("versioning-test.txt", *test_dir.uuid())
		.unwrap();
	let second = client.upload_file(second, b"second content").await.unwrap();

	let versions = client.list_file_versions(&second).await.unwrap();
	assert!(
		versions.len() >= 2,
		"expected at least 2 versions (current + previous) with versioning enabled, got {}",
		versions.len()
	);
	let expected_name = first.name().expect("first upload should have a name");
	for v in &versions {
		assert_eq!(
			v.metadata().name(),
			Some(expected_name),
			"all versions should share the same filename"
		);
	}

	client.set_versioning_enabled(false).await.unwrap();
	tokio::time::sleep(Duration::from_secs(2)).await;

	let third = client
		.make_file_builder("versioning-test.txt", *test_dir.uuid())
		.unwrap();
	let third = client.upload_file(third, b"third content").await.unwrap();

	let versions_after_disable = client.list_file_versions(&third).await.unwrap();
	assert!(
		versions_after_disable.len() <= versions.len(),
		"versioning disabled should NOT grow the version chain (was {}, now {})",
		versions.len(),
		versions_after_disable.len()
	);

	restore_versioning(original_versioning).await;
}

#[shared_test_runtime]
async fn events_returns_recent_events() {
	let client = test_utils::RESOURCES.client().await;
	let events = client.get_user_events(None, None).await.unwrap();
	let ok_count = events.iter().filter(|r| r.is_ok()).count();
	assert!(
		ok_count > 0,
		"expected at least one decryptable recent event, got {} events ({} Err)",
		events.len(),
		events.len() - ok_count
	);
}

#[shared_test_runtime]
async fn event_by_uuid_fetches_single_event() {
	let client = test_utils::RESOURCES.client().await;
	let events = client.get_user_events(None, None).await.unwrap();
	let first = events
		.iter()
		.find_map(|r| r.as_ref().ok())
		.cloned()
		.expect("at least one decryptable recent event");
	let fetched = client.get_user_event(first.uuid).await.unwrap();
	// Full structural equality — proves the single-event endpoint returns the
	// same content as the list, not just an event-shaped echo of the uuid+id.
	assert_eq!(fetched, first);
}

const EVENT_POLL_TIMEOUT: Duration = Duration::from_secs(60);
const EVENT_POLL_INTERVAL: Duration = Duration::from_secs(3);
/// Tolerance for client/server clock skew when filtering events by timestamp.
const EVENT_SINCE_SKEW: chrono::Duration = chrono::Duration::seconds(5);

/// Poll the events endpoint until `matcher` returns Some, or panic on timeout.
///
/// Events older than `since` (with [`EVENT_SINCE_SKEW`] tolerance for clock
/// skew) are filtered out before the matcher sees them — this prevents stale
/// events from previous tests or prior runs from accidentally matching.
/// Per-event deserialization failures (Err entries) are also ignored.
///
/// Callers should snapshot `Utc::now()` *before* performing the action that
/// is expected to produce the event.
async fn poll_for_event<F, T>(
	client: &Arc<Client>,
	since: chrono::DateTime<chrono::Utc>,
	mut matcher: F,
	description: &str,
) -> T
where
	F: FnMut(&DecryptedUserEvent) -> Option<T>,
{
	let cutoff = since - EVENT_SINCE_SKEW;
	let deadline = Instant::now() + EVENT_POLL_TIMEOUT;
	let mut last_count = 0;
	loop {
		match client.get_user_events(None, None).await {
			Ok(events) => {
				last_count = events.len();
				for event in events.iter().filter_map(|r| r.as_ref().ok()) {
					if event.timestamp < cutoff {
						continue;
					}
					if let Some(value) = matcher(event) {
						return value;
					}
				}
			}
			Err(e) => {
				// Rate limited or transient; retry after the interval.
				eprintln!("events fetch error (will retry): {e:?}");
			}
		}
		if Instant::now() >= deadline {
			panic!(
				"event '{description}' not found within {:?} (last poll returned {} events, cutoff {cutoff})",
				EVENT_POLL_TIMEOUT, last_count
			);
		}
		tokio::time::sleep(EVENT_POLL_INTERVAL).await;
	}
}

/// Assert the file metadata enum decoded cleanly and that the name matches.
fn assert_file_meta_name(meta: &FileMeta<'_>, expected: &str) {
	match meta {
		FileMeta::Decoded(decoded) => assert_eq!(decoded.name(), expected),
		other => panic!("expected FileMeta::Decoded with name {expected:?}, got {other:?}"),
	}
}

/// Assert the directory metadata enum decoded cleanly and that the name matches.
fn assert_dir_meta_name(meta: &DirectoryMeta<'_>, expected: &str) {
	match meta {
		DirectoryMeta::Decoded(_) => {
			assert_eq!(meta.name(), Some(expected));
		}
		other => panic!("expected DirectoryMeta::Decoded with name {expected:?}, got {other:?}"),
	}
}

/// Generate a unique test-prefixed name. The shared test dir is cleaned up after
/// the run, but we still want each test's files to be distinguishable.
fn unique_name(prefix: &str) -> String {
	use base64::{Engine, prelude::BASE64_URL_SAFE_NO_PAD};
	let suffix: [u8; 6] = rand::random();
	format!(
		"event-{prefix}-{}.txt",
		BASE64_URL_SAFE_NO_PAD.encode(suffix)
	)
}

fn unique_dir_name(prefix: &str) -> String {
	use base64::{Engine, prelude::BASE64_URL_SAFE_NO_PAD};
	let suffix: [u8; 6] = rand::random();
	format!("event-{prefix}-{}", BASE64_URL_SAFE_NO_PAD.encode(suffix))
}

#[shared_test_runtime]
async fn events_file_upload_trash_restore_delete() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let dir = &resources.dir;
	let since = chrono::Utc::now();

	let file_name = unique_name("lifecycle");
	let file = client.make_file_builder(&file_name, *dir.uuid()).unwrap();
	let mut file = client
		.upload_file(file, b"lifecycle file contents")
		.await
		.unwrap();

	// fileUploaded
	let upload_event = poll_for_event(
		client,
		since,
		|event| match &event.kind {
			DecryptedUserEventKind::FileUploaded(info)
				if file_meta_name(&info.metadata) == Some(&file_name) =>
			{
				Some(info.clone())
			}
			_ => None,
		},
		"fileUploaded",
	)
	.await;
	assert_file_meta_name(&upload_event.metadata, &file_name);

	// trash → fileTrash
	client.trash_file(&mut file).await.unwrap();
	let trash_event = poll_for_event(
		client,
		since,
		|event| match &event.kind {
			DecryptedUserEventKind::FileTrash(info)
				if file_meta_name(&info.metadata) == Some(&file_name) =>
			{
				Some(info.clone())
			}
			_ => None,
		},
		"fileTrash",
	)
	.await;
	assert_file_meta_name(&trash_event.metadata, &file_name);

	// restore → fileRestored
	client.restore_file(&mut file).await.unwrap();
	let restore_event = poll_for_event(
		client,
		since,
		|event| match &event.kind {
			DecryptedUserEventKind::FileRestored(info)
				if file_meta_name(&info.metadata) == Some(&file_name) =>
			{
				Some(info.clone())
			}
			_ => None,
		},
		"fileRestored",
	)
	.await;
	assert_file_meta_name(&restore_event.metadata, &file_name);

	// permanent delete → deleteFilePermanently
	client.delete_file_permanently(file).await.unwrap();
	let delete_event = poll_for_event(
		client,
		since,
		|event| match &event.kind {
			DecryptedUserEventKind::DeleteFilePermanently(info)
				if file_meta_name(&info.metadata) == Some(&file_name) =>
			{
				Some(info.clone())
			}
			_ => None,
		},
		"deleteFilePermanently",
	)
	.await;
	assert_file_meta_name(&delete_event.metadata, &file_name);
}

#[shared_test_runtime]
async fn events_file_metadata_changed_and_move() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let dir = &resources.dir;
	let since = chrono::Utc::now();

	let original_name = unique_name("rename");
	let file = client
		.make_file_builder(&original_name, *dir.uuid())
		.unwrap();
	let mut file = client.upload_file(file, b"to be renamed").await.unwrap();

	// rename → fileMetadataChanged event (the backend logs a metadata change)
	let new_name = unique_name("renamed");
	client
		.update_file_metadata(
			&mut file,
			FileMetaChanges::default().name(&new_name).unwrap(),
		)
		.await
		.unwrap();

	let rename_event = poll_for_event(
		client,
		since,
		|event| match &event.kind {
			DecryptedUserEventKind::FileMetadataChanged(info)
				if file_meta_name(&info.metadata) == Some(&new_name)
					&& file_meta_name(&info.old_metadata) == Some(&original_name) =>
			{
				Some(info.clone())
			}
			_ => None,
		},
		"fileMetadataChanged (rename)",
	)
	.await;
	assert_file_meta_name(&rename_event.metadata, &new_name);
	assert_file_meta_name(&rename_event.old_metadata, &original_name);

	// move → fileMoved
	let move_target = client
		.create_dir(&dir.into(), &unique_dir_name("move-target"))
		.await
		.unwrap();
	client
		.move_file(&mut file, &(&move_target).into())
		.await
		.unwrap();

	let move_event = poll_for_event(
		client,
		since,
		|event| match &event.kind {
			DecryptedUserEventKind::FileMoved(info)
				if file_meta_name(&info.metadata) == Some(&new_name) =>
			{
				Some(info.clone())
			}
			_ => None,
		},
		"fileMoved",
	)
	.await;
	assert_file_meta_name(&move_event.metadata, &new_name);
}

#[shared_test_runtime]
async fn events_file_versioned_on_duplicate_upload() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let dir = &resources.dir;
	let since = chrono::Utc::now();

	let _lock = client
		.acquire_lock_with_default(LOCK_VERSIONING)
		.await
		.unwrap();
	let original_versioning = client.get_user_info().await.unwrap().versioning_enabled;
	if !original_versioning {
		client.set_versioning_enabled(true).await.unwrap();
	}

	let file_name = unique_name("versioned");

	let first = client.make_file_builder(&file_name, *dir.uuid()).unwrap();
	let _first = client.upload_file(first, b"v1").await.unwrap();
	tokio::time::sleep(Duration::from_secs(2)).await;

	let second = client.make_file_builder(&file_name, *dir.uuid()).unwrap();
	let _second = client.upload_file(second, b"v2").await.unwrap();

	// fileVersioned event for our filename should appear after the second upload
	let versioned_event = poll_for_event(
		client,
		since,
		|event| match &event.kind {
			DecryptedUserEventKind::FileVersioned(info)
				if file_meta_name(&info.metadata) == Some(&file_name) =>
			{
				Some(info.clone())
			}
			_ => None,
		},
		"fileVersioned",
	)
	.await;
	assert_file_meta_name(&versioned_event.metadata, &file_name);

	// Restore previous versioning state
	if !original_versioning {
		client.set_versioning_enabled(false).await.unwrap();
	}
}

#[shared_test_runtime]
async fn events_folder_creation_rename_color_change() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let dir = &resources.dir;
	let since = chrono::Utc::now();

	let folder_name = unique_dir_name("created");
	let mut folder = client.create_dir(&dir.into(), &folder_name).await.unwrap();

	// subFolderCreated
	let create_event = poll_for_event(
		client,
		since,
		|event| match &event.kind {
			DecryptedUserEventKind::SubFolderCreated(info)
				if dir_meta_name(&info.name) == Some(&folder_name) =>
			{
				Some(info.clone())
			}
			_ => None,
		},
		"subFolderCreated",
	)
	.await;
	assert_dir_meta_name(&create_event.name, &folder_name);

	// rename → folderMetadataChanged (the backend logs this for rename)
	let new_name = unique_dir_name("renamed");
	client
		.update_dir_metadata(
			&mut folder,
			DirectoryMetaChanges::default().name(&new_name).unwrap(),
		)
		.await
		.unwrap();
	let rename_event = poll_for_event(
		client,
		since,
		|event| match &event.kind {
			DecryptedUserEventKind::FolderMetadataChanged(info)
				if dir_meta_name(&info.name) == Some(&new_name)
					&& dir_meta_name(&info.old_name) == Some(&folder_name) =>
			{
				Some(info.clone())
			}
			_ => None,
		},
		"folderMetadataChanged",
	)
	.await;
	assert_dir_meta_name(&rename_event.name, &new_name);
	assert_dir_meta_name(&rename_event.old_name, &folder_name);

	// color change → folderColorChanged
	client
		.set_dir_color(&mut folder, DirColor::Blue)
		.await
		.unwrap();
	let color_event = poll_for_event(
		client,
		since,
		|event| match &event.kind {
			DecryptedUserEventKind::FolderColorChanged(info)
				if dir_meta_name(&info.name) == Some(&new_name) =>
			{
				Some(info.clone())
			}
			_ => None,
		},
		"folderColorChanged",
	)
	.await;
	assert_dir_meta_name(&color_event.name, &new_name);
}

#[shared_test_runtime]
async fn events_folder_trash_restore_move_delete() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let dir = &resources.dir;
	let since = chrono::Utc::now();

	let folder_name = unique_dir_name("trash-cycle");
	let mut folder = client.create_dir(&dir.into(), &folder_name).await.unwrap();

	// trash → folderTrash
	client.trash_dir(&mut folder).await.unwrap();
	let trash_event = poll_for_event(
		client,
		since,
		|event| match &event.kind {
			DecryptedUserEventKind::FolderTrash(info)
				if dir_meta_name(&info.name) == Some(&folder_name) =>
			{
				Some(info.clone())
			}
			_ => None,
		},
		"folderTrash",
	)
	.await;
	assert_dir_meta_name(&trash_event.name, &folder_name);

	// restore → folderRestored
	client.restore_dir(&mut folder).await.unwrap();
	let restore_event = poll_for_event(
		client,
		since,
		|event| match &event.kind {
			DecryptedUserEventKind::FolderRestored(info)
				if dir_meta_name(&info.name) == Some(&folder_name) =>
			{
				Some(info.clone())
			}
			_ => None,
		},
		"folderRestored",
	)
	.await;
	assert_dir_meta_name(&restore_event.name, &folder_name);

	// move → folderMoved
	let move_target = client
		.create_dir(&dir.into(), &unique_dir_name("move-target"))
		.await
		.unwrap();
	client
		.move_dir(&mut folder, &(&move_target).into())
		.await
		.unwrap();
	let move_event = poll_for_event(
		client,
		since,
		|event| match &event.kind {
			DecryptedUserEventKind::FolderMoved(info)
				if dir_meta_name(&info.name) == Some(&folder_name) =>
			{
				Some(info.clone())
			}
			_ => None,
		},
		"folderMoved",
	)
	.await;
	assert_dir_meta_name(&move_event.name, &folder_name);

	// permanent delete → deleteFolderPermanently
	client.delete_dir_permanently(folder).await.unwrap();
	let delete_event = poll_for_event(
		client,
		since,
		|event| match &event.kind {
			DecryptedUserEventKind::DeleteFolderPermanently(info)
				if dir_meta_name(&info.name) == Some(&folder_name) =>
			{
				Some(info.clone())
			}
			_ => None,
		},
		"deleteFolderPermanently",
	)
	.await;
	assert_dir_meta_name(&delete_event.name, &folder_name);
}

#[shared_test_runtime]
async fn events_item_favorite() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let dir = &resources.dir;
	let since = chrono::Utc::now();

	let file_name = unique_name("favorite");
	let file = client.make_file_builder(&file_name, *dir.uuid()).unwrap();
	let mut file = client
		.upload_file(file, b"will be favorited")
		.await
		.unwrap();

	// file favorite
	client.set_file_favorite(&mut file, true).await.unwrap();
	let file_fav_event = poll_for_event(
		client,
		since,
		|event| match &event.kind {
			DecryptedUserEventKind::ItemFavorite(info)
				if info.value && file_meta_name(&info.metadata) == Some(&file_name) =>
			{
				Some(info.clone())
			}
			_ => None,
		},
		"itemFavorite (file, set)",
	)
	.await;
	assert_file_meta_name(&file_fav_event.metadata, &file_name);

	// unfavorite
	client.set_file_favorite(&mut file, false).await.unwrap();
	let file_unfav_event = poll_for_event(
		client,
		since,
		|event| match &event.kind {
			DecryptedUserEventKind::ItemFavorite(info)
				if !info.value && file_meta_name(&info.metadata) == Some(&file_name) =>
			{
				Some(info.clone())
			}
			_ => None,
		},
		"itemFavorite (file, unset)",
	)
	.await;
	assert_file_meta_name(&file_unfav_event.metadata, &file_name);
}

/// Sanity-check: every event returned from a real account decrypts into a
/// `Decoded` variant (i.e. the encryption version default we picked actually
/// matches what the backend stored). This catches regressions where a future
/// schema change leaves us with `FileMeta::Encrypted` or `DecryptedUTF8`. Also
/// asserts that no events failed to parse at the wire level.
#[shared_test_runtime]
async fn events_all_decryptable() {
	let client = test_utils::RESOURCES.client().await;
	let results = client.get_user_events(None, None).await.unwrap();

	// First: every event must have parsed at the wire level.
	let parse_failures: Vec<_> = results.iter().filter_map(|r| r.as_ref().err()).collect();
	assert!(
		parse_failures.is_empty(),
		"some events failed to parse: {:#?}",
		parse_failures
	);
	let events: Vec<_> = results.into_iter().filter_map(Result::ok).collect();

	let mut undecryptable: Vec<(UuidStr, &'static str, String)> = Vec::new();

	let check_file = |uuid: UuidStr, kind: &'static str, meta: &FileMeta<'_>, out: &mut Vec<_>| {
		if !matches!(meta, FileMeta::Decoded(_)) {
			out.push((uuid, kind, format!("{meta:?}")));
		}
	};
	let check_dir =
		|uuid: UuidStr, kind: &'static str, meta: &DirectoryMeta<'_>, out: &mut Vec<_>| {
			if !matches!(meta, DirectoryMeta::Decoded(_)) {
				out.push((uuid, kind, format!("{meta:?}")));
			}
		};

	for event in &events {
		let uuid = event.uuid;
		match &event.kind {
			DecryptedUserEventKind::FileUploaded(info)
			| DecryptedUserEventKind::FileVersioned(info)
			| DecryptedUserEventKind::FileRestored(info)
			| DecryptedUserEventKind::VersionedFileRestored(info)
			| DecryptedUserEventKind::FileMoved(info)
			| DecryptedUserEventKind::FileTrash(info)
			| DecryptedUserEventKind::FileRm(info)
			| DecryptedUserEventKind::FileLinkEdited(info)
			| DecryptedUserEventKind::DeleteFilePermanently(info) => {
				check_file(uuid, event.event_type(), &info.metadata, &mut undecryptable);
			}
			DecryptedUserEventKind::FileRenamed(info)
			| DecryptedUserEventKind::FileMetadataChanged(info) => {
				check_file(uuid, event.event_type(), &info.metadata, &mut undecryptable);
				check_file(
					uuid,
					event.event_type(),
					&info.old_metadata,
					&mut undecryptable,
				);
			}
			DecryptedUserEventKind::FileShared(info) => {
				check_file(uuid, event.event_type(), &info.metadata, &mut undecryptable);
			}
			DecryptedUserEventKind::FolderTrash(info)
			| DecryptedUserEventKind::FolderMoved(info)
			| DecryptedUserEventKind::SubFolderCreated(info)
			| DecryptedUserEventKind::BaseFolderCreated(info)
			| DecryptedUserEventKind::FolderRestored(info)
			| DecryptedUserEventKind::FolderColorChanged(info)
			| DecryptedUserEventKind::DeleteFolderPermanently(info) => {
				check_dir(uuid, event.event_type(), &info.name, &mut undecryptable);
			}
			DecryptedUserEventKind::FolderRenamed(info)
			| DecryptedUserEventKind::FolderMetadataChanged(info) => {
				check_dir(uuid, event.event_type(), &info.name, &mut undecryptable);
				check_dir(uuid, event.event_type(), &info.old_name, &mut undecryptable);
			}
			DecryptedUserEventKind::FolderShared(info) => {
				check_dir(uuid, event.event_type(), &info.name, &mut undecryptable);
			}
			DecryptedUserEventKind::ItemFavorite(info) => {
				// item favorite metadata could be either file or folder; we
				// at least require it to be decrypted (Decoded or DecryptedUTF8 — the latter for folders).
				match &info.metadata {
					FileMeta::Decoded(_) | FileMeta::DecryptedUTF8(_) => {}
					other => undecryptable.push((uuid, "itemFavorite", format!("{other:?}"))),
				}
			}
			// Variants with no encrypted payload — nothing to check.
			_ => {}
		}
	}

	assert!(
		undecryptable.is_empty(),
		"some event metadata failed to decode: {:#?}",
		undecryptable
	);
}

#[shared_test_runtime]
async fn set_nickname_round_trip() {
	use base64::{Engine, prelude::BASE64_URL_SAFE_NO_PAD};

	let client = test_utils::RESOURCES.client().await;
	let original = client.get_user_info().await.unwrap().nick_name;

	let suffix: [u8; 6] = rand::random();
	let new_nick = format!("test-nick-{}", BASE64_URL_SAFE_NO_PAD.encode(suffix));

	client.set_nickname(Some(new_nick.clone())).await.unwrap();
	let after = client.get_user_info().await.unwrap().nick_name;
	assert_eq!(after, Some(new_nick), "nickname should be updated");

	client.set_nickname(original.clone()).await.unwrap();
	let restored = client.get_user_info().await.unwrap().nick_name;
	assert_eq!(restored, original, "nickname should be restored");
}

#[shared_test_runtime]
async fn get_gdpr_info_returns_user_data() {
	let client = test_utils::RESOURCES.client().await;
	let gdpr = client.get_gdpr_info().await.unwrap();

	// Email on the GDPR response should match the authenticated user's email.
	assert_eq!(gdpr.user.email, client.email());

	// Events list is per the user's audit log (the same data backing
	// /v3/user/events); we don't assert content, just that the response is
	// shaped correctly enough to parse.
	let _ = gdpr.events.ip_addresses.len();
	let _ = gdpr.events.user_agents.len();
}

#[shared_test_runtime]
async fn update_personal_info_round_trip() {
	use base64::{Engine, prelude::BASE64_URL_SAFE_NO_PAD};
	use filen_sdk_rs::user::UpdatePersonalInfo;
	use futures::FutureExt;
	use std::panic::AssertUnwindSafe;

	let client = test_utils::RESOURCES.client().await;
	let _lock = client
		.acquire_lock_with_default(LOCK_PERSONAL_INFO)
		.await
		.unwrap();

	let suffix: [u8; 6] = rand::random();
	let tag = BASE64_URL_SAFE_NO_PAD.encode(suffix);
	let test_first = format!("TestFirst-{tag}");
	let test_last = format!("TestLast-{tag}");
	let test_company = format!("TestCompany-{tag}");
	let test_country = "United States".to_string();
	let test_street = format!("TestStreet-{tag}");
	let test_street_no = "42".to_string();
	let test_postal = "12345".to_string();
	let test_city = format!("TestCity-{tag}");

	// Capture original values so we can restore even if the test body panics.
	let before = client.get_gdpr_info().await.unwrap().user;

	let result = AssertUnwindSafe(async {
		client
			.update_personal_info(&UpdatePersonalInfo {
				city: Some(&test_city),
				company_name: Some(&test_company),
				country: Some(&test_country),
				first_name: Some(&test_first),
				last_name: Some(&test_last),
				postal_code: Some(&test_postal),
				street: Some(&test_street),
				street_number: Some(&test_street_no),
				vat_id: None,
			})
			.await
			.unwrap();

		// Verify via the GDPR endpoint (which exposes the personal-info fields).
		let after = client.get_gdpr_info().await.unwrap().user;
		assert_eq!(after.first_name.as_deref(), Some(test_first.as_str()));
		assert_eq!(after.last_name.as_deref(), Some(test_last.as_str()));
		assert_eq!(after.company_name.as_deref(), Some(test_company.as_str()));
		assert_eq!(after.country.as_deref(), Some(test_country.as_str()));
		assert_eq!(after.street.as_deref(), Some(test_street.as_str()));
		assert_eq!(
			after.street_number.as_deref(),
			Some(test_street_no.as_str())
		);
		assert_eq!(after.postal_code.as_deref(), Some(test_postal.as_str()));
		assert_eq!(after.city.as_deref(), Some(test_city.as_str()));
	})
	.catch_unwind()
	.await;

	// Restore happens regardless of whether the assertions panicked.
	let restore = client
		.update_personal_info(&UpdatePersonalInfo {
			city: before.city.as_deref(),
			company_name: before.company_name.as_deref(),
			country: before.country.as_deref(),
			first_name: before.first_name.as_deref(),
			last_name: before.last_name.as_deref(),
			postal_code: before.postal_code.as_deref(),
			street: before.street.as_deref(),
			street_number: before.street_number.as_deref(),
			vat_id: before.vat_id.as_deref(),
		})
		.await;

	if let Err(panic) = result {
		std::panic::resume_unwind(panic);
	}
	restore.unwrap();
}
