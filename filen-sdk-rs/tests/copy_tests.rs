use std::{borrow::Cow, sync::Arc};

use filen_macros::shared_test_runtime;
#[cfg(feature = "malformed")]
use filen_sdk_rs::fs::copy::{RenameReason, SkipReason};
use filen_sdk_rs::{
	ErrorKind,
	auth::{Client, http::ClientConfig, unauth::UnauthClient},
	connect::{DirPublicInfo, DirPublicLink, PublicLinkSharedClientExt},
	consts::{CHUNK_SIZE, FILE_CHUNK_SIZE_EXTRA_USIZE},
	fs::{
		HasName, HasParent, HasRemoteInfo, HasUUID,
		categories::{DirType, NonRootItemType},
		copy::{
			CopyConfig, CopyEvent, CopyFailed, CopyPhase, CopyRequest, CopySource, CopySourceDir,
			CopyStage, JobControl, PlanTotals,
		},
		dir::RemoteDirectory,
		file::{
			AnonymousRemoteFile, RemoteFile,
			enums::RemoteFileType,
			traits::{HasFileInfo, HasFileMeta, HasRemoteFileInfo},
		},
		name::ValidatedName,
	},
	io::client_impl::IoSharedClientExt,
};
use filen_types::{api::v3::dir::color::DirColor, fs::Uuid, traits::CowHelpersExt};
use futures::{StreamExt, stream};

mod copy_helpers;
use copy_helpers::{Recorder, SignalOnCreate, assert_same_files, contents, copy, data, upload};

#[shared_test_runtime]
async fn copy_tree_keeps_contents_and_metadata() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = resources.client.clone();
	let test_dir = &resources.dir;

	let mut source = client.create_dir(&test_dir.into(), "source").await.unwrap();
	client
		.set_dir_color(&mut source, DirColor::Blue)
		.await
		.unwrap();
	let sub = client.create_dir(&(&source).into(), "sub").await.unwrap();
	let small = upload(&client, &source, "small.txt", b"hello").await;
	let big = upload(&client, &sub, "big.bin", &data(3 * CHUNK_SIZE + 17, 1)).await;
	let empty = upload(&client, &sub, "empty", b"").await;
	let destination = client
		.create_dir(&test_dir.into(), "destination")
		.await
		.unwrap();

	let recorder = Arc::new(Recorder::default());
	let report = client
		.clone()
		.copy_items(
			vec![CopySource::Dir(CopySourceDir::Normal(source.clone()))],
			destination.clone().into(),
			CopyConfig::default(),
			recorder.clone(),
			JobControl::default(),
		)
		.await
		.unwrap();
	assert!(report.failures.is_empty());
	assert_eq!(report.counts.files_done, 3);
	assert_eq!(report.counts.dirs_created, 2);
	assert_eq!(report.top_level.len(), 1);
	assert_eq!(recorder.created.lock().unwrap().len(), 1);
	assert_eq!(
		recorder.planned.lock().unwrap()[0].dest_uuid,
		report.top_level[0].item.uuid()
	);
	assert_eq!(
		recorder.updates.lock().unwrap().last().unwrap().phase,
		CopyPhase::Done
	);

	let (dirs, files) = contents(&client, &destination).await;
	let copied_source = dirs.iter().find(|(p, _)| p == "source").unwrap();
	let copied_source = client.get_dir(copied_source.1.uuid()).await.unwrap();
	assert_eq!(copied_source.color, DirColor::Blue);
	assert_eq!(copied_source.created(), source.created());
	assert!(dirs.iter().any(|(p, _)| p == "source/sub"));

	for (path, original) in [
		("source/small.txt", &small),
		("source/sub/big.bin", &big),
		("source/sub/empty", &empty),
	] {
		let (_, copy) = files.iter().find(|(p, _)| p == path).unwrap();
		assert_ne!(copy.uuid(), original.uuid());
		assert_eq!(copy.size(), original.size());
		assert_eq!(copy.mime(), original.mime());
		assert_eq!(copy.created(), original.created());
		assert_eq!(copy.last_modified(), original.last_modified());
		assert_eq!(
			client.download_file(copy).await.unwrap(),
			client.download_file(original).await.unwrap(),
			"{path} has the same contents"
		);
	}
}

