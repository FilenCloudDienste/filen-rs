//! The resuming upload of a local file: one pass over the source, which hashes every chunk and
//! sends those the journal does not record as acknowledged, then `upload/done`.

use std::{
	future::poll_fn,
	io::{Seek as _, SeekFrom},
	path::{Path, PathBuf},
	pin::pin,
	task::{Context, Waker},
};

use chrono::{DateTime, Utc};
use filen_types::{crypto::Blake3Hash, fs::Uuid};
use futures::{StreamExt as _, stream::FuturesUnordered};
use tokio::{sync::SemaphorePermit, time::Instant};
use tracing::{debug, warn};

use super::{
	JOURNAL_FLUSH_CHUNKS, JOURNAL_FLUSH_INTERVAL, MIN_JOURNALED_CHUNKS,
	backend::ResumeBackend,
	policy::{ResumeDecision, SourceStamp, decide},
	record::{UploadJournalRecord, open, seal},
	store::{UploadJournalId, UploadJournalStore},
};
use crate::{
	Error, ErrorKind,
	consts::{CHUNK_SIZE, CHUNK_SIZE_U64, FILE_CHUNK_SIZE_EXTRA},
	crypto::{self, shared::MetaCrypter},
	error::ErrorExt as _,
	fs::file::{
		BaseFile, FileBuilder, RemoteFile,
		chunk::Chunk,
		client_impl::build_exif_tee_from_builder,
		exif::ExifTeeState,
		write::{RemoteFileInfo, UploadCompletion},
	},
	io::{
		FilenMetaExt,
		client_impl::{fill_from, on_blocking_thread},
	},
	progress::ThrottledProgress,
	util::{MaybeArc, MaybeSendCallback},
};

/// The local file a resuming upload sends, opened and stated once by the caller.
pub(crate) struct ResumeSource {
	/// Resolved: its name is set, and its created and modified times are the file's unless the
	/// caller set them.
	pub(crate) builder: FileBuilder,
	pub(crate) file: std::fs::File,
	pub(crate) meta: std::fs::Metadata,
	pub(crate) path: PathBuf,
}

/// How a resuming upload reports progress: `callback` gets the bytes sent, `on_resumed`, once
/// before anything is sent, the bytes an earlier upload sent that are not sent again. When a
/// refused chunk shows that the upload was already finished, `on_resumed` gets the bytes this one
/// did not send too, so the two add up to the size.
pub(crate) struct ResumeCallbacks<'a> {
	pub(crate) callback: Option<MaybeSendCallback<'a, u64>>,
	pub(crate) on_resumed: Option<MaybeSendCallback<'a, u64>>,
}

/// Why `upload/done` refused an upload, by the server's error code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UploadDoneRejection {
	/// `one_or_more_chunks_missing`: the journal is out of date, or the chunks expired.
	ChunksMissing,
	/// `chunk_mismatch`: fewer chunks were declared than sent.
	ChunkCountTooLow,
	/// `upload_chunks_not_matching`: more chunks were declared than sent.
	ChunkCountTooHigh,
	/// `invalid_upload_key`: the chunks were sent under another upload key.
	InvalidUploadKey,
}

impl UploadDoneRejection {
	pub(crate) fn of(error: &Error) -> Option<Self> {
		match error.server_code()?.as_str() {
			"one_or_more_chunks_missing" => Some(Self::ChunksMissing),
			"chunk_mismatch" => Some(Self::ChunkCountTooLow),
			"upload_chunks_not_matching" => Some(Self::ChunkCountTooHigh),
			"invalid_upload_key" => Some(Self::InvalidUploadKey),
			_ => None,
		}
	}
}

/// Whether a chunk upload failed because the upload was already finished: the server refuses
/// chunks of an upload `upload/done` registered.
fn is_blacklisted(error: &Error) -> bool {
	error.server_code().as_deref() == Some("upload_blacklisted")
}

