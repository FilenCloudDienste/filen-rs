//! The end of an extraction: its items propagated to the shares and links the destination gained
//! meanwhile, and the archive removed once what it held is verified to be in the drive; or, after
//! a wrong password showed late, the directories it created trashed.

use std::{collections::HashSet, time::Duration};

use filen_types::fs::Uuid;
use futures::future::join_all;

use crate::{
	Error,
	connect::ConnectedTargets,
	consts::MAX_SMALL_PARALLEL_REQUESTS,
	fs::{
		HasUUID,
		archive::{
			dispose::{
				DisposalBackend, DisposalOutcome, ExpectedFile, KeptReason, Removing,
				SourceDisposal, Tree, dispose_file,
			},
			format::ArchiveFormat,
			names::ROOT,
		},
		categories::NonRootItemType,
		drive_job::{
			counts::ItemCounts,
			lock::{LockWait, wait_for_lock},
		},
		file::traits::HasRemoteFileInfo,
	},
	job::Stopped,
	util::sleep,
};

use super::{
	super::report::{ExtractEvent, ExtractTopLevelKey, ExtractTopLevelTrashed, ExtractedTopLevel},
	Driver,
};

/// How long an extraction that found its password wrong late waits for the drive lock to trash
/// the directories it created, before it leaves them: the job has already ended, and nothing
/// else would end the wait while another client holds the lock.
pub(super) const LATE_TRASH_LOCK_WAIT: Duration = Duration::from_secs(60);

impl<B: DisposalBackend> Driver<B> {
	/// The destination may have been shared or linked while the extraction ran; items created
	/// before that were propagated to the old targets only. Propagate everything created (each
	/// top-level item with its subtree) to the new ones, as many items at once as other small
	/// requests, under the drive lock, which a pause gives back until it is over.
	pub(super) async fn recheck_targets(&mut self) -> Result<(), Stopped> {
		self.reporter.checkpoint(&self.control).await?;
		let destination = self.destination.uuid();
		let current = match self
			.control
			.until_stopping(self.backend.connected_targets(destination))
			.await?
		{
			Ok(current) => current,
			Err(error) => {
				// the extraction itself succeeded; only report
				tracing::warn!("failed to re-check the extraction destination's shares: {error}");
				return Ok(());
			}
		};
		let added = current.without(&self.targets);
		if added.is_empty() || self.report.top_level.is_empty() {
			return Ok(());
		}
		let items = self.report.top_level.len() + self.top_level_beyond.len();
		let mut next = 0;
		while next < items {
			let _lock = loop {
				match wait_for_lock(&*self.backend, &self.control, &self.reporter.ops()).await? {
					LockWait::Locked(held) => break held,
					LockWait::Paused => self.reporter.checkpoint(&self.control).await?,
					LockWait::Failed(error) => {
						tracing::warn!(
							"failed to lock the drive to propagate extracted items: {error}"
						);
						return Ok(());
					}
				}
			};
			while next < items && !self.control.is_pause_requested() {
				// a cancel is not kept waiting for every item
				if self.control.is_stopping() {
					return Err(Stopped);
				}
				let end = (next + MAX_SMALL_PARALLEL_REQUESTS).min(items);
				let propagated = join_all((next..end).map(|index| {
					propagate_top_level(
						&*self.backend,
						&self.report.top_level,
						&self.top_level_beyond,
						index,
						&added,
					)
				}))
				.await;
				for (dest_uuid, errors) in propagated {
					self.report_propagation(dest_uuid, errors);
				}
				next = end;
				self.reporter.tick();
			}
		}
		Ok(())
	}

	/// Whether every entry the archive holds is extracted: none failed, and none skipped but the
	/// macOS metadata left out on purpose.
	pub(super) fn complete(&self, counts: ItemCounts) -> bool {
		counts.files_failed + counts.dirs_failed + counts.entries_skipped - self.left_out == 0
	}

