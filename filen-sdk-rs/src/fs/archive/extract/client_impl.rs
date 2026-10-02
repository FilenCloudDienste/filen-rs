//! The public extract API on [`Client`].

use std::sync::Arc;

use filen_types::fs::Uuid;
use futures::AsyncWrite;

use crate::{
	auth::Client,
	fs::{
		HasName, HasUUID,
		archive::{
			ArchivePassword,
			config::ArchiveConfig,
			worker::{self, CodecStart},
		},
		drive_job::backend::ClientBackend,
		file::{enums::RemoteFileType, traits::HasFileInfo},
	},
	job::JobControl,
	util::MaybeSend,
};

use super::{
	ArchiveEntryId, ArchiveSource, EntryDownloadCallback, EntryDownloadFailed, EntryDownloadReport,
	ExpansionLimit, ExtractCallback, ExtractFailed, ExtractReport, ExtractRequest, ExtractWhat,
	codec::{CodecLimits, StreamJob, Task, extract_stream},
	download::{DownloadTask, EntryDownloadReporter, choose_entry, run_download},
	engine::{ArchiveDisposal, CodecResult, ExtractTask, run_extract},
	list::{ListCallback, ListFailed, ListReport, ListReporter, ListTask, run_list},
	report::Reporter,
};

/// How an extraction runs: its limits, the archive's password and what it leaves out. The
/// client's [`ArchiveConfig`] sets the rest (codec memory, and how many archive jobs run at
/// once).
#[derive(Debug, Clone)]
pub struct ExtractConfig {
	/// Storage still free on the account, if the caller knows it. A job that needs more fails
	/// with [`ErrorKind::MaxStorageReached`](crate::ErrorKind); one that needs exactly this much
	/// fits. A zip or 7z states its files' sizes in its index, so one stating more for the files
	/// it will extract (those skipped for their path or method left out) fails before anything
	/// is created; a tar or single compressed file is only known as it is read, so it is checked as
	/// it goes, and what was extracted so far is kept. A zip entry found overlapping another
	/// only once it is read still counts up front, so such a zip may be refused though it fits.
	pub max_bytes: Option<u64>,
	/// Most directories and files created; an archive with more fails with
	/// [`ErrorKind::ArchiveTooLarge`](crate::ErrorKind).
	pub max_items: Option<u64>,
	/// The guard against decompression bombs, which fails an archive that decodes to more with
	/// [`ErrorKind::ArchiveTooLarge`](crate::ErrorKind); `None` turns it off. The bindings
	/// cannot turn it off: there, leaving it out means the default.
	pub expansion_limit: Option<ExpansionLimit>,
	/// For an archive with encrypted entries. Checked before anything is created on a 7z's
	/// encrypted header, or else by reading the encrypted entry quickest to read in full, when
	/// that takes at most 16 MiB of the archive. Otherwise it is checked as entries are
	/// extracted: a wrong password found then fails the job with
	/// [`ErrorKind::ArchiveWrongPassword`](crate::ErrorKind), and when no file was extracted by
	/// then, the job tries to move the folders it created to the trash. It keeps any that now
	/// hold a file (someone else may have put one there meanwhile) or that it could not list or
	/// trash, and all of them when it cannot get the drive lock in time.
	///
	/// A partial extraction ([`ExtractWhat::Entries`]) of a zip or 7z needs it only when an entry
	/// it takes is encrypted (or the 7z's header is), and checks it on those entries alone.
	pub password: Option<ArchivePassword>,
	/// Leaves out the metadata macOS writes beside files where it cannot keep it with them,
	/// reported skipped as [`ExtractSkipReason::MacMetadata`](super::ExtractSkipReason::MacMetadata):
	/// AppleDouble files (named `._name` or kept in a `__MACOSX` folder of Finder's zips, told by
	/// the 8 bytes they start with), a tar's hard links to them, and the `__MACOSX` folders (and
	/// folders in them) that hold nothing else. A folder there that holds anything of the
	/// user's, or nothing at all, is created. Left out on purpose, they keep nothing from
	/// removing the archive once the rest is extracted. `true` by default; `false` extracts them
	/// as ordinary files.
	pub skip_mac_metadata: bool,
}

impl Default for ExtractConfig {
	fn default() -> Self {
		Self {
			max_bytes: None,
			max_items: None,
			expansion_limit: Some(ExpansionLimit::default()),
			password: None,
			skip_mac_metadata: true,
		}
	}
}

/// How a listing reads an archive, and which extraction's verdicts it shows: those of an
/// extraction with the same settings.
#[derive(Debug, Clone)]
pub struct ListConfig {
	/// As [`ExtractConfig::expansion_limit`]: a 7z link's target is read only while the
	/// archive states no more than this allows.
	pub expansion_limit: Option<ExpansionLimit>,
	/// As [`ExtractConfig::skip_mac_metadata`]: whether the entries left out as macOS metadata
	/// are listed skipped.
	pub skip_mac_metadata: bool,
	/// As [`ExtractConfig::password`]: checked on the index or an entry when it can be (see
	/// [`ListReport::password`]).
	pub password: Option<ArchivePassword>,
}