/// Uploads `source`, resuming the upload `journal` records for it when that upload can still be
/// finished, and journaling this one as its chunks are acknowledged. Returns the uploaded file and
/// the source, read to its end.
pub(crate) async fn upload_resuming<B: ResumeBackend>(
	backend: &B,
	source: ResumeSource,
	journal: &dyn UploadJournalStore,
	callbacks: ResumeCallbacks<'_>,
) -> Result<(RemoteFile, std::fs::File), Error> {
	let ResumeSource {
		builder,
		file: mut source,
		meta,
		path,
	} = source;
	let stamp = SourceStamp::of(&meta);
	let (mut exif_tee, mut file) = build_exif_tee_from_builder(builder);
	let id = UploadJournalId::new(&path, file.parent(), file.name());
	let chunks = stamp.size.div_ceil(CHUNK_SIZE_U64);
	let journal = Journal {
		store: journal,
		id,
		path: &path,
	};
	let crypter = backend.crypter();
	let uploader = Uploader {
		backend,
		crypter: &*crypter,
		// A file of a few chunks is quick to send again, and not worth a journal.
		journal: (chunks >= MIN_JOURNALED_CHUNKS).then_some(&journal),
		path: &path,
		size: stamp.size,
		modified: FilenMetaExt::modified(&meta),
		progress: ThrottledProgress::new(callbacks.callback),
	};
	if uploader.journal.is_none() {
		journal.remove().await;
	}

	let resumable = match uploader.journal {
		Some(journal) => {
			journal
				.load_resumable(&*crypter, stamp, &file, backend.now())
				.await
		}
		None => None,
	};
	let (mut record, mut resumed) = match resumable {
		Some(record) => {
			if let Some(existing) = backend.find_file(record.uuid()).await? {
				// An earlier upload/done went through, and its answer was lost.
				debug!(
					"the upload of {} was already finished, as {}",
					path.display(),
					existing.uuid
				);
				backend.propagate_existing(&existing).await?;
				journal.remove().await;
				report(callbacks.on_resumed.as_ref(), stamp.size);
				return Ok((existing, source));
			}
			file.root.uuid = record.uuid();
			file.root.key = *record.key();
			(record, true)
		}
		None => (fresh_record(id, &file, stamp)?, false),
	};
	if resumed && let Some(acked) = record.acked_bytes() {
		report(callbacks.on_resumed.as_ref(), acked);
	}

	// The times of the first pass that read the whole source, EXIF's if the parser ran, for a
	// pass started over without the parser: the source is unchanged.
	let mut final_times = None;
	loop {
		let read = match uploader
			.pass(&file, &mut record, source, exif_tee.take())
			.await?
		{
			PassOutcome::Read(read) => read,
			PassOutcome::Blacklisted { error, source } => {
				return match backend.find_file(file.uuid()).await? {
					Some(existing) => {
						backend.propagate_existing(&existing).await?;
						journal.remove().await;
						// The finished upload holds the chunks this one did not send: each chunk
						// sent was acknowledged, and reported, before the refusal.
						if let Some(progress) = &uploader.progress {
							progress.flush();
						}
						if let Some(acked) = record.acked_bytes() {
							report(
								callbacks.on_resumed.as_ref(),
								stamp.size.saturating_sub(acked),
							);
						}
						Ok((existing, source))
					}
					None => {
						journal.remove().await;
						Err(error)
					}
				};
			}
		};
		source = read.source;
		let final_times = *final_times.get_or_insert_with(|| {
			read.exif_times
				.unwrap_or((file.created(), file.last_modified()))
		});
		let completion = UploadCompletion {
			written: stamp.size,
			num_chunks: chunks,
			hash: read.hash,
			final_times,
		};
		uploader.save(&mut record).await;
		let info = record.stored_at().cloned().unwrap_or_default();
		let error = match backend
			.register(&file, record.upload_key(), completion, info)
			.await
		{
			Ok(uploaded) => {
				journal.remove().await;
				return Ok((uploaded, source));
			}
			Err(error) => error,
		};
		match UploadDoneRejection::of(&error) {
			Some(UploadDoneRejection::ChunksMissing | UploadDoneRejection::InvalidUploadKey) => {
				journal.remove().await;
				// Nothing is sent again under the old upload key: the server's behaviour past the
				// chunks' expiry is unknown. A fresh upload is started once.
				if !resumed {
					return Err(error);
				}
				warn!(
					"the server refused to finish the resumed upload of {} ({error}), uploading it \
					 from the start",
					path.display()
				);
				resumed = false;
				file.root.uuid = Uuid::new_v4();
				file.root.key = backend.make_file_key();
				record = fresh_record(id, &file, stamp)?;
				source = rewind(source).await?;
			}
			Some(
				UploadDoneRejection::ChunkCountTooLow | UploadDoneRejection::ChunkCountTooHigh,
			) => {
				journal.remove().await;
				return Err(error);
			}
			// The upload stays resumable.
			None => return Err(error),
		}
	}
}