	/// Removes the archive if the extraction is verified; what became of it.
	pub(super) async fn dispose_archive(
		&mut self,
		how: SourceDisposal,
		parent: Uuid,
	) -> DisposalOutcome {
		let kept = DisposalOutcome::kept;
		let counts = self.reporter.counts();
		if !self.complete(counts) {
			return kept(KeptReason::Incomplete);
		}
		if self.report.unaccounted_bytes > 0 {
			return kept(KeptReason::UnaccountedData {
				bytes: self.report.unaccounted_bytes,
			});
		}
		if self.report.duplicates.is_some() {
			return kept(KeptReason::Incomplete);
		}
		if self.unchecked_entries > 0 {
			return kept(KeptReason::Unconfirmed);
		}
		if !matches!(
			self.opened.as_ref().map(|opened| opened.layout),
			Some(ArchiveFormat::Zip | ArchiveFormat::SevenZ)
		) {
			// A streaming archive's entries carry no checksum of their own (a tar's) or share
			// one for the whole stream: the whole archive, read front to back, has to match the
			// hash in its metadata. A zip's or a 7z's entries were each checked as they were
			// read.
			let Some(read) = self.feed.read_whole() else {
				return kept(KeptReason::Unconfirmed);
			};
			match self.archive.hash() {
				Some(expected) if expected != read => return kept(KeptReason::HashMismatch),
				None if how == SourceDisposal::DeletePermanently => {
					return kept(KeptReason::HashUnavailable);
				}
				_ => {}
			}
		}
		if !self.output_confirmed(counts).await {
			return kept(if self.control.is_stopping() {
				KeptReason::Interrupted
			} else {
				KeptReason::Unconfirmed
			});
		}
		let archive = ExpectedFile::of(&*self.archive, self.archive.uuid(), parent);
		let ops = self.reporter.ops();
		let removing = Removing {
			control: &self.control,
			ops: &ops,
			output: None,
		};
		dispose_file(&*self.backend, archive, how, removing).await
	}

	/// Whether the server holds exactly what the counts say was created: every file at its size
	/// and every directory, listed again below the items created in the destination. Listed
	/// as many at once as other small requests; a pause is waited out between them, holding
	/// nothing, and a cancel drops those in flight and ends the check unconfirmed.
	async fn output_confirmed(&mut self, counts: ItemCounts) -> bool {
		let mut found = Tree::default();
		if !self.into_destination {
			// each request counts in flight, so a pause is only reported once it is over
			let _listing = self.reporter.op();
			let listed = self
				.control
				.until_stopping(self.backend.list_tree(self.dirs[ROOT].uuid))
				.await;
			match listed {
				Ok(Ok(tree)) => found = tree,
				Ok(Err(error)) => return self.unconfirmed(&error),
				Err(Stopped) => return false,
			}
			// the folder itself
			found.dirs.insert(self.dirs[ROOT].uuid);
		}
		// the new folder is listed whole above; the items past the report's records are
		// checked as recheck_targets goes through them
		let items: Vec<(Uuid, bool)> = self
			.report
			.top_level
			.iter()
			.filter(|top| top.key != ExtractTopLevelKey::Root)
			.map(|top| (top.item.uuid(), matches!(top.item, NonRootItemType::Dir(_))))
			.chain(self.top_level_beyond.iter().copied())
			.collect();
		for batch in items.chunks(MAX_SMALL_PARALLEL_REQUESTS) {
			if self.reporter.checkpoint(&self.control).await.is_err() {
				return false;
			}
			let listing = self.reporter.op();
			// a listing removes nothing: a cancel drops the ones in flight
			let listed = self
				.control
				.until_stopping(join_all(
					batch
						.iter()
						.map(|&(uuid, is_dir)| created_tree(&*self.backend, uuid, is_dir)),
				))
				.await;
			drop(listing);
			let Ok(listed) = listed else {
				return false;
			};
			for tree in listed {
				match tree {
					Ok(Some(tree)) => {
						found.dirs.extend(tree.dirs);
						found.files.extend(tree.files);
					}
					Ok(None) => return false,
					Err(error) => return self.unconfirmed(&error),
				}
			}
			self.reporter.tick();
		}
		// the very items the job created, each file at the size it wrote
		found.files.len() as u64 == counts.files_done
			&& found
				.files
				.values()
				.try_fold(0u64, |sum, &size| sum.checked_add(size))
				== Some(counts.bytes_done)
			&& found.dirs.len() as u64 == counts.dirs_created
			&& found.digest() == self.created_digest
	}

	/// Logs a request that failed while confirming the output, which keeps the archive as
	/// unconfirmed.
	fn unconfirmed(&self, error: &Error) -> bool {
		tracing::warn!(
			"archive {}: failed to confirm the extracted items: {error}",
			self.archive.uuid()
		);
		false
	}

