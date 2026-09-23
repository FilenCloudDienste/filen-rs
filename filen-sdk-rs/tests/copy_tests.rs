use std::{
	borrow::Cow,
	sync::{Arc, Mutex},
};

use filen_macros::shared_test_runtime;
use filen_sdk_rs::{
	ErrorKind,
	auth::{Client, http::ClientConfig, unauth::UnauthClient},
	connect::{DirPublicLink, PublicLinkSharedClientExt},
	consts::CHUNK_SIZE,
	fs::{
		HasName, HasUUID,
		categories::{DirType, Normal, fs::CategoryFS},
		copy::{
			CopiedTopLevel, CopyCallback, CopyOptions, CopyPhase, CopySource, CopySourceDir,
			CopyUpdate, JobControl, PlannedTopLevelItem,
		},
		dir::RemoteDirectory,
		file::{RemoteFile, traits::HasFileInfo},
	},
	io::client_impl::IoSharedClientExt,
};
use filen_types::api::v3::dir::color::DirColor;

#[derive(Default)]
struct Recorder {
	planned: Mutex<Vec<PlannedTopLevelItem>>,
	created: Mutex<Vec<CopiedTopLevel>>,
	updates: Mutex<Vec<CopyUpdate>>,
}

impl CopyCallback for Recorder {
	fn top_level_planned(&self, items: Vec<PlannedTopLevelItem>) {
		self.planned.lock().unwrap().extend(items);
	}

	fn top_level_created(&self, item: CopiedTopLevel) {
		self.created.lock().unwrap().push(item);
	}

	fn update(&self, update: CopyUpdate) {
		self.updates.lock().unwrap().push(update);
	}
}

async fn upload(client: &Client, parent: &RemoteDirectory, name: &str, data: &[u8]) -> RemoteFile {
	let builder = client.make_file_builder(name, parent.uuid()).unwrap();
	client.upload_file(builder, data).await.unwrap()
}

fn data(len: usize, seed: u8) -> Vec<u8> {
	(0..len)
		.map(|i| (i as u8).wrapping_mul(31) ^ seed)
		.collect()
}

