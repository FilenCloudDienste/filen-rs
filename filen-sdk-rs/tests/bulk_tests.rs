use chrono::Utc;
use filen_macros::shared_test_runtime;
use filen_sdk_rs::fs::{
	CreatedDir, HasName, HasParent, HasUUID, NewDir, NewDirParent, dir::RemoteDirectory,
};
use filen_types::{
	api::v3::{bulk::FileOrDir, dir},
	fs::{ParentUuid, Uuid},
};

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
	assert_eq!(
		x,
		CreatedDir {
			uuid: existing.uuid(),
			merged: true,
		}
	);
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

fn dir_uuids(dirs: &[RemoteDirectory]) -> Vec<Uuid> {
	dirs.iter().map(HasUUID::uuid).collect()
}
