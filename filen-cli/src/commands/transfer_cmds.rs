//! [cli-doc] transfers
//! Files and directories are transferred between your computer and your Filen drive with
//! `upload` and `download`. Directories are always transferred recursively.
//!
//! Like `cp`, both commands place the transferred item *inside* the destination when it is an
//! existing directory or ends in `/`, keeping its name: `upload ./notes /backups` creates
//! `/backups/notes`, and `download /backups/notes .` creates `./notes`. Any other destination is
//! the transferred item's new path: `upload ./notes.txt /backups/old.txt` uploads it as `old.txt`,
//! and `download /backups/notes ./old-notes` creates `./old-notes`. Missing directories on the way
//! to the destination are created, in the drive and locally. Downloading `/` writes the whole
//! drive's contents into the destination.
//!
//! Uploading a file where the drive already holds a file of the same name replaces it: the upload
//! becomes the current file, and the one it replaced is kept as an earlier version when file
//! versioning is enabled for your account. Uploading a directory onto an existing one adds its
//! contents to it, replacing same-named files the same way. A downloaded file replaces a local
//! file at its path, except when the local file already has the same contents, which is then left
//! as it is. In a directory download, a local file of the same size is also left as it is when
//! the drive holds no hash of the file's contents and the local file is not older. Uploads keep
//! each file's modification and creation times, and downloads set the modification time on the
//! local file (and the creation time on macOS and Windows).
//!
//! With `--skip-existing`, `upload` leaves out each file the drive already holds a file of the same
//! name for (the drive compares names case-insensitively), and with `--update` it leaves one out
//! unless its size differs or it was modified after the drive's copy. With `--no-clobber`,
//! `download` never replaces a local file. The files left out are counted in the result and listed
//! with `-v`, and so are the files a directory download leaves as they are for already holding
//! the same contents.
//!
//! With `--include` and `--exclude`, a directory transfer takes only some of the items inside the
//! directory. Both take a gitignore-style pattern, can be given several times, and match the path
//! of an item relative to the transferred directory, case-insensitively: `*.jpg` matches every
//! JPEG file, `photos/` every directory named `photos`, and `docs/*.md` the Markdown files
//! directly inside `docs` at the top. With `--include`, only the files a pattern matches are
//! transferred, along with everything inside a directory a pattern matches and the directories on
//! the way to them; an excluded item is left out, a directory with everything inside it, even
//! where an include pattern matches it. A file named as the item to transfer is transferred
//! whatever the patterns say.
//!
//! Like `cp`, both commands take several items at once, followed by a destination that is then
//! always a directory, created when missing: `upload a.txt b.txt ./photos /backups` uploads all
//! three into `/backups`. An item that fails is listed at the end while the others are still
//! transferred, and the result adds them all up. A shell expands a wildcard such as `*.jpg` into
//! such a list of items; the Windows command prompt does not, and remote paths take no wildcards.
//!
//! With `-` in place of the local path, `upload` reads stdin and `download` writes stdout, as in
//! `tar -cz notes | filen upload - /backups/notes.tar.gz` and
//! `filen download /backups/notes.tar.gz - | tar -xz`. The upload's destination then has to name
//! the remote file, and only a single file can be downloaded to stdout, which carries nothing but
//! the file: there is no progress line, and the result goes to stderr. `-` works only for a single
//! command, not in the REPL or among commands piped into the CLI, where stdin carries the commands.
//!
//! An uploaded symlink is uploaded under its own name with the contents it points to. Inside an
//! uploaded directory, symlinks are followed the same way; with `--no-follow-symlinks` they are
//! skipped instead (neither the link nor what it points to is uploaded): listed with `-v`, counted
//! in the result without it, and listed in the `--json` result's `skipped_symlinks`.
//!
//! While a transfer runs, its progress line shows the current speed and the time left, and the
//! result ends with the number of files and bytes transferred, the time taken and the average
//! speed. With `-v`, each file of a directory transfer is listed as it completes. The global
//! flags `--concurrency`, `--upload-bandwidth-kbps` and `--download-bandwidth-kbps` limit the
//! concurrent connections and cap the upload and download bandwidth, in kilobytes per second.
//!
//! When a single item inside a directory transfer fails, the rest of the transfer still completes,
//! the failures are listed at the end, and the command exits with a nonzero status. With
//! `--json`, the result is printed as one object instead: `{"destination": ..., "files": ...,
//! "bytes": ..., "skipped": ..., "skipped_symlinks": [...], "failed": [{"path": ...,
//! "error": ...}]}`, where `path` is `null` for a failure not about a single item. Press Ctrl+C
//! to cancel a running transfer: what was transferred by then is reported on stderr and the
//! command exits with status 130, where the TypeScript CLI printed "Aborted".

use std::{
	borrow::Cow,
	collections::{HashMap, VecDeque},
	fmt,
	path::{Path, PathBuf},
	pin::{Pin, pin},
	sync::{Arc, Mutex},
	task::{Context, Poll, ready},
	time::{Duration, Instant},
};

use anyhow::{Context as _, Result};
use clap::{Args, Subcommand};
use clap_complete::engine::{ArgValueCompleter, PathCompleter};
use console::{Term, style};
use filen_sdk_rs::{
	Error, ErrorKind,
	auth::Client,
	fs::{
		HasName as _, HasParent as _, HasUUID as _,
		categories::{DirType, NonRootFileType, NonRootItemType, Normal},
		file::{FileBuilderOptionalName, traits::HasFileInfo as _},
		name::ValidatedName,
	},
	io::{
		CategoryDirDownloadExtPub as _, DirDownloadCallback, DirDownloadOptions, DirUploadCallback,
		DirUploadOptions, ExistingLocalFile, ExistingRemoteFile, PathMatcher, RemoteDirectory,
		RemoteFile, SymlinkPolicy, TransferFilter, client_impl::IoSharedClientExt as _,
		is_safe_descendant,
	},
	sync_engine::PathPatterns,
	util::MaybeSendCallback,
};
use filen_types::fs::{ParentUuid, Uuid};
use serde_json::json;
use tokio::{
	io::{AsyncRead as _, AsyncWrite as _, ReadBuf},
	select,
};

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
		/// Local file or directory to upload, or "-" to upload stdin as the file the destination
		/// names
		#[arg(add = ArgValueCompleter::new(PathCompleter::any()))]
		source: String,
		/// More local files or directories to upload, followed by the destination: the directory in
		/// the Filen drive to upload into, or, after a single item, that item's new path when it is
		/// not an existing directory and does not end in "/"; missing directories are created
		/// (default: the current working directory)
		#[arg(value_name = "PATH", add = FilenCompleter::file_or_directory())]
		paths: Vec<String>,
		/// Skip the symlinks inside an uploaded directory instead of uploading what they point to
		/// (counted in the result, listed with -v)
		#[arg(long)]
		no_follow_symlinks: bool,
		/// Skip each file the drive already holds a file of the same name for, instead of
		/// replacing it (listed with -v)
		#[arg(long, conflicts_with = "update")]
		skip_existing: bool,
		/// Replace a file the drive already holds a file of the same name for only when its size
		/// differs or it was modified since, and skip it otherwise (listed with -v)
		#[arg(long)]
		update: bool,
		#[command(flatten)]
		filter: FilterArgs,
	},
	/// Download a file or directory (recursively) from the Filen drive
	Download {
		/// File or directory to download ("/" for the entire Filen drive)
		#[arg(add = FilenCompleter::file_or_directory())]
		source: String,
		/// More files or directories to download, followed by the destination: the local directory
		/// to download into, or, after a single item, that item's new path when it is not an
		/// existing directory and does not end in a path separator; missing directories are
		/// created (default: the current local directory); "-" writes a single file to stdout
		#[arg(value_name = "PATH", add = ArgValueCompleter::new(PathCompleter::any()))]
		paths: Vec<String>,
		/// Never replace a local file: skip each file whose local path is taken (listed with -v)
		#[arg(long)]
		no_clobber: bool,
		#[command(flatten)]
		filter: FilterArgs,
	},
}