/// `dir`'s recursive contents, keyed by the path below `dir`.
async fn contents(
	client: &Client,
	dir: &RemoteDirectory,
) -> (Vec<(String, RemoteDirectory)>, Vec<(String, RemoteFile)>) {
	let (dirs, files) = Normal::list_dir_recursive(
		client,
		&DirType::Dir(Cow::Borrowed(dir)),
		None::<&fn(u64, Option<u64>)>,
		(),
	)
	.await
	.unwrap();
	let path_of = |uuid: filen_types::fs::Uuid| {
		let mut parts = Vec::new();
		let mut current = uuid;
		while current != dir.uuid() {
			let parent = dirs.iter().find(|d| d.uuid() == current).unwrap();
			parts.push(parent.name().unwrap().to_owned());
			current = (*parent.parent()).try_into().unwrap();
		}
		parts.reverse();
		parts.join("/")
	};
	use filen_sdk_rs::fs::HasParent;
	let dir_paths = dirs
		.iter()
		.map(|d| (path_of(d.uuid()), d.clone()))
		.collect();
	let file_paths = files
		.iter()
		.map(|f| {
			let parent: filen_types::fs::Uuid = (*f.parent()).try_into().unwrap();
			let prefix = path_of(parent);
			let path = if prefix.is_empty() {
				f.name().unwrap().to_owned()
			} else {
				format!("{prefix}/{}", f.name().unwrap())
			};
			(path, f.clone())
		})
		.collect();
	(dir_paths, file_paths)
}

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
	let outcome = client
		.clone()
		.copy_items(
			vec![CopySource::Dir(CopySourceDir::Normal(source.clone()))],
			destination.clone().into(),
			CopyOptions::default(),
			recorder.clone(),
			JobControl::default(),
		)
		.await;
	outcome.result.unwrap();
	assert!(outcome.report.failures.is_empty());
	assert_eq!(outcome.report.counts.files_done, 3);
	assert_eq!(outcome.report.counts.dirs_created, 2);
	assert_eq!(outcome.report.top_level.len(), 1);
	assert_eq!(recorder.created.lock().unwrap().len(), 1);
	assert_eq!(
		recorder.planned.lock().unwrap()[0].dest_uuid,
		outcome.report.top_level[0].item.uuid()
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
		let outcome = client
			.clone()
			.copy_items(
				vec![CopySource::File(file.clone().into())],
				test_dir.clone().into(),
				CopyOptions::default(),
				Arc::new(Recorder::default()),
				JobControl::default(),
			)
			.await;
		outcome.result.unwrap();
		assert_eq!(
			outcome.report.top_level[0].item.name(),
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
		let outcome = client
			.clone()
			.copy_items(
				vec![CopySource::Dir(CopySourceDir::Normal(source.clone()))],
				destination.clone().into(),
				CopyOptions::default(),
				Arc::new(Recorder::default()),
				JobControl::default(),
			)
			.await;
		assert_eq!(outcome.result.unwrap_err().kind(), ErrorKind::InvalidState);
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
	let config = ClientConfig::default().with_memory_budget(2 * (CHUNK_SIZE + 28));
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

	let outcome = client
		.clone()
		.copy_items(
			originals
				.iter()
				.cloned()
				.map(|f| CopySource::File(f.into()))
				.collect(),
			destination.clone().into(),
			CopyOptions::default(),
			Arc::new(Recorder::default()),
			JobControl::default(),
		)
		.await;
	outcome.result.unwrap();

	let (_, files) = contents(&client, &destination).await;
	assert_eq!(files.len(), 4);
	for original in &originals {
		let (_, copy) = files
			.iter()
			.find(|(_, f)| f.name() == original.name())
			.unwrap();
		assert_eq!(
			client.download_file(copy).await.unwrap(),
			client.download_file(original).await.unwrap()
		);
	}
}

#[shared_test_runtime]
async fn copy_from_a_public_link_into_a_linked_directory() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = resources.client.clone();
	let test_dir = &resources.dir;
	let unauthed = client.get_unauthed();

	async fn link_info(
		client: &Client,
		dir: &RemoteDirectory,
	) -> filen_sdk_rs::connect::DirPublicInfo {
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
	let outcome = client
		.clone()
		.copy_items(
			vec![linked_source()],
			source.clone().into(),
			CopyOptions::default(),
			Arc::new(Recorder::default()),
			JobControl::default(),
		)
		.await;
	assert_eq!(outcome.result.unwrap_err().kind(), ErrorKind::InvalidState);

	// the destination: a directory inside another public link, so the copy must be added to
	// that link too
	let linked = client.create_dir(&test_dir.into(), "linked").await.unwrap();
	let destination = client
		.create_dir(&(&linked).into(), "copies")
		.await
		.unwrap();
	let destination_link = link_info(&client, &linked).await;

	let outcome = client
		.clone()
		.copy_items(
			vec![linked_source()],
			destination.clone().into(),
			CopyOptions::default(),
			Arc::new(Recorder::default()),
			JobControl::default(),
		)
		.await;
	outcome.result.unwrap();
	let copied_root = outcome.report.top_level[0].item.uuid();

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

	let (cancel, cancel_rx) = tokio::sync::watch::channel(false);
	struct CancelOnCreate(Arc<Recorder>, tokio::sync::watch::Sender<bool>);
	impl CopyCallback for CancelOnCreate {
		fn top_level_planned(&self, items: Vec<PlannedTopLevelItem>) {
			self.0.top_level_planned(items);
		}
		fn top_level_created(&self, item: CopiedTopLevel) {
			self.0.top_level_created(item);
			self.1.send_replace(true);
		}
		fn update(&self, update: CopyUpdate) {
			self.0.update(update);
		}
	}
	let recorder = Arc::new(Recorder::default());
	let outcome = client
		.clone()
		.copy_items(
			vec![CopySource::Dir(CopySourceDir::Normal(source))],
			destination.clone().into(),
			CopyOptions::default(),
			CancelOnCreate(recorder.clone(), cancel),
			JobControl::new(None, Some(cancel_rx)),
		)
		.await;
	assert_eq!(outcome.result.unwrap_err().kind(), ErrorKind::Cancelled);
	assert_eq!(
		outcome.report.top_level.len(),
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
