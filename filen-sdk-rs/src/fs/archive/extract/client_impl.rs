//! The public extract API on [`Client`].

use std::sync::Arc;

use filen_types::fs::Uuid;

use crate::{
	auth::Client,
	fs::{
		HasName,
		archive::worker,
		drive_job::backend::ClientBackend,
		file::{enums::RemoteFileType, traits::HasFileInfo},
	},
	job::JobControl,
};

use super::{
	ArchiveSource, ArchiveTotals, ExtractCallback, ExtractConfig, ExtractFailed, ExtractReport,
	ExtractRequest,
	codec::{CodecLimits, StreamJob, extract_stream},
	engine::{ExtractTask, run_extract},
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
	/// Links, devices and entries whose paths climb out of the destination are skipped and
	/// reported. Names taken in their directory get keep-both names.
	///
	/// Up to [`ArchiveConfig::job_concurrency`] archive jobs run at once; a later one waits,
	/// reporting [`ExtractPhase::WaitingForWorker`](super::ExtractPhase::WaitingForWorker). It
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
		let ExtractRequest::All {
			archive,
			destination,
			root,
		} = request;
		let disposal_requested = matches!(archive, ArchiveSource::Dispose { .. });
		let (archive, dispose) = match archive {
			ArchiveSource::Keep(archive) => (archive, None),
			ArchiveSource::Dispose { file, how } => {
				// a file in the trash has no directory to confirm it is still in, and is kept
				let dispose = Uuid::try_from(file.parent).ok().map(|parent| (how, parent));
				(RemoteFileType::from(file), dispose)
			}
		};
		let archives = self.client().state().archives().clone();
		let reporter = Reporter::new(
			callback,
			ArchiveTotals::Streaming {
				archive_bytes: archive.size(),
			},
		);
		let job = StreamJob {
			name: archive.name().unwrap_or_default().to_owned(),
			len: archive.size(),
			limits: CodecLimits {
				decoder_memory: archives.codec_mem_budget,
				max_members: archives.max_members,
				expansion: config.expansion_limit,
				max_index_bytes: archives.max_index_bytes,
				max_bytes: config.max_bytes,
			},
			password: config.password,
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
			config: archives,
			start: Box::new(move || worker::start(move |port| extract_stream(&port, job))),
			dispose,
			disposal_requested,
		})
		.await
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::fs::archive::extract::{ExtractRoot, ExtractUpdate, ExtractedTopLevel};

	#[expect(dead_code, reason = "only named by the compile-time check below")]
	struct Ignore;

	impl ExtractCallback for Ignore {
		fn on_top_level_created(&self, _: Vec<ExtractedTopLevel>) {}
		fn on_update(&self, _: ExtractUpdate) {}
	}

	/// The bindings run the extraction on the SDK's multi-threaded runtime, which needs a `Send`
	/// future.
	fn _extract_future_is_send(
		client: Arc<Client>,
		archive: crate::fs::file::enums::RemoteFileType<'static>,
		destination: crate::fs::categories::DirType<'static, crate::fs::categories::Normal>,
	) {
		fn assert_send<T: Send>(_: T) {}
		assert_send(client.extract_archive(
			ExtractRequest::All {
				archive: ArchiveSource::Keep(archive),
				destination,
				root: ExtractRoot::Destination,
			},
			ExtractConfig::default(),
			Ignore,
			JobControl::default(),
		));
	}
}
