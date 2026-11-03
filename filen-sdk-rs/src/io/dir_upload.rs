use std::{
	collections::{HashMap, VecDeque},
	ops::Deref,
	path::{Path, PathBuf},
	sync::{
		Arc,
		atomic::{AtomicU64, Ordering},
	},
};

use chrono::{DateTime, Utc};
use filen_types::fs::{ParentUuid, Uuid};

use crate::{
	Error, ErrorKind,
	auth::Client,
	consts::{CALLBACK_INTERVAL, MAX_SMALL_PARALLEL_REQUESTS},
	error::ResultExt,
	fs::{
		HasName, HasRemoteInfo, HasUUID,
		categories::normal::list_parent_uuid,
		dir::RemoteDirectory,
		file::{FileBuilderOptionalName, RemoteFile, traits::HasFileInfo},
	},
	io::{
		FilenMetaExt,
		abort_on_drop::AbortOnDropHandle,
		fs_tree::{DirChildrenInfo, Entry},
	},
};

/// Callback trait for folder upload operations
///
/// Folder uploads are implemented using a two step process
/// 1. Scan the folder to determine total size and number of files/folders
/// 2. Upload the folder contents while reporting progress
///
/// This means there can be a minor mismatch between the reported total size/files/folders
/// during scanning and the actual totals during upload, if files are added/removed
/// from the folder between the two steps.
pub trait DirUploadCallback: Send + Sync {
	fn on_scan_progress(&self, known_dirs: u64, known_files: u64, known_bytes: u64);
	fn on_scan_errors(&self, errors: Vec<Error>);
	fn on_scan_complete(&self, total_dirs: u64, total_files: u64, total_bytes: u64);
	fn on_upload_update(
		&self,
		uploaded_dirs: Vec<RemoteDirectory>,
		uploaded_files: Vec<RemoteFile>,
		uploaded_bytes: u64,
	);
	fn on_upload_errors(&self, errors: Vec<(PathBuf, Error)>);
	/// The symlinks the scan left out under [`SymlinkPolicy::Skip`], by their local paths. Called
	/// once, before [`on_scan_complete`](Self::on_scan_complete), when there were any.
	fn on_scan_skipped_symlinks(&self, paths: Vec<PathBuf>) {
		let _ = paths;
	}
	/// The files the upload left out because the remote directory already holds a file of the same
	/// name that [`ExistingRemoteFile`] keeps, by their local paths, with their total size in bytes.
	/// Called along with [`on_upload_update`](Self::on_upload_update).
	fn on_upload_skipped(&self, skipped_files: Vec<PathBuf>, skipped_bytes: u64) {
		let _ = (skipped_files, skipped_bytes);
	}
}

/// What a directory upload does with the symlinks inside the directory. The directory itself is
/// followed when it is a symlink.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SymlinkPolicy {
	/// Upload what a link points to under the link's name, as if it were there.
	#[default]
	Follow,
	/// Leave links out, uploading neither them nor what they point to.
	Skip,
}

/// What a directory upload does with a local file when the remote directory it goes into already
/// holds a file of the same name. Names are compared case-insensitively, as the server compares
/// them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ExistingRemoteFile {
	/// Upload it: it becomes the current file at that name, and the server keeps the one it
	/// replaces as an earlier version when versioning is enabled for the account.
	#[default]
	Replace,
	/// Leave the remote file as it is and the local file out.
	Keep,
	/// Upload it, as [`Replace`](Self::Replace) does, only when its size differs from the remote
	/// file's or it was modified after the remote file's modification time; otherwise leave it out,
	/// as [`Keep`](Self::Keep) does.
	ReplaceIfChanged,
}

impl ExistingRemoteFile {
	/// Whether the local file with metadata `local` is uploaded over `remote`, the remote file of
	/// the same name.
	pub fn replaces(self, local: &std::fs::Metadata, remote: &RemoteFile) -> bool {
		self.replaces_stamped(FileStamp::of_local(local), FileStamp::of_remote(remote))
	}

	fn replaces_stamped(self, local: FileStamp, remote: FileStamp) -> bool {
		match self {
			Self::Replace => true,
			Self::Keep => false,
			Self::ReplaceIfChanged => local.size != remote.size || local.modified > remote.modified,
		}
	}
}

/// What [`ExistingRemoteFile::ReplaceIfChanged`] compares of a file.
#[derive(Clone, Copy, Debug)]
struct FileStamp {
	size: u64,
	/// To the millisecond, as an upload records it.
	modified: DateTime<Utc>,
}

