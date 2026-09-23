// Shared between the integration-test binaries that copy (copy_tests, connect_tests); each
// binary compiles its own copy and uses a subset.
#![allow(dead_code)]

use std::{
	borrow::Cow,
	sync::{Arc, Mutex},
};

use filen_sdk_rs::{
	auth::Client,
	fs::{
		HasName, HasUUID,
		categories::{DirType, Normal, fs::CategoryFS},
		copy::{
			CopiedTopLevel, CopyCallback, CopyOptions, CopyOutcome, CopySource, CopySourceDir,
			CopyUpdate, JobControl, PlannedTopLevelItem,
		},
		dir::RemoteDirectory,
		file::{RemoteFile, traits::HasFileInfo},
	},
	io::client_impl::IoSharedClientExt,
};
use tokio::sync::watch;

#[derive(Default)]
pub struct Recorder {
	pub planned: Mutex<Vec<PlannedTopLevelItem>>,
	pub created: Mutex<Vec<CopiedTopLevel>>,
	pub updates: Mutex<Vec<CopyUpdate>>,
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

pub async fn upload(
	client: &Client,
	parent: &RemoteDirectory,
	name: &str,
	data: &[u8],
) -> RemoteFile {
	let builder = client.make_file_builder(name, parent.uuid()).unwrap();
	client.upload_file(builder, data).await.unwrap()
}

pub fn data(len: usize, seed: u8) -> Vec<u8> {
	(0..len)
		.map(|i| (i as u8).wrapping_mul(31) ^ seed)
		.collect()
}

/// `dir`'s recursive contents, keyed by the path below `dir`.
pub async fn contents(
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

/// Copies `sources` into `destination` with default options and no pause or cancel.
pub async fn copy(
	client: &Arc<Client>,
	sources: Vec<CopySource>,
	destination: &RemoteDirectory,
) -> CopyOutcome<CopySourceDir> {
	client
		.clone()
		.copy_items(
			sources,
			destination.clone().into(),
			CopyOptions::default(),
			Arc::new(Recorder::default()),
			JobControl::default(),
		)
		.await
}

/// Asserts that every file at `path` in `copied` has the bytes of the matching original.
pub async fn assert_same_files(
	client: &Client,
	copied: &[(String, RemoteFile)],
	originals: &[(&str, &RemoteFile)],
) {
	for (path, original) in originals {
		let (_, copy) = copied
			.iter()
			.find(|(p, _)| p == path)
			.unwrap_or_else(|| panic!("{path} was copied"));
		assert_ne!(copy.uuid(), original.uuid());
		assert_eq!(copy.size(), original.size(), "{path} has the same size");
		assert_eq!(
			client.download_file(copy).await.unwrap(),
			client.download_file(*original).await.unwrap(),
			"{path} has the same contents"
		);
	}
}

/// Pauses the job as soon as its first top-level item exists, so the test can change the
/// world in a known state before resuming it.
pub struct PauseOnCreate {
	pub recorder: Arc<Recorder>,
	pub pause: watch::Sender<bool>,
}

impl CopyCallback for PauseOnCreate {
	fn top_level_planned(&self, items: Vec<PlannedTopLevelItem>) {
		self.recorder.top_level_planned(items);
	}
	fn top_level_created(&self, item: CopiedTopLevel) {
		self.recorder.top_level_created(item);
		self.pause.send_replace(true);
	}
	fn update(&self, update: CopyUpdate) {
		self.recorder.update(update);
	}
}

/// Waits (real time, bounded) until the job reports itself paused.
pub async fn wait_until_paused(recorder: &Recorder) {
	for _ in 0..600 {
		if recorder.updates.lock().unwrap().iter().any(|u| u.paused) {
			return;
		}
		tokio::time::sleep(std::time::Duration::from_millis(100)).await;
	}
	panic!("the copy never reported itself paused");
}
