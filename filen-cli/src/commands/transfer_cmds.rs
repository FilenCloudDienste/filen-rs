//! [cli-doc] transfers
//! Files and directories are transferred between your computer and your Filen drive with
//! `upload` and `download`. Directories are always transferred recursively.
//!
//! Like `cp`, both commands place the transferred item *inside* the destination when it is an
//! existing directory or ends in `/`, keeping its name: `upload ./notes /backups` creates
//! `/backups/notes`, and `download /backups/notes .` creates `./notes`. Any other destination is
//! the transferred item's new path: `upload ./notes.txt /backups/old.txt` uploads it as `old.txt`,
//! and `download /backups/notes ./old-notes` creates `./old-notes`. Transferring a file to the path
//! of an existing file replaces it (in the drive, with a new version of it). Missing directories
//! on the way to the destination are created. Downloading `/` writes the whole drive's contents
//! into the destination.
//!
//! When a single item inside a directory transfer fails, the rest of the transfer still completes
//! and the failures are listed at the end. Press Ctrl+C to cancel a running transfer.

use std::{
	borrow::Cow,
	collections::VecDeque,
	path::{Path, PathBuf},
	pin::pin,
	sync::{Arc, Mutex},
	time::{Duration, Instant},
};

use anyhow::{Context as _, Result};
use clap::Subcommand;
use clap_complete::engine::{ArgValueCompleter, PathCompleter};
use console::{Term, style};
use filen_sdk_rs::{
	Error, ErrorKind,
	auth::Client,
	fs::{
		HasName as _, HasUUID as _,
		categories::{DirType, NonRootFileType, NonRootItemType, Normal},
		file::{FileBuilderOptionalName, traits::HasFileInfo as _},
		name::ValidatedName,
	},
	io::{
		CategoryDirDownloadExtPub as _, DirDownloadCallback, DirUploadCallback, RemoteDirectory,
		RemoteFile, client_impl::IoSharedClientExt as _, is_safe_descendant,
	},
	util::MaybeSendCallback,
};
use tokio::select;

use crate::{
	CommandResult,
	auth::LazyClient,
	commands::CommandContext,
	completion::FilenCompleter,
	docs::DocElement,
	error::CliError,
	interrupt,
	ui::{self, UI},
	util::RemotePath,
};

#[derive(Debug, Subcommand)]
pub(crate) enum TransferCommands {
	/// Upload a local file or directory (recursively) to the Filen drive
	Upload {
		/// Local file or directory to upload
		#[arg(add = ArgValueCompleter::new(PathCompleter::any()))]
		source: String,
		/// Directory in the Filen drive to upload into, or the uploaded item's new path when it is
		/// not an existing directory and does not end in "/"; missing directories are created
		/// (default: the current working directory)
		#[arg(add = FilenCompleter::file_or_directory())]
		destination: Option<String>,
	},
	/// Download a file or directory (recursively) from the Filen drive
	Download {
		/// File or directory to download ("/" for the entire Filen drive)
		#[arg(add = FilenCompleter::file_or_directory())]
		source: String,
		/// Local directory to download into, or the downloaded item's new path when it is not an
		/// existing directory and does not end in a path separator; missing directories are
		/// created (default: the current local directory)
		#[arg(add = ArgValueCompleter::new(PathCompleter::any()))]
		destination: Option<String>,
	},
}

/// The docs' "Transferring Files" section, before `sync`.
pub(crate) const DOCS: &[DocElement] = &[
	DocElement::DocFragment("transfers"),
	DocElement::CommandHelp("upload"),
	DocElement::CommandHelp("download"),
];

impl TransferCommands {
	pub(super) async fn execute(self, ctx: CommandContext<'_>) -> Result<CommandResult> {
		let CommandContext {
			ui,
			client,
			working_path,
			..
		} = ctx;
		match self {
			Self::Upload {
				source,
				destination,
			} => upload(ui, client, working_path, &source, destination.as_deref()).await?,
			Self::Download {
				source,
				destination,
			} => download(ui, client, working_path, &source, destination.as_deref()).await?,
		}
		Ok(CommandResult::default())
	}
}

/// Upload a local file, or a local directory and everything below it, into a remote directory.
pub(crate) async fn upload(
	ui: &mut UI,
	client: &mut LazyClient,
	working_path: &RemotePath,
	source_str: &str,
	destination_str: Option<&str>,
) -> Result<()> {
	// Made absolute without resolving symlinks, so an uploaded link keeps its own name.
	let source = std::path::absolute(source_str)
		.map_err(|_| UI::failure(&format!("No such local file or directory: {}", source_str)))?;
	std::fs::symlink_metadata(&source)
		.map_err(|_| UI::failure(&format!("No such local file or directory: {}", source_str)))?;
	let is_dir = std::fs::metadata(&source)
		.map_err(|e| UI::failure(&format!("Cannot read {}: {}", source.display(), e)))?
		.is_dir();
	let name = upload_name(&source)?;

	// Without a destination, upload into the working directory.
	let into = destination_str.is_none_or(|destination| destination.ends_with('/'));
	let destination_path = working_path.navigate(destination_str.unwrap_or(""));
	let client = client.get(ui).await?.clone();
	let destination = find_remote(&client, &destination_path).await?;
	let target = resolve_upload_target(
		&destination_path,
		into,
		destination.existing(),
		&name,
		is_dir,
	)?;
	let parent_dir = match destination {
		RemoteItem::Dir(dir) if target.parent == destination_path => dir,
		_ => find_or_create_remote_dir(&client, &target.parent).await?,
	};

	let target_path = target.parent.navigate(target.name.as_ref());

	let progress = TransferProgress::new(TransferKind::Upload, ui);
	let transferred = if is_dir {
		run_cancellable(
			&progress,
			upload_directory(
				&client,
				&progress,
				&source,
				target.name.as_ref(),
				&parent_dir,
			),
		)
		.await?
	} else {
		run_cancellable(
			&progress,
			upload_file(
				&client,
				&progress,
				&source,
				target.name.as_ref(),
				&parent_dir,
			),
		)
		.await?
	};
	transferred.context("Failed to upload")?;

	finish(
		ui,
		&progress,
		&format!("{} to {}", source.display(), target_path),
	)
}