impl FileStamp {
	fn of_local(metadata: &std::fs::Metadata) -> Self {
		Self {
			size: FilenMetaExt::size(metadata),
			modified: FilenMetaExt::modified(metadata),
		}
	}

	/// A remote file without a recorded modification time counts as modified when it was uploaded.
	fn of_remote(file: &RemoteFile) -> Self {
		Self {
			size: HasFileInfo::size(file),
			modified: HasFileInfo::last_modified(file).unwrap_or_else(|| file.timestamp()),
		}
	}
}

/// How a directory upload walks the local directory and treats the remote one. The default
/// follows symlinks and replaces same-named remote files.
#[derive(Clone, Debug, Default)]
pub struct DirUploadOptions {
	/// What to do with the symlinks inside the directory.
	pub symlinks: SymlinkPolicy,
	/// What to do with a file the remote directory already holds a file of the same name as.
	pub existing: ExistingRemoteFile,
}

/// The files of a remote directory a [`ExistingRemoteFile`] other than `Replace` looks at, by
/// [`name_key`].
type ExistingFiles = HashMap<String, Box<RemoteFile>>;

/// The key two names the server takes for the same name share: it compares names by a hash of
/// their lowercase form.
fn name_key(name: &str) -> String {
	name.to_lowercase()
}

// todo, accept &impl HasUuidContents instead of RemoteDirectory for target_folder
impl Client {
	// this could definitely be optimized further, but for now it works
	pub(crate) async fn upload_fs_tree_from_path_into_target<C>(
		self: Arc<Self>,
		callback: impl Deref<Target = C>,
		path: PathBuf,
		tree: &super::fs_tree::FSTree<
			super::fs_tree::ExtraLocalDirData,
			super::fs_tree::ExtraLocalFileData,
		>,
		target_folder: &RemoteDirectory,
		existing: ExistingRemoteFile,
	) -> Result<(), Error>
	where
		C: DirUploadCallback + ?Sized,
	{
		// Keeps the drive lock between the steps below. Each step (creating a directory,
		// registering a file) takes the lock again itself: it gets this one while its lease is
		// confirmed and a fresh lease once it is lost, so no step writes under a lost lease. The
		// steps are independent, each checked by the server against its parent's uuid, so the
		// upload goes on across such a change of lease.
		let _lock = self.lock_drive().await?;

		let (file_upload_result_sender, mut file_upload_result_receiver) =
			tokio::sync::mpsc::channel::<FileUploadResult>(16);
		let (dir_upload_result_sender, mut dir_upload_result_receiver) =
			tokio::sync::mpsc::channel::<DirUploadResult>(16);

		let (entry_to_upload_sender, entry_to_upload_receiver) =
			tokio::sync::mpsc::channel::<EntryToUploadInfo>(64);

		let mut pending = PendingCallbacks::default();
		let uploaded_bytes = Arc::new(AtomicU64::new(0));

		let mut update_interval = tokio::time::interval(CALLBACK_INTERVAL);
		update_interval.reset();

		let mut unsent_children = VecDeque::new();

		let root_files = self.existing_files(target_folder.uuid(), existing).await?;
		dir_upload_result_sender
			.send(DirUploadResult(
				// have to clone here
				Ok((target_folder.clone(), tree.root_children(), root_files)),
				path,
			))
			.await
			.expect("dir_upload_task_sender.send panicked");
		let mut in_flight = 1; // root dir upload

		// Tie the detached manager task to this future's lifetime: if the caller
		// cancels (drops this future), the guard aborts the manager, and the
		// manager's `JoinSet` — dropped as its task unwinds — aborts every
		// queued and in-flight upload it spawned. Without this, up to 64 buffered
		// files would keep uploading after the user cancelled.
		let task = AbortOnDropHandle::new(self.spawn_upload_task_manager(
			dir_upload_result_sender,
			file_upload_result_sender,
			entry_to_upload_receiver,
			uploaded_bytes.clone(),
			existing,
		));

		loop {
			tokio::select! {
				biased;
				_ = update_interval.tick() => {
					pending.send(&uploaded_bytes, &callback);
					drain_unsent_entries(&entry_to_upload_sender, &mut unsent_children, &mut in_flight)?;
					if in_flight == 0 && unsent_children.is_empty() {
						break;
					}
				},
				Some(result) = file_upload_result_receiver.recv() => {
					in_flight -= 1;
					match result {
						FileUploadResult(Ok(FileUploaded::Uploaded(file)), _) => {
							pending.files.push(*file);
						},
						FileUploadResult(Ok(FileUploaded::Kept { bytes }), path) => {
							pending.skipped.push(path);
							pending.skipped_bytes = pending.skipped_bytes.saturating_add(bytes);
						},
						FileUploadResult(Err(error), path) => {
							pending.errors.push((path, error));
						}
					}
					drain_unsent_entries(&entry_to_upload_sender, &mut unsent_children, &mut in_flight)?;
					if in_flight == 0 && unsent_children.is_empty() {
						break;
					}
				}
				Some(result) = dir_upload_result_receiver.recv() => {
					in_flight -= 1;
					match result {
						DirUploadResult(Ok((dir, children_info, mut existing_files)), path) => {
							let new_children = tree.list_children(children_info);
							let parent_uuid = dir.uuid();
							pending.dirs.push(dir);
							for child in new_children {
								let entry_info = EntryToUploadInfo {
									entry: match child {
										Entry::Dir(dir) => EntryToUpload::Dir(
											path.join(tree.get_name(dir)).to_path_buf(),
											dir.children_info(),
										),
										Entry::File(file) => {
											let name = tree.get_name(file);
											EntryToUpload::File(
												path.join(name),
												existing_files.remove(&name_key(name)),
											)
										}
									},
									parent: parent_uuid,
								};
								try_send_entry(
									&entry_to_upload_sender,
									entry_info,
									&mut in_flight,
									&mut unsent_children,
								)?;
							}
						}
						DirUploadResult(Err(error), path) => {
							pending.errors.push((path, error));
						}
					}
					drain_unsent_entries(&entry_to_upload_sender, &mut unsent_children, &mut in_flight)?;
					if in_flight == 0 && unsent_children.is_empty() {
						break;
					}
				}
			}
		}
		std::mem::drop(entry_to_upload_sender);
		task.join().await.expect("upload task manager panicked");
		pending.send(&uploaded_bytes, &callback);
		Ok(())
	}

