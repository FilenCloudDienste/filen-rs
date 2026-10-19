use std::borrow::Cow;

use chrono::TimeZone;
use filen_types::crypto::EncryptedString;

use super::*;
use crate::{
	consts::CHUNK_SIZE_U64,
	crypto::{file::FileKey, shared::CreateRandom, v3::EncryptionKey},
	fs::{
		file::{
			AnonymousRemoteFile, RemoteFile,
			meta::{DecryptedFileMeta, FileMeta},
		},
		name::encode_name,
	},
};

fn dir(name: Option<&str>) -> SourceDir<()> {
	SourceDir {
		uuid: Uuid::new_v4(),
		name: name.map(str::to_owned),
		created: Some(Utc.with_ymd_and_hms(2020, 1, 1, 0, 0, 0).unwrap()),
		color: DirColor::Blue,
		handle: (),
	}
}

fn file_with_meta(meta: FileMeta<'static>, size: u64) -> RemoteFileType<'static> {
	let anonymous: AnonymousRemoteFile = RemoteFile::from_meta(
		Uuid::new_v4(),
		(),
		Uuid::new_v4().into(),
		size,
		size.div_ceil(CHUNK_SIZE_U64),
		"de-1",
		"bucket",
		Utc::now(),
		false,
		meta,
	);
	RemoteFileType::File(Cow::Owned(anonymous))
}

fn file(name: &str, size: u64) -> RemoteFileType<'static> {
	file_with_meta(
		FileMeta::Decoded(DecryptedFileMeta {
			name: Cow::Owned(name.to_owned()),
			size,
			mime: Cow::Borrowed("application/octet-stream"),
			key: FileKey::V3(EncryptionKey::generate()),
			last_modified: Utc::now(),
			created: None,
			hash: None,
		}),
		size,
	)
}

fn undecryptable_file(size: u64) -> RemoteFileType<'static> {
	file_with_meta(
		FileMeta::Encrypted(EncryptedString(Cow::Borrowed("garbage"))),
		size,
	)
}

fn listed<T>(parent: &SourceDir<()>, item: T) -> Listed<T> {
	Listed {
		parent: parent.uuid,
		item,
	}
}

fn planner(destination: Uuid, existing: &[&str]) -> ItemPlanner {
	let mut planner = ItemPlanner::default();
	planner.add_destination(destination, existing.iter().copied());
	planner
}

fn request(source: PlanSource<()>, destination: Uuid) -> PlanRequest<()> {
	PlanRequest {
		source,
		destination,
		name: None,
	}
}

fn name(dir_or_file: &ValidatedName) -> &str {
	dir_or_file.as_ref()
}

/// Every item comes after the directory it is created in.
fn assert_parent_first(plan: &ItemPlan<()>) {
	for (index, dir) in plan.dirs.iter().enumerate() {
		if let DestParent::Planned(parent) = dir.parent {
			assert!(parent < index, "dir {index} precedes its parent {parent}");
		}
	}
	for file in &plan.files {
		if let DestParent::Planned(parent) = file.parent {
			assert!(parent < plan.dirs.len());
		}
	}
}

#[test]
fn the_same_source_twice_gets_two_names() {
	let destination = Uuid::new_v4();
	let source = file("a.txt", 10);
	let plan = planner(destination, &[])
		.plan(vec![
			request(PlanSource::File(source.clone()), destination),
			request(PlanSource::File(source), destination),
		])
		.unwrap();
	let names: Vec<&str> = plan.files.iter().map(|f| name(&f.name)).collect();
	assert_eq!(names, ["a.txt", "a (1).txt"]);
	assert_ne!(plan.files[0].dest_uuid, plan.files[1].dest_uuid);
	assert_eq!(plan.top_level.len(), 2);
	assert_eq!(plan.totals.bytes, 20);
}