#[shared_test_runtime]
async fn copying_into_the_same_parent_keeps_both() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = resources.client.clone();
	let test_dir = &resources.dir;
	let file = upload(&client, test_dir, "a.txt", b"content").await;

	for expected in ["a (1).txt", "a (2).txt"] {
		let report = copy(
			&client,
			vec![CopySource::File(file.clone().into())],
			test_dir,
		)
		.await
		.unwrap();
		assert_eq!(
			report.top_level[0].item.name(),
			Some(expected),
			"the copy gets the next free name"
		);
	}
}

#[shared_test_runtime]
async fn copying_a_directory_into_itself_is_rejected() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = resources.client.clone();
	let test_dir = &resources.dir;
	let source = client.create_dir(&test_dir.into(), "source").await.unwrap();
	let sub = client.create_dir(&(&source).into(), "sub").await.unwrap();

	for destination in [&source, &sub] {
		let CopyFailed { error, .. } = copy(
			&client,
			vec![CopySource::Dir(CopySourceDir::Normal(source.clone()))],
			destination,
		)
		.await
		.unwrap_err();
		assert_eq!(error.kind(), ErrorKind::InvalidState);
	}
	let (dirs, files) = contents(&client, &source).await;
	assert_eq!((dirs.len(), files.len()), (1, 0), "nothing was created");
}

#[shared_test_runtime]
async fn copy_completes_on_a_small_memory_budget() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let test_dir = &resources.dir;
	let (email, password, two_factor) = test_utils::RESOURCES.get_credentials();
	// two encrypted chunks: the smallest budget every build accepts
	let config =
		ClientConfig::default().with_memory_budget(2 * (CHUNK_SIZE + FILE_CHUNK_SIZE_EXTRA_USIZE));
	let client = Arc::new(
		UnauthClient::from_config(config)
			.unwrap()
			.login(email, &password, &two_factor)
			.await
			.unwrap(),
	);

	let source = client.create_dir(&test_dir.into(), "source").await.unwrap();
	let mut originals = Vec::new();
	for i in 0..4 {
		let name = format!("f{i}.bin");
		originals.push(upload(&client, &source, &name, &data(3 * CHUNK_SIZE + i, i as u8)).await);
	}
	let destination = client
		.create_dir(&test_dir.into(), "destination")
		.await
		.unwrap();

	copy(
		&client,
		originals
			.iter()
			.cloned()
			.map(|f| CopySource::File(f.into()))
			.collect(),
		&destination,
	)
	.await
	.unwrap();

	let (_, files) = contents(&client, &destination).await;
	assert_eq!(files.len(), 4);
	assert_same_files(
		&client,
		&files,
		originals.iter().map(|f| (f.name().unwrap(), f)),
	)
	.await;
}

#[shared_test_runtime]
async fn copy_from_a_public_link_into_a_linked_directory() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = resources.client.clone();
	let test_dir = &resources.dir;
	let unauthed = client.get_unauthed();

	async fn link_info(client: &Client, dir: &RemoteDirectory) -> DirPublicInfo {
		let link: DirPublicLink = client
			.public_link_dir::<fn(u64, Option<u64>)>(dir, None)
			.await
			.unwrap()
			.try_into()
			.unwrap();
		client
			.get_unauthed()
			.get_dir_public_link_info(*link.uuid(), &link.key_string())
			.await
			.unwrap()
	}

	// the source: a directory read through its public link
	let source = client.create_dir(&test_dir.into(), "source").await.unwrap();
	upload(&client, &source, "inside.txt", b"linked content").await;
	let source_link = link_info(&client, &source).await;
	let linked_source = || {
		CopySource::Dir(CopySourceDir::Linked(
			DirType::Root(Cow::Owned(source_link.root.clone())),
			source_link.link.clone(),
		))
	};

	// copying it into itself is rejected
	let CopyFailed { error, .. } = copy(&client, vec![linked_source()], &source)
		.await
		.unwrap_err();
	assert_eq!(error.kind(), ErrorKind::InvalidState);

	// the destination: a directory inside another public link, so the copy must be added to
	// that link too
	let linked = client.create_dir(&test_dir.into(), "linked").await.unwrap();
	let destination = client
		.create_dir(&(&linked).into(), "copies")
		.await
		.unwrap();
	let destination_link = link_info(&client, &linked).await;

	let report = copy(&client, vec![linked_source()], &destination)
		.await
		.unwrap();
	let copied_root = report.top_level[0].item.uuid();

	let (dirs, _) = unauthed
		.list_linked_dir::<fn(u64, Option<u64>)>(
			&(&destination_link.root).into(),
			&destination_link.link,
			None,
		)
		.await
		.unwrap();
	let copies = dirs
		.iter()
		.find(|d| d.uuid() == destination.uuid())
		.unwrap();
	let (dirs, _) = unauthed
		.list_linked_dir::<fn(u64, Option<u64>)>(&copies.into(), &destination_link.link, None)
		.await
		.unwrap();
	let copied = dirs.iter().find(|d| d.uuid() == copied_root).unwrap();
	assert_eq!(copied.name(), Some("source"));
	let (_, files) = unauthed
		.list_linked_dir::<fn(u64, Option<u64>)>(&copied.into(), &destination_link.link, None)
		.await
		.unwrap();
	assert_eq!(files.len(), 1);
	assert_eq!(files[0].name(), Some("inside.txt"));
}