	fn spawn_upload_task_manager(
		self: Arc<Self>,
		dir_upload_sender: tokio::sync::mpsc::Sender<DirUploadResult>,
		file_upload_sender: tokio::sync::mpsc::Sender<FileUploadResult>,
		mut entry_to_upload_receiver: tokio::sync::mpsc::Receiver<EntryToUploadInfo>,
		bytes_progress_counter: Arc<AtomicU64>,
		existing: ExistingRemoteFile,
	) -> tokio::task::JoinHandle<()> {
		let semaphore = Arc::new(tokio::sync::Semaphore::new(MAX_SMALL_PARALLEL_REQUESTS));
		tokio::task::spawn(async move {
			// propagate aborts to all spawned tasks
			let mut join_set = tokio::task::JoinSet::new();
			loop {
				let permit = semaphore.clone().acquire_owned().await;
				{
					let permit = permit.expect("should be able to acquire semaphore permit");
					let entry_info = match entry_to_upload_receiver.recv().await {
						Some(info) => info,
						None => break, // channel closed, exit the loop
					};
					let client = self.clone();
					match entry_info.entry {
						EntryToUpload::File(path, remote) => {
							let file_sender = file_upload_sender.clone();
							let bytes_counter = bytes_progress_counter.clone();
							join_set.spawn(async move {
								let result = client
									.upload_child_file_unless_kept(
										&path,
										&entry_info.parent,
										remote.as_deref().map(|remote| (remote, existing)),
										bytes_counter,
									)
									.await;
								let _ = file_sender.send(FileUploadResult(result, path)).await;
								std::mem::drop(permit);
							});
						}
						EntryToUpload::Dir(path, children_info) => {
							let dir_sender = dir_upload_sender.clone();
							join_set.spawn(async move {
								let result = client
									.upload_child_dir_listing_files(
										&path,
										&entry_info.parent,
										existing,
									)
									.await;
								let _ = dir_sender
									.send(DirUploadResult(
										result.map(|(dir, files)| (dir, children_info, files)),
										path,
									))
									.await;
								std::mem::drop(permit);
							});
						}
					}
				}
			}
			join_set.join_all().await;
		})
	}

