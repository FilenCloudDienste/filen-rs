//! [cli-doc] transfers
//! Files and directories are transferred between your computer and your Filen drive with
//! `upload` and `download`. Directories are always transferred recursively.
//!
//! Both commands place the transferred item *inside* the destination directory, keeping its name
//! (like `cp`): `upload ./notes /backups` creates `/backups/notes`, and `download /backups/notes .`
//! creates `./notes`. Downloading `/` writes the whole drive's contents into the destination.
//!
//! When a single item inside a directory transfer fails, the rest of the transfer still completes
//! and the failures are listed at the end. Press Ctrl+C to cancel a running transfer.

// AI: written by Claude Code 11/08/2026

use std::{
	borrow::Cow,
	path::{Path, PathBuf},
	sync::{Arc, Mutex},
	time::{Duration, Instant},
};

use anyhow::{Context as _, Result};
use console::{Term, style};
use filen_sdk_rs::{
	Error,
	auth::Client,
	fs::{
		HasName as _,
		categories::{DirType, NonRootFileType, NonRootItemType, Normal},
		file::traits::HasFileInfo as _,
	},
	io::{
		CategoryDirDownloadExtPub as _, DirDownloadCallback, DirUploadCallback, RemoteDirectory,
		RemoteFile, client_impl::IoSharedClientExt as _,
	},
	util::MaybeSendCallback,
};
use tokio::select;
use unicode_width::{UnicodeWidthChar as _, UnicodeWidthStr as _};

use crate::{
	auth::LazyClient,
	ui::{self, UI},
	util::RemotePath,
};

/// Upload a local file, or a local directory and everything below it, into a remote directory.
pub(crate) async fn upload(
	ui: &mut UI,
	client: &mut LazyClient,
	working_path: &RemotePath,
	source_str: &str,
	destination_str: Option<&str>,
) -> Result<()> {
	// Canonicalizing both checks that the source exists and gives it a real file name, which
	// paths like "." or "some/dir/.." don't have.
	let source = std::fs::canonicalize(source_str)
		.map_err(|_| UI::failure(&format!("No such local file or directory: {}", source_str)))?;
	let name = source
		.file_name()
		.and_then(|name| name.to_str())
		.ok_or_else(|| UI::failure(&format!("Cannot upload {}", source.display())))?
		.to_string();

	let destination_path = working_path.navigate(destination_str.unwrap_or(""));
	let client = client.get(ui).await?.clone();
	let destination_dir = match client
		.find_item_at_path(&destination_path.0)
		.await
		.context("Failed to find directory")?
	{
		Some(NonRootFileType::Dir(dir)) => Ok(DirType::Dir(dir)),
		Some(NonRootFileType::Root(root)) => Ok(DirType::Root(root)),
		Some(_) => Err(UI::failure(&format!(
			"Not a directory: {}",
			destination_path
		))),
		None => Err(UI::failure(&format!(
			"No such directory: {}",
			destination_path
		))),
	}?;

	let progress = TransferProgress::new(TransferKind::Upload, ui);
	let transferred = if source.is_dir() {
		run_cancellable(
			&progress,
			upload_directory(&client, &progress, &source, &name, &destination_dir),
		)
		.await?
	} else {
		run_cancellable(
			&progress,
			upload_file(&client, &progress, &source, &destination_dir),
		)
		.await?
	};
	transferred.context("Failed to upload")?;

	finish(
		ui,
		&progress,
		"upload",
		&format!(
			"Uploaded {} to {}",
			source.display(),
			destination_path.navigate(&name)
		),
	)
}

/// Download a remote file, or a remote directory and everything below it, into a local directory.
pub(crate) async fn download(
	ui: &mut UI,
	client: &mut LazyClient,
	working_path: &RemotePath,
	source_str: &str,
	destination_str: Option<&str>,
) -> Result<()> {
	let destination_str = destination_str.unwrap_or(".");
	let destination = std::fs::canonicalize(destination_str)
		.map_err(|_| UI::failure(&format!("No such local directory: {}", destination_str)))?;
	if !destination.is_dir() {
		return Err(UI::failure(&format!(
			"Not a directory: {}",
			destination.display()
		)));
	}

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
	let target = match &item {
		NonRootFileType::File(file) => {
			destination.join(file.name().context("Failed to decrypt file name")?)
		}
		NonRootFileType::Dir(dir) => {
			destination.join(dir.name().context("Failed to decrypt directory name")?)
		}
		// The drive root has no name of its own, so its contents go straight into the destination.
		NonRootFileType::Root(_) => destination.clone(),
	};

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
		"download",
		&format!("Downloaded {} to {}", source_path, target.display()),
	)
}

