use chrono::Utc;
use filen_macros::shared_test_runtime;
use filen_sdk_rs::{
	ErrorKind,
	auth::Client,
	fs::{
		HasName, HasParent, HasUUID, NewDir, NewDirParent, dir::RemoteDirectory, file::RemoteFile,
	},
};
use filen_types::{
	api::v3::{bulk::FileOrDir, dir, file},
	fs::{ParentUuid, Uuid},
};

mod drive_helpers;

fn new_dir(name: &str, parent: NewDirParent) -> NewDir<'_> {
	NewDir {
		name,
		created: Utc::now(),
		parent,
	}
}

#[shared_test_runtime]
async fn create_dirs_puts_children_under_the_uuids_their_parents_were_answered() {
	let (resources, _lock) = test_utils::RESOURCES.get_resources_with_lock().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let results = client
		.create_dirs_for_test(&[
			new_dir("a", NewDirParent::Existing(test_dir.uuid())),
			new_dir("b", NewDirParent::Earlier(0)),
			new_dir("c", NewDirParent::Earlier(1)),
		])
		.await
		.unwrap();
	let [a, b, c] = results
		.into_iter()
		.map(Result::unwrap)
		.collect::<Vec<_>>()
		.try_into()
		.unwrap();
	assert!(!a.merged && !b.merged && !c.merged, "{a:?} {b:?} {c:?}");

	for (created, name, parent) in [
		(a, "a", test_dir.uuid()),
		(b, "b", a.uuid),
		(c, "c", b.uuid),
	] {
		let found = client.get_dir(created.uuid).await.unwrap();
		assert_eq!(found.name(), Some(name));
		assert_eq!(
			*found.parent(),
			ParentUuid::Uuid(parent),
			"parent of {name}"
		);
	}
}

#[shared_test_runtime]
async fn create_dirs_merges_an_existing_dir_and_creates_its_children_in_it() {
	let (resources, _lock) = test_utils::RESOURCES.get_resources_with_lock().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let existing = client.create_dir(&test_dir.into(), "x").await.unwrap();

	let results = client
		.create_dirs_for_test(&[
			new_dir("x", NewDirParent::Existing(test_dir.uuid())),
			new_dir("y", NewDirParent::Earlier(0)),
		])
		.await
		.unwrap();
	let [x, y] = results
		.into_iter()
		.map(Result::unwrap)
		.collect::<Vec<_>>()
		.try_into()
		.unwrap();
	assert_eq!((x.uuid, x.merged), (existing.uuid(), true));
	assert!(!y.merged, "{y:?}");

	let found = client.get_dir(y.uuid).await.unwrap();
	assert_eq!(found.name(), Some("y"));
	assert_eq!(*found.parent(), ParentUuid::Uuid(existing.uuid()));
	let (dirs, _) = client
		.list_dir(&test_dir.into(), None::<&fn(u64, Option<u64>)>)
		.await
		.unwrap();
	assert_eq!(dir_uuids(&dirs), [existing.uuid()]);
}

#[shared_test_runtime]
async fn trash_items_trashes_the_topmost_dirs_it_is_given() {
	// Without the drive lock held across the test: the trash lock below is taken before the drive
	// lock everywhere else (`empty_trash` locks the drive), so holding both in the other order
	// could deadlock with another leg. Each bulk call locks the drive itself.
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let created = client
		.create_dirs_for_test(&[
			new_dir("kept", NewDirParent::Existing(test_dir.uuid())),
			new_dir("p", NewDirParent::Existing(test_dir.uuid())),
			new_dir("p_child", NewDirParent::Earlier(1)),
			new_dir("q", NewDirParent::Existing(test_dir.uuid())),
		])
		.await
		.unwrap()
		.into_iter()
		.map(|result| result.unwrap().uuid)
		.collect::<Vec<_>>();
	let [kept, p, _, q] = created.try_into().unwrap();

	// Hold the trash lock across trash -> get_dir: another leg's account-global empty-trash
	// (serialized on this lock) would permanently delete the dirs before they are looked up.
	let _trash_lock = client
		.acquire_lock_with_default(test_utils::locks::TRASH)
		.await
		.unwrap();
	let results = client
		.trash_items_for_test(&[
			FileOrDir::Dir(dir::trash::Request { uuid: p }),
			FileOrDir::Dir(dir::trash::Request { uuid: q }),
		])
		.await
		.unwrap();
	assert_eq!(results.len(), 2);
	for result in results {
		result.unwrap();
	}

	for uuid in [p, q] {
		let found = client.get_dir(uuid).await.unwrap();
		assert_eq!(*found.parent(), ParentUuid::Trash(test_dir.uuid()));
	}
	let (dirs, _) = client
		.list_dir(&test_dir.into(), None::<&fn(u64, Option<u64>)>)
		.await
		.unwrap();
	assert_eq!(dir_uuids(&dirs), [kept]);
}

