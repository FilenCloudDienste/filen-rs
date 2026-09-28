//! The public extract API on [`Client`].

use std::sync::Arc;

use filen_types::fs::Uuid;

use crate::{
	Error, ErrorKind,
	auth::Client,
	fs::{
		HasName, HasUUID,
		archive::{config::ArchiveConfig, worker},
		drive_job::backend::ClientBackend,
		file::{enums::RemoteFileType, traits::HasFileInfo},
	},
	job::JobControl,
};

use super::{
	ArchiveEntryId, ArchiveSource, ArchiveTotals, ExtractCallback, ExtractConfig, ExtractFailed,
	ExtractPhase, ExtractReport, ExtractRequest,
	codec::{CodecLimits, Selection, StreamJob, Task, extract_stream},
	engine::{ExtractTask, run_extract},
	list::{
		ArchiveListing, ListCallback, ListConfig, ListFailed, ListReporter, ListTask, run_list,
	},
	report::Reporter,
};

impl Client {
	/// Extracts an archive into the user's drive. Nothing can decode on the server (items are
	/// end-to-end encrypted), so the archive is downloaded and decoded here, and every entry is
	/// encrypted and uploaded as a new item.
	///
	/// Reads:
	/// - tars, bare or compressed with gzip, bzip2, xz, LZMA, lzip, lz4, brotli or zstd, and
	///   single compressed files: front to back, each entry extracted as it is read, none known
	///   before it is reached;
	/// - zips (stored, Deflate, Deflate64, bzip2, LZMA, XZ, zstd; ZipCrypto or AES encrypted) and
	///   7z archives (LZMA, LZMA2, PPMd, bzip2, Deflate(64), zstd, the branch and delta filters;
	///   AES encrypted, headers included): from their index, entry by entry, each checked against
	///   the checksum the archive lists for it.
	///
	/// Symbolic links, devices and entries whose paths climb out of the destination are skipped
	/// and reported; a tar's hard link is extracted as a copy of the file it names. Names taken
	/// in their directory get keep-both names. See [`ExtractConfig::skip_mac_metadata`] for the
	/// metadata macOS writes beside files. [`ExtractRequest::Entries`] extracts part of the
	/// archive, as [`Client::list_archive`] lists it, or again what failed.
	///
	/// Up to [`ArchiveConfig::job_concurrency`] archive jobs run at once; a later one waits,
	/// reporting [`ExtractPhase::WaitingForWorker`]. It
	/// reports its progress to `callback` and can be paused, resumed and cancelled through
	/// `control`. A failed entry does not stop the others; a damaged archive, running out of
	/// storage or the codec dying does.
	///
	/// Returns the report (what was created, failed, skipped or renamed). An extraction that
	/// ended early fails with the report so far: what it created stays.
	///
	/// [`ArchiveConfig::job_concurrency`]: crate::fs::archive::config::ArchiveConfig::job_concurrency
	pub async fn extract_archive(
		self: Arc<Self>,
		request: ExtractRequest,
		config: ExtractConfig,
		callback: impl ExtractCallback,
		control: JobControl,
	) -> Result<ExtractReport, ExtractFailed> {
		let (archive, dispose, disposal_requested, destination, root, selection) = match request {
			ExtractRequest::All {
				archive,
				destination,
				root,
			} => match archive {
				ArchiveSource::Keep(archive) => (archive, None, false, destination, root, None),
				ArchiveSource::Dispose { file, how } => {
					// a file in the trash has no directory to confirm it is still in, and is kept
					let dispose = Uuid::try_from(file.parent).ok().map(|parent| (how, parent));
					let archive = RemoteFileType::from(file);
					(archive, dispose, true, destination, root, None)
				}
			},
			ExtractRequest::Entries {
				archive,
				ids,
				base,
				destination,
				root,
			} => {
				let selection = check_entries(archive.uuid(), &ids)
					.map(|()| Selection::new(ids.into_iter().map(|id| u64::from(id.index)), base));
				(archive, None, false, destination, root, Some(selection))
			}
		};
		let totals = ArchiveTotals::Streaming {
			archive_bytes: archive.size(),
		};
		let reporter = Reporter::new(callback, totals);
		let selection = match selection.transpose() {
			Ok(selection) => selection,
			Err(error) => {
				reporter.finish(ExtractPhase::Failed);
				return Err(ExtractFailed {
					report: ExtractReport::new(totals),
					error: Arc::new(error),
				});
			}
		};
		let archives = self.client().state().archives().clone();
		let base = selection
			.as_ref()
			.map(|selection| selection.base().to_vec())
			.unwrap_or_default();
		let job = StreamJob {
			name: archive.name().unwrap_or_default().to_owned(),
			len: archive.size(),
			limits: codec_limits(&archives, &config),
			password: config.password,
			skip_mac_metadata: config.skip_mac_metadata,
			task: Task::Extract(selection),
		};
		run_extract(ExtractTask {
			backend: Arc::new(ClientBackend::new(self)),
			control,
			reporter,
			archive,
			destination,
			root,
			max_bytes: config.max_bytes,
			max_items: config.max_items,
			expansion: config.expansion_limit,
			base,
			config: archives,
			start: Box::new(move || worker::start(move |port| extract_stream(&port, job))),
			dispose,
			disposal_requested,
		})
		.await
	}