#[test]
fn top_level_file_gets_a_keep_both_name() {
	let destination = Uuid::new_v4();
	let plan = planner(destination, &["a.txt"])
		.plan(vec![request(
			PlanSource::File(file("a.txt", 10)),
			destination,
		)])
		.unwrap();

	assert_eq!(plan.files.len(), 1);
	assert_eq!(name(&plan.files[0].name), "a (1).txt");
	assert_eq!(plan.files[0].parent, DestParent::Existing(destination));
	assert_eq!(
		plan.top_level,
		vec![PlannedTopLevel {
			request: 0,
			item: PlannedItem::File(0)
		}]
	);
	// a keep-both rename at the top level is expected, not reported
	assert!(plan.renamed.is_empty());
	assert_eq!(
		plan.totals,
		PlanTotals {
			dirs: 0,
			files: 1,
			bytes: 10
		}
	);
}

#[test]
fn destinations_with_hidden_names_are_carried_into_the_plan() {
	let (verified, unverified) = (Uuid::new_v4(), Uuid::new_v4());
	let mut planner = ItemPlanner::default();
	planner.add_destination(verified, std::iter::empty());
	planner.add_destination(unverified, std::iter::empty());
	planner.mark_unverified(unverified);
	let plan = planner
		.plan(vec![
			request(PlanSource::File(file("a", 1)), verified),
			request(PlanSource::File(file("b", 1)), unverified),
		])
		.unwrap();
	assert_eq!(plan.unverified_destinations, HashSet::from([unverified]));
}

#[test]
fn requests_into_one_destination_share_its_names() {
	let destination = Uuid::new_v4();
	let mut second = request(PlanSource::File(file("b.txt", 1)), destination);
	second.name = Some(ValidatedName::try_from("a.txt").unwrap());
	let plan = planner(destination, &[])
		.plan(vec![
			request(PlanSource::File(file("a.txt", 1)), destination),
			second,
		])
		.unwrap();
	let names = plan.files.iter().map(|f| name(&f.name)).collect::<Vec<_>>();
	assert_eq!(names, ["a.txt", "a (1).txt"]);
	assert_eq!(plan.top_level.len(), 2);
}

#[test]
fn tree_is_planned_parent_first_with_metadata_and_counts() {
	let destination = Uuid::new_v4();
	let root = dir(Some("Photos"));
	let year = dir(Some("2020"));
	let plan = planner(destination, &["Photos"])
		.plan(vec![request(
			PlanSource::Dir {
				dirs: vec![listed(&root, year.clone())],
				files: vec![
					listed(&year, file("x.jpg", 5)),
					listed(&root, file("y.txt", 7)),
					listed(&root, file("empty", 0)),
				],
				root: root.clone(),
			},
			destination,
		)])
		.unwrap();
	assert_parent_first(&plan);

	assert_eq!(plan.dirs.len(), 2);
	let photos = &plan.dirs[0];
	assert_eq!(name(&photos.name), "Photos (1)");
	assert_eq!(photos.parent, DestParent::Existing(destination));
	assert_eq!(photos.source_uuid, root.uuid);
	assert_eq!(photos.created, root.created);
	assert_eq!(photos.color, DirColor::Blue);
	assert_eq!(photos.source_path, "Photos");
	assert_eq!(
		(
			photos.descendant_dirs,
			photos.descendant_files,
			photos.descendant_bytes
		),
		(1, 3, 12)
	);

	let year_planned = &plan.dirs[1];
	assert_eq!(name(&year_planned.name), "2020");
	assert_eq!(year_planned.parent, DestParent::Planned(0));
	assert_eq!(year_planned.source_path, "Photos/2020");
	assert_eq!(
		(
			year_planned.descendant_dirs,
			year_planned.descendant_files,
			year_planned.descendant_bytes
		),
		(0, 1, 5)
	);

	let by_name = |n: &str| plan.files.iter().find(|f| name(&f.name) == n).unwrap();
	assert_eq!(by_name("x.jpg").parent, DestParent::Planned(1));
	assert_eq!(by_name("x.jpg").source_path, "Photos/2020/x.jpg");
	assert_eq!(by_name("y.txt").parent, DestParent::Planned(0));
	assert_eq!(by_name("empty").size, 0);

	assert_eq!(
		plan.totals,
		PlanTotals {
			dirs: 2,
			files: 3,
			bytes: 12
		}
	);
	assert_eq!(
		plan.top_level,
		vec![PlannedTopLevel {
			request: 0,
			item: PlannedItem::Dir(0)
		}]
	);
	let mut dest_uuids = plan
		.dirs
		.iter()
		.map(|d| d.dest_uuid)
		.chain(plan.files.iter().map(|f| f.dest_uuid))
		.collect::<Vec<_>>();
	dest_uuids.sort();
	dest_uuids.dedup();
	assert_eq!(dest_uuids.len(), 5, "every planned item gets its own uuid");
}