/// Dirs `a`, `b` and `s` and the file `f.txt` in `test_dir`.
async fn move_fixtures(client: &Client, test_dir: &RemoteDirectory) -> ([Uuid; 3], RemoteFile) {
	let dirs = client
		.create_dirs_for_test(&[
			new_dir("a", NewDirParent::Existing(test_dir.uuid())),
			new_dir("b", NewDirParent::Existing(test_dir.uuid())),
			new_dir("s", NewDirParent::Existing(test_dir.uuid())),
		])
		.await
		.unwrap()
		.into_iter()
		.map(|result| result.unwrap().uuid)
		.collect::<Vec<_>>();
	let file = drive_helpers::upload(client, test_dir, "f.txt", b"f").await;
	(dirs.try_into().unwrap(), file)
}

#[shared_test_runtime]
async fn move_items_moves_files_and_dirs_into_their_destinations() {
	let (resources, _lock) = test_utils::RESOURCES.get_resources_with_lock().await;
	let client = &resources.client;
	let test_dir = &resources.dir;
	let ([a, b, s], f) = move_fixtures(client, test_dir).await;

	let results = client
		.move_items_for_test(&[
			FileOrDir::File(file::r#move::Request {
				uuid: f.uuid(),
				new_parent: a,
			}),
			FileOrDir::Dir(dir::r#move::Request { uuid: s, to: b }),
		])
		.await
		.unwrap();
	assert_eq!(results.len(), 2);
	for result in results {
		result.unwrap();
	}

	let found = client.get_file(f.uuid()).await.unwrap();
	assert_eq!(*found.parent(), ParentUuid::Uuid(a));
	let found = client.get_dir(s).await.unwrap();
	assert_eq!(*found.parent(), ParentUuid::Uuid(b));
	let (dirs, files) = client
		.list_dir(&test_dir.into(), None::<&fn(u64, Option<u64>)>)
		.await
		.unwrap();
	assert_eq!(names(&files), Vec::<&str>::new());
	assert_eq!(names(&dirs), ["a", "b"]);
}

