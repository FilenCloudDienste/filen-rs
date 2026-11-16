use notify::event::{
	AccessKind, AccessMode, CreateKind, DataChange, Flag, MetadataKind, RemoveKind,
};

use super::*;
use crate::test_support::root_path;

fn root() -> PathBuf {
	PathBuf::from("/sync/root")
}

fn event(kind: EventKind, paths: &[&str]) -> Event {
	paths.iter().fold(Event::new(kind), |event, path| {
		event.add_path(path.split('\\').fold(root(), |full, name| full.join(name)))
	})
}

fn events(event: Event) -> Vec<LocalEvent> {
	local_events(&root(), Ok(event))
}

#[test]
fn a_lost_event_a_root_change_and_an_error_walk_the_root() {
	let lost = Event::new(EventKind::Other).set_flag(Flag::Rescan);
	assert_eq!(events(lost), vec![LocalEvent::Rescan]);
	let mut changed = Event::new(EventKind::Any).add_path(root());
	// The notice `notify`'s macOS backend gives an event on the watched root itself.
	changed.attrs.set_info("root changed");
	assert_eq!(events(changed), vec![LocalEvent::Rescan]);
	assert_eq!(
		local_events(&root(), Err(notify::Error::generic("overflow"))),
		vec![LocalEvent::Rescan]
	);
}

#[test]
fn a_path_outside_the_root_walks_the_root() {
	let outside =
		Event::new(EventKind::Create(CreateKind::Any)).add_path(PathBuf::from("/other/a"));
	assert_eq!(events(outside), vec![LocalEvent::Rescan]);
	let up = Event::new(EventKind::Create(CreateKind::Any)).add_path(root().join("../a"));
	assert_eq!(events(up), vec![LocalEvent::Rescan]);
	// The root itself removed or renamed away.
	let removed = Event::new(EventKind::Remove(RemoveKind::Folder)).add_path(root());
	assert_eq!(events(removed), vec![LocalEvent::Rescan]);
	let renamed =
		Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::From))).add_path(root());
	assert_eq!(events(renamed), vec![LocalEvent::Rescan]);
	// A change of the root's own metadata is nothing to look at.
	let touched =
		Event::new(EventKind::Modify(ModifyKind::Metadata(MetadataKind::Any))).add_path(root());
	assert_eq!(events(touched), vec![]);
}

#[test]
fn each_half_of_a_rename_is_a_look_at_its_path() {
	// Windows reports the two halves apart (measured: Name(From), Name(To)).
	assert_eq!(
		events(event(
			EventKind::Modify(ModifyKind::Name(RenameMode::From)),
			&["a.txt"]
		)),
		vec![LocalEvent::Modified(root_path("a.txt"))]
	);
	assert_eq!(
		events(event(
			EventKind::Modify(ModifyKind::Name(RenameMode::To)),
			&[r"d\b.txt"]
		)),
		vec![LocalEvent::Modified(root_path(r"d\b.txt"))]
	);
	assert_eq!(
		events(event(
			EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
			&["a.txt", "b.txt"]
		)),
		vec![LocalEvent::Renamed {
			from: root_path("a.txt"),
			to: root_path("b.txt"),
		}]
	);
}

#[test]
fn creates_writes_and_removes_are_looks_at_their_paths() {
	assert_eq!(
		events(event(EventKind::Create(CreateKind::Any), &["new.txt"])),
		vec![LocalEvent::Created(root_path("new.txt"))]
	);
	assert_eq!(
		events(event(
			EventKind::Modify(ModifyKind::Data(DataChange::Any)),
			&[r"d\a.txt"]
		)),
		vec![LocalEvent::Modified(root_path(r"d\a.txt"))]
	);
	assert_eq!(
		events(event(EventKind::Remove(RemoveKind::Any), &["gone.txt"])),
		vec![LocalEvent::Removed(root_path("gone.txt"))]
	);
	assert_eq!(
		events(event(EventKind::Any, &["a.txt"])),
		vec![LocalEvent::Modified(root_path("a.txt"))]
	);
}

#[test]
fn reads_are_ignored_but_a_closed_writer_is_looked_at() {
	assert_eq!(
		events(event(
			EventKind::Access(AccessKind::Close(AccessMode::Write)),
			&["a.txt"]
		)),
		vec![LocalEvent::Modified(root_path("a.txt"))]
	);
	for access in [
		AccessKind::Read,
		AccessKind::Open(AccessMode::Any),
		AccessKind::Close(AccessMode::Read),
	] {
		assert_eq!(events(event(EventKind::Access(access), &["a.txt"])), vec![]);
	}
}

#[test]
fn a_name_the_core_cannot_name_is_looked_at_through_its_directory() {
	// A trailing space: NTFS holds it, the core skips it.
	assert_eq!(
		events(event(EventKind::Create(CreateKind::Any), &[r"d\bad "])),
		vec![LocalEvent::Created(root_path("d"))]
	);
	assert_eq!(
		events(event(EventKind::Create(CreateKind::Any), &[r"bad \x.txt"])),
		vec![LocalEvent::Created(RootPath::root())]
	);
}