	/// Uploads the local file at `path` into `parent`, unless `existing` names the remote file of
	/// the same name there and the policy that keeps it.
	async fn upload_child_file_unless_kept(
		&self,
		path: &Path,
		parent: &Uuid,
		existing: Option<(&RemoteFile, ExistingRemoteFile)>,
		uploaded_bytes: Arc<AtomicU64>,
	) -> Result<FileUploaded, Error> {
		if let Some((remote, policy)) = existing {
			let metadata = tokio::fs::metadata(path).await.map_err(|e| {
				Error::custom_with_source(
					ErrorKind::IO,
					e,
					Some(format!("getting metadata for file {:?}", path)),
				)
			})?;
			if !policy.replaces(&metadata, remote) {
				return Ok(FileUploaded::Kept {
					bytes: FilenMetaExt::size(&metadata),
				});
			}
		}
		self.upload_file_from_path_with_builder(
			FileBuilderOptionalName::new(*parent),
			path.to_owned(),
			Some(Arc::new(|bytes_downloaded| {
				uploaded_bytes.fetch_add(bytes_downloaded, Ordering::Relaxed);
			})),
		)
		.await
		.context("uploading file from path")
		.map(|(file, _)| FileUploaded::Uploaded(Box::new(file)))
	}

	/// Creates the directory at `path` in `parent`, or takes the one already there, and lists the
	/// files in it that `existing` looks at.
	async fn upload_child_dir_listing_files(
		&self,
		path: &Path,
		parent: &Uuid,
		existing: ExistingRemoteFile,
	) -> Result<(RemoteDirectory, ExistingFiles), Error> {
		let dir = self.upload_child_dir_from_path(path, parent).await?;
		let files = self.existing_files(dir.uuid(), existing).await?;
		Ok((dir, files))
	}

	/// The files in the remote directory `dir` that `existing` looks at: none for
	/// [`ExistingRemoteFile::Replace`], which needs no listing. A file whose name cannot be
	/// decrypted is left out, as no local name can be told to match it.
	async fn existing_files(
		&self,
		dir: Uuid,
		existing: ExistingRemoteFile,
	) -> Result<ExistingFiles, Error> {
		match existing {
			ExistingRemoteFile::Replace => Ok(ExistingFiles::new()),
			ExistingRemoteFile::Keep | ExistingRemoteFile::ReplaceIfChanged => {
				let (_, files) =
					list_parent_uuid(self, ParentUuid::Uuid(dir), None::<&fn(u64, Option<u64>)>)
						.await
						.context("listing the files already in a directory during folder upload")?;
				Ok(files
					.into_iter()
					.filter_map(|file| Some((name_key(file.name()?), Box::new(file))))
					.collect())
			}
		}
	}

	async fn upload_child_dir_from_path(
		&self,
		path: &Path,
		parent: &Uuid,
	) -> Result<RemoteDirectory, Error> {
		let metadata = tokio::fs::metadata(path).await.map_err(|e| {
			Error::custom_with_source(
				ErrorKind::IO,
				e,
				Some(format!("getting metadata for directory {:?}", path)),
			)
		})?;
		let created = FilenMetaExt::created(&metadata);
		self.inner_create_dir_with_created(
			*parent,
			path.file_name()
				.expect("path name should be valid")
				.to_str()
				.expect("path name should be utf8"),
			created,
		)
		.await
		.context("creating directory during folder upload")
	}
}

/// Attempts to send an entry to the upload channel
///
/// Returns Ok(true) if the entry could not be sent due to full channel
/// Returns Ok(false) if the entry was sent successfully
/// Returns Err if the channel is closed
fn try_send_entry(
	sender: &tokio::sync::mpsc::Sender<EntryToUploadInfo>,
	entry: EntryToUploadInfo,
	in_flight: &mut usize,
	unsent: &mut VecDeque<EntryToUploadInfo>,
) -> Result<bool, Error> {
	match sender.try_send(entry) {
		Ok(()) => *in_flight += 1,
		Err(tokio::sync::mpsc::error::TrySendError::Closed(entry)) => {
			unsent.push_front(entry);
			return Err(Error::custom(
				ErrorKind::Internal,
				"entry_to_upload_sender channel closed",
			));
		}
		Err(e) => {
			unsent.push_front(e.into_inner());
			return Ok(true);
		}
	}
	Ok(false)
}

fn drain_unsent_entries(
	sender: &tokio::sync::mpsc::Sender<EntryToUploadInfo>,
	unsent: &mut VecDeque<EntryToUploadInfo>,
	in_flight: &mut usize,
) -> Result<(), Error> {
	while let Some(entry) = unsent.pop_front() {
		if try_send_entry(sender, entry, in_flight, unsent)? {
			break;
		}
	}
	Ok(())
}