	/// Lists an archive's entries without extracting any: what each one is, and what extracting
	/// it with the same settings as `config` would do with it (skip it, and why).
	///
	/// A zip's or 7z's index says nearly all: the index is read, and besides it only the
	/// smallest encrypted entry, as an extraction reads it, to check the password (see
	/// [`ArchiveListing::password`]), and what tells a link's target: each zip symlink's data (at
	/// most 4096 bytes, unencrypted ones only), and a 7z symlink's or reparse point's when it is
	/// within the first 16 MiB of its folder and the archive states no more than the
	/// [`ExpansionLimit`](super::ExpansionLimit) allows; past that, a 7z link is listed without
	/// its target and a reparse point as a file. A tar's members, or what a single compressed
	/// file decodes to, are only known by reading it all, which takes as long as downloading it;
	/// that is reported as it goes, and can be paused and cancelled.
	///
	/// Entries are delivered to `callback` in batches as they are read, and the listing keeps the
	/// first [`MAX_LISTED_ENTRIES`](super::MAX_LISTED_ENTRIES) of them within
	/// [`MAX_LISTED_BYTES`](super::MAX_LISTED_BYTES): see [`ArchiveListing`].
	/// A listing takes one of the [`ArchiveConfig::job_concurrency`] archive jobs.
	///
	/// [`ArchiveConfig::job_concurrency`]: crate::fs::archive::config::ArchiveConfig::job_concurrency
	pub async fn list_archive(
		self: Arc<Self>,
		archive: RemoteFileType<'static>,
		config: ListConfig,
		callback: impl ListCallback,
		control: JobControl,
	) -> Result<ArchiveListing, ListFailed> {
		let archives = self.client().state().archives().clone();
		let config = ExtractConfig::from(config);
		let job = StreamJob {
			name: archive.name().unwrap_or_default().to_owned(),
			len: archive.size(),
			limits: codec_limits(&archives, &config),
			password: config.password,
			skip_mac_metadata: config.skip_mac_metadata,
			task: Task::List {
				archive: archive.uuid(),
			},
		};
		run_list(ListTask {
			backend: Arc::new(ClientBackend::new(self)),
			control,
			reporter: ListReporter::new(callback, archive.size()),
			archive,
			config: archives,
			start: Box::new(move || worker::start(move |port| extract_stream(&port, job))),
		})
		.await
	}
}

/// What the codec may spend on an archive, from the client's settings and the job's.
fn codec_limits(archives: &ArchiveConfig, config: &ExtractConfig) -> CodecLimits {
	CodecLimits {
		decoder_memory: archives.codec_mem_budget,
		max_members: archives.max_members,
		expansion: config.expansion_limit,
		max_index_bytes: archives.max_index_bytes,
		max_bytes: config.max_bytes,
	}
}

/// Checks `ids`, the entries chosen to extract, name some of `archive`'s.
pub(crate) fn check_entries(archive: Uuid, ids: &[ArchiveEntryId]) -> Result<(), Error> {
	if ids.is_empty() {
		return Err(Error::custom(
			ErrorKind::InvalidState,
			"no entry was chosen to extract",
		));
	}
	if ids.iter().any(|id| id.archive != archive) {
		return Err(Error::custom(
			ErrorKind::InvalidState,
			"an entry chosen to extract is of another archive",
		));
	}
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::fs::archive::extract::{
		ArchiveEntry, ExtractRoot, ExtractUpdate, ExtractedTopLevel, ListUpdate,
	};

	#[expect(dead_code, reason = "only named by the compile-time checks below")]
	struct Ignore;

	impl ExtractCallback for Ignore {
		fn on_top_level_created(&self, _: Vec<ExtractedTopLevel>) {}
		fn on_update(&self, _: ExtractUpdate) {}
	}

	impl ListCallback for Ignore {
		fn on_entries_batch(&self, _: Vec<ArchiveEntry>) {}
		fn on_update(&self, _: ListUpdate) {}
	}

	#[test]
	fn the_entries_chosen_are_checked_before_anything_runs() {
		let archive = Uuid::from_u128(1);
		let id = |archive, index| ArchiveEntryId { archive, index };
		assert!(check_entries(archive, &[id(archive, 3), id(archive, 1)]).is_ok());
		for ids in [vec![], vec![id(archive, 0), id(Uuid::from_u128(2), 0)]] {
			assert_eq!(
				check_entries(archive, &ids).unwrap_err().kind(),
				ErrorKind::InvalidState,
				"{ids:?}"
			);
		}
	}

	/// The bindings run the extraction and the listing on the SDK's multi-threaded runtime,
	/// which needs `Send` futures.
	fn _extract_future_is_send(
		client: Arc<Client>,
		archive: crate::fs::file::enums::RemoteFileType<'static>,
		destination: crate::fs::categories::DirType<'static, crate::fs::categories::Normal>,
	) {
		fn assert_send<T: Send>(_: T) {}
		assert_send(client.clone().extract_archive(
			ExtractRequest::All {
				archive: ArchiveSource::Keep(archive.clone()),
				destination,
				root: ExtractRoot::Destination,
			},
			ExtractConfig::default(),
			Ignore,
			JobControl::default(),
		));
		assert_send(client.list_archive(
			archive,
			ListConfig::default(),
			Ignore,
			JobControl::default(),
		));
	}
}
