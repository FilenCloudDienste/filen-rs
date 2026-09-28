//! What the binding tests share: a record of what a job delivered, and items of the user's drive
//! to hand a binding.

use std::{
	borrow::Cow,
	sync::{Arc, Mutex},
};

use chrono::{DateTime, Utc};
use filen_types::{
	api::v3::dir::color::DirColor,
	fs::{ParentUuid, StableUuid, Uuid},
};
use tokio::sync::mpsc::UnboundedSender;

use crate::{
	Error,
	crypto::{file::FileKey, shared::CreateRandom, v3::EncryptionKey},
	fs::{
		dir::{
			RemoteDirectory,
			meta::{DecryptedDirectoryMeta, DirectoryMeta},
		},
		file::{
			RemoteFile,
			meta::{DecryptedFileMeta, FileMeta},
		},
	},
	js::ManagedFuture,
};

/// Records what a job delivered, each callback by a number it carries, in the order it came.
#[derive(Default)]
pub(crate) struct Recorder(Mutex<Vec<u64>>);

impl Recorder {
	pub(crate) fn push(&self, value: u64) {
		self.0.lock().unwrap().push(value);
	}
}

/// Runs a job that sends what `send` sends (returning the numbers the callbacks record, in the
/// order sent) through [`ManagedFuture::into_ordered_job`], each delivered through `deliver`,
/// and checks it all reached the callbacks, in order, by the time the call resolved.
pub(crate) fn delivered_in_order<T: Send + 'static>(
	deliver: fn(&Recorder, T),
	send: impl FnOnce(UnboundedSender<T>) -> Vec<u64> + Send + 'static,
) {
	let recorder = Arc::new(Recorder::default());
	let managed = ManagedFuture {
		abort_signal: None,
		pause_signal: None,
	};
	let job = managed.into_ordered_job(
		{
			let recorder = Arc::clone(&recorder);
			move |delivery| deliver(&recorder, delivery)
		},
		move |sender, _control| async move { Ok::<_, Error>(send(sender)) },
	);
	let expected = futures::executor::block_on(job).expect("the job's own result");
	assert_eq!(*recorder.0.lock().unwrap(), expected);
}

/// The directory the items below are in.
pub(crate) const PARENT: Uuid = Uuid::from_u128(0x9a);

/// A directory `name` of the user's drive.
pub(crate) fn drive_dir(uuid: Uuid, name: &str) -> RemoteDirectory {
	RemoteDirectory::from_meta(
		uuid,
		ParentUuid::Uuid(PARENT),
		DirColor::Blue,
		false,
		DateTime::<Utc>::UNIX_EPOCH,
		DirectoryMeta::Decoded(DecryptedDirectoryMeta {
			name: Cow::Owned(name.to_owned()),
			created: None,
		}),
	)
}

/// A file `name` of the user's drive, `size` bytes long.
pub(crate) fn drive_file(uuid: Uuid, name: &str, size: u64) -> RemoteFile {
	RemoteFile::from_meta(
		uuid,
		StableUuid::new_for_test(uuid),
		PARENT.into(),
		size,
		1,
		"de-1",
		"bucket",
		DateTime::<Utc>::UNIX_EPOCH,
		false,
		FileMeta::Decoded(DecryptedFileMeta {
			name: Cow::Owned(name.to_owned()),
			size,
			mime: Cow::Borrowed("application/octet-stream"),
			key: FileKey::V3(EncryptionKey::generate()),
			last_modified: DateTime::<Utc>::UNIX_EPOCH,
			created: None,
			hash: None,
		}),
	)
}