#[shared_test_runtime]
async fn cancel_ends_the_copy_and_reports_what_was_created() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = resources.client.clone();
	let test_dir = &resources.dir;
	let source = client.create_dir(&test_dir.into(), "source").await.unwrap();
	for i in 0..3 {
		upload(&client, &source, &format!("f{i}"), &data(2 * CHUNK_SIZE, i)).await;
	}
	let destination = client
		.create_dir(&test_dir.into(), "destination")
		.await
		.unwrap();

	let (control, controller) = JobControl::new();
	let recorder = Arc::new(Recorder::default());
	let CopyFailed { report, error } = client
		.clone()
		.copy_items(
			vec![CopySource::Dir(CopySourceDir::Normal(source))],
			destination.clone().into(),
			CopyConfig::default(),
			SignalOnCreate {
				recorder: recorder.clone(),
				signal: move || controller.cancel(),
			},
			control,
		)
		.await
		.unwrap_err();
	assert_eq!(error.kind(), ErrorKind::Cancelled);
	assert_eq!(
		report.top_level.len(),
		1,
		"the created directory is reported"
	);
	assert_eq!(
		recorder.updates.lock().unwrap().last().unwrap().phase,
		CopyPhase::Cancelled
	);
	let (dirs, files) = contents(&client, &destination).await;
	assert_eq!(dirs.len(), 1);
	assert!(
		files.len() < 3,
		"the copy stopped before copying every file"
	);
}

/// Deletes a directory created outside the per-test directory when the test ends, pass or fail.
/// Should that fail, its `rs-` name lets the drive's test-directory sweep remove it later.
struct DeleteOnDrop {
	client: Arc<Client>,
	dir: Option<RemoteDirectory>,
}

impl Drop for DeleteOnDrop {
	fn drop(&mut self) {
		let (client, dir) = (self.client.clone(), self.dir.take());
		let cleanup = async move {
			if let Some(dir) = dir
				&& let Err(e) = client.delete_dir_permanently(dir).await
			{
				eprintln!("failed to clean up a copy in the drive root: {e}");
			}
		};
		match tokio::runtime::Handle::try_current() {
			Ok(handle) => {
				handle.spawn(cleanup);
			}
			Err(_) => test_utils::rt().block_on(cleanup),
		}
	}
}

// ── Public-link sources ─────────────────────────────────────────────────────