#[test]
fn copying_a_directory_into_itself_or_a_descendant_is_rejected() {
	let root = dir(Some("a"));
	let child = dir(Some("b"));
	let source = || PlanSource::Dir {
		root: root.clone(),
		dirs: vec![listed(&root, child.clone())],
		files: Vec::new(),
	};
	for destination in [root.uuid, child.uuid] {
		let error = planner(destination, &[])
			.plan(vec![request(source(), destination)])
			.unwrap_err();
		assert_eq!(error.kind(), ErrorKind::InvalidState);
		assert!(
			error
				.to_string()
				.contains("cannot copy a directory into itself or one of its subdirectories"),
			"{error}"
		);
	}
}

#[test]
fn an_invalid_request_rejects_the_whole_plan() {
	let destination = Uuid::new_v4();
	let root = dir(Some("a"));
	let error = planner(destination, &[])
		.plan(vec![
			request(PlanSource::File(file("ok.txt", 1)), destination),
			request(
				PlanSource::Dir {
					root: root.clone(),
					dirs: Vec::new(),
					files: Vec::new(),
				},
				root.uuid,
			),
		])
		.unwrap_err();
	// the second request's destination was never registered
	assert_eq!(error.kind(), ErrorKind::Internal);
}

#[test]
fn case_duplicate_siblings_are_renamed_not_dropped() {
	let destination = Uuid::new_v4();
	let root = dir(Some("root"));
	let clash_dir = dir(Some("x"));
	let plan = planner(destination, &[])
		.plan(vec![request(
			PlanSource::Dir {
				dirs: vec![listed(&root, clash_dir)],
				files: vec![
					listed(&root, file("A.txt", 1)),
					listed(&root, file("a.txt", 2)),
					listed(&root, file("X", 3)),
				],
				root: root.clone(),
			},
			destination,
		)])
		.unwrap();

	let names = plan.files.iter().map(|f| name(&f.name)).collect::<Vec<_>>();
	assert_eq!(names, ["A.txt", "a (1).txt", "X (1)"]);
	assert_eq!(name(&plan.dirs[1].name), "x");
	assert_eq!(plan.renamed.len(), 2);
	assert!(
		plan.renamed
			.iter()
			.all(|r| r.reason == RenameReason::DuplicateName)
	);
	assert_eq!(plan.totals.files, 3, "nothing is dropped");
}

#[test]
fn undecryptable_dir_is_named_by_uuid_and_keeps_its_contents() {
	let destination = Uuid::new_v4();
	let root = dir(Some("root"));
	let hidden = dir(None);
	let plan = planner(destination, &[])
		.plan(vec![request(
			PlanSource::Dir {
				dirs: vec![listed(&root, hidden.clone())],
				files: vec![listed(&hidden, file("inside.txt", 4))],
				root,
			},
			destination,
		)])
		.unwrap();

	assert_eq!(name(&plan.dirs[1].name), hidden.uuid.to_string());
	assert_eq!(plan.files.len(), 1);
	assert_eq!(plan.files[0].parent, DestParent::Planned(1));
	assert_eq!(plan.renamed.len(), 1);
	assert_eq!(plan.renamed[0].reason, RenameReason::Undecryptable);
	assert_eq!(plan.renamed[0].source_uuid, hidden.uuid);
}

#[test]
fn undecryptable_top_level_dir_is_reported_too() {
	let destination = Uuid::new_v4();
	let root = dir(None);
	let plan = planner(destination, &[])
		.plan(vec![request(
			PlanSource::Dir {
				root: root.clone(),
				dirs: Vec::new(),
				files: Vec::new(),
			},
			destination,
		)])
		.unwrap();
	assert_eq!(name(&plan.dirs[0].name), root.uuid.to_string());
	assert_eq!(plan.renamed[0].reason, RenameReason::Undecryptable);
}