	/// A wrong password that only showed once entries were read (no entry was small enough to
	/// check it on first) leaves the directories created so far and no file: they go to the
	/// trash, so a retry with the right password starts clean, and out of the report's top-level
	/// items. Trashed, never deleted: they can be restored. A directory that now holds a file, or
	/// that could not be listed or trashed, is kept, as all of them are when the drive lock is
	/// not had within [`LATE_TRASH_LOCK_WAIT`]; the report's top-level items list what was
	/// kept.
	pub(super) async fn trash_created_dirs(&mut self) {
		let dirs = self
			.report
			.top_level
			.iter()
			.filter_map(|top| match &top.item {
				NonRootItemType::Dir(dir) => Some(dir.uuid()),
				NonRootItemType::File(_) => None,
			})
			.chain(
				self.top_level_beyond
					.iter()
					.filter(|(_, is_dir)| *is_dir)
					.map(|(uuid, _)| *uuid),
			)
			.collect::<Vec<_>>();
		if dirs.is_empty() {
			return;
		}
		// the job has already stopped, so the lock is taken directly rather than through a wait
		// a stop would end, and for a bounded time; holding it, nothing written under the lock
		// lands between a folder's listing and its removal
		let lock = tokio::select! {
			lock = self.backend.acquire_drive_lock() => lock,
			() = sleep(LATE_TRASH_LOCK_WAIT) => {
				tracing::warn!(
					"archive {}: the drive lock was not had within {LATE_TRASH_LOCK_WAIT:?}, so \
					 the directories created before the wrong password showed are kept",
					self.archive.uuid()
				);
				return;
			}
		};
		let _lock = match lock {
			Ok(lock) => lock,
			Err(error) => {
				tracing::warn!(
					"archive {}: failed to lock the drive to trash the directories created \
					 before the wrong password showed: {error}",
					self.archive.uuid()
				);
				return;
			}
		};
		let mut trashed = HashSet::new();
		for uuid in dirs {
			// the job created no file: one in there now is someone else's, and keeps the folder
			match self.backend.list_tree(uuid).await {
				Ok(tree) if tree.files.is_empty() => {}
				Ok(_) => continue,
				Err(error) => {
					tracing::warn!(
						"archive {}: failed to list a directory before trashing it: {error}",
						self.archive.uuid()
					);
					continue;
				}
			}
			match self.backend.trash_dir(uuid).await {
				Ok(()) => {
					trashed.insert(uuid);
					let trashed = ExtractTopLevelTrashed { dest_uuid: uuid };
					self.reporter.event(ExtractEvent::TopLevelTrashed(trashed));
				}
				Err(error) => tracing::warn!(
					"archive {}: failed to trash a directory created before the wrong password \
					 showed: {error}",
					self.archive.uuid()
				),
			}
		}
		self.report
			.top_level
			.retain(|top| !trashed.contains(&top.item.uuid()));
		self.top_level_beyond
			.retain(|(uuid, _)| !trashed.contains(uuid));
	}
}

/// Propagates top-level item `index` of `kept` followed by `beyond` (those past the report's
/// records, fetched again: only when the destination changed, which is rare, rather than
/// holding every one of them for the whole job) with its subtree to `added`; its uuid, and
/// what failed.
async fn propagate_top_level<B: DisposalBackend>(
	backend: &B,
	kept: &[ExtractedTopLevel],
	beyond: &[(Uuid, bool)],
	index: usize,
	added: &ConnectedTargets,
) -> (Uuid, Vec<Error>) {
	if let Some(top) = kept.get(index) {
		return (
			top.item.uuid(),
			backend.propagate_tree(added, &top.item).await,
		);
	}
	let (uuid, is_dir) = beyond[index - kept.len()];
	let errors = match backend.normal_item(uuid, is_dir).await {
		Ok(item) => backend.propagate_tree(added, &item).await,
		Err(error) => vec![error],
	};
	(uuid, errors)
}

/// What the server holds of a top-level item the job created: itself, and everything below a
/// directory; `None` for a file in the trash.
async fn created_tree<B: DisposalBackend>(
	backend: &B,
	uuid: Uuid,
	is_dir: bool,
) -> Result<Option<Tree>, Error> {
	let mut tree = Tree::default();
	if is_dir {
		tree = backend.list_tree(uuid).await?;
		tree.dirs.insert(uuid);
	} else {
		let state = backend.file_state(uuid).await?;
		if state.trash {
			return Ok(None);
		}
		tree.files.insert(uuid, state.size);
	}
	Ok(Some(tree))
}