/// What a transfer's destination path names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Existing {
	Dir,
	File,
	Nothing,
}

/// A remote path looked up with [`find_remote`], keeping the directory it names.
enum RemoteItem<'a> {
	Dir(DirType<'a, Normal>),
	File,
	Nothing,
}

impl RemoteItem<'_> {
	fn existing(&self) -> Existing {
		match self {
			Self::Dir(_) => Existing::Dir,
			Self::File => Existing::File,
			Self::Nothing => Existing::Nothing,
		}
	}
}

/// Look up what `path` names in the drive.
async fn find_remote<'a>(client: &'a Client, path: &RemotePath) -> Result<RemoteItem<'a>> {
	match client.find_item_at_path(&path.0).await {
		Ok(Some(NonRootFileType::Dir(dir))) => Ok(RemoteItem::Dir(DirType::Dir(dir))),
		Ok(Some(NonRootFileType::Root(root))) => Ok(RemoteItem::Dir(DirType::Root(root))),
		Ok(Some(NonRootFileType::File(_))) => Ok(RemoteItem::File),
		Ok(None) => Ok(RemoteItem::Nothing),
		Err(e) if e.kind() == ErrorKind::InvalidType => Err(UI::failure(&format!(
			"Path contains a file in between: {}",
			path
		))),
		Err(e) => Err(e).context("Failed to find file or directory"),
	}
}

/// The directory at `path`, created along with any missing directories above it, like `mkdir -p`.
///
/// No drive lock spans this, the lookup of the destination before it and the upload after it, so
/// they are not atomic: the checks only turn the usual mistakes into clear failures early, and the
/// server still decides on the upload itself.
async fn find_or_create_remote_dir<'a>(
	client: &'a Client,
	path: &RemotePath,
) -> Result<DirType<'a, Normal>> {
	client.find_or_create_dir(&path.0).await.map_err(|e| {
		if e.kind() == ErrorKind::InvalidType {
			UI::failure(&format!("Path contains a file in between: {}", path))
		} else {
			anyhow::Error::from(e).context("Failed to create remote directory")
		}
	})
}

/// Where an upload goes: under `name` in the remote directory `parent`.
#[derive(Debug, PartialEq, Eq)]
struct UploadTarget {
	parent: RemotePath,
	name: ValidatedName,
}

impl UploadTarget {
	/// `name` under `parent`, refused before anything is uploaded when the drive does not take it
	/// as a name, whether for a file or a directory.
	fn new(parent: RemotePath, name: &str) -> Result<Self> {
		let name = ValidatedName::try_from(name)
			.map_err(|e| UI::failure(&format!("Cannot upload as {:?}: {}", name, e)))?;
		Ok(Self { parent, name })
	}
}

/// Where uploading the item named `source_name` to `destination` puts it. An existing directory,
/// or a destination given as one (`into`, from a trailing "/"), takes the item under its own name;
/// any other destination is the item's new path. The target's parent may not exist yet. A file may
/// replace an existing file, which becomes a new version of it, but a directory may not.
fn resolve_upload_target(
	destination: &RemotePath,
	into: bool,
	existing: Existing,
	source_name: &str,
	source_is_dir: bool,
) -> Result<UploadTarget> {
	let new_path = || {
		let name = destination
			.basename()
			.ok_or_else(|| UI::failure(&format!("Cannot upload to {}", destination)))?;
		UploadTarget::new(destination.parent(), name)
	};
	match existing {
		Existing::Dir => UploadTarget::new(destination.clone(), source_name),
		Existing::File if into => Err(UI::failure(&format!("Not a directory: {}", destination))),
		Existing::File if source_is_dir => Err(UI::failure(&format!(
			"Cannot upload a directory onto the file {}",
			destination
		))),
		Existing::File => new_path(),
		Existing::Nothing if into => UploadTarget::new(destination.clone(), source_name),
		Existing::Nothing => new_path(),
	}
}

/// The name an upload of `source` takes: its last component as given, so a symlink keeps its own
/// name instead of its target's. A path that ends without a name ("." or "some/dir/..") takes the
/// name of the directory it resolves to.
fn upload_name(source: &Path) -> Result<String> {
	let name = match source.file_name() {
		Some(name) => Cow::Borrowed(name),
		None => Cow::Owned(
			std::fs::canonicalize(source)
				.with_context(|| format!("Failed to resolve {}", source.display()))?
				.file_name()
				.ok_or_else(|| UI::failure(&format!("Cannot upload {}", source.display())))?
				.to_os_string(),
		),
	};
	name.to_str()
		.map(str::to_owned)
		.ok_or_else(|| UI::failure(&format!("Cannot upload {}", source.display())))
}