#[shared_test_runtime]
async fn copies_from_public_links() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = resources.client.clone();
	let test_dir = &resources.dir;
	let unauthed = client.get_unauthed();

	let source = client.create_dir(&test_dir.into(), "source").await.unwrap();
	let sub = client.create_dir(&(&source).into(), "sub").await.unwrap();
	let in_sub = upload(&client, &sub, "in-sub.txt", &data(2 * CHUNK_SIZE, 5)).await;
	let file = upload(&client, test_dir, "linked.txt", b"a linked file").await;
	let destination = client
		.create_dir(&test_dir.into(), "destination")
		.await
		.unwrap();

	// a password-protected directory link
	let password = "copy-password";
	let mut link_rw = client
		.public_link_dir::<fn(u64, Option<u64>)>(&source, None)
		.await
		.unwrap();
	link_rw.set_password(password.to_owned());
	client.update_dir_link(&source, &link_rw).await.unwrap();
	let link: DirPublicLink = link_rw.try_into().unwrap();
	let info = unauthed
		.get_dir_public_link_info(*link.uuid(), &link.key_string())
		.await
		.unwrap();

	// without the password the listing is refused and nothing is created
	let CopyFailed { error, .. } = copy(
		&client,
		vec![CopySource::Dir(CopySourceDir::Linked(
			DirType::Root(Cow::Owned(info.root.clone())),
			info.link.clone(),
		))],
		&destination,
	)
	.await
	.unwrap_err();
	assert_eq!(error.kind(), ErrorKind::WrongPassword);
	let (dirs, files) = contents(&client, &destination).await;
	assert!(dirs.is_empty() && files.is_empty());

	// with it: a directory nested in the link, and a file link
	let mut link = info.link;
	link.set_password(password.to_owned());
	let (linked_dirs, _) = unauthed
		.list_linked_dir::<fn(u64, Option<u64>)>(&(&info.root).into(), &link, None)
		.await
		.unwrap();
	let sub_linked = linked_dirs
		.into_iter()
		.find(|d| d.uuid() == sub.uuid())
		.unwrap();
	let file_link = client.public_link_file(&file).await.unwrap();
	let file_key = file.key().unwrap().to_str();
	let linked_file = unauthed
		.get_linked_file(file_link.uuid(), file_key.as_ref(), None)
		.await
		.unwrap();
	let report = copy(
		&client,
		vec![
			CopySource::Dir(CopySourceDir::Linked(
				DirType::Dir(Cow::Owned(sub_linked)),
				link,
			)),
			CopySource::File(linked_file.into()),
		],
		&destination,
	)
	.await
	.unwrap();
	assert!(report.failures.is_empty());
	let (_, copied) = contents(&client, &destination).await;
	assert_same_files(
		&client,
		&copied,
		[("sub/in-sub.txt", &in_sub), ("linked.txt", &file)],
	)
	.await;
}

// ── Requests, destinations and names ────────────────────────────────────────

#[shared_test_runtime]
async fn copy_items_to_takes_a_destination_and_name_per_request() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = resources.client.clone();
	let test_dir = &resources.dir;

	let file = upload(&client, test_dir, "f.txt", b"one file, many copies").await;
	let dir = client.create_dir(&test_dir.into(), "d").await.unwrap();
	upload(&client, &dir, "inside.txt", b"inside").await;
	let first = client.create_dir(&test_dir.into(), "first").await.unwrap();
	let second = client.create_dir(&test_dir.into(), "second").await.unwrap();
	upload(&client, &second, "renamed.txt", b"already here").await;

	let request =
		|source: CopySource, destination: &RemoteDirectory, name: Option<&str>| CopyRequest {
			source,
			destination: destination.clone().into(),
			name: name.map(|name| ValidatedName::try_from(name).unwrap()),
		};
	let report = client
		.clone()
		.copy_items_to(
			vec![
				request(CopySource::File(file.clone().into()), &first, None),
				request(
					CopySource::File(file.clone().into()),
					&second,
					Some("renamed.txt"),
				),
				request(
					CopySource::Dir(CopySourceDir::Normal(dir.clone())),
					&second,
					Some("dir copy"),
				),
				// the same source into the same place again
				request(CopySource::File(file.clone().into()), &first, None),
			],
			CopyConfig::default(),
			Arc::new(Recorder::default()),
			JobControl::default(),
		)
		.await
		.unwrap();
	let name_of = |request: usize| {
		report
			.top_level
			.iter()
			.find(|t| t.request == request)
			.unwrap()
			.item
			.name()
			.unwrap()
			.to_owned()
	};
	assert_eq!(name_of(0), "f.txt");
	assert_eq!(
		name_of(1),
		"renamed (1).txt",
		"a given name is kept both too"
	);
	assert_eq!(name_of(2), "dir copy");
	assert_eq!(name_of(3), "f (1).txt");
	let (_, in_second) = contents(&client, &second).await;
	assert!(in_second.iter().any(|(p, _)| p == "dir copy/inside.txt"));
}

#[shared_test_runtime]
async fn copies_into_the_drive_root() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = resources.client.clone();
	let name = format!("rs-copy-{}", Uuid::new_v4());
	let source = client
		.create_dir(&(&resources.dir).into(), &name)
		.await
		.unwrap();
	let result = client
		.clone()
		.copy_items(
			vec![CopySource::Dir(CopySourceDir::Normal(source))],
			client.root().clone().into(),
			CopyConfig::default(),
			Arc::new(Recorder::default()),
			JobControl::default(),
		)
		.await;
	// whatever was created is cleaned up, even when the copy failed
	let report = match &result {
		Ok(report) => report,
		Err(failed) => &failed.report,
	};
	// the report stays in `result` to be unwrapped below, so the guard gets its own copy
	let _cleanup = DeleteOnDrop {
		client: client.clone(),
		dir: report.top_level.first().and_then(|t| match &t.item {
			NonRootItemType::Dir(dir) => Some(RemoteDirectory::clone(dir)),
			NonRootItemType::File(_) => None,
		}),
	};
	let report = result.unwrap();
	let copied = client
		.get_dir(report.top_level[0].item.uuid())
		.await
		.unwrap();
	assert_eq!(copied.name(), Some(name.as_str()));
	assert_eq!(
		Uuid::try_from(*copied.parent()).unwrap(),
		client.root().uuid()
	);
}