#[shared_test_runtime]
async fn move_items_fails_only_the_item_the_server_refuses() {
	let (resources, _lock) = test_utils::RESOURCES.get_resources_with_lock().await;
	let client = &resources.client;
	let test_dir = &resources.dir;
	let ([a, b, s], f) = move_fixtures(client, test_dir).await;

	let results = client
		.move_items_for_test(&[
			FileOrDir::File(file::r#move::Request {
				uuid: f.uuid(),
				new_parent: a,
			}),
			FileOrDir::Dir(dir::r#move::Request { uuid: a, to: a }),
			FileOrDir::Dir(dir::r#move::Request { uuid: s, to: b }),
		])
		.await
		.unwrap();
	let [moved_file, into_itself, moved_dir] = results.try_into().unwrap();
	moved_file.unwrap();
	assert_eq!(
		into_itself.unwrap_err().server_code().as_deref(),
		Some("cannot_move_folder_into_itself")
	);
	moved_dir.unwrap();

	let found = client.get_file(f.uuid()).await.unwrap();
	assert_eq!(*found.parent(), ParentUuid::Uuid(a));
	let found = client.get_dir(s).await.unwrap();
	assert_eq!(*found.parent(), ParentUuid::Uuid(b));
	let found = client.get_dir(a).await.unwrap();
	assert_eq!(*found.parent(), ParentUuid::Uuid(test_dir.uuid()));
}

#[shared_test_runtime]
async fn delete_items_permanently_deletes_files_and_dirs() {
	let (resources, _lock) = test_utils::RESOURCES.get_resources_with_lock().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let mut d = client.create_dir(&test_dir.into(), "d").await.unwrap();
	let f = drive_helpers::upload(client, test_dir, "f.txt", b"f").await;

	let results = client
		.delete_items_permanently_for_test(&[
			FileOrDir::File(file::delete::permanent::Request { uuid: f.uuid() }),
			FileOrDir::Dir(dir::delete::permanent::Request { uuid: d.uuid() }),
		])
		.await
		.unwrap();
	assert_eq!(results.len(), 2);
	for result in results {
		result.unwrap();
	}

	assert_eq!(
		client.get_file(f.uuid()).await.unwrap_err().kind(),
		ErrorKind::FileNotFound
	);
	// Until the server purges a deleted folder, `v3/dir` still answers it as trashed, as after a
	// single `dir/delete/permanent`, so the purge is not something to wait for. Restoring tells it
	// apart from a trashed folder (restored) and from one left alone (`folder_not_in_trash`).
	let restored = client.restore_dir(&mut d).await.unwrap_err();
	assert_eq!(
		(restored.kind(), restored.server_code().as_deref()),
		(ErrorKind::FolderNotFound, Some("folder_not_found"))
	);
	let (dirs, files) = client
		.list_dir(&test_dir.into(), None::<&fn(u64, Option<u64>)>)
		.await
		.unwrap();
	assert_eq!((dirs.len(), files.len()), (0, 0));
}

#[shared_test_runtime]
async fn update_metadata_renames_files_and_dirs() {
	let (resources, _lock) = test_utils::RESOURCES.get_resources_with_lock().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let a = drive_helpers::upload(client, test_dir, "a.txt", b"a").await;
	let b = client.create_dir(&test_dir.into(), "b").await.unwrap();

	let items = [
		client
			.metadata_item_for_test(FileOrDir::File(&a), "a2.txt")
			.await
			.unwrap(),
		client
			.metadata_item_for_test(FileOrDir::Dir(&b), "b2")
			.await
			.unwrap(),
	];
	let results = client.update_metadata_for_test(&items).await.unwrap();
	assert_eq!(results.len(), 2);
	for result in results {
		result.unwrap();
	}

	// Each name decrypts from what the server stored, the only check that the item's
	// encryption is right.
	let found = client.get_file(a.uuid()).await.unwrap();
	assert_eq!(
		(found.uuid(), found.name(), found.parent()),
		(a.uuid(), Some("a2.txt"), &ParentUuid::Uuid(test_dir.uuid()))
	);
	let found = client.get_dir(b.uuid()).await.unwrap();
	assert_eq!(
		(found.uuid(), found.name(), found.parent()),
		(b.uuid(), Some("b2"), &ParentUuid::Uuid(test_dir.uuid()))
	);
}

#[shared_test_runtime]
async fn trash_items_trashes_files_and_dirs_in_one_request() {
	// Without the drive lock held, for the reason trash_items_trashes_the_topmost_dirs_it_is_given
	// gives.
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	let d = client
		.create_dirs_for_test(&[new_dir("d", NewDirParent::Existing(test_dir.uuid()))])
		.await
		.unwrap()
		.remove(0)
		.unwrap()
		.uuid;
	let f = drive_helpers::upload(client, test_dir, "f.txt", b"f").await;

	// Held across trash -> get_*: another leg's empty-trash would delete the items first.
	let _trash_lock = client
		.acquire_lock_with_default(test_utils::locks::TRASH)
		.await
		.unwrap();
	let results = client
		.trash_items_for_test(&[
			FileOrDir::File(file::trash::Request { uuid: f.uuid() }),
			FileOrDir::Dir(dir::trash::Request { uuid: d }),
		])
		.await
		.unwrap();
	assert_eq!(results.len(), 2);
	for result in results {
		result.unwrap();
	}

	let found = client.get_file(f.uuid()).await.unwrap();
	assert_eq!(*found.parent(), ParentUuid::Trash(test_dir.uuid()));
	let found = client.get_dir(d).await.unwrap();
	assert_eq!(*found.parent(), ParentUuid::Trash(test_dir.uuid()));
	let (dirs, files) = client
		.list_dir(&test_dir.into(), None::<&fn(u64, Option<u64>)>)
		.await
		.unwrap();
	assert_eq!((dirs.len(), files.len()), (0, 0));
}

#[shared_test_runtime]
async fn create_dirs_fails_only_a_dir_named_like_a_file_in_its_parent() {
	let (resources, _lock) = test_utils::RESOURCES.get_resources_with_lock().await;
	let client = &resources.client;
	let test_dir = &resources.dir;

	drive_helpers::upload(client, test_dir, "x", b"x").await;

	let results = client
		.create_dirs_for_test(&[
			new_dir("x", NewDirParent::Existing(test_dir.uuid())),
			new_dir("y", NewDirParent::Existing(test_dir.uuid())),
		])
		.await
		.unwrap();
	let [x, y] = results.try_into().unwrap();
	assert_eq!(
		x.unwrap_err().server_code().as_deref(),
		Some("file_with_same_name_exists_in_parent")
	);
	let y = y.unwrap();
	assert!(!y.merged, "{y:?}");

	let (dirs, files) = client
		.list_dir(&test_dir.into(), None::<&fn(u64, Option<u64>)>)
		.await
		.unwrap();
	assert_eq!(dir_uuids(&dirs), [y.uuid]);
	assert_eq!(names(&files), ["x"]);
}

fn dir_uuids(dirs: &[RemoteDirectory]) -> Vec<Uuid> {
	dirs.iter().map(HasUUID::uuid).collect()
}

/// The names of `items`, sorted: a listing comes back in no set order.
fn names(items: &[impl HasName]) -> Vec<&str> {
	let mut names = items
		.iter()
		.map(|item| item.name().unwrap())
		.collect::<Vec<_>>();
	names.sort_unstable();
	names
}