/// Download a remote file, or a remote directory and everything below it, to a local path.
pub(crate) async fn download(
	ui: &mut UI,
	client: &mut LazyClient,
	working_path: &RemotePath,
	source_str: &str,
	destination_str: Option<&str>,
) -> Result<()> {
	let destination_str = destination_str.unwrap_or(".");
	let into = destination_str.ends_with(std::path::is_separator);
	let existing = local_existing(Path::new(destination_str))?;
	let destination = match existing {
		Existing::Dir | Existing::File => std::fs::canonicalize(destination_str),
		Existing::Nothing => std::path::absolute(destination_str),
	}
	.map_err(|e| UI::failure(&format!("Cannot read {}: {}", destination_str, e)))?;

	let source_path = working_path.navigate(source_str);
	let client = client.get(ui).await?.clone();
	let Some(item) = client
		.find_item_at_path(&source_path.0)
		.await
		.context("Failed to find file or directory")?
	else {
		return Err(UI::failure(&format!(
			"No such file or directory: {}",
			source_path
		)));
	};
	let source = match &item {
		NonRootFileType::File(file) => {
			DownloadSource::File(file.name().context("Failed to decrypt file name")?)
		}
		NonRootFileType::Dir(dir) => {
			DownloadSource::Dir(dir.name().context("Failed to decrypt directory name")?)
		}
		NonRootFileType::Root(_) => DownloadSource::Root,
	};
	let target = resolve_download_target(&destination, into, existing, source, &source_path)?;
	// A directory download creates its own target directory and any missing ones above it.
	if let DownloadSource::File(_) = source
		&& let Some(parent) = target.parent()
	{
		std::fs::create_dir_all(parent).map_err(|e| {
			UI::failure(&format!(
				"Cannot create directory {}: {}",
				parent.display(),
				e
			))
		})?;
	}

	let progress = TransferProgress::new(TransferKind::Download, ui);
	let transferred = match &item {
		NonRootFileType::File(file) => {
			run_cancellable(
				&progress,
				download_file(&client, &progress, file.as_ref(), &target),
			)
			.await?
		}
		NonRootFileType::Dir(dir) => {
			let dir = DirType::Dir(Cow::Owned(dir.as_ref().clone()));
			run_cancellable(
				&progress,
				download_directory(&client, &progress, dir, &target),
			)
			.await?
		}
		NonRootFileType::Root(root) => {
			let root = DirType::Root(Cow::Owned(root.as_ref().clone()));
			run_cancellable(
				&progress,
				download_directory(&client, &progress, root, &target),
			)
			.await?
		}
	};
	transferred.context("Failed to download")?;

	finish(
		ui,
		&progress,
		&format!("{} to {}", source_path, target.display()),
	)
}

/// What a local path names, following symlinks.
fn local_existing(path: &Path) -> Result<Existing> {
	match std::fs::metadata(path) {
		Ok(metadata) if metadata.is_dir() => Ok(Existing::Dir),
		Ok(_) => Ok(Existing::File),
		Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Existing::Nothing),
		Err(e) => Err(UI::failure(&format!(
			"Cannot read {}: {}",
			path.display(),
			e
		))),
	}
}

/// The item a download is of, with its name.
#[derive(Clone, Copy)]
enum DownloadSource<'a> {
	File(&'a str),
	Dir(&'a str),
	/// The drive root, which has no name of its own.
	Root,
}

/// Where downloading `source` (found at `source_path`) to `destination` puts it. An existing
/// directory, or a destination given as one (`into`, from a trailing separator), takes the item
/// under its own name, and the drive root's contents go straight into it; any other destination
/// is the item's new path. A file may replace an existing file, but a directory may not. The
/// target's parent may not exist yet.
fn resolve_download_target(
	destination: &Path,
	into: bool,
	existing: Existing,
	source: DownloadSource<'_>,
	source_path: &RemotePath,
) -> Result<PathBuf> {
	let inside = || match source {
		DownloadSource::File(name) | DownloadSource::Dir(name) => {
			download_target(destination, name).ok_or_else(|| {
				UI::failure(&format!(
					"Cannot download {}: its name {:?} is not a plain file name",
					source_path, name
				))
			})
		}
		DownloadSource::Root => Ok(destination.to_path_buf()),
	};
	match existing {
		Existing::Dir => inside(),
		Existing::File if into => Err(UI::failure(&format!(
			"Not a directory: {}",
			destination.display()
		))),
		Existing::File => match source {
			DownloadSource::File(_) => Ok(destination.to_path_buf()),
			DownloadSource::Dir(_) | DownloadSource::Root => Err(UI::failure(&format!(
				"Cannot download a directory onto the file {}",
				destination.display()
			))),
		},
		Existing::Nothing if into => inside(),
		Existing::Nothing => Ok(destination.to_path_buf()),
	}
}

/// Where an item named `name` is downloaded to inside `destination`, or `None` when the name is not
/// a single plain path component (`..`, an absolute path, one holding a separator) and joining it
/// would leave `destination`. The name comes from the item's decrypted metadata, which the server
/// does not validate.
fn download_target(destination: &Path, name: &str) -> Option<PathBuf> {
	is_safe_descendant(name).then(|| destination.join(name))
}

/// Await `transfer` until it completes or the user hits Ctrl+C, erasing the progress line either
/// way. Cancelling drops the transfer future, which is how the SDK aborts the workers it spawned.
async fn run_cancellable<T>(
	progress: &TransferProgress,
	transfer: impl Future<Output = Result<T>>,
) -> Result<Result<T>> {
	let mut ctrl_c = interrupt::own();
	let mut idle_redraws = tokio::time::interval(IDLE_REDRAW_INTERVAL);
	let mut transfer = pin!(transfer);
	let outcome = loop {
		select! {
			() = ctrl_c.pressed() => break None,
			result = &mut transfer => break Some(result),
			_ = idle_redraws.tick() => progress.redraw_idle(),
		}
	};
	progress.clear();
	outcome.ok_or_else(|| CliError::Interrupted.into())
}

/// Print the per-item failures the SDK reported through the progress callbacks, then either the
/// success message or a summary of what did transfer. Directory transfers return `Ok` even when
/// individual items failed, so without this the command would claim success while items were
/// missing.
fn finish(ui: &mut UI, progress: &TransferProgress, subject: &str) -> Result<()> {
	let kind = progress.kind;
	let (files, bytes) = progress.done();
	let errors = progress.take_errors();
	for error in errors.iter().take(MAX_PRINTED_ERRORS) {
		ui.print_failure(error);
	}
	if errors.len() > MAX_PRINTED_ERRORS {
		ui.print_failure(&format!(
			"... and {} more",
			errors.len() - MAX_PRINTED_ERRORS
		));
	}
	let summary = transfer_summary(files, bytes, progress.started.elapsed());
	if errors.is_empty() {
		ui.print_success(&format!("{} {}: {}", kind.past(), subject, summary));
		Ok(())
	} else {
		if !ui.is_quiet() {
			ui.print_muted(&format!("{} {}", kind.past(), summary));
		}
		Err(UI::failure(&format!(
			"{} item(s) failed to {}",
			errors.len(),
			kind.action()
		)))
	}
}

async fn upload_file(
	client: &Client,
	progress: &TransferProgress,
	source: &Path,
	name: &str,
	destination_dir: &DirType<'_, Normal>,
) -> Result<()> {
	let mut builder = FileBuilderOptionalName::new(destination_dir.uuid());
	builder
		.name(name)
		.map_err(|e| UI::failure(&format!("Cannot upload as {:?}: {}", name, e)))?;
	let size = std::fs::metadata(source)
		.with_context(|| format!("Failed to read {}", source.display()))?
		.len();
	progress.start_single_item(size);
	client
		.upload_file_from_path_with_builder(
			builder,
			source.to_path_buf(),
			Some(progress.byte_callback()),
		)
		.await
		.context("Failed to upload file")?;
	progress.complete_single_item();
	Ok(())
}

async fn upload_directory(
	client: &Arc<Client>,
	progress: &TransferProgress,
	source: &Path,
	name: &str,
	destination_dir: &DirType<'_, Normal>,
) -> Result<()> {
	// `upload_dir_recursively` uploads the *contents* of `source` into an already existing remote
	// directory, so create (or reuse) the destination's `name` subdirectory first. This also
	// covers uploading into the drive root, which is not a `RemoteDirectory` and could therefore
	// not be passed as the target at all.
	let target = client
		.find_or_create_dir_starting_at(destination_dir.clone(), name)
		.await
		.context("Failed to create remote directory")?;
	let DirType::Dir(target) = target else {
		return Err(UI::failure(&format!("Cannot upload into: {}", name)));
	};
	progress.start_scan();
	client
		.clone()
		.upload_dir_recursively(source.to_path_buf(), progress, target.as_ref())
		.await
		.context("Failed to upload directory")
}

async fn download_file(
	client: &Client,
	progress: &TransferProgress,
	source: &RemoteFile,
	target: &Path,
) -> Result<()> {
	progress.start_single_item(source.size());
	client
		.download_file_to_path_unless_same_hash(source, target, Some(progress.byte_callback()))
		.await
		.context("Failed to download file")?;
	progress.complete_single_item();
	Ok(())
}

async fn download_directory(
	client: &Arc<Client>,
	progress: &TransferProgress,
	source: DirType<'static, Normal>,
	target: &Path,
) -> Result<()> {
	let target = target
		.to_str()
		.ok_or_else(|| {
			UI::failure(&format!(
				"Destination path is not valid UTF-8: {}",
				target.display()
			))
		})?
		.to_string();
	progress.start_scan();
	Normal::download_dir_recursively(client.clone(), target, progress, source, ())
		.await
		.context("Failed to download directory")
}

// progress reporting

/// At most this many per-item failures are listed after a transfer; a broken directory tree can
/// produce one per file.
const MAX_PRINTED_ERRORS: usize = 10;

/// Minimum time between two redraws of the progress line. Directory transfers already aggregate
/// their callbacks, single-file transfers are throttled by the SDK — this only guards against
/// both of those changing.
const RENDER_INTERVAL: Duration = Duration::from_millis(100);

/// How far back the speed in the progress line looks, so that it follows the current rate rather
/// than the whole transfer's average.
const SPEED_WINDOW: Duration = Duration::from_secs(5);

/// How often the progress line is redrawn while nothing moves, which no callback reports, so that
/// the speed of a stalled transfer falls.
const IDLE_REDRAW_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Clone, Copy)]
pub(crate) enum TransferKind {
	Upload,
	Download,
}