#[shared_test_runtime]
async fn names_clash_the_way_the_server_compares_them() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = resources.client.clone();
	let test_dir = &resources.dir;

	let source = client.create_dir(&test_dir.into(), "source").await.unwrap();
	let lower = upload(&client, &source, "a.txt", b"lower").await;
	let umlaut = upload(&client, &source, "äbc.txt", b"umlaut").await;
	let long_name = format!("{}.txt", "é".repeat(125)); // 254 bytes
	let long = upload(&client, &source, &long_name, b"long").await;
	let destination = client
		.create_dir(&test_dir.into(), "destination")
		.await
		.unwrap();
	upload(&client, &destination, "A.TXT", b"upper").await;
	upload(&client, &destination, "ÄBC.txt", b"upper umlaut").await;

	let report = copy(
		&client,
		vec![
			CopySource::File(lower.into()),
			CopySource::File(umlaut.into()),
		],
		&destination,
	)
	.await
	.unwrap();
	let names: Vec<_> = report
		.top_level
		.iter()
		.map(|t| t.item.name().unwrap().to_owned())
		.collect();
	assert!(names.contains(&"a (1).txt".to_owned()), "{names:?}");
	assert!(names.contains(&"äbc (1).txt".to_owned()), "{names:?}");

	// a name at the byte limit, copied next to itself, is shortened to fit its counter
	let report = copy(&client, vec![CopySource::File(long.into())], &source)
		.await
		.unwrap();
	let copied = report.top_level[0].item.name().unwrap().to_owned();
	assert_ne!(copied, long_name);
	assert!(copied.len() <= 255, "{} bytes", copied.len());
	assert!(copied.ends_with(" (1).txt"), "{copied}");
}

// ── Sizes and shapes ────────────────────────────────────────────────────────

#[shared_test_runtime]
async fn copies_chunk_boundaries_many_files_and_deep_and_wide_trees() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = resources.client.clone();
	let test_dir = &resources.dir;

	let source = client.create_dir(&test_dir.into(), "source").await.unwrap();
	let sizes = [
		("zero", 0),
		("one", 1),
		("chunk-minus-one", CHUNK_SIZE - 1),
		("one-chunk", CHUNK_SIZE),
		("chunk-plus-one", CHUNK_SIZE + 1),
		("three-chunks", 3 * CHUNK_SIZE),
		("two-chunks-plus-one", 2 * CHUNK_SIZE + 1),
	];
	let mut boundary_files = Vec::new();
	for (i, (name, size)) in sizes.iter().enumerate() {
		boundary_files.push((
			*name,
			upload(&client, &source, name, &data(*size, i as u8)).await,
		));
	}
	let many = client.create_dir(&(&source).into(), "many").await.unwrap();
	stream::iter(0..40)
		.map(|i| {
			let client = client.clone();
			let many = many.clone();
			async move { upload(&client, &many, &format!("small-{i}"), &data(i + 1, 9)).await }
		})
		.buffer_unordered(8)
		.collect::<Vec<_>>()
		.await;
	let mut deepest = client.create_dir(&(&source).into(), "deep").await.unwrap();
	for level in 0..12 {
		deepest = client
			.create_dir(&(&deepest).into(), &format!("level-{level}"))
			.await
			.unwrap();
	}
	upload(&client, &deepest, "bottom.txt", b"bottom").await;
	let wide = client.create_dir(&(&source).into(), "wide").await.unwrap();
	stream::iter(0..30)
		.map(|i| {
			let client = client.clone();
			let wide = wide.clone();
			async move {
				let dir = client
					.create_dir(&(&wide).into(), &format!("w{i}"))
					.await
					.unwrap();
				upload(&client, &dir, "leaf", &[i as u8]).await;
			}
		})
		.buffer_unordered(8)
		.collect::<Vec<_>>()
		.await;
	let destination = client
		.create_dir(&test_dir.into(), "destination")
		.await
		.unwrap();

	let report = copy(
		&client,
		vec![CopySource::Dir(CopySourceDir::Normal(source.clone()))],
		&destination,
	)
	.await
	.unwrap();
	assert!(report.failures.is_empty());
	let (source_dirs, source_files) = contents(&client, &source).await;
	let (copied_dirs, copied_files) = contents(&client, &destination).await;
	let mut expected_dirs: Vec<String> = std::iter::once("source".to_owned())
		.chain(source_dirs.iter().map(|(p, _)| format!("source/{p}")))
		.collect();
	let mut got_dirs: Vec<String> = copied_dirs.iter().map(|(p, _)| p.clone()).collect();
	expected_dirs.sort();
	got_dirs.sort();
	assert_eq!(got_dirs, expected_dirs, "the tree is copied exactly");
	let mut expected_files: Vec<(String, u64)> = source_files
		.iter()
		.map(|(p, f)| (format!("source/{p}"), f.size()))
		.collect();
	let mut got_files: Vec<(String, u64)> = copied_files
		.iter()
		.map(|(p, f)| (p.clone(), f.size()))
		.collect();
	expected_files.sort();
	got_files.sort();
	assert_eq!(got_files, expected_files, "every file, once, at its size");
	assert_same_files(
		&client,
		&copied_files,
		boundary_files
			.iter()
			.map(|(name, file)| (format!("source/{name}"), file)),
	)
	.await;
}