/// Await `transfer` until it completes or the user hits Ctrl+C, erasing the progress line either
/// way. Cancelling drops the transfer future, which is how the SDK aborts the workers it spawned.
async fn run_cancellable<T>(
	progress: &TransferProgress,
	transfer: impl Future<Output = Result<T>>,
) -> Result<Result<T>> {
	let mut stop_rx = crate::CTRLC_TX.subscribe();
	let outcome = select! {
		_ = stop_rx.recv() => None,
		result = transfer => Some(result),
	};
	progress.clear();
	outcome.ok_or_else(|| UI::failure("Canceled"))
}

/// Print the per-item failures the SDK reported through the progress callbacks, then either the
/// success message or a summary failure. Directory transfers return `Ok` even when individual
/// items failed, so without this the command would claim success while items were missing.
fn finish(
	ui: &mut UI,
	progress: &TransferProgress,
	action: &str,
	success_message: &str,
) -> Result<()> {
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
	if errors.is_empty() {
		ui.print_success(success_message);
		Ok(())
	} else {
		Err(UI::failure(&format!(
			"{} item(s) failed to {}",
			errors.len(),
			action
		)))
	}
}

async fn upload_file(
	client: &Client,
	progress: &TransferProgress,
	source: &Path,
	destination_dir: &DirType<'_, Normal>,
) -> Result<()> {
	let size = std::fs::metadata(source)
		.with_context(|| format!("Failed to read {}", source.display()))?
		.len();
	progress.start_single_item(size);
	client
		.upload_file_from_path(
			destination_dir,
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
		.download_file_to_path(source, target, Some(progress.byte_callback()))
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
}

#[derive(Default)]
struct ProgressState {
	/// While scanning, the totals are still growing and no bytes have moved yet.
	scanning: bool,
	total_files: u64,
	total_bytes: u64,
	done_files: u64,
	done_bytes: u64,
	last_render: Option<Instant>,
	/// Whether there is a progress line on screen that still needs erasing.
	line_shown: bool,
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
		self.render(&mut state, true);
	}

	fn advance(&self, files: u64, bytes: u64) {
		let mut state = self.state.lock().unwrap();
		state.done_files += files;
		state.done_bytes += bytes;
		self.render(&mut state, false);
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
		let line = if state.scanning {
			format!(
				"Scanning... {} in {} files",
				ui::format_size(state.total_bytes),
				state.total_files
			)
		} else if state.total_bytes > 0 {
			format!(
				"{} {} / {} ({}%) - {}/{} files",
				self.kind.verb(),
				ui::format_size(state.done_bytes),
				ui::format_size(state.total_bytes),
				(state.done_bytes * 100 / state.total_bytes).min(100),
				state.done_files,
				state.total_files
			)
		} else {
			format!(
				"{} {}/{} files",
				self.kind.verb(),
				state.done_files,
				state.total_files
			)
		};
		let line = truncate_to_width(&line, self.term.size().1 as usize);
		let _ = self.term.clear_line();
		let _ = self.term.write_str(&style(line).dim().to_string());
		let _ = self.term.flush();
		state.line_shown = true;
	}
}

/// Cut `line` down to `width` display columns. An over-long line would wrap onto a second row,
/// and the in-place redraw only ever clears one row.
fn truncate_to_width(line: &str, width: usize) -> String {
	if line.width() <= width {
		return line.to_string();
	}
	if width == 0 {
		return String::new();
	}
	let mut truncated = String::new();
	let mut used = 0;
	for c in line.chars() {
		let char_width = c.width().unwrap_or(0);
		if used + char_width > width - 1 {
			break;
		}
		truncated.push(c);
		used += char_width;
	}
	truncated.push('…');
	truncated
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
	use super::*;

	#[test]
	fn truncate_to_width_keeps_short_lines() {
		assert_eq!(truncate_to_width("Uploading", 20), "Uploading");
		assert_eq!(truncate_to_width("Uploading", 9), "Uploading");
	}

	#[test]
	fn truncate_to_width_cuts_long_lines() {
		let truncated = truncate_to_width("Uploading 1 MiB / 2 MiB", 10);
		assert_eq!(truncated, "Uploading…");
		assert_eq!(truncated.width(), 10);
	}

	#[test]
	fn truncate_to_width_handles_wide_chars() {
		// Each of these is two columns wide, so only two of them fit alongside the ellipsis.
		let truncated = truncate_to_width("上传上传上传", 5);
		assert_eq!(truncated, "上传…");
		assert!(truncated.width() <= 5);
	}

	#[test]
	fn truncate_to_width_zero_is_empty() {
		assert_eq!(truncate_to_width("Uploading", 0), "");
	}
}