/// The flags choosing which items inside a transferred directory are transferred.
#[derive(Debug, Args)]
pub(crate) struct FilterArgs {
	/// Transfer only the files inside a directory that match this gitignore-style pattern, along
	/// with the directories on the way to them and everything inside a directory it matches
	/// (repeatable)
	#[arg(long, value_name = "PATTERN")]
	include: Vec<String>,
	/// Leave out the items inside a directory that match this gitignore-style pattern, a directory
	/// with everything inside it, even where --include matches them (repeatable)
	#[arg(long, value_name = "PATTERN")]
	exclude: Vec<String>,
}

impl FilterArgs {
	/// The filter the flags ask for, or a usage error naming a pattern that does not compile.
	fn transfer_filter(&self) -> Result<TransferFilter> {
		Ok(TransferFilter {
			include: path_patterns(&self.include)?,
			exclude: path_patterns(&self.exclude)?,
		})
	}
}

/// `patterns` compiled, or `None` when there are none.
fn path_patterns(patterns: &[String]) -> Result<Option<Arc<dyn PathMatcher>>> {
	if patterns.is_empty() {
		return Ok(None);
	}
	let patterns = PathPatterns::parse(patterns.iter().map(String::as_str))
		.map_err(|e| UI::usage_failure(&e.to_string()))?;
	Ok(Some(Arc::new(patterns)))
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
				paths,
				no_follow_symlinks,
				skip_existing,
				update,
				filter,
			} => {
				let options = DirUploadOptions {
					symlinks: if no_follow_symlinks {
						SymlinkPolicy::Skip
					} else {
						SymlinkPolicy::Follow
					},
					existing: existing_remote_policy(skip_existing, update),
					filter: filter.transfer_filter()?,
				};
				let (sources, destination) = sources_and_destination(source, paths);
				upload(
					ui,
					client,
					working_path,
					&sources,
					destination.as_deref(),
					options,
				)
				.await?
			}
			Self::Download {
				source,
				paths,
				no_clobber,
				filter,
			} => {
				let options = DirDownloadOptions {
					existing: if no_clobber {
						ExistingLocalFile::Keep
					} else {
						ExistingLocalFile::ReplaceUnlessSame
					},
					filter: filter.transfer_filter()?,
				};
				let (sources, destination) = sources_and_destination(source, paths);
				download(
					ui,
					client,
					working_path,
					&sources,
					destination.as_deref(),
					options,
				)
				.await?
			}
		}
		Ok(CommandResult::default())
	}
}

/// What `upload` does with a file the drive already holds one of the same name for, from its
/// `--skip-existing` and `--update` flags, which clap keeps from being given together.
fn existing_remote_policy(skip_existing: bool, update: bool) -> ExistingRemoteFile {
	if skip_existing {
		ExistingRemoteFile::Keep
	} else if update {
		ExistingRemoteFile::ReplaceIfChanged
	} else {
		ExistingRemoteFile::Replace
	}
}

/// The items a transfer takes and its destination, from the paths after the first item: as with
/// `cp`, the last of two or more paths is the destination.
fn sources_and_destination(
	source: String,
	mut paths: Vec<String>,
) -> (Vec<String>, Option<String>) {
	let destination = paths.pop();
	let mut sources = Vec::with_capacity(paths.len() + 1);
	sources.push(source);
	sources.extend(paths);
	(sources, destination)
}

/// Whether `error` ends the whole command rather than only the transfer of the item it came from.
fn ends_the_command(error: &anyhow::Error) -> bool {
	matches!(
		error.downcast_ref::<CliError>(),
		Some(CliError::Interrupted | CliError::StdoutClosed)
	)
}

/// The failure of one of several items, named as given.
fn source_failure(source: &str, error: &anyhow::Error) -> TransferFailure {
	TransferFailure {
		path: Some(source.to_owned()),
		error: format!("{:#}", error),
	}
}

/// A local item to upload.
struct LocalSource {
	/// Made absolute without resolving symlinks, so an uploaded link keeps its own name.
	path: PathBuf,
	is_dir: bool,
	name: String,
}

impl LocalSource {
	fn read(source_str: &str) -> Result<Self> {
		let path = std::path::absolute(source_str).map_err(|_| {
			UI::failure(&format!("No such local file or directory: {}", source_str))
		})?;
		std::fs::symlink_metadata(&path).map_err(|_| {
			UI::failure(&format!("No such local file or directory: {}", source_str))
		})?;
		let is_dir = std::fs::metadata(&path)
			.map_err(|e| UI::failure(&format!("Cannot read {}: {}", path.display(), e)))?
			.is_dir();
		let name = upload_name(&path)?;
		Ok(Self { path, is_dir, name })
	}
}

/// Upload local files, or local directories and everything below them, into a remote directory,
/// as `options` say; each source itself is followed when it is a symlink. Several sources go into
/// `destination_str` as a directory, and one that fails is reported among the failures while the
/// others are still uploaded.
pub(crate) async fn upload(
	ui: &mut UI,
	client: &mut LazyClient,
	working_path: &RemotePath,
	sources: &[String],
	destination_str: Option<&str>,
	options: DirUploadOptions,
) -> Result<()> {
	let several = sources.len() > 1;
	if sources.iter().any(|source| source == STDIO) {
		ensure_stdio_is_free(ui)?;
		if several {
			return Err(UI::usage_failure(
				"Cannot upload stdin (\"-\") together with other items",
			));
		}
		return upload_stdin(ui, client, working_path, destination_str, options.existing).await;
	}
	// A single source that cannot be read fails before anything is asked of the drive.
	let mut locals = Vec::with_capacity(sources.len());
	for source in sources {
		match LocalSource::read(source) {
			Err(e) if !several => return Err(e),
			local => locals.push((source, local)),
		}
	}

	// Without a destination, upload into the working directory.
	let into = several || destination_str.is_none_or(|destination| destination.ends_with('/'));
	let destination_path = working_path.navigate(destination_str.unwrap_or(""));
	let client = client.get(ui).await?.clone();
	if several && let RemoteItem::File = find_remote(&client, &destination_path).await? {
		return Err(UI::failure(&format!(
			"Not a directory: {}",
			destination_path
		)));
	}

	let progress = TransferProgress::new(TransferKind::Upload, ui);
	let mut uploaded = None;
	for (source, local) in locals {
		let result = match local {
			Ok(local) => {
				upload_source(
					ui,
					&client,
					&progress,
					local,
					&destination_path,
					into,
					&options,
				)
				.await
			}
			Err(e) => Err(e),
		};
		match result {
			Ok(done) => uploaded = Some(done),
			Err(e) if several && !ends_the_command(&e) => {
				progress.push_failures([source_failure(source, &e)]);
			}
			Err(e) => return Err(e),
		}
	}

	let (subject, destination) = if several {
		(
			format!("{} items to {}", sources.len(), destination_path),
			destination_path.to_string(),
		)
	} else {
		let (source, target) = uploaded
			.expect("a single upload that did not fail has a target (should be impossible)");
		(format!("{} to {}", source, target), target)
	};
	finish(ui, &progress, &subject, &destination)
}