// ── Pause, resume and cancel ────────────────────────────────────────────────

#[shared_test_runtime]
async fn a_copy_cancelled_before_it_starts_creates_nothing() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = resources.client.clone();
	let test_dir = &resources.dir;
	let source = client.create_dir(&test_dir.into(), "source").await.unwrap();
	upload(&client, &source, "a.txt", b"never copied").await;
	let destination = client
		.create_dir(&test_dir.into(), "destination")
		.await
		.unwrap();
	let (control, controller) = JobControl::new();
	controller.cancel();
	let recorder = Arc::new(Recorder::default());
	let CopyFailed { error, .. } = client
		.clone()
		.copy_items(
			vec![CopySource::Dir(CopySourceDir::Normal(source))],
			destination.clone().into(),
			CopyConfig::default(),
			recorder.clone(),
			control,
		)
		.await
		.unwrap_err();
	assert_eq!(error.kind(), ErrorKind::Cancelled);
	assert_eq!(
		recorder.updates.lock().unwrap().last().unwrap().phase,
		CopyPhase::Cancelled
	);
	let (dirs, files) = contents(&client, &destination).await;
	assert!(dirs.is_empty() && files.is_empty());
}

#[shared_test_runtime]
async fn pausing_and_resuming_many_times_copies_everything_once() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = resources.client.clone();
	let test_dir = &resources.dir;
	let source = client.create_dir(&test_dir.into(), "source").await.unwrap();
	let mut originals = Vec::new();
	for i in 0..4 {
		originals.push(
			upload(
				&client,
				&source,
				&format!("f{i}"),
				&data(2 * CHUNK_SIZE + i, i as u8),
			)
			.await,
		);
	}
	let destination = client
		.create_dir(&test_dir.into(), "destination")
		.await
		.unwrap();
	let (control, controller) = JobControl::new();
	let running = tokio::spawn({
		let client = client.clone();
		let source = source.clone();
		let destination = destination.clone();
		async move {
			client
				.copy_items(
					vec![CopySource::Dir(CopySourceDir::Normal(source))],
					destination.into(),
					CopyConfig::default(),
					Arc::new(Recorder::default()),
					control,
				)
				.await
		}
	});
	for _ in 0..8 {
		tokio::time::sleep(std::time::Duration::from_millis(300)).await;
		controller.pause();
		tokio::time::sleep(std::time::Duration::from_millis(300)).await;
		controller.resume();
	}
	running.await.unwrap().unwrap();
	let (_, copied) = contents(&client, &destination).await;
	assert_eq!(copied.len(), 4, "no file is copied twice");
	assert_same_files(
		&client,
		&copied,
		originals
			.iter()
			.enumerate()
			.map(|(i, original)| (format!("source/f{i}"), original)),
	)
	.await;
}

// ── Failures, retry and the pre-flight check ────────────────────────────────