impl Default for ListConfig {
	fn default() -> Self {
		let ExtractConfig {
			expansion_limit,
			skip_mac_metadata,
			password,
			..
		} = ExtractConfig::default();
		Self {
			expansion_limit,
			skip_mac_metadata,
			password,
		}
	}
}

impl ListConfig {
	/// The extraction whose verdicts the listing shows: its settings, and neither cap.
	pub(crate) fn into_extraction(self) -> ExtractConfig {
		ExtractConfig {
			max_bytes: None,
			max_items: None,
			expansion_limit: self.expansion_limit,
			skip_mac_metadata: self.skip_mac_metadata,
			password: self.password,
		}
	}
}

/// How [`Client::download_archive_entry`] reads its archive. The client's [`ArchiveConfig`] sets
/// the rest (codec memory, and how many archive jobs run at once).
#[derive(Debug, Clone)]
pub struct EntryDownloadConfig {
	/// As [`ExtractConfig::expansion_limit`]: an archive whose entries state more in all than it
	/// allows is refused before the entry is read; `None` turns the guard off.
	pub expansion_limit: Option<ExpansionLimit>,
	/// For an encrypted entry, or any entry of a 7z whose header is encrypted. Checked on the
	/// entry itself as it is read, never on another entry first: a plain entry needs none.
	pub password: Option<ArchivePassword>,
	/// Most bytes a 7z solid block may decode and throw away before the entry: its
	/// [`EntryAccess::SolidBlock`](super::EntryAccess::SolidBlock) `skipped_bytes`, as a
	/// listing shows them. An entry stored after more fails with
	/// [`ErrorKind::ArchiveSolidSkipExceeded`](crate::ErrorKind::ArchiveSolidSkipExceeded) once
	/// the index is read, before any of its block is fetched. `None` allows any skip. `Some(0)`
	/// by default: a solid entry stored after others is downloaded only once the caller accepted
	/// what it costs.
	pub max_solid_skip: Option<u64>,
}