#[test]
fn undecryptable_files_are_skipped_and_reported() {
	let destination = Uuid::new_v4();
	let root = dir(Some("root"));
	let hidden = undecryptable_file(9);
	let hidden_uuid = hidden.uuid();
	let top = undecryptable_file(3);
	let top_uuid = top.uuid();
	let plan = planner(destination, &[])
		.plan(vec![
			request(
				PlanSource::Dir {
					dirs: Vec::new(),
					files: vec![listed(&root, hidden)],
					root,
				},
				destination,
			),
			request(PlanSource::File(top), destination),
		])
		.unwrap();

	assert!(plan.files.is_empty());
	assert_eq!(plan.skipped.len(), 2);
	assert_eq!(plan.skipped[0].bytes, 9);
	assert_eq!(
		plan.skipped[0].reason,
		SkipReason::UndecryptableFile { uuid: hidden_uuid }
	);
	assert_eq!(plan.skipped[0].source_path, format!("root/{hidden_uuid}"));
	assert_eq!(
		plan.skipped[1].reason,
		SkipReason::UndecryptableFile { uuid: top_uuid }
	);
	// a skipped top-level file has no planned top-level item
	assert_eq!(plan.top_level.len(), 1);
	assert_eq!(plan.totals.bytes, 0);
}

#[test]
fn unreachable_entries_are_reported_in_one_aggregate() {
	let destination = Uuid::new_v4();
	let root = dir(Some("root"));
	let orphan_parent = dir(Some("gone"));
	let orphan = dir(Some("orphan"));
	let plan = planner(destination, &[])
		.plan(vec![request(
			PlanSource::Dir {
				dirs: vec![listed(&orphan_parent, orphan.clone())],
				files: vec![
					listed(&orphan, file("f", 6)),
					listed(&root, file("kept", 1)),
				],
				root,
			},
			destination,
		)])
		.unwrap();

	assert_eq!(plan.dirs.len(), 1);
	assert_eq!(plan.files.len(), 1);
	assert_eq!(plan.skipped.len(), 1);
	assert_eq!(plan.skipped[0].reason, SkipReason::Unreachable { count: 2 });
	assert_eq!(plan.skipped[0].bytes, 6);
}

#[test]
fn invalid_legacy_names_are_encoded_and_reported() {
	let destination = Uuid::new_v4();
	let root = dir(Some("root"));
	let plan = planner(destination, &[])
		.plan(vec![request(
			PlanSource::Dir {
				dirs: Vec::new(),
				files: vec![listed(&root, file("a:b.txt", 1))],
				root,
			},
			destination,
		)])
		.unwrap();
	let encoded = encode_name("a:b.txt").unwrap();
	assert_eq!(plan.files[0].name, encoded);
	assert_eq!(plan.renamed.len(), 1);
	assert_eq!(plan.renamed[0].reason, RenameReason::InvalidName);
}

#[test]
fn deep_trees_are_planned_iteratively() {
	const DEPTH: usize = 10_000;
	let destination = Uuid::new_v4();
	let root = dir(Some("root"));
	let mut parent = root.clone();
	let mut dirs = Vec::with_capacity(DEPTH);
	for _ in 0..DEPTH {
		let child = dir(Some("d"));
		dirs.push(listed(&parent, child.clone()));
		parent = child;
	}
	let files = vec![listed(&parent, file("leaf", 2))];

	let plan = std::thread::Builder::new()
		.stack_size(256 * 1024)
		.spawn(move || {
			planner(destination, &[])
				.plan(vec![request(
					PlanSource::Dir { root, dirs, files },
					destination,
				)])
				.unwrap()
		})
		.unwrap()
		.join()
		.expect("planning a deep tree must not overflow the stack");

	assert_eq!(plan.dirs.len(), DEPTH + 1);
	assert_parent_first(&plan);
	assert_eq!(plan.dirs[0].descendant_dirs, DEPTH as u64);
	assert_eq!(plan.dirs[0].descendant_bytes, 2);
	assert_eq!(plan.dirs[DEPTH].descendant_files, 1);
}