/// A record of a new upload of `file`, under a new upload key.
fn fresh_record(
	id: UploadJournalId,
	file: &BaseFile,
	stamp: SourceStamp,
) -> Result<UploadJournalRecord, Error> {
	let upload_key = crypto::shared::generate_random_base64_values(32, &mut rand::rng());
	UploadJournalRecord::new(id, file, upload_key, stamp, CHUNK_SIZE_U64)
}

fn report(callback: Option<&MaybeSendCallback<'_, u64>>, bytes: u64) {
	if let Some(callback) = callback
		&& bytes > 0
	{
		callback(bytes);
	}
}

async fn rewind(source: std::fs::File) -> Result<std::fs::File, Error> {
	let (source, _) = on_blocking_thread(source, Vec::new(), |source, _| {
		source.seek(SeekFrom::Start(0)).map(drop)
	})
	.await?;
	Ok(source)
}

/// Where one upload's journal is kept.
struct Journal<'a> {
	store: &'a dyn UploadJournalStore,
	id: UploadJournalId,
	path: &'a Path,
}

impl Journal<'_> {
	/// The journaled upload of the source, if it can be resumed as the upload of `file` from a
	/// source looking like `stamp`. A journal that cannot be is removed.
	async fn load_resumable(
		&self,
		crypter: &impl MetaCrypter,
		stamp: SourceStamp,
		file: &BaseFile,
		now: DateTime<Utc>,
	) -> Option<UploadJournalRecord> {
		let sealed = match self.store.load(&self.id).await {
			Ok(sealed) => sealed?,
			Err(e) => {
				warn!(
					"failed to load the journal of the upload of {}, uploading it from the start: {e}",
					self.path.display()
				);
				return None;
			}
		};
		let record = match open(&sealed, &self.id, crypter).await {
			Ok(record) => record,
			Err(cause) => {
				warn!(
					"removing the unreadable journal of the upload of {}: {cause}",
					self.path.display()
				);
				self.remove().await;
				return None;
			}
		};
		match decide(&record, stamp, file, now) {
			ResumeDecision::Resume => Some(record),
			ResumeDecision::StartFresh(reason) => {
				debug!(
					"uploading {} from the start ({reason:?})",
					self.path.display()
				);
				self.remove().await;
				None
			}
		}
	}

	/// Removes the journal. Failing to is logged: a journal left behind is dropped when it no
	/// longer matches the source, or when it is too old.
	async fn remove(&self) {
		if let Err(e) = self.store.remove(&self.id).await {
			warn!(
				"failed to remove the journal of the upload of {}: {e}",
				self.path.display()
			);
		}
	}
}

/// What a pass over the source ended with.
enum PassOutcome {
	/// Every chunk was read and acknowledged.
	Read(ReadSource),
	/// A chunk was refused because the upload was already finished.
	Blacklisted { error: Error, source: std::fs::File },
}

struct ReadSource {
	source: std::fs::File,
	hash: Blake3Hash,
	/// The created and modified times the EXIF parser found, when it ran.
	exif_times: Option<(DateTime<Utc>, DateTime<Utc>)>,
}

/// When a pass last saved its journal.
struct FlushState {
	/// Chunks acknowledged since.
	unsaved: u64,
	saved_at: Instant,
}