impl TransferKind {
	fn verb(self) -> &'static str {
		match self {
			Self::Upload => "Uploading",
			Self::Download => "Downloading",
		}
	}

	fn past(self) -> &'static str {
		match self {
			Self::Upload => "Uploaded",
			Self::Download => "Downloaded",
		}
	}

	fn action(self) -> &'static str {
		match self {
			Self::Upload => "upload",
			Self::Download => "download",
		}
	}
}

#[derive(Default)]
struct ProgressState {
	/// While scanning, the totals are still growing and no bytes have moved yet.
	scanning: bool,
	total_files: u64,
	total_bytes: u64,
	done_files: u64,
	done_bytes: u64,
	/// `(when, done_bytes)`, oldest first, reaching back just past [`SPEED_WINDOW`].
	speed_samples: VecDeque<(Instant, u64)>,
	last_render: Option<Instant>,
	/// Whether there is a progress line on screen that still needs erasing.
	line_shown: bool,
}

impl ProgressState {
	/// Note `done_bytes` at `now` for the speed, at most once per [`RENDER_INTERVAL`], and drop the
	/// samples the window no longer needs.
	fn sample_speed(&mut self, now: Instant) {
		if self
			.speed_samples
			.back()
			.is_some_and(|&(at, _)| now.saturating_duration_since(at) < RENDER_INTERVAL)
		{
			return;
		}
		self.speed_samples.push_back((now, self.done_bytes));
		// Keep the newest sample from before the window, so that the speed spans all of it.
		while self
			.speed_samples
			.get(1)
			.is_some_and(|&(at, _)| now.saturating_duration_since(at) >= SPEED_WINDOW)
		{
			self.speed_samples.pop_front();
		}
	}

	/// Note that `now` has come, whether or not anything moved, so that a stalled transfer's speed
	/// falls to zero once [`SPEED_WINDOW`] has passed. Nothing is noted while scanning, where no
	/// bytes move yet, or before the speed is first sampled. Returns whether it was noted.
	fn note_time(&mut self, now: Instant) -> bool {
		let noted = !self.scanning && !self.speed_samples.is_empty();
		if noted {
			self.sample_speed(now);
		}
		noted
	}

	/// The current speed in bytes per second, or `None` while the samples span no time yet.
	fn speed(&self, now: Instant) -> Option<u64> {
		let &(since, bytes_then) = self.speed_samples.front()?;
		bytes_per_second(
			self.done_bytes.saturating_sub(bytes_then),
			now.saturating_duration_since(since),
		)
	}
}