/// Upload `local` to `destination_path`, inside it when `into` says so, returning the source and
/// the remote path it was uploaded to.
async fn upload_source(
	ui: &mut UI,
	client: &Arc<Client>,
	progress: &TransferProgress,
	local: LocalSource,
	destination_path: &RemotePath,
	into: bool,
	options: &DirUploadOptions,
) -> Result<(String, String)> {
	let destination = find_remote(client, destination_path).await?;
	let target = resolve_upload_target(
		destination_path,
		into,
		destination.existing(),
		&local.name,
		local.is_dir,
	)?;
	let parent_dir = match destination {
		RemoteItem::Dir(dir) if &target.parent == destination_path => dir,
		_ => find_or_create_remote_dir(client, &target.parent).await?,
	};

	let target_path = target.parent.navigate(target.name.as_ref());

	let transferred = if local.is_dir {
		run_cancellable(
			ui,
			progress,
			upload_directory(
				client,
				progress,
				&local.path,
				target.name.as_ref(),
				&target_path,
				&parent_dir,
				options.clone(),
			),
		)
		.await?
	} else {
		run_cancellable(
			ui,
			progress,
			upload_file(
				client,
				progress,
				&local.path,
				target.name.as_ref(),
				&parent_dir,
				options.existing,
			),
		)
		.await?
	};
	transferred.context("Failed to upload")?;
	Ok((local.path.display().to_string(), target_path.to_string()))
}

/// The path standing for stdin as an upload's source and for stdout as a download's destination.
const STDIO: &str = "-";

/// Refuses [`STDIO`] where stdin carries the CLI's commands, in the REPL and for commands piped
/// into it: an upload would read the next commands as the file, and a download would write the
/// file among the commands' output.
fn ensure_stdio_is_free(ui: &UI) -> Result<()> {
	if ui.commands_on_stdin() {
		return Err(UI::usage_failure(
			"\"-\" (stdin or stdout) only works in a single command, such as `filen upload - \
			 /notes/today.txt`, not in the REPL or in commands piped into the CLI",
		));
	}
	Ok(())
}

/// Upload what is read from stdin as the remote file at `destination_str`, replacing a file there
/// unless `existing` keeps it.
async fn upload_stdin(
	ui: &mut UI,
	client: &mut LazyClient,
	working_path: &RemotePath,
	destination_str: Option<&str>,
	existing: ExistingRemoteFile,
) -> Result<()> {
	let destination_str = stdin_destination(destination_str)?;
	match existing {
		ExistingRemoteFile::Replace | ExistingRemoteFile::Keep => {}
		ExistingRemoteFile::ReplaceIfChanged => {
			return Err(UI::usage_failure(
				"--update cannot compare stdin with the drive's file: use --skip-existing",
			));
		}
	}
	let destination_path = working_path.navigate(destination_str);
	let client = client.get(ui).await?.clone();
	let destination = find_remote(&client, &destination_path).await?;
	let target = stdin_upload_target(&destination_path, destination.existing())?;
	let parent_dir = match destination {
		RemoteItem::Dir(dir) => dir,
		RemoteItem::File | RemoteItem::Nothing => {
			find_or_create_remote_dir(&client, &target.parent).await?
		}
	};
	let target_path = target.parent.navigate(target.name.as_ref());

	let progress = TransferProgress::new(TransferKind::Upload, ui);
	run_cancellable(
		ui,
		&progress,
		upload_reader(
			&client,
			&progress,
			target.name.as_ref(),
			&parent_dir,
			existing,
		),
	)
	.await?
	.context("Failed to upload")?;
	let destination = target_path.to_string();
	finish(
		ui,
		&progress,
		&format!("stdin to {}", destination),
		&destination,
	)
}

/// The remote path given for an upload from stdin, which has to name the file to upload as.
fn stdin_destination(destination_str: Option<&str>) -> Result<&str> {
	match destination_str {
		Some(destination) if !destination.ends_with('/') => Ok(destination),
		_ => Err(UI::usage_failure(
			"Uploading stdin (\"-\") needs the remote file's path, such as /notes/today.txt",
		)),
	}
}

/// Where an upload from stdin to `destination` goes: `destination` is the file's path, which may
/// replace an existing file but not a directory.
fn stdin_upload_target(destination: &RemotePath, existing: Existing) -> Result<UploadTarget> {
	match existing {
		Existing::Dir => Err(UI::failure(&format!(
			"Cannot upload stdin as the directory {}: give the file's path",
			destination
		))),
		Existing::File | Existing::Nothing => {
			resolve_upload_target(destination, false, existing, "", false)
		}
	}
}

/// Upload stdin as `name` into `destination_dir`, unless `existing` keeps a file of that name
/// already there.
async fn upload_reader(
	client: &Client,
	progress: &TransferProgress,
	name: &str,
	destination_dir: &DirType<'_, Normal>,
	existing: ExistingRemoteFile,
) -> Result<()> {
	let builder = client
		.make_file_builder(name, destination_dir.uuid())
		.map_err(|e| UI::failure(&format!("Cannot upload as {:?}: {}", name, e)))?;
	progress.start_single_item(0);
	if existing == ExistingRemoteFile::Keep
		&& same_named_file(client, destination_dir, name)
			.await?
			.is_some()
	{
		progress.skip([STDIO.to_owned()], 0);
		return Ok(());
	}
	let mut stdin = StdinReader(tokio::io::stdin());
	let file = client
		.upload_file_from_reader(builder, &mut stdin, Some(progress.byte_callback()), None)
		.await
		.context("Failed to upload file")?;
	progress.complete_single_item_of(file.size());
	Ok(())
}

/// Download the remote file at `source_str` to stdout. Nothing else is written there: the result
/// goes to stderr, and there is no progress line.
async fn download_to_stdout(
	ui: &mut UI,
	client: &mut LazyClient,
	working_path: &RemotePath,
	source_str: &str,
) -> Result<()> {
	if ui.json {
		return Err(UI::usage_failure(
			"--json cannot be used when downloading to stdout (\"-\")",
		));
	}
	let source_path = working_path.navigate(source_str);
	let client = client.get(ui).await?.clone();
	let file = match client
		.find_item_at_path(&source_path.0)
		.await
		.context("Failed to find file or directory")?
	{
		Some(NonRootFileType::File(file)) => file,
		Some(NonRootFileType::Dir(_) | NonRootFileType::Root(_)) => {
			return Err(UI::failure(&format!(
				"Cannot download the directory {} to stdout",
				source_path
			)));
		}
		None => {
			return Err(UI::failure(&format!(
				"No such file or directory: {}",
				source_path
			)));
		}
	};

	let progress = TransferProgress::new(TransferKind::Download, ui).without_progress_line();
	let streaming = interrupt::stream_data();
	let downloaded = run_cancellable(ui, &progress, async {
		progress.start_single_item(file.size());
		let mut stdout = StdoutWriter(tokio::io::stdout());
		client
			.download_file_to_writer(file.as_ref(), &mut stdout, Some(progress.byte_callback()))
			.await?;
		progress.complete_single_item();
		Ok(())
	})
	.await?;
	drop(streaming);
	if let Err(e) = downloaded {
		// What was cut short is of no use to a reader that closed stdout.
		if interrupt::stdout_closed() {
			return Err(CliError::StdoutClosed.into());
		}
		return Err(e.context("Failed to download"));
	}
	if !ui.is_quiet() {
		let (files, bytes) = progress.done();
		let summary = transfer_summary(files, bytes, 0, progress.started.elapsed());
		ui.print_muted_to_stderr(&format!(
			"Downloaded {} to stdout: {}",
			source_path, summary
		));
	}
	Ok(())
}