impl FlushState {
	fn due(&self) -> bool {
		self.unsaved >= JOURNAL_FLUSH_CHUNKS || self.saved_at.elapsed() >= JOURNAL_FLUSH_INTERVAL
	}
}

/// A chunk's upload: its index, length and the server's answer.
type SentChunk = (u64, u64, Result<RemoteFileInfo, Error>);

/// One resuming upload's fixed parts.
struct Uploader<'a, B, C> {
	backend: &'a B,
	crypter: &'a C,
	/// `None` for a source too small to journal.
	journal: Option<&'a Journal<'a>>,
	path: &'a Path,
	size: u64,
	modified: DateTime<Utc>,
	progress: Option<MaybeArc<ThrottledProgress<'a>>>,
}

impl<B, C> Uploader<'_, B, C>
where
	B: ResumeBackend,
	C: MetaCrypter,
{
	/// Reads `source` from its start, hashing every chunk and sending those `record` does not
	/// hold as acknowledged, and records each one acknowledged.
	async fn pass(
		&self,
		file: &BaseFile,
		record: &mut UploadJournalRecord,
		mut source: std::fs::File,
		mut exif_tee: Option<ExifTeeState>,
	) -> Result<PassOutcome, Error> {
		// The chunk uploads borrow it while the record they update is borrowed mutably.
		let upload_key = record.upload_key().to_owned();
		let mut flush = FlushState {
			unsaved: 0,
			saved_at: Instant::now(),
		};
		let mut active_since = Instant::now();
		let mut hasher = blake3::Hasher::new();
		let mut in_flight = FuturesUnordered::new();
		let mut read = 0;
		let mut idx = 0;
		while read < self.size {
			let len = CHUNK_SIZE.min(usize::try_from(self.size - read).unwrap_or(usize::MAX));
			let budget = FILE_CHUNK_SIZE_EXTRA.saturating_add(
				u32::try_from(len).expect("a chunk is at most 1 MiB (should be impossible)"),
			);
			let mut acquire = pin!(Chunk::acquire(budget, self.backend.state()));
			// The uploads in flight hold the budget, so their answers are taken while waiting.
			let chunk = loop {
				tokio::select! {
					biased;
					Some(sent) = in_flight.next(), if !in_flight.is_empty() => {
						if let Some(error) = self
							.on_sent(sent, record, &mut flush, &mut active_since)
							.await?
						{
							return Ok(PassOutcome::Blacklisted { error, source });
						}
					}
					chunk = &mut acquire => break chunk,
				}
			};
			let (data, permit) = chunk.into_parts();
			let (returned, data) = on_blocking_thread(source, data, move |source, data| {
				fill_from(source, data, len)
			})
			.await?;
			source = returned;
			if data.len() < len {
				return Err(self.changed().await);
			}
			hasher.update_rayon(&data);
			if let Some(tee) = exif_tee.as_mut() {
				// Backpressure first, then hand the bytes over once: on Pending the tee keeps
				// them, and the next wait sends them first.
				poll_fn(|cx| tee.poll_tee(cx, &[])).await;
				let _ = tee.poll_tee(&mut Context::from_waker(Waker::noop()), &data);
			}
			read += data.len() as u64;
			if !record.acked().contains(idx) {
				in_flight.push(send_chunk(
					self.backend,
					file,
					&upload_key,
					idx,
					data,
					permit,
				));
			}
			idx += 1;
		}
		while let Some(sent) = in_flight.next().await {
			if let Some(error) = self
				.on_sent(sent, record, &mut flush, &mut active_since)
				.await?
			{
				return Ok(PassOutcome::Blacklisted { error, source });
			}
		}
		record.add_active_millis(elapsed_millis(active_since));

		// The read that finds the end: a byte there means the source grew.
		let (source, past_end) =
			on_blocking_thread(source, Vec::with_capacity(1), |source, data| {
				fill_from(source, data, 1)
			})
			.await?;
		if !past_end.is_empty() {
			return Err(self.changed().await);
		}
		let path = self.path.to_owned();
		let (stat, path) = tokio::task::spawn_blocking(move || (std::fs::metadata(&path), path))
			.await
			.map_err(std::io::Error::from)?;
		let stat = stat.map_err(|e| {
			e.with_context(format!(
				"File at path {} was modified during upload",
				path.display()
			))
		})?;
		if FilenMetaExt::modified(&stat) != self.modified {
			return Err(self.changed().await);
		}

		if let Some(progress) = &self.progress {
			progress.flush();
		}
		let exif_times = match exif_tee.as_mut() {
			Some(tee) => match poll_fn(|cx| tee.poll_finalize(cx)).await {
				Ok(times) => Some(times),
				Err(e) => {
					// A file whose EXIF does not parse is uploaded with its own times.
					debug!("EXIF tee failed to finalize: {e}");
					None
				}
			},
			None => None,
		};
		Ok(PassOutcome::Read(ReadSource {
			source,
			hash: Blake3Hash::from(hasher.finalize()),
			exif_times,
		}))
	}

	/// Takes a chunk upload's answer: records an acknowledged chunk, saving the journal when it is
	/// due, and hands back a refusal of a finished upload. Any other failure fails the pass, after
	/// the journal is saved with what was acknowledged before it.
	async fn on_sent(
		&self,
		(idx, len, answer): SentChunk,
		record: &mut UploadJournalRecord,
		flush: &mut FlushState,
		active_since: &mut Instant,
	) -> Result<Option<Error>, Error> {
		let info = match answer {
			Ok(info) => info,
			Err(e) if is_blacklisted(&e) => return Ok(Some(e)),
			Err(e) => {
				record.add_active_millis(elapsed_millis(*active_since));
				*active_since = Instant::now();
				self.save(record).await;
				return Err(e);
			}
		};
		record.ack(idx, info, self.backend.now())?;
		if let Some(progress) = &self.progress {
			progress.report(len);
		}
		flush.unsaved += 1;
		if flush.due() {
			record.add_active_millis(elapsed_millis(*active_since));
			*active_since = Instant::now();
			self.save(record).await;
			*flush = FlushState {
				unsaved: 0,
				saved_at: Instant::now(),
			};
		}
		Ok(None)
	}

	/// Seals and saves `record`, when the source is journaled and a chunk was acknowledged.
	/// Failing to is logged: the upload goes on, and a later resume only sends more again.
	async fn save(&self, record: &mut UploadJournalRecord) {
		let Some(journal) = self.journal else {
			return;
		};
		if record.first_ack_at().is_none() {
			return;
		}
		let saved = match seal(record, self.crypter).await {
			Ok(sealed) => journal.store.save(&journal.id, &sealed).await,
			Err(e) => Err(e),
		};
		if let Err(e) = saved {
			warn!(
				"failed to save the journal of the upload of {}: {e}",
				self.path.display()
			);
		}
	}

	/// The source changed while it was read: its journal is removed, and the upload fails.
	async fn changed(&self) -> Error {
		if let Some(journal) = self.journal {
			journal.remove().await;
		}
		Error::custom(
			ErrorKind::FileChangedDuringSync,
			format!(
				"File at path {} was modified during upload",
				self.path.display()
			),
		)
	}
}

/// Uploads chunk `idx` of `file` from `data`, holding `permit`, its share of the memory budget,
/// until the server answers.
async fn send_chunk<'a, B: ResumeBackend>(
	backend: &'a B,
	file: &'a BaseFile,
	upload_key: &'a str,
	idx: u64,
	data: Vec<u8>,
	permit: SemaphorePermit<'a>,
) -> SentChunk {
	let len = data.len() as u64;
	let answer = backend.upload_chunk(file, upload_key, idx, data).await;
	drop(permit);
	(idx, len, answer.map(|(_, info)| info))
}

fn elapsed_millis(since: Instant) -> u64 {
	u64::try_from(since.elapsed().as_millis()).unwrap_or(u64::MAX)
}