/// A source file whose chunks do not exist fails at download without stopping the copy; the
/// failure names the directory it was to be created in, and a retry through `copy_items_to`
/// lands there under the failure's name.
#[shared_test_runtime]
async fn a_failed_file_is_reported_with_its_parent_and_can_be_retried_there() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = resources.client.clone();
	let test_dir = &resources.dir;
	let kept = upload(&client, test_dir, "kept.txt", b"kept").await;
	let real = upload(&client, test_dir, "gone.bin", &data(2 * CHUNK_SIZE, 7)).await;
	// the same file under a uuid the server holds no chunks for
	let missing: AnonymousRemoteFile = RemoteFile::from_meta(
		Uuid::new_v4(),
		(),
		*real.parent(),
		real.size(),
		real.chunks(),
		real.region(),
		real.bucket(),
		real.timestamp(),
		false,
		real.get_meta().to_owned_cow(),
	);
	let destination = client
		.create_dir(&test_dir.into(), "destination")
		.await
		.unwrap();

	let recorder = Arc::new(Recorder::default());
	let report = client
		.clone()
		.copy_items(
			vec![
				CopySource::File(kept.clone().into()),
				CopySource::File(RemoteFileType::File(Cow::Owned(missing))),
			],
			destination.clone().into(),
			CopyConfig::default(),
			recorder.clone(),
			JobControl::default(),
		)
		.await
		.unwrap();
	let [failure] = report.failures.as_slice() else {
		panic!("one failure: {:?}", report.failures);
	};
	assert_eq!(failure.info.stage, CopyStage::Download);
	assert_eq!(failure.info.error.kind(), ErrorKind::FileChunkNotFound);
	assert_eq!(failure.info.dest_name, "gone.bin");
	assert_eq!(failure.info.dest_parent_dir.uuid(), destination.uuid());
	assert_eq!(report.counts.files_done, 1, "the other file is copied");
	assert!(recorder.updates.lock().unwrap().iter().any(|u| {
		u.events
			.iter()
			.any(|e| matches!(e, CopyEvent::FileFailed(info) if info.dest_name == "gone.bin"))
	}));

	// the failed source can never be read, so the retry copies the real file, into the
	// failure's directory under the failure's name
	client
		.clone()
		.copy_items_to(
			vec![CopyRequest {
				source: CopySource::File(real.clone().into()),
				destination: failure.info.dest_parent_dir.clone(),
				name: Some(ValidatedName::try_from(failure.info.dest_name.as_str()).unwrap()),
			}],
			CopyConfig::default(),
			Arc::new(Recorder::default()),
			JobControl::default(),
		)
		.await
		.unwrap();
	let (_, copied) = contents(&client, &destination).await;
	assert_same_files(&client, &copied, [("gone.bin", &real), ("kept.txt", &kept)]).await;
}

#[shared_test_runtime]
async fn max_bytes_is_checked_before_anything_is_written() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = resources.client.clone();
	let test_dir = &resources.dir;
	let source = client.create_dir(&test_dir.into(), "source").await.unwrap();
	upload(&client, &source, "a.bin", &data(1000, 1)).await;
	upload(&client, &source, "b.bin", &data(24, 2)).await;
	let destination = client
		.create_dir(&test_dir.into(), "destination")
		.await
		.unwrap();
	let copy_with = |max_bytes: u64, recorder: Arc<Recorder>| {
		client.clone().copy_items(
			vec![CopySource::Dir(CopySourceDir::Normal(source.clone()))],
			destination.clone().into(),
			CopyConfig {
				max_bytes: Some(max_bytes),
			},
			recorder,
			JobControl::default(),
		)
	};

	let recorder = Arc::new(Recorder::default());
	let CopyFailed { report, error } = copy_with(1023, recorder.clone()).await.unwrap_err();
	assert_eq!(error.kind(), ErrorKind::MaxStorageReached);
	let needs = PlanTotals {
		dirs: 1,
		files: 2,
		bytes: 1024,
	};
	assert_eq!(report.totals, needs, "the refusal says what the copy needs");
	assert_eq!(
		(
			report.counts.dirs_not_attempted,
			report.counts.files_not_attempted,
			report.counts.bytes_not_attempted
		),
		(needs.dirs, needs.files, needs.bytes)
	);
	let last = recorder.updates.lock().unwrap().last().unwrap().clone();
	assert_eq!(
		(last.phase, last.totals, last.counts),
		(CopyPhase::Failed, needs, report.counts)
	);
	let (dirs, files) = contents(&client, &destination).await;
	assert!(dirs.is_empty() && files.is_empty(), "nothing was written");

	copy_with(1024, Arc::new(Recorder::default()))
		.await
		.unwrap();
	let (_, files) = contents(&client, &destination).await;
	assert_eq!(files.len(), 2);
}