/// `bytes` per second over `elapsed`, or `None` when it is under a millisecond.
fn bytes_per_second(bytes: u64, elapsed: Duration) -> Option<u64> {
	let millis = elapsed.as_millis();
	(millis > 0).then(|| u64::try_from(u128::from(bytes) * 1000 / millis).unwrap_or(u64::MAX))
}

/// The progress line for `state` at `now`.
fn progress_line(kind: TransferKind, state: &ProgressState, now: Instant) -> String {
	if state.scanning {
		return format!(
			"Scanning... {} in {} files",
			ui::format_size(state.total_bytes),
			state.total_files
		);
	}
	if state.total_bytes == 0 {
		return format!(
			"{} {}/{} files",
			kind.verb(),
			state.done_files,
			state.total_files
		);
	}
	let percent = u128::from(state.done_bytes) * 100 / u128::from(state.total_bytes);
	let mut line = format!(
		"{} {} / {} ({}%) - {}/{} files",
		kind.verb(),
		ui::format_size(state.done_bytes),
		ui::format_size(state.total_bytes),
		percent.min(100),
		state.done_files,
		state.total_files
	);
	if let Some(speed) = state.speed(now) {
		line.push_str(&format!(" - {}/s", ui::format_size(speed)));
		// A stalled transfer has no end in sight.
		if speed > 0 {
			let remaining = state.total_bytes.saturating_sub(state.done_bytes);
			line.push_str(&format!(
				", {} left",
				format_duration(remaining.div_ceil(speed))
			));
		}
	}
	line
}

/// `secs` as the two largest of hours, minutes and seconds.
fn format_duration(secs: u64) -> String {
	match secs {
		0..60 => format!("{}s", secs),
		60..3600 => format!("{}m {:02}s", secs / 60, secs % 60),
		_ => format!("{}h {:02}m", secs / 3600, secs % 3600 / 60),
	}
}

/// A transfer's elapsed time: to a tenth of a second under a minute.
fn format_elapsed(elapsed: Duration) -> String {
	if elapsed < Duration::from_secs(60) {
		format!("{:.1}s", elapsed.as_secs_f64())
	} else {
		format_duration(elapsed.as_secs())
	}
}

fn files_and_bytes(files: u64, bytes: u64) -> String {
	format!(
		"{} {}, {}",
		files,
		if files == 1 { "file" } else { "files" },
		ui::format_size(bytes)
	)
}

/// What a transfer moved and how fast, on average.
fn transfer_summary(files: u64, bytes: u64, elapsed: Duration) -> String {
	let summary = format!(
		"{} in {}",
		files_and_bytes(files, bytes),
		format_elapsed(elapsed)
	);
	match bytes_per_second(bytes, elapsed) {
		Some(speed) => format!("{} ({}/s)", summary, ui::format_size(speed)),
		None => summary,
	}
}

/// Draws a single, redrawn-in-place progress line for one upload or download.
///
/// The SDK delivers its progress callbacks from worker threads, so this cannot go through [`UI`]
/// (which is `&mut`-borrowed by the command) and instead writes to the terminal itself.
pub(crate) struct TransferProgress {
	kind: TransferKind,
	term: Term,
	/// Whether the progress line may be drawn at all: never in quiet or JSON mode, and never when
	/// stdout is redirected, where the redraws would end up in the captured output.
	enabled: bool,
	started: Instant,
	state: Mutex<ProgressState>,
	errors: Mutex<Vec<String>>,
}

impl TransferProgress {
	pub(crate) fn new(kind: TransferKind, ui: &UI) -> Self {
		let term = Term::stdout();
		let enabled = term.is_term() && !ui.is_quiet() && !ui.json;
		Self {
			kind,
			term,
			enabled,
			started: Instant::now(),
			state: Mutex::new(ProgressState::default()),
			errors: Mutex::new(Vec::new()),
		}
	}

	/// A byte-delta callback for the SDK's single-file transfer methods.
	fn byte_callback(&self) -> MaybeSendCallback<'_, u64> {
		Arc::new(|bytes| self.advance(0, bytes))
	}

	/// Begin a single-file transfer of known size — there is no scan phase for those.
	fn start_single_item(&self, size: u64) {
		let mut state = self.state.lock().unwrap();
		state.total_files = 1;
		state.total_bytes = size;
		state.sample_speed(Instant::now());
		self.render(&mut state, true);
	}

	/// Mark the single file as fully transferred. The SDK's throttle may still be holding the last
	/// few byte deltas, so set the counters outright instead of adding to them.
	fn complete_single_item(&self) {
		let mut state = self.state.lock().unwrap();
		state.done_files = state.total_files;
		state.done_bytes = state.total_bytes;
		self.render(&mut state, true);
	}

	fn start_scan(&self) {
		let mut state = self.state.lock().unwrap();
		state.scanning = true;
		self.render(&mut state, true);
	}

	fn report_scan(&self, known_files: u64, known_bytes: u64) {
		let mut state = self.state.lock().unwrap();
		state.total_files = known_files;
		state.total_bytes = known_bytes;
		self.render(&mut state, false);
	}

	fn finish_scan(&self, total_files: u64, total_bytes: u64) {
		let mut state = self.state.lock().unwrap();
		state.scanning = false;
		state.total_files = total_files;
		state.total_bytes = total_bytes;
		state.sample_speed(Instant::now());
		self.render(&mut state, true);
	}

	/// Redraw the line for the time passed, as callbacks redraw it only when something moved.
	fn redraw_idle(&self) {
		let mut state = self.state.lock().unwrap();
		if state.note_time(Instant::now()) {
			self.render(&mut state, false);
		}
	}

	fn advance(&self, files: u64, bytes: u64) {
		let mut state = self.state.lock().unwrap();
		state.done_files = state.done_files.saturating_add(files);
		state.done_bytes = state.done_bytes.saturating_add(bytes);
		state.sample_speed(Instant::now());
		self.render(&mut state, false);
	}

	/// The files and bytes transferred so far.
	fn done(&self) -> (u64, u64) {
		let state = self.state.lock().unwrap();
		(state.done_files, state.done_bytes)
	}

	fn push_errors(&self, errors: impl IntoIterator<Item = String>) {
		self.errors.lock().unwrap().extend(errors);
	}

	fn take_errors(&self) -> Vec<String> {
		std::mem::take(&mut self.errors.lock().unwrap())
	}

	/// Erase the progress line so the command's own output starts on a clean row.
	fn clear(&self) {
		let mut state = self.state.lock().unwrap();
		if state.line_shown {
			let _ = self.term.clear_line();
			let _ = self.term.flush();
			state.line_shown = false;
		}
	}

	fn render(&self, state: &mut ProgressState, force: bool) {
		if !self.enabled {
			return;
		}
		let now = Instant::now();
		if !force
			&& let Some(last) = state.last_render
			&& now.duration_since(last) < RENDER_INTERVAL
		{
			return;
		}
		state.last_render = Some(now);
		let line = progress_line(self.kind, state, now);
		let line = ui::truncate_to_width(&line, self.term.size().1 as usize);
		let _ = self.term.clear_line();
		let _ = self.term.write_str(&style(line).dim().to_string());
		let _ = self.term.flush();
		state.line_shown = true;
	}
}