impl Default for EntryDownloadConfig {
	fn default() -> Self {
		let ExtractConfig {
			expansion_limit,
			password,
			..
		} = ExtractConfig::default();
		Self {
			expansion_limit,
			password,
			max_solid_skip: Some(0),
		}
	}
}

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
	/// metadata macOS writes beside files. [`ExtractWhat::Entries`] extracts part of the
	/// archive, as [`Client::list_archive`] lists it, or again what failed.
	///
	/// Up to [`ArchiveConfig::job_concurrency`] archive jobs run at once; a later one waits,
	/// reporting [`ExtractPhase::WaitingForWorker`](super::ExtractPhase::WaitingForWorker). It
	/// reports its progress to `callback` and can be paused, resumed and cancelled through
	/// `control`. A failed entry does not stop the others; a damaged archive, running out of
	/// storage or the codec dying does.
	///
	/// While paused the job holds no drive lock and no reservation from the client's memory
	/// budget, but it keeps its archive job slot, its decoders' state (up to the client's
	/// archive codec budget) and a chunk each of input and output, so it can go on where it
	/// stopped.
	///
	/// Returns the report (what was created, failed, skipped or renamed). An extraction that
	/// ended early fails with the report so far: what it created stays.
	///
	/// [`ArchiveConfig::job_concurrency`]: crate::fs::archive::ArchiveConfig::job_concurrency
	pub async fn extract_archive(
		self: Arc<Self>,
		request: ExtractRequest,
		config: ExtractConfig,
		callback: impl ExtractCallback,
		control: JobControl,
	) -> Result<ExtractReport, ExtractFailed> {
		let ExtractRequest {
			what,
			destination,
			root,
		} = request;
		let (archive, dispose, selection) = match what {
			ExtractWhat::All(ArchiveSource::Keep(archive)) => (archive, None, None),
			ExtractWhat::All(ArchiveSource::Dispose { file, how }) => {
				let dispose = match Uuid::try_from(file.parent) {
					Ok(parent) => ArchiveDisposal::Remove { how, parent },
					Err(_) => ArchiveDisposal::Unavailable,
				};
				(RemoteFileType::from(file), Some(dispose), None)
			}
			ExtractWhat::Entries(selection) => {
				let (archive, selection) = selection.into_parts();
				(archive, None, Some(selection))
			}
		};
		let reporter = Reporter::new(callback, archive.size());
		let archives = self.archives().clone();
		let base = selection
			.as_ref()
			.map(|selection| selection.base().to_vec())
			.unwrap_or_default();
		let (max_bytes, max_items, expansion) =
			(config.max_bytes, config.max_items, config.expansion_limit);
		let start = start_codec(&archive, &archives, config, Task::Extract(selection));
		run_extract(ExtractTask {
			backend: Arc::new(ClientBackend::new(self)),
			control,
			reporter,
			archive,
			destination,
			root,
			max_bytes,
			max_items,
			expansion,
			base,
			config: archives,
			start,
			dispose,
		})
		.await
	}

	/// Lists an archive's entries without extracting any: what each one is, and what extracting
	/// it with the same settings as `config` would do with it (skip it, and why).
	///
	/// A zip's or 7z's index says nearly all: the index is read, and besides it only the
	/// smallest encrypted entry, as an extraction reads it, to check the password (see
	/// [`ListReport::password`]), and what tells a link's target: each zip symlink's data (at
	/// most 4096 bytes, unencrypted ones only), and a 7z symlink's or reparse point's when it is
	/// within the first 16 MiB of its folder and the archive states no more than the
	/// [`ExpansionLimit`] allows; past that, a 7z link is listed without
	/// its target and a reparse point as a file. A tar's members, or what a single compressed
	/// file decodes to, are only known by reading it all, which takes as long as downloading it;
	/// that is reported as it goes, and can be paused and cancelled. While paused the listing
	/// holds no drive lock and no reservation from the client's memory budget, but it keeps its
	/// archive job slot and its decoders' state (up to the client's archive codec budget).
	///
	/// Entries are delivered to `callback` in batches as they are read, and the listing keeps the
	/// first [`MAX_LISTED_ENTRIES`](super::MAX_LISTED_ENTRIES) of them within
	/// [`MAX_LISTED_BYTES`](super::MAX_LISTED_BYTES): see [`ListReport`].
	/// A listing takes one of the [`ArchiveConfig::job_concurrency`] archive jobs.
	///
	/// [`ArchiveConfig::job_concurrency`]: crate::fs::archive::ArchiveConfig::job_concurrency
	pub async fn list_archive(
		self: Arc<Self>,
		archive: RemoteFileType<'static>,
		config: ListConfig,
		callback: impl ListCallback,
		control: JobControl,
	) -> Result<ListReport, ListFailed> {
		let archives = self.archives().clone();
		let task = Task::List {
			archive: archive.uuid(),
		};
		let start = start_codec(&archive, &archives, config.into_extraction(), task);
		run_list(ListTask {
			backend: Arc::new(ClientBackend::new(self)),
			control,
			reporter: ListReporter::new(callback, archive.size()),
			archive,
			config: archives,
			start,
		})
		.await
	}

	/// Writes the data of `entry`, a file of the zip or 7z `archive`, to `writer`, then closes
	/// `writer`. Only the archive's first chunk (which tells its format), its index and the entry
	/// are read, and for a 7z entry stored in a solid block, what the block stores before it as
	/// well, decoded and thrown away (see [`EntryDownloadConfig::max_solid_skip`]).
	///
	/// Data reaches `writer` as it is decoded, before the CRC-32 or authentication code at its
	/// end is checked: on `Err`, `writer` is left unclosed, and what it holds is unverified.
	/// `report.checked` is `false` for a 7z entry whose header lists no CRC-32.
	///
	/// Fails before a byte is written for:
	/// - a directory or a link, with [`ErrorKind::InvalidState`](crate::ErrorKind::InvalidState);
	/// - an entry an extraction would skip (an unsafe path, an unsupported method, overlapping
	///   data), with the kind its reason maps to;
	/// - an id of another archive ([`ErrorKind::InvalidState`](crate::ErrorKind::InvalidState));
	/// - a 7z entry stored after more of its solid block than `config.max_solid_skip` allows
	///   ([`ErrorKind::ArchiveSolidSkipExceeded`](crate::ErrorKind::ArchiveSolidSkipExceeded)),
	///   before any of its block is fetched;
	/// - a tar or a single compressed file
	///   ([`ErrorKind::ArchiveUnsupported`](crate::ErrorKind::ArchiveUnsupported)), which are
	///   only read front to back.
	///
	/// The password is checked on this entry alone, as it is read: a plain entry needs none.
	/// The download takes one of the [`ArchiveConfig::job_concurrency`] archive job slots,
	/// reports its progress to `callback`, and can be paused and cancelled through `control`; a
	/// pause takes effect between two writes to `writer`. It takes no drive lock and creates
	/// nothing in the drive.
	///
	/// [`ArchiveConfig::job_concurrency`]: crate::fs::archive::ArchiveConfig::job_concurrency
	pub async fn download_archive_entry<W>(
		self: Arc<Self>,
		archive: RemoteFileType<'static>,
		entry: ArchiveEntryId,
		writer: &mut W,
		config: EntryDownloadConfig,
		callback: impl EntryDownloadCallback,
		control: JobControl,
	) -> Result<EntryDownloadReport, EntryDownloadFailed>
	where
		W: AsyncWrite + Unpin + MaybeSend,
	{
		let reporter = EntryDownloadReporter::new(callback, archive.size());
		let (archive, selection) = choose_entry(archive, entry, &reporter)?;
		let archives = self.archives().clone();
		let EntryDownloadConfig {
			expansion_limit,
			password,
			max_solid_skip,
		} = config;
		// the entry was chosen by its id, so macOS metadata is not left out
		let extraction = ExtractConfig {
			max_bytes: None,
			max_items: None,
			expansion_limit,
			password,
			skip_mac_metadata: false,
		};
		let task = Task::Download {
			selection,
			max_solid_skip,
		};
		let start = start_codec(&archive, &archives, extraction, task);
		run_download(
			DownloadTask {
				backend: Arc::new(ClientBackend::new(self)),
				control,
				reporter,
				archive,
				ordinal: u64::from(entry.index),
				config: archives,
				start,
			},
			writer,
		)
		.await
	}
}