#[shared_test_runtime]
async fn missing_sources_and_destinations_fail_before_anything_is_created() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = resources.client.clone();
	let test_dir = &resources.dir;
	let destination = client
		.create_dir(&test_dir.into(), "destination")
		.await
		.unwrap();

	let source = client.create_dir(&test_dir.into(), "source").await.unwrap();
	client.delete_dir_permanently(source.clone()).await.unwrap();
	let recorder = Arc::new(Recorder::default());
	client
		.clone()
		.copy_items(
			vec![CopySource::Dir(CopySourceDir::Normal(source))],
			destination.clone().into(),
			CopyConfig::default(),
			recorder.clone(),
			JobControl::default(),
		)
		.await
		.expect_err("a deleted source fails the scan");
	assert_eq!(
		recorder.updates.lock().unwrap().last().unwrap().phase,
		CopyPhase::Failed
	);
	let (dirs, files) = contents(&client, &destination).await;
	assert!(dirs.is_empty() && files.is_empty());

	let file = upload(&client, test_dir, "a.txt", b"nowhere to go").await;
	let gone = client.create_dir(&test_dir.into(), "gone").await.unwrap();
	client.delete_dir_permanently(gone.clone()).await.unwrap();
	let failed = copy(&client, vec![CopySource::File(file.into())], &gone)
		.await
		.expect_err("a deleted destination fails the copy");
	assert!(failed.report.top_level.is_empty());
}

// ── Undecryptable entries (needs the `malformed` feature) ───────────────────

#[cfg(feature = "malformed")]
#[shared_test_runtime]
async fn undecryptable_entries_are_renamed_or_skipped() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = resources.client.clone();
	let test_dir = &resources.dir;

	let source = client.create_dir(&test_dir.into(), "source").await.unwrap();
	let ok = upload(&client, &source, "ok.txt", b"readable").await;
	let unreadable = client
		.create_malformed_file(
			&(&source).into(),
			"unreadable.txt",
			"not metadata",
			"not a mime",
			"not a size",
		)
		.await
		.unwrap();
	let hidden = client
		.create_malformed_dir(&(&source).into(), "hidden", "not metadata")
		.await
		.unwrap();
	let hidden_dir = client.get_dir(hidden).await.unwrap();
	let inside = upload(&client, &hidden_dir, "inside.txt", b"kept").await;

	let destination = client
		.create_dir(&test_dir.into(), "destination")
		.await
		.unwrap();
	// a destination entry whose name the listing cannot show still takes its name
	client
		.create_malformed_file(
			&(&destination).into(),
			"clash.txt",
			"not metadata",
			"not a mime",
			"not a size",
		)
		.await
		.unwrap();
	let clash = upload(&client, test_dir, "clash.txt", b"clash").await;
	let clash_uuid = clash.uuid();

	let report = copy(
		&client,
		vec![
			CopySource::Dir(CopySourceDir::Normal(source)),
			CopySource::File(clash.into()),
		],
		&destination,
	)
	.await
	.unwrap();
	assert_eq!(report.skipped.len(), 1);
	assert_eq!(
		report.skipped[0].reason,
		SkipReason::UndecryptableFile { uuid: unreadable }
	);
	assert_eq!(report.renamed.len(), 2, "renames: {:?}", report.renamed);
	let hidden_rename = report
		.renamed
		.iter()
		.find(|r| r.reason == RenameReason::Undecryptable)
		.expect("the undecryptable directory is renamed");
	assert_eq!(hidden_rename.source_uuid, hidden);
	assert_eq!(hidden_rename.name.as_ref(), hidden.to_string());
	// the destination's unreadable clash.txt still takes its name, so the copy is renamed too
	let clash_rename = report
		.renamed
		.iter()
		.find(|r| r.reason == RenameReason::DuplicateName)
		.expect("the clashing file is renamed");
	assert_eq!(clash_rename.source_uuid, clash_uuid);
	assert_eq!(clash_rename.name.as_ref(), "clash (1).txt");
	let (_, copied) = contents(&client, &destination).await;
	assert_same_files(
		&client,
		&copied,
		[
			("source/ok.txt".to_owned(), &ok),
			(format!("source/{hidden}/inside.txt"), &inside),
		],
	)
	.await;
	assert!(
		report
			.top_level
			.iter()
			.any(|t| t.item.name() == Some("clash (1).txt")),
		"the hidden name is found and kept"
	);
}