impl DirUploadCallback for TransferProgress {
	fn on_scan_progress(&self, _known_dirs: u64, known_files: u64, known_bytes: u64) {
		self.report_scan(known_files, known_bytes);
	}

	fn on_scan_errors(&self, errors: Vec<Error>) {
		self.push_errors(errors.iter().map(|e| e.to_string()));
	}

	fn on_scan_complete(&self, _total_dirs: u64, total_files: u64, total_bytes: u64) {
		self.finish_scan(total_files, total_bytes);
	}

	fn on_upload_update(
		&self,
		_uploaded_dirs: Vec<RemoteDirectory>,
		uploaded_files: Vec<RemoteFile>,
		uploaded_bytes: u64,
	) {
		self.advance(uploaded_files.len() as u64, uploaded_bytes);
	}

	fn on_upload_errors(&self, errors: Vec<(PathBuf, Error)>) {
		self.push_errors(
			errors
				.iter()
				.map(|(path, e)| format!("{}: {}", path.display(), e)),
		);
	}
}

impl DirDownloadCallback<Normal> for TransferProgress {
	fn on_query_download_progress(&self, _known_bytes: u64, _total_bytes: Option<u64>) {}

	fn on_scan_progress(&self, _known_dirs: u64, known_files: u64, known_bytes: u64) {
		self.report_scan(known_files, known_bytes);
	}

	fn on_scan_errors(&self, errors: Vec<Error>) {
		self.push_errors(errors.iter().map(|e| e.to_string()));
	}

	fn on_scan_complete(&self, _total_dirs: u64, total_files: u64, total_bytes: u64) {
		self.finish_scan(total_files, total_bytes);
	}

	fn on_download_update(
		&self,
		_downloaded_dirs: Vec<(RemoteDirectory, String)>,
		downloaded_files: Vec<(RemoteFile, String)>,
		downloaded_bytes: u64,
	) {
		self.advance(downloaded_files.len() as u64, downloaded_bytes);
	}

	fn on_download_errors(&self, errors: Vec<(Error, String, NonRootItemType<'static, Normal>)>) {
		self.push_errors(errors.iter().map(|(e, path, _)| format!("{}: {}", path, e)));
	}
}

#[cfg(test)]
mod tests {
	use std::{
		path::Path,
		time::{Duration, Instant},
	};

	use filen_sdk_rs::fs::name::ValidatedName;

	use super::{
		DownloadSource, Existing, ProgressState, SPEED_WINDOW, TransferKind, UploadTarget,
		download_target, format_duration, local_existing, progress_line, resolve_download_target,
		resolve_upload_target, transfer_summary,
	};
	use crate::util::RemotePath;

	const MIB: u64 = 1024 * 1024;

	/// Halfway through a 1 MiB upload, with the given speed samples.
	fn halfway(samples: &[(Instant, u64)]) -> ProgressState {
		ProgressState {
			total_files: 1,
			total_bytes: MIB,
			done_bytes: MIB / 2,
			speed_samples: samples.iter().copied().collect(),
			..ProgressState::default()
		}
	}

	#[test]
	fn the_progress_line_shows_the_speed_and_the_time_left() {
		let start = Instant::now();
		let state = halfway(&[(start, 0)]);
		assert_eq!(
			progress_line(TransferKind::Upload, &state, start + Duration::from_secs(2)),
			"Uploading 512 KiB / 1 MiB (50%) - 0/1 files - 256 KiB/s, 2s left"
		);
	}

	#[test]
	fn the_progress_line_has_no_speed_before_any_time_has_passed() {
		let start = Instant::now();
		let state = halfway(&[(start, 0)]);
		assert_eq!(
			progress_line(TransferKind::Download, &state, start),
			"Downloading 512 KiB / 1 MiB (50%) - 0/1 files"
		);
	}

	#[test]
	fn a_stalled_transfer_shows_no_time_left() {
		let start = Instant::now();
		let state = halfway(&[(start, MIB / 2), (start + SPEED_WINDOW, MIB / 2)]);
		assert_eq!(
			progress_line(
				TransferKind::Upload,
				&state,
				start + SPEED_WINDOW + Duration::from_secs(1)
			),
			"Uploading 512 KiB / 1 MiB (50%) - 0/1 files - 0 B/s"
		);
	}

	#[test]
	fn a_transfer_of_empty_files_counts_files_only() {
		let start = Instant::now();
		let state = ProgressState {
			total_files: 3,
			done_files: 1,
			speed_samples: [(start, 0)].into_iter().collect(),
			..ProgressState::default()
		};
		assert_eq!(
			progress_line(TransferKind::Upload, &state, start + Duration::from_secs(1)),
			"Uploading 1/3 files"
		);
	}