/// Starts the codec that reads `archive` for `task`, under the client's `archives` settings and
/// the job's `config`.
fn start_codec(
	archive: &RemoteFileType<'static>,
	archives: &ArchiveConfig,
	config: ExtractConfig,
	task: Task,
) -> CodecStart<CodecResult> {
	let job = StreamJob {
		name: archive.name().unwrap_or_default().to_owned(),
		len: archive.size(),
		limits: CodecLimits {
			decoder_memory: archives.codec_mem_budget(),
			max_members: archives.max_members,
			expansion: config.expansion_limit,
			max_index_bytes: archives.max_index_bytes,
			max_bytes: config.max_bytes,
		},
		password: config.password,
		skip_mac_metadata: config.skip_mac_metadata,
		task,
	};
	Box::new(move || worker::start(move |port| extract_stream(&port, job)))
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::{
		ErrorKind,
		fs::{
			archive::extract::{
				ArchiveEntry, EntryDownloadUpdate, EntrySelection, ExtractRoot, ExtractUpdate,
				ExtractedTopLevel, ListUpdate,
			},
			categories::{DirType, Normal},
			drive_job::test_support::remote_file,
		},
	};

	#[expect(dead_code, reason = "only named by the compile-time checks below")]
	struct Ignore;

	impl ExtractCallback for Ignore {
		fn on_top_level_batch(&self, _: Vec<ExtractedTopLevel>) {}
		fn on_update(&self, _: ExtractUpdate) {}
	}

	impl ListCallback for Ignore {
		fn on_entries_batch(&self, _: Vec<ArchiveEntry>) {}
		fn on_update(&self, _: ListUpdate) {}
	}

	impl EntryDownloadCallback for Ignore {
		fn on_update(&self, _: EntryDownloadUpdate) {}
	}

	#[test]
	fn the_entries_chosen_are_checked_before_anything_runs() {
		let archive = Uuid::from_u128(1);
		let file = remote_file(archive, Uuid::from_u128(2), "a.tar", b"", None);
		let id = |archive, index| ArchiveEntryId { archive, index };
		let chosen = |ids| EntrySelection::new(file.clone(), ids, Vec::new());
		assert!(chosen(vec![id(archive, 3), id(archive, 1)]).is_ok());
		for ids in [vec![], vec![id(archive, 0), id(Uuid::from_u128(2), 0)]] {
			assert_eq!(
				chosen(ids.clone()).unwrap_err().kind(),
				ErrorKind::InvalidState,
				"{ids:?}"
			);
		}
	}

	/// The bindings run the extraction, the listing and the download on the SDK's
	/// multi-threaded runtime, which needs `Send` futures.
	fn _extract_future_is_send(
		client: Arc<Client>,
		archive: RemoteFileType<'static>,
		destination: DirType<'static, Normal>,
	) {
		fn assert_send<T: Send>(_: T) {}
		assert_send(client.clone().extract_archive(
			ExtractRequest {
				what: ExtractWhat::All(ArchiveSource::Keep(archive.clone())),
				destination,
				root: ExtractRoot::Destination,
			},
			ExtractConfig::default(),
			Ignore,
			JobControl::default(),
		));
		assert_send(client.clone().list_archive(
			archive.clone(),
			ListConfig::default(),
			Ignore,
			JobControl::default(),
		));
		let entry = ArchiveEntryId {
			archive: archive.uuid(),
			index: 0,
		};
		assert_send(client.download_archive_entry(
			archive,
			entry,
			&mut futures::io::sink(),
			EntryDownloadConfig::default(),
			Ignore,
			JobControl::default(),
		));
	}
}