/// What the upload has to report at the next callback.
#[derive(Default)]
struct PendingCallbacks {
	dirs: Vec<RemoteDirectory>,
	files: Vec<RemoteFile>,
	errors: Vec<(PathBuf, Error)>,
	skipped: Vec<PathBuf>,
	skipped_bytes: u64,
}

impl PendingCallbacks {
	fn send<C>(&mut self, uploaded_bytes: &Arc<AtomicU64>, callback: &impl Deref<Target = C>)
	where
		C: DirUploadCallback + ?Sized,
	{
		let callback = callback.deref();
		let bytes = uploaded_bytes.swap(0, Ordering::Relaxed);
		if !self.dirs.is_empty() || !self.files.is_empty() || bytes > 0 {
			callback.on_upload_update(
				std::mem::take(&mut self.dirs),
				std::mem::take(&mut self.files),
				bytes,
			);
		}
		if !self.skipped.is_empty() {
			callback.on_upload_skipped(
				std::mem::take(&mut self.skipped),
				std::mem::take(&mut self.skipped_bytes),
			);
		}
		if !self.errors.is_empty() {
			callback.on_upload_errors(std::mem::take(&mut self.errors));
		}
	}
}

struct DirUploadResult(
	Result<(RemoteDirectory, DirChildrenInfo, ExistingFiles), Error>,
	PathBuf,
);
struct FileUploadResult(Result<FileUploaded, Error>, PathBuf);

/// What became of a file the upload got to.
enum FileUploaded {
	Uploaded(Box<RemoteFile>),
	/// Left out for the remote file of the same name, `bytes` long locally.
	Kept {
		bytes: u64,
	},
}

struct EntryToUploadInfo {
	entry: EntryToUpload,
	parent: Uuid,
}
enum EntryToUpload {
	/// A file, with the remote file of the same name already in its directory, when the upload's
	/// [`ExistingRemoteFile`] looks at those.
	File(PathBuf, Option<Box<RemoteFile>>),
	Dir(PathBuf, DirChildrenInfo),
}

#[cfg(test)]
mod tests {
	use chrono::{DateTime, Duration, Utc};

	use super::{ExistingRemoteFile, FileStamp, name_key};

	fn stamp(size: u64, modified: DateTime<Utc>) -> FileStamp {
		FileStamp { size, modified }
	}

	fn remote() -> FileStamp {
		stamp(100, DateTime::<Utc>::UNIX_EPOCH + Duration::days(1000))
	}

	#[test]
	fn replace_uploads_over_any_remote_file() {
		assert!(ExistingRemoteFile::Replace.replaces_stamped(remote(), remote()));
	}

	#[test]
	fn keep_leaves_any_remote_file_alone() {
		let changed = stamp(5, remote().modified + Duration::days(1));
		assert!(!ExistingRemoteFile::Keep.replaces_stamped(changed, remote()));
	}

	#[test]
	fn an_unchanged_file_is_left_out_when_replacing_only_changed_ones() {
		assert!(!ExistingRemoteFile::ReplaceIfChanged.replaces_stamped(remote(), remote()));
	}

	#[test]
	fn an_older_file_of_the_same_size_is_left_out_when_replacing_only_changed_ones() {
		let older = stamp(100, remote().modified - Duration::milliseconds(1));
		assert!(!ExistingRemoteFile::ReplaceIfChanged.replaces_stamped(older, remote()));
	}

	#[test]
	fn a_newer_file_replaces_the_remote_one_when_replacing_only_changed_ones() {
		let newer = stamp(100, remote().modified + Duration::milliseconds(1));
		assert!(ExistingRemoteFile::ReplaceIfChanged.replaces_stamped(newer, remote()));
	}

	#[test]
	fn a_file_of_another_size_replaces_the_remote_one_when_replacing_only_changed_ones() {
		let resized = stamp(101, remote().modified - Duration::days(1));
		assert!(ExistingRemoteFile::ReplaceIfChanged.replaces_stamped(resized, remote()));
	}

	#[test]
	fn names_differing_in_case_share_a_key() {
		assert_eq!(name_key("Photo.JPG"), name_key("photo.jpg"));
		assert_eq!(name_key("Ärger.txt"), "ärger.txt");
	}
}