	#[test]
	fn the_speed_window_keeps_one_sample_from_before_it() {
		let start = Instant::now();
		let mut state = ProgressState::default();
		for second in 0..=10 {
			state.done_bytes = second * 100;
			state.sample_speed(start + Duration::from_secs(second));
		}
		let first = state.speed_samples.front().unwrap();
		assert_eq!(first.0, start + Duration::from_secs(5));
		assert_eq!(state.speed(start + Duration::from_secs(10)), Some(100));
	}

	#[test]
	fn the_time_passing_while_stalled_brings_the_speed_down_to_zero() {
		let start = Instant::now();
		let mut state = ProgressState::default();
		state.sample_speed(start);
		state.done_bytes = 500;
		state.sample_speed(start + Duration::from_secs(1));
		for second in 2..=7 {
			assert!(state.note_time(start + Duration::from_secs(second)));
		}
		assert_eq!(state.speed(start + Duration::from_secs(7)), Some(0));
	}

	#[test]
	fn no_time_is_noted_while_scanning() {
		let start = Instant::now();
		let mut state = ProgressState {
			scanning: true,
			speed_samples: [(start, 0)].into_iter().collect(),
			..ProgressState::default()
		};
		assert!(!state.note_time(start + Duration::from_secs(1)));
		assert_eq!(state.speed_samples.len(), 1);
	}

	#[test]
	fn no_time_is_noted_before_the_speed_is_first_sampled() {
		let mut state = ProgressState::default();
		assert!(!state.note_time(Instant::now()));
		assert!(state.speed_samples.is_empty());
	}

	#[test]
	fn progress_after_a_gap_unnoted_by_time_averages_over_the_gap() {
		let start = Instant::now();
		let mut state = ProgressState::default();
		state.sample_speed(start);
		state.done_bytes = 100;
		state.sample_speed(start + Duration::from_secs(1));
		state.done_bytes = 1100;
		state.sample_speed(start + Duration::from_secs(11));
		assert_eq!(state.speed(start + Duration::from_secs(11)), Some(100));
	}

	#[test]
	fn durations_show_their_two_largest_units() {
		assert_eq!(format_duration(0), "0s");
		assert_eq!(format_duration(59), "59s");
		assert_eq!(format_duration(61), "1m 01s");
		assert_eq!(format_duration(3600 + 120 + 5), "1h 02m");
	}

	#[test]
	fn the_summary_gives_the_average_speed() {
		assert_eq!(
			transfer_summary(3, 2 * MIB, Duration::from_secs(4)),
			"3 files, 2 MiB in 4.0s (512 KiB/s)"
		);
	}

	#[test]
	fn the_summary_of_an_instant_transfer_has_no_speed() {
		assert_eq!(
			transfer_summary(1, 100, Duration::ZERO),
			"1 file, 100 B in 0.0s"
		);
	}

	fn upload_target(parent: &str, name: &str) -> UploadTarget {
		UploadTarget {
			parent: RemotePath::new(parent),
			name: ValidatedName::try_from(name).unwrap(),
		}
	}

	#[test]
	fn an_upload_to_an_existing_directory_goes_inside_it() {
		assert_eq!(
			resolve_upload_target(
				&RemotePath::new("/dir"),
				false,
				Existing::Dir,
				"a.txt",
				false
			)
			.unwrap(),
			upload_target("/dir", "a.txt")
		);
	}

	#[test]
	fn an_upload_to_the_root_goes_inside_it() {
		assert_eq!(
			resolve_upload_target(&RemotePath::new("/"), true, Existing::Dir, "dir", true).unwrap(),
			upload_target("/", "dir")
		);
	}

	#[test]
	fn an_upload_to_a_new_path_takes_its_name() {
		assert_eq!(
			resolve_upload_target(
				&RemotePath::new("/dir/new.txt"),
				false,
				Existing::Nothing,
				"a.txt",
				false
			)
			.unwrap(),
			upload_target("/dir", "new.txt")
		);
	}

	#[test]
	fn a_directory_upload_to_a_new_path_takes_its_name() {
		assert_eq!(
			resolve_upload_target(
				&RemotePath::new("/dir/new"),
				false,
				Existing::Nothing,
				"a",
				true
			)
			.unwrap(),
			upload_target("/dir", "new")
		);
	}

	#[test]
	fn a_file_upload_onto_an_existing_file_replaces_it() {
		assert_eq!(
			resolve_upload_target(
				&RemotePath::new("/dir/old.txt"),
				false,
				Existing::File,
				"a.txt",
				false
			)
			.unwrap(),
			upload_target("/dir", "old.txt")
		);
	}

	#[test]
	fn a_directory_upload_onto_an_existing_file_is_refused() {
		assert_eq!(
			resolve_upload_target(
				&RemotePath::new("/dir/old.txt"),
				false,
				Existing::File,
				"a",
				true
			)
			.unwrap_err()
			.to_string(),
			"Cannot upload a directory onto the file /dir/old.txt"
		);
	}

	#[test]
	fn a_directory_upload_to_a_name_the_drive_does_not_take_is_refused() {
		assert_eq!(
			resolve_upload_target(
				&RemotePath::new("/x/bad:name"),
				false,
				Existing::Nothing,
				"d",
				true
			)
			.unwrap_err()
			.to_string(),
			"Cannot upload as \"bad:name\": invalid filename \"bad:name\": filename contains \
			 forbidden character ':' at position 3"
		);
	}

	#[test]
	fn an_upload_into_a_file_given_as_a_directory_is_refused() {
		assert_eq!(
			resolve_upload_target(
				&RemotePath::new("/dir/old.txt"),
				true,
				Existing::File,
				"a.txt",
				false
			)
			.unwrap_err()
			.to_string(),
			"Not a directory: /dir/old.txt"
		);
	}