/// Stdin, as the reader an upload takes.
///
/// Tokio reads stdin on a blocking thread, and a read waiting for input cannot be cancelled. That
/// does not hold up a canceled upload: Ctrl+C ends the CLI with `process::exit`, which does not
/// wait for the runtime's threads, and stdin is only read for a single command, never in the REPL.
struct StdinReader(tokio::io::Stdin);

impl futures::AsyncRead for StdinReader {
	fn poll_read(
		self: Pin<&mut Self>,
		cx: &mut Context<'_>,
		buf: &mut [u8],
	) -> Poll<std::io::Result<usize>> {
		let mut read = ReadBuf::new(buf);
		ready!(Pin::new(&mut self.get_mut().0).poll_read(cx, &mut read))?;
		Poll::Ready(Ok(read.filled().len()))
	}
}

/// Stdout, as the writer a download takes. A reader that closed stdout ends the command, as it
/// does for printed output ([`ui::write_stdout_line`]).
struct StdoutWriter(tokio::io::Stdout);

impl StdoutWriter {
	fn note<T>(result: std::io::Result<T>) -> std::io::Result<T> {
		if let Err(e) = &result
			&& e.kind() == std::io::ErrorKind::BrokenPipe
		{
			interrupt::close_stdout();
		}
		result
	}
}

impl futures::AsyncWrite for StdoutWriter {
	fn poll_write(
		self: Pin<&mut Self>,
		cx: &mut Context<'_>,
		buf: &[u8],
	) -> Poll<std::io::Result<usize>> {
		Pin::new(&mut self.get_mut().0)
			.poll_write(cx, buf)
			.map(Self::note)
	}

	fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
		Pin::new(&mut self.get_mut().0)
			.poll_flush(cx)
			.map(Self::note)
	}

	fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
		Pin::new(&mut self.get_mut().0)
			.poll_shutdown(cx)
			.map(Self::note)
	}
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

/// Where a download goes.
struct DownloadDestination<'a> {
	path: &'a Path,
	/// Whether the item goes inside `path` under its own name, as with a trailing separator.
	into: bool,
	/// What `path` named before the download.
	existing: Existing,
}

/// Download remote files, or remote directories and everything below them, to a local path, as
/// `options` say. Several sources go into `destination_str` as a directory, and one that fails is
/// reported among the failures while the others are still downloaded.
pub(crate) async fn download(
	ui: &mut UI,
	client: &mut LazyClient,
	working_path: &RemotePath,
	sources: &[String],
	destination_str: Option<&str>,
	options: DirDownloadOptions,
) -> Result<()> {
	let several = sources.len() > 1;
	if destination_str == Some(STDIO) {
		ensure_stdio_is_free(ui)?;
		let [source] = sources else {
			return Err(UI::usage_failure(
				"Only a single file can be downloaded to stdout (\"-\")",
			));
		};
		return download_to_stdout(ui, client, working_path, source).await;
	}
	let destination_str = destination_str.unwrap_or(".");
	let into = several || destination_str.ends_with(std::path::is_separator);
	let existing = local_existing(Path::new(destination_str))?;
	if several && existing == Existing::File {
		return Err(UI::failure(&format!(
			"Not a directory: {}",
			destination_str
		)));
	}
	let destination_path = match existing {
		Existing::Dir | Existing::File => std::fs::canonicalize(destination_str),
		Existing::Nothing => std::path::absolute(destination_str),
	}
	.map_err(|e| UI::failure(&format!("Cannot read {}: {}", destination_str, e)))?;
	let destination = DownloadDestination {
		path: &destination_path,
		into,
		existing,
	};

	let client = client.get(ui).await?.clone();
	let progress = TransferProgress::new(TransferKind::Download, ui);
	let mut downloaded = None;
	for source in sources {
		let source_path = working_path.navigate(source);
		match download_source(ui, &client, &progress, &source_path, &destination, &options).await {
			Ok(target) => downloaded = Some((source_path, target)),
			Err(e) if several && !ends_the_command(&e) => {
				progress.push_failures([source_failure(source, &e)]);
			}
			Err(e) => return Err(e),
		}
	}

	let (subject, destination) = if several {
		let destination = destination_path.display().to_string();
		(
			format!("{} items to {}", sources.len(), destination),
			destination,
		)
	} else {
		let (source_path, target) = downloaded
			.expect("a single download that did not fail has a target (should be impossible)");
		(format!("{} to {}", source_path, target), target)
	};
	finish(ui, &progress, &subject, &destination)
}

