// Shared between the integration-test binaries that copy (copy_tests, connect_tests); each
// binary compiles its own copy and uses a subset.
#![allow(dead_code)]

use std::{
	sync::{Arc, Mutex},
	time::Duration,
};

use filen_sdk_rs::{
	ErrorKind,
	auth::Client,
	fs::{
		HasUUID,
		categories::{Normal, fs::CategoryFSExt},
		copy::{
			CopiedTopLevel, CopyCallback, CopyConfig, CopyFailed, CopyReport, CopySource,
			CopyUpdate, JobControl, PlannedTopLevelItem, RunState,
		},
		dir::RemoteDirectory,
		file::{RemoteFile, traits::HasFileInfo},
	},
	io::client_impl::IoSharedClientExt,
};

#[derive(Default)]
pub struct Recorder {
	pub planned: Mutex<Vec<PlannedTopLevelItem>>,
	pub created: Mutex<Vec<CopiedTopLevel>>,
	pub updates: Mutex<Vec<CopyUpdate>>,
}

impl CopyCallback for Recorder {
	fn on_top_level_planned(&self, items: Vec<PlannedTopLevelItem>) {
		self.planned.lock().unwrap().extend(items);
	}

	fn on_top_level_created(&self, item: CopiedTopLevel) {
		self.created.lock().unwrap().push(item);
	}

	fn on_update(&self, update: CopyUpdate) {
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

/// `len` bytes that repeat every 251 bytes, so no two chunks of a file are alike.
pub fn data(len: usize, seed: u8) -> Vec<u8> {
	(0..len).map(|i| (i % 251) as u8 ^ seed).collect()
}

/// `dir`'s recursive contents, keyed by the path below `dir`. Entries whose metadata cannot be
/// decrypted have no name, so no path: they are left out, along with everything below them.
pub async fn contents(
	client: &Arc<Client>,
	dir: &RemoteDirectory,
) -> (Vec<(String, RemoteDirectory)>, Vec<(String, RemoteFile)>) {
	let (dirs, files) = Normal::list_dir_recursive_with_paths(
		Arc::clone(client),
		dir.into(),
		None::<&fn(u64, Option<u64>)>,
		&mut |errors| {
			for error in errors {
				assert!(
					error.kind() == ErrorKind::Walk
						&& error
							.inner_message()
							.is_some_and(|m| m.starts_with("encrypted metadata could not be read")),
					"unexpected listing error: {error}"
				);
			}
		},
		(),
	)
	.await
	.unwrap();
	(
		dirs.into_iter().map(|(d, path)| (path, d)).collect(),
		files.into_iter().map(|(f, path)| (path, f)).collect(),
	)
}

/// Copies `sources` into `destination` with the default config and no pause or cancel.
pub async fn copy(
	client: &Arc<Client>,
	sources: Vec<CopySource>,
	destination: &RemoteDirectory,
) -> Result<CopyReport, CopyFailed> {
	client
		.clone()
		.copy_items(
			sources,
			destination.clone().into(),
			CopyConfig::default(),
			Arc::new(Recorder::default()),
			JobControl::default(),
		)
		.await
}

/// Asserts that every file at `path` in `copied` has the bytes of the matching original.
pub async fn assert_same_files<'a>(
	client: &Client,
	copied: &[(String, RemoteFile)],
	originals: impl IntoIterator<Item = (impl AsRef<str>, &'a RemoteFile)>,
) {
	for (path, original) in originals {
		let path = path.as_ref();
		let (_, copy) = copied
			.iter()
			.find(|(p, _)| p == path)
			.unwrap_or_else(|| panic!("{path} was copied"));
		assert_ne!(copy.uuid(), original.uuid());
		assert_eq!(copy.size(), original.size(), "{path} has the same size");
		assert_eq!(
			client.download_file(copy).await.unwrap(),
			client.download_file(original).await.unwrap(),
			"{path} has the same contents"
		);
	}
}

/// Runs `signal` as soon as the job's first top-level item exists, so the test can pause or
/// cancel the job in a known state.
pub struct SignalOnCreate<F> {
	pub recorder: Arc<Recorder>,
	pub signal: F,
}

impl<F: Fn() + Send + Sync + 'static> CopyCallback for SignalOnCreate<F> {
	fn on_top_level_planned(&self, items: Vec<PlannedTopLevelItem>) {
		self.recorder.on_top_level_planned(items);
	}
	fn on_top_level_created(&self, item: CopiedTopLevel) {
		self.recorder.on_top_level_created(item);
		(self.signal)();
	}
	fn on_update(&self, update: CopyUpdate) {
		self.recorder.on_update(update);
	}
}

/// Waits (real time, at most a minute) until the job reports itself paused.
pub async fn wait_until_paused(recorder: &Recorder) {
	let paused = || {
		recorder
			.updates
			.lock()
			.unwrap()
			.iter()
			.any(|u| u.run_state == RunState::Paused)
	};
	tokio::time::timeout(Duration::from_secs(60), async {
		while !paused() {
			tokio::time::sleep(Duration::from_millis(100)).await;
		}
	})
	.await
	.expect("the copy never reported itself paused");
}