	#[test]
	fn an_upload_into_a_missing_directory_goes_inside_it() {
		assert_eq!(
			resolve_upload_target(
				&RemotePath::new("/x/y"),
				true,
				Existing::Nothing,
				"a.txt",
				false
			)
			.unwrap(),
			upload_target("/x/y", "a.txt")
		);
	}

	fn download_target_of(
		destination: &str,
		into: bool,
		existing: Existing,
		source: DownloadSource<'_>,
	) -> Result<std::path::PathBuf, String> {
		resolve_download_target(
			Path::new(destination),
			into,
			existing,
			source,
			&RemotePath::new("/remote/item"),
		)
		.map_err(|e| e.to_string())
	}

	#[test]
	fn a_download_to_an_existing_directory_goes_inside_it() {
		assert_eq!(
			download_target_of("/dest", false, Existing::Dir, DownloadSource::File("a.txt")),
			Ok(Path::new("/dest/a.txt").to_path_buf())
		);
	}

	#[test]
	fn a_download_to_a_new_path_takes_its_name() {
		assert_eq!(
			download_target_of(
				"/dest/renamed.txt",
				false,
				Existing::Nothing,
				DownloadSource::File("a.txt")
			),
			Ok(Path::new("/dest/renamed.txt").to_path_buf())
		);
		assert_eq!(
			download_target_of(
				"/dest/new",
				false,
				Existing::Nothing,
				DownloadSource::Dir("a")
			),
			Ok(Path::new("/dest/new").to_path_buf())
		);
	}

	#[test]
	fn a_download_into_a_missing_directory_goes_inside_it() {
		assert_eq!(
			download_target_of("/dest/", true, Existing::Nothing, DownloadSource::Dir("a")),
			Ok(Path::new("/dest/a").to_path_buf())
		);
	}

	#[test]
	fn the_drive_root_is_downloaded_into_a_missing_directory() {
		assert_eq!(
			download_target_of("/dest", false, Existing::Nothing, DownloadSource::Root),
			Ok(Path::new("/dest").to_path_buf())
		);
	}

	#[test]
	fn a_file_download_onto_an_existing_file_replaces_it() {
		assert_eq!(
			download_target_of(
				"/dest/old.txt",
				false,
				Existing::File,
				DownloadSource::File("a.txt")
			),
			Ok(Path::new("/dest/old.txt").to_path_buf())
		);
	}

	#[test]
	fn a_directory_download_onto_an_existing_file_is_refused() {
		assert_eq!(
			download_target_of(
				"/dest/old.txt",
				false,
				Existing::File,
				DownloadSource::Dir("a")
			),
			Err("Cannot download a directory onto the file /dest/old.txt".to_owned())
		);
	}

	#[test]
	fn a_download_into_a_file_given_as_a_directory_is_refused() {
		assert_eq!(
			download_target_of(
				"/dest/old.txt/",
				true,
				Existing::File,
				DownloadSource::File("a.txt")
			),
			Err("Not a directory: /dest/old.txt/".to_owned())
		);
	}

	#[test]
	fn an_unsafe_name_is_refused_when_the_download_goes_inside_the_destination() {
		assert_eq!(
			download_target_of("/dest", false, Existing::Dir, DownloadSource::File("..")),
			Err(
				"Cannot download /remote/item: its name \"..\" is not a plain file name".to_owned()
			)
		);
	}

	#[test]
	fn a_local_path_is_looked_up_as_a_directory_a_file_or_nothing() {
		let dir = tempfile::tempdir().unwrap();
		std::fs::write(dir.path().join("a.txt"), b"data").unwrap();
		assert_eq!(local_existing(dir.path()).unwrap(), Existing::Dir);
		assert_eq!(
			local_existing(&dir.path().join("a.txt")).unwrap(),
			Existing::File
		);
		assert_eq!(
			local_existing(&dir.path().join("missing")).unwrap(),
			Existing::Nothing
		);
	}

	#[test]
	fn a_plain_name_is_downloaded_into_the_destination() {
		assert_eq!(
			download_target(Path::new("/dest"), "notes.txt"),
			Some(Path::new("/dest/notes.txt").to_path_buf())
		);
	}

	#[test]
	fn a_parent_reference_is_refused() {
		assert_eq!(download_target(Path::new("/dest"), ".."), None);
		assert_eq!(download_target(Path::new("/dest"), "../x"), None);
	}

	#[test]
	fn an_absolute_name_is_refused() {
		assert_eq!(download_target(Path::new("/dest"), "/etc/x"), None);
	}

	#[test]
	fn a_name_holding_a_separator_is_refused() {
		assert_eq!(download_target(Path::new("/dest"), "a/b"), None);
	}
}

#[cfg(all(test, unix))]
mod upload_name_tests {
	use std::os::unix::fs::symlink;

	use super::upload_name;

	#[test]
	fn a_symlinked_file_keeps_the_links_name() {
		let dir = tempfile::tempdir().unwrap();
		std::fs::write(dir.path().join("target.txt"), b"data").unwrap();
		symlink(dir.path().join("target.txt"), dir.path().join("link.txt")).unwrap();
		assert_eq!(
			upload_name(&dir.path().join("link.txt")).unwrap(),
			"link.txt"
		);
	}

	#[test]
	fn a_symlinked_directory_keeps_the_links_name() {
		let dir = tempfile::tempdir().unwrap();
		std::fs::create_dir(dir.path().join("target")).unwrap();
		symlink(dir.path().join("target"), dir.path().join("link")).unwrap();
		assert_eq!(upload_name(&dir.path().join("link")).unwrap(), "link");
	}

	#[test]
	fn a_path_ending_in_a_parent_reference_takes_the_resolved_directorys_name() {
		let dir = tempfile::tempdir().unwrap();
		std::fs::create_dir_all(dir.path().join("outer").join("inner")).unwrap();
		assert_eq!(
			upload_name(&dir.path().join("outer").join("inner").join("..")).unwrap(),
			"outer"
		);
	}
}