/// Download the item at `source_path` to `destination`, returning the local path it was
/// downloaded to.
async fn download_source(
	ui: &mut UI,
	client: &Arc<Client>,
	progress: &TransferProgress,
	source_path: &RemotePath,
	destination: &DownloadDestination<'_>,
	options: &DirDownloadOptions,
) -> Result<String> {
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
	let target = resolve_download_target(
		destination.path,
		destination.into,
		destination.existing,
		source,
		source_path,
	)?;
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

	let transferred = match &item {
		NonRootFileType::File(file) => {
			run_cancellable(
				ui,
				progress,
				download_file(client, progress, file.as_ref(), &target, options.existing),
			)
			.await?
		}
		NonRootFileType::Dir(dir) => {
			let dir = DirType::Dir(Cow::Owned(dir.as_ref().clone()));
			run_cancellable(
				ui,
				progress,
				download_directory(client, progress, dir, &target, options.clone()),
			)
			.await?
		}
		NonRootFileType::Root(root) => {
			let root = DirType::Root(Cow::Owned(root.as_ref().clone()));
			run_cancellable(
				ui,
				progress,
				download_directory(client, progress, root, &target, options.clone()),
			)
			.await?
		}
	};
	transferred.context("Failed to download")?;
	Ok(target.display().to_string())
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
/// way. Cancelling drops the transfer future, which is how the SDK aborts the workers it spawned,
/// and reports on stderr what had been transferred by then.
async fn run_cancellable<T>(
	ui: &mut UI,
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
	match outcome {
		Some(result) => Ok(result),
		// Nobody reads a report on what a closed stdout cut short.
		None if interrupt::stdout_closed() => Err(CliError::StdoutClosed.into()),
		None => {
			let (files, bytes) = progress.done();
			ui.print_muted_to_stderr(&canceled_message(progress.kind, files, bytes));
			Err(CliError::Interrupted.into())
		}
	}
}

/// What a canceled transfer reports before the failure itself.
fn canceled_message(kind: TransferKind, files: u64, bytes: u64) -> String {
	format!(
		"{} {} before canceling",
		kind.past(),
		files_and_bytes(files, bytes)
	)
}

/// Report the transfer's result: in JSON, or as the per-item failures the SDK reported through
/// the progress callbacks followed by the success message or a summary of what did transfer.
/// Directory transfers return `Ok` even when individual items failed, so without this the command
/// would claim success while items were missing.
fn finish(
	ui: &mut UI,
	progress: &TransferProgress,
	subject: &str,
	destination: &str,
) -> Result<()> {
	let kind = progress.kind;
	let (files, bytes) = progress.done();
	let skipped = progress.skipped();
	let failures = progress.take_failures();
	let skipped_symlinks = progress.take_skipped_symlinks();
	if ui.json {
		ui.print_json(result_json(
			destination,
			files,
			bytes,
			skipped,
			&failures,
			&skipped_symlinks,
		))?;
	} else {
		if !skipped_symlinks.is_empty() && !progress.list_files && !ui.is_quiet() {
			ui.print_muted(&skipped_symlinks_line(skipped_symlinks.len()));
		}
		for failure in failures.iter().take(MAX_PRINTED_ERRORS) {
			ui.print_failure(&failure.to_string());
		}
		if failures.len() > MAX_PRINTED_ERRORS {
			ui.print_failure(&format!(
				"... and {} more",
				failures.len() - MAX_PRINTED_ERRORS
			));
		}
		let summary = transfer_summary(files, bytes, skipped, progress.started.elapsed());
		if failures.is_empty() {
			ui.print_success(&format!("{} {}: {}", kind.past(), subject, summary));
		} else if !ui.is_quiet() {
			ui.print_muted(&format!("{} {}", kind.past(), summary));
		}
	}
	if failures.is_empty() {
		Ok(())
	} else {
		Err(UI::failure(&format!(
			"{} item(s) failed to {}",
			failures.len(),
			kind.action()
		)))
	}
}

/// The `--json` result of a transfer to `destination`.
fn result_json(
	destination: &str,
	files: u64,
	bytes: u64,
	skipped: u64,
	failures: &[TransferFailure],
	skipped_symlinks: &[String],
) -> serde_json::Value {
	json!({
		"destination": destination,
		"files": files,
		"bytes": bytes,
		"skipped": skipped,
		"skipped_symlinks": skipped_symlinks,
		"failed": failures
			.iter()
			.map(|failure| json!({ "path": failure.path, "error": failure.error }))
			.collect::<Vec<_>>(),
	})
}

/// Upload the file `source` as `name` into `destination_dir`, unless `existing` keeps a file of
/// that name already there.
async fn upload_file(
	client: &Client,
	progress: &TransferProgress,
	source: &Path,
	name: &str,
	destination_dir: &DirType<'_, Normal>,
	existing: ExistingRemoteFile,
) -> Result<()> {
	let mut builder = FileBuilderOptionalName::new(destination_dir.uuid());
	builder
		.name(name)
		.map_err(|e| UI::failure(&format!("Cannot upload as {:?}: {}", name, e)))?;
	let metadata = std::fs::metadata(source)
		.with_context(|| format!("Failed to read {}", source.display()))?;
	let size = metadata.len();
	progress.start_single_item(size);
	if existing != ExistingRemoteFile::Replace
		&& let Some(remote) = same_named_file(client, destination_dir, name).await?
		&& !existing.replaces(&metadata, &remote)
	{
		progress.skip([source.display().to_string()], size);
		return Ok(());
	}
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

/// The file in `dir` the server takes for one named `name`: it compares names by their lowercase
/// form, as a directory upload does too.
async fn same_named_file(
	client: &Client,
	dir: &DirType<'_, Normal>,
	name: &str,
) -> Result<Option<RemoteFile>> {
	let (_, files) = client
		.list_dir(dir, None::<&fn(u64, Option<u64>)>)
		.await
		.context("Failed to list remote directory")?;
	let name = name.to_lowercase();
	Ok(files.into_iter().find(|file| {
		file.name()
			.is_some_and(|other| other.to_lowercase() == name)
	}))
}

/// Upload the directory `source` as `name` into `destination_dir`, which makes it the remote
/// directory at `target_path`.
async fn upload_directory(
	client: &Arc<Client>,
	progress: &TransferProgress,
	source: &Path,
	name: &str,
	target_path: &RemotePath,
	destination_dir: &DirType<'_, Normal>,
	options: DirUploadOptions,
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
	progress.start_upload_listing(target.uuid(), target_path);
	progress.start_scan();
	client
		.clone()
		.upload_dir_recursively_with_options(
			source.to_path_buf(),
			progress,
			target.as_ref(),
			options,
		)
		.await
		.context("Failed to upload directory")
}

/// Download `source` to `target`, unless `existing` keeps a local file already there.
async fn download_file(
	client: &Client,
	progress: &TransferProgress,
	source: &RemoteFile,
	target: &Path,
	existing: ExistingLocalFile,
) -> Result<()> {
	progress.start_single_item(source.size());
	match existing {
		// Whatever is at the path, even a dangling symlink, is kept.
		ExistingLocalFile::Keep if std::fs::symlink_metadata(target).is_ok() => {
			progress.skip([target.display().to_string()], source.size());
			return Ok(());
		}
		ExistingLocalFile::Keep | ExistingLocalFile::ReplaceUnlessSame => {}
	}
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
	options: DirDownloadOptions,
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
	Normal::download_dir_recursively_with_options(
		client.clone(),
		target,
		progress,
		source,
		(),
		options,
	)
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

/// An item of a transfer that failed.
#[derive(Debug, PartialEq, Eq)]
struct TransferFailure {
	/// The item's path, unless the failure is not about a single item (such as a failed listing).
	path: Option<String>,
	error: String,
}

impl fmt::Display for TransferFailure {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match &self.path {
			Some(path) => write!(f, "{}: {}", path, self.error),
			None => f.write_str(&self.error),
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
	/// The files left out for what was already at their target, which are not in the totals.
	skipped_files: u64,
	/// The counters as the item being transferred found them, which its own counts add to: a
	/// transfer of several items adds them all up.
	item_base: ItemBase,
	/// `(when, done_bytes)`, oldest first, reaching back just past [`SPEED_WINDOW`].
	speed_samples: VecDeque<(Instant, u64)>,
	last_render: Option<Instant>,
	/// Whether there is a progress line on screen that still needs erasing.
	line_shown: bool,
	/// The remote path of each directory a listed upload created, by uuid, to list each uploaded
	/// file under its path. Empty when files are not listed.
	upload_dir_paths: HashMap<Uuid, RemotePath>,
	/// The local paths of the symlinks a directory upload skipped.
	skipped_symlinks: Vec<String>,
}

/// The counters at the start of one of a transfer's items.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ItemBase {
	total_files: u64,
	total_bytes: u64,
	done_files: u64,
	done_bytes: u64,
}

impl ProgressState {
	/// Start counting a new item on top of what the items before it counted.
	fn start_item(&mut self) {
		self.item_base = ItemBase {
			total_files: self.total_files,
			total_bytes: self.total_bytes,
			done_files: self.done_files,
			done_bytes: self.done_bytes,
		};
	}

	/// The item's own totals so far, on top of the earlier items'.
	fn set_item_totals(&mut self, files: u64, bytes: u64) {
		self.total_files = self.item_base.total_files.saturating_add(files);
		self.total_bytes = self.item_base.total_bytes.saturating_add(bytes);
	}

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

	/// Note the directories an upload created, whose parent was noted before, under their paths.
	/// A directory is reported no later than the files and directories inside it.
	fn note_upload_dirs<'a>(
		&mut self,
		dirs: impl IntoIterator<Item = (Uuid, &'a ParentUuid, Option<&'a str>)>,
	) {
		for (uuid, parent, name) in dirs {
			if let ParentUuid::Uuid(parent) = parent
				&& let Some(name) = name
				&& let Some(parent_path) = self.upload_dir_paths.get(parent)
			{
				let path = parent_path.navigate(name);
				self.upload_dir_paths.insert(uuid, path);
			}
		}
	}

	/// The remote path of an uploaded file, as far as it is known: its name alone when its parent
	/// was not noted, and its uuid when its name could not be decrypted.
	fn uploaded_file_path(&self, uuid: Uuid, parent: &ParentUuid, name: Option<&str>) -> String {
		let Some(name) = name else {
			return uuid.to_string();
		};
		match parent {
			ParentUuid::Uuid(parent) => match self.upload_dir_paths.get(parent) {
				Some(parent_path) => parent_path.navigate(name).to_string(),
				None => name.to_owned(),
			},
			ParentUuid::Trash(_)
			| ParentUuid::Recents
			| ParentUuid::Favorites
			| ParentUuid::Links => name.to_owned(),
		}
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

/// What a transfer moved and how fast, on average, and how many files it left out for what was
/// already at their target.
fn transfer_summary(files: u64, bytes: u64, skipped: u64, elapsed: Duration) -> String {
	let mut summary = format!(
		"{} in {}",
		files_and_bytes(files, bytes),
		format_elapsed(elapsed)
	);
	if let Some(speed) = bytes_per_second(bytes, elapsed) {
		summary.push_str(&format!(" ({}/s)", ui::format_size(speed)));
	}
	if skipped > 0 {
		summary.push_str(&format!(", {} skipped", skipped));
	}
	summary
}

/// Draws a single, redrawn-in-place progress line for one upload or download, and with `-v` lists
/// each file a directory transfer moved above it.
///
/// The SDK delivers its progress callbacks from worker threads, so this cannot go through [`UI`]
/// (which is `&mut`-borrowed by the command) and instead writes to the terminal itself.
pub(crate) struct TransferProgress {
	kind: TransferKind,
	term: Term,
	/// Whether the progress line may be drawn at all: never in quiet or JSON mode, and never when
	/// stdout is redirected, where the redraws would end up in the captured output.
	enabled: bool,
	/// Whether to print each file a directory transfer moved, as `-v` asks, unless the output is
	/// JSON.
	list_files: bool,
	started: Instant,
	state: Mutex<ProgressState>,
	failures: Mutex<Vec<TransferFailure>>,
}

impl TransferProgress {
	pub(crate) fn new(kind: TransferKind, ui: &UI) -> Self {
		let term = Term::stdout();
		let enabled = term.is_term() && !ui.is_quiet() && !ui.json;
		Self {
			kind,
			term,
			enabled,
			list_files: ui.is_verbose() && !ui.json,
			started: Instant::now(),
			state: Mutex::new(ProgressState::default()),
			failures: Mutex::new(Vec::new()),
		}
	}

	/// A byte-delta callback for the SDK's single-file transfer methods.
	fn byte_callback(&self) -> MaybeSendCallback<'_, u64> {
		Arc::new(|bytes| self.advance(0, bytes))
	}

	/// Begin a single-file transfer of known size — there is no scan phase for those.
	fn start_single_item(&self, size: u64) {
		let mut state = self.state.lock().unwrap();
		state.start_item();
		state.set_item_totals(1, size);
		state.sample_speed(Instant::now());
		self.render(&mut state, true);
	}

	/// Mark the single file as fully transferred. The SDK's throttle may still be holding the last
	/// few byte deltas, so set the counters outright instead of adding to them.
	fn complete_single_item(&self) {
		let mut state = self.state.lock().unwrap();
		let base = state.item_base;
		state.done_files = base.done_files.saturating_add(1);
		state.done_bytes = base
			.done_bytes
			.saturating_add(state.total_bytes.saturating_sub(base.total_bytes));
		self.render(&mut state, true);
	}

	/// Mark the single file whose size was not known up front as fully transferred, at `size`.
	fn complete_single_item_of(&self, size: u64) {
		self.state.lock().unwrap().set_item_totals(1, size);
		self.complete_single_item();
	}

	/// The progress without its line, for a transfer whose output stdout carries.
	fn without_progress_line(mut self) -> Self {
		self.enabled = false;
		self
	}

	/// List the files an upload puts into the remote directory `uuid` under `path`.
	fn start_upload_listing(&self, uuid: Uuid, path: &RemotePath) {
		if self.list_files {
			self.state
				.lock()
				.unwrap()
				.upload_dir_paths
				.insert(uuid, path.clone());
		}
	}

	fn start_scan(&self) {
		let mut state = self.state.lock().unwrap();
		state.start_item();
		state.scanning = true;
		self.render(&mut state, true);
	}

	fn report_scan(&self, known_files: u64, known_bytes: u64) {
		let mut state = self.state.lock().unwrap();
		state.set_item_totals(known_files, known_bytes);
		self.render(&mut state, false);
	}

	fn finish_scan(&self, total_files: u64, total_bytes: u64) {
		let mut state = self.state.lock().unwrap();
		state.scanning = false;
		state.set_item_totals(total_files, total_bytes);
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
		self.advance_locked(&mut state, files, bytes);
	}

	fn advance_locked(&self, state: &mut ProgressState, files: u64, bytes: u64) {
		state.done_files = state.done_files.saturating_add(files);
		state.done_bytes = state.done_bytes.saturating_add(bytes);
		state.sample_speed(Instant::now());
		self.render(state, false);
	}

	/// The files and bytes transferred so far.
	fn done(&self) -> (u64, u64) {
		let state = self.state.lock().unwrap();
		(state.done_files, state.done_bytes)
	}

	/// The files left out so far for what was already at their target.
	fn skipped(&self) -> u64 {
		self.state.lock().unwrap().skipped_files
	}

	/// Note the files at `paths`, `bytes` long together, as left out for what was already at their
	/// target: they leave the totals, and `-v` lists them.
	fn skip(&self, paths: impl IntoIterator<Item = String>, bytes: u64) {
		let paths = paths.into_iter().collect::<Vec<_>>();
		let files = u64::try_from(paths.len()).unwrap_or(u64::MAX);
		let mut state = self.state.lock().unwrap();
		state.skipped_files = state.skipped_files.saturating_add(files);
		state.total_files = state.total_files.saturating_sub(files);
		state.total_bytes = state.total_bytes.saturating_sub(bytes);
		if self.list_files {
			self.print_above(&mut state, skipped_lines(paths));
		}
		self.render(&mut state, false);
	}

	fn push_failures(&self, failures: impl IntoIterator<Item = TransferFailure>) {
		self.failures.lock().unwrap().extend(failures);
	}

	fn take_failures(&self) -> Vec<TransferFailure> {
		std::mem::take(&mut self.failures.lock().unwrap())
	}

	/// The local paths of the symlinks a directory upload skipped, taken out of the progress.
	fn take_skipped_symlinks(&self) -> Vec<String> {
		std::mem::take(&mut self.state.lock().unwrap().skipped_symlinks)
	}

	/// Print `lines` above the progress line, erasing it first so that neither tears the other.
	fn print_above(&self, state: &mut ProgressState, lines: impl IntoIterator<Item = String>) {
		let mut lines = lines.into_iter().peekable();
		if lines.peek().is_none() {
			return;
		}
		self.clear_locked(state);
		for line in lines {
			// With nowhere left to report a failed write, it is dropped, as in `UI::print`.
			let _ = self.term.write_line(&line);
		}
		self.render(state, true);
	}

	/// Erase the progress line so the command's own output starts on a clean row.
	fn clear(&self) {
		let mut state = self.state.lock().unwrap();
		self.clear_locked(&mut state);
	}

	fn clear_locked(&self, state: &mut ProgressState) {
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

/// A failure the SDK reports without the item it is about.
fn scan_failures(errors: Vec<Error>) -> impl Iterator<Item = TransferFailure> {
	errors.into_iter().map(|e| TransferFailure {
		path: None,
		error: e.to_string(),
	})
}

impl DirUploadCallback for TransferProgress {
	fn on_scan_progress(&self, _known_dirs: u64, known_files: u64, known_bytes: u64) {
		self.report_scan(known_files, known_bytes);
	}

	fn on_scan_errors(&self, errors: Vec<Error>) {
		self.push_failures(scan_failures(errors));
	}

	fn on_scan_complete(&self, _total_dirs: u64, total_files: u64, total_bytes: u64) {
		self.finish_scan(total_files, total_bytes);
	}

	fn on_upload_update(
		&self,
		uploaded_dirs: Vec<RemoteDirectory>,
		uploaded_files: Vec<RemoteFile>,
		uploaded_bytes: u64,
	) {
		let mut state = self.state.lock().unwrap();
		if self.list_files {
			state.note_upload_dirs(
				uploaded_dirs
					.iter()
					.map(|dir| (dir.uuid(), dir.parent(), dir.name())),
			);
			let paths = uploaded_files
				.iter()
				.map(|file| state.uploaded_file_path(file.uuid(), file.parent(), file.name()))
				.collect::<Vec<_>>();
			self.print_above(&mut state, paths);
		}
		self.advance_locked(&mut state, uploaded_files.len() as u64, uploaded_bytes);
	}

	fn on_upload_errors(&self, errors: Vec<(PathBuf, Error)>) {
		self.push_failures(errors.into_iter().map(|(path, e)| TransferFailure {
			path: Some(path.display().to_string()),
			error: e.to_string(),
		}));
	}

	fn on_upload_skipped(&self, skipped_files: Vec<PathBuf>, skipped_bytes: u64) {
		self.skip(
			skipped_files.iter().map(|path| path.display().to_string()),
			skipped_bytes,
		);
	}

	fn on_scan_skipped_symlinks(&self, paths: Vec<PathBuf>) {
		let paths = paths
			.iter()
			.map(|path| path.display().to_string())
			.collect::<Vec<_>>();
		let mut state = self.state.lock().unwrap();
		if self.list_files {
			self.print_above(&mut state, skipped_symlink_lines(&paths));
		}
		state.skipped_symlinks.extend(paths);
	}
}

/// The `-v` lines for the files a transfer left out for what was already at their target.
fn skipped_lines(paths: Vec<String>) -> impl Iterator<Item = String> {
	paths.into_iter().map(|path| format!("Skipped {}", path))
}

/// The `-v` lines for the symlinks a directory upload skipped.
fn skipped_symlink_lines(paths: &[String]) -> impl Iterator<Item = String> {
	paths.iter().map(|path| format!("Skipped symlink {}", path))
}

/// The line that counts the symlinks a directory upload skipped, where `-v` does not list them.
fn skipped_symlinks_line(count: usize) -> String {
	format!(
		"Skipped {} {} (-v lists them)",
		count,
		if count == 1 { "symlink" } else { "symlinks" }
	)
}

impl DirDownloadCallback<Normal> for TransferProgress {
	fn on_query_download_progress(&self, _known_bytes: u64, _total_bytes: Option<u64>) {}

	fn on_scan_progress(&self, _known_dirs: u64, known_files: u64, known_bytes: u64) {
		self.report_scan(known_files, known_bytes);
	}

	fn on_scan_errors(&self, errors: Vec<Error>) {
		self.push_failures(scan_failures(errors));
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
		let mut state = self.state.lock().unwrap();
		let files = downloaded_files.len() as u64;
		if self.list_files {
			self.print_above(
				&mut state,
				downloaded_files.into_iter().map(|(_, path)| path),
			);
		}
		self.advance_locked(&mut state, files, downloaded_bytes);
	}

	fn on_download_skipped(&self, skipped_files: Vec<(RemoteFile, String)>) {
		let bytes = skipped_files
			.iter()
			.fold(0u64, |bytes, (file, _)| bytes.saturating_add(file.size()));
		self.skip(skipped_files.into_iter().map(|(_, path)| path), bytes);
	}

	fn on_download_errors(&self, errors: Vec<(Error, String, NonRootItemType<'static, Normal>)>) {
		self.push_failures(errors.into_iter().map(|(e, path, _)| TransferFailure {
			path: Some(path),
			error: e.to_string(),
		}));
	}
}

#[cfg(test)]
mod tests {
	use std::{
		path::{Path, PathBuf},
		time::{Duration, Instant},
	};

	use filen_types::fs::{ParentUuid, Uuid};
	use serde_json::json;

	use filen_sdk_rs::{
		fs::name::ValidatedName,
		io::{DirUploadCallback as _, ExistingRemoteFile},
	};

	use super::{
		DownloadSource, Existing, FilterArgs, ProgressState, SPEED_WINDOW, TransferFailure,
		TransferKind, TransferProgress, UploadTarget, canceled_message, download_target,
		ends_the_command, ensure_stdio_is_free, existing_remote_policy, format_duration,
		local_existing, progress_line, resolve_download_target, resolve_upload_target, result_json,
		skipped_symlinks_line, source_failure, sources_and_destination, stdin_destination,
		stdin_upload_target, transfer_summary,
	};
	use crate::{
		error::CliError,
		ui::{UI, test_support::Printed},
		util::RemotePath,
	};

	const MIB: u64 = 1024 * 1024;

	/// A progress that draws and lists nothing.
	fn quiet_progress() -> TransferProgress {
		TransferProgress {
			kind: TransferKind::Upload,
			term: console::Term::stdout(),
			enabled: false,
			list_files: false,
			started: Instant::now(),
			state: std::sync::Mutex::new(ProgressState::default()),
			failures: std::sync::Mutex::new(Vec::new()),
		}
	}

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
			transfer_summary(3, 2 * MIB, 0, Duration::from_secs(4)),
			"3 files, 2 MiB in 4.0s (512 KiB/s)"
		);
	}

	#[test]
	fn the_summary_of_an_instant_transfer_has_no_speed() {
		assert_eq!(
			transfer_summary(1, 100, 0, Duration::ZERO),
			"1 file, 100 B in 0.0s"
		);
	}

	#[test]
	fn the_summary_counts_the_skipped_files() {
		assert_eq!(
			transfer_summary(1, MIB, 2, Duration::from_secs(1)),
			"1 file, 1 MiB in 1.0s (1 MiB/s), 2 skipped"
		);
	}

	#[test]
	fn skipping_takes_the_files_out_of_the_totals() {
		let progress = quiet_progress();
		progress.start_single_item(MIB);
		progress.skip(["a.txt".to_owned()], MIB);
		let state = progress.state.lock().unwrap();
		assert_eq!(
			(state.total_files, state.total_bytes, state.skipped_files),
			(0, 0, 1)
		);
	}

	fn filter_args(include: &[&str], exclude: &[&str]) -> FilterArgs {
		FilterArgs {
			include: include.iter().map(|p| (*p).to_owned()).collect(),
			exclude: exclude.iter().map(|p| (*p).to_owned()).collect(),
		}
	}

	#[test]
	fn no_filter_flags_take_everything() {
		let filter = filter_args(&[], &[]).transfer_filter().unwrap();
		assert!(filter.include.is_none() && filter.exclude.is_none());
	}

	#[test]
	fn the_filter_flags_match_gitignore_style() {
		let filter = filter_args(&["*.jpg"], &["drafts/"])
			.transfer_filter()
			.unwrap();
		let include = filter.include.unwrap();
		let exclude = filter.exclude.unwrap();
		assert!(include.matches("photos/a.JPG", false));
		assert!(!include.matches("notes.txt", false));
		assert!(exclude.matches("docs/drafts", true));
		assert!(!exclude.matches("docs/drafts", false));
	}

	#[test]
	fn a_pattern_that_does_not_compile_is_a_usage_error() {
		let error = filter_args(&[], &["[z-a]"]).transfer_filter().unwrap_err();
		assert_eq!(
			error.downcast_ref::<CliError>().map(CliError::exit_code),
			Some(2)
		);
	}

	#[test]
	fn a_single_path_is_the_source_alone() {
		assert_eq!(
			sources_and_destination("a".to_owned(), Vec::new()),
			(vec!["a".to_owned()], None)
		);
	}

	#[test]
	fn the_last_of_several_paths_is_the_destination() {
		assert_eq!(
			sources_and_destination("a".to_owned(), vec!["b".to_owned()]),
			(vec!["a".to_owned()], Some("b".to_owned()))
		);
		assert_eq!(
			sources_and_destination("a".to_owned(), vec!["b".to_owned(), "c".to_owned()]),
			(vec!["a".to_owned(), "b".to_owned()], Some("c".to_owned()))
		);
	}

	#[test]
	fn the_counts_of_several_items_add_up() {
		let progress = quiet_progress();
		progress.start_single_item(100);
		progress.complete_single_item();
		progress.start_single_item(50);
		progress.complete_single_item();
		progress.start_scan();
		progress.finish_scan(3, 300);
		let state = progress.state.lock().unwrap();
		assert_eq!(
			(
				state.total_files,
				state.total_bytes,
				state.done_files,
				state.done_bytes
			),
			(5, 450, 2, 150)
		);
	}

	#[test]
	fn an_interrupt_ends_the_command_but_an_items_failure_does_not() {
		assert!(ends_the_command(&CliError::Interrupted.into()));
		assert!(!ends_the_command(&UI::failure("No such file")));
	}

	#[test]
	fn the_failure_of_one_of_several_items_is_named_as_given() {
		let error = UI::failure("No such file or directory: /x").context("Failed to download");
		assert_eq!(
			source_failure("x", &error),
			TransferFailure {
				path: Some("x".to_owned()),
				error: "Failed to download: No such file or directory: /x".to_owned(),
			}
		);
	}

	#[test]
	fn the_existing_file_flags_pick_the_policy() {
		assert_eq!(
			existing_remote_policy(false, false),
			ExistingRemoteFile::Replace
		);
		assert_eq!(
			existing_remote_policy(true, false),
			ExistingRemoteFile::Keep
		);
		assert_eq!(
			existing_remote_policy(false, true),
			ExistingRemoteFile::ReplaceIfChanged
		);
	}

	#[test]
	fn a_canceled_transfer_reports_what_it_had_moved() {
		assert_eq!(
			canceled_message(TransferKind::Download, 2, MIB),
			"Downloaded 2 files, 1 MiB before canceling"
		);
	}

	#[test]
	fn the_skipped_symlinks_are_counted_where_they_are_not_listed() {
		assert_eq!(
			skipped_symlinks_line(1),
			"Skipped 1 symlink (-v lists them)"
		);
		assert_eq!(
			skipped_symlinks_line(3),
			"Skipped 3 symlinks (-v lists them)"
		);
	}

	#[test]
	fn skipped_symlinks_are_kept_for_the_result_even_when_not_listed() {
		let progress = quiet_progress();
		progress
			.on_scan_skipped_symlinks(vec![PathBuf::from("/local/a"), PathBuf::from("/local/b")]);
		assert_eq!(progress.take_skipped_symlinks(), ["/local/a", "/local/b"]);
	}

	#[test]
	fn stdin_and_stdout_are_refused_where_they_carry_the_commands() {
		let mut ui = Printed::default().ui();
		ui.set_commands_on_stdin();
		assert_eq!(
			ensure_stdio_is_free(&ui).unwrap_err().to_string(),
			"\"-\" (stdin or stdout) only works in a single command, such as `filen upload - \
			 /notes/today.txt`, not in the REPL or in commands piped into the CLI"
		);
	}

	#[test]
	fn stdin_and_stdout_are_free_for_a_single_command() {
		assert!(ensure_stdio_is_free(&Printed::default().ui()).is_ok());
	}

	#[test]
	fn the_json_result_lists_the_failures() {
		let failures = [
			TransferFailure {
				path: Some("/local/a.txt".to_owned()),
				error: "Permission denied".to_owned(),
			},
			TransferFailure {
				path: None,
				error: "Listing failed".to_owned(),
			},
		];
		assert_eq!(
			result_json("/dir", 4, 1000, 2, &failures, &["/local/link".to_owned()]),
			json!({
				"destination": "/dir",
				"files": 4,
				"bytes": 1000,
				"skipped": 2,
				"skipped_symlinks": ["/local/link"],
				"failed": [
					{ "path": "/local/a.txt", "error": "Permission denied" },
					{ "path": null, "error": "Listing failed" },
				],
			})
		);
	}

	#[test]
	fn a_failure_is_shown_with_its_path() {
		let failure = TransferFailure {
			path: Some("a.txt".to_owned()),
			error: "Permission denied".to_owned(),
		};
		assert_eq!(failure.to_string(), "a.txt: Permission denied");
	}

	#[test]
	fn an_uploaded_file_is_listed_under_its_directorys_path() {
		let root = Uuid::from_u128(1);
		let nested = Uuid::from_u128(2);
		let mut state = ProgressState::default();
		state
			.upload_dir_paths
			.insert(root, RemotePath::new("/backups/notes"));
		state.note_upload_dirs([(nested, &ParentUuid::Uuid(root), Some("nested"))]);
		assert_eq!(
			state.uploaded_file_path(Uuid::from_u128(3), &ParentUuid::Uuid(nested), Some("a.txt")),
			"/backups/notes/nested/a.txt"
		);
	}

	#[test]
	fn an_uploaded_file_in_an_unknown_directory_is_listed_by_name() {
		let state = ProgressState::default();
		assert_eq!(
			state.uploaded_file_path(
				Uuid::from_u128(3),
				&ParentUuid::Uuid(Uuid::from_u128(9)),
				Some("a.txt")
			),
			"a.txt"
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

	#[test]
	fn an_upload_from_stdin_needs_a_file_path() {
		assert_eq!(
			stdin_destination(Some("notes/today.txt")).unwrap(),
			"notes/today.txt"
		);
		for destination in [None, Some("notes/")] {
			let error = stdin_destination(destination).unwrap_err();
			assert_eq!(
				error.downcast_ref::<CliError>().map(CliError::exit_code),
				Some(2)
			);
		}
	}

	#[test]
	fn an_upload_from_stdin_goes_to_its_path() {
		for existing in [Existing::Nothing, Existing::File] {
			assert_eq!(
				stdin_upload_target(&RemotePath::new("/x/today.txt"), existing).unwrap(),
				upload_target("/x", "today.txt")
			);
		}
	}

	#[test]
	fn an_upload_from_stdin_onto_a_directory_is_refused() {
		let error = stdin_upload_target(&RemotePath::new("/x/notes"), Existing::Dir).unwrap_err();
		assert_eq!(
			error.to_string(),
			"Cannot upload stdin as the directory /x/notes: give the file's path"
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
