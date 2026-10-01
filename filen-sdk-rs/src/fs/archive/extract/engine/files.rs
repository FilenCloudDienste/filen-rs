//! The files of an extraction: each uploaded chunk by chunk as its data comes in, then registered
//! once all of it is up and its directory exists, in archive order.

use std::{borrow::Cow, sync::Arc};

use chrono::Utc;
use filen_types::{crypto::Blake3Hash, fs::Uuid};
use tokio::sync::OwnedSemaphorePermit;

use crate::{
	Error, ErrorKind,
	consts::MAX_SMALL_PARALLEL_REQUESTS,
	fs::{
		HasUUID,
		archive::{
			dispose::{DisposalBackend, file_digest},
			extract::{
				report::{
					ExtractActiveFile, ExtractFailure, ExtractRenameReason, ExtractReport,
					ExtractRetry, ExtractStage, ExtractTopLevelKey, Reporter,
				},
				storage_exceeded,
			},
			input::take_memory,
			names::ROOT,
		},
		categories::NonRootItemType,
		drive_job::{
			CHUNKS_PER_FILE,
			backend::UploadSpec,
			finalize::{
				FinalizeError, FinalizeTask, Finalized, UnlessPaused,
				finalize_new_file_unless_paused,
			},
			name_retry::NameRetry,
		},
		file::write::{RemoteFileInfo, UploadCompletion},
		name::keep_both::NameShape,
	},
	util::MaybeSendBoxFuture,
};

use super::{
	Driver, FilePhase, FileSlot, FileSource, LinkPhase, NewFile, SlotSource, dirs::DirState, record,
};

impl<B: DisposalBackend> Driver<B> {
	pub(super) fn open_file(&mut self, new: NewFile) {
		let NewFile {
			ordinal,
			entry,
			path,
			parent,
			name,
			size,
			modified,
			source,
			link_key,
		} = new;
		let dest_uuid = Uuid::new_v4();
		let parent_uuid = self.dirs.slots[parent].uuid;
		let upload = self.backend.begin_upload(UploadSpec {
			uuid: dest_uuid,
			parent: parent_uuid,
			// the upload owns its name; the file may be renamed again before it is registered
			name: name.clone(),
			mime: None,
		});
		let active = ExtractActiveFile {
			entry,
			dest_uuid,
			dest_parent: parent_uuid,
			name: name.as_ref().to_owned(),
			size,
			bytes_done: 0,
		};
		self.files.insert(
			ordinal,
			FileSlot {
				path,
				parent,
				upload: Arc::new(upload),
				active: active.clone(),
				name,
				hasher: blake3::Hasher::new(),
				written: 0,
				next_index: 0,
				uploading: 0,
				info: None,
				modified,
				phase: FilePhase::Receiving,
				link_key,
				source: match source {
					FileSource::Codec => SlotSource::Codec,
					FileSource::Link { .. } => SlotSource::Link(LinkPhase::Resolving),
				},
			},
		);
		match source {
			FileSource::Codec => self.current = Some(ordinal),
			FileSource::Link { target } => {
				let backend = Arc::clone(&self.backend);
				let op = self.reporter.op();
				self.links.sources.push(Box::pin(async move {
					let result = backend.normal_item(target, false).await;
					drop(op);
					(ordinal, result)
				}));
			}
		}
		if let DirState::Failed(error) = &self.dirs.slots[parent].state {
			let error = Arc::clone(error);
			self.fail_file(ordinal, ExtractStage::CreateDirectory, error);
		} else {
			self.reporter.file_started(active);
		}
	}

	/// The current file's data ended.
	pub(super) fn end_file(&mut self) {
		// a file's events after the job stopped taking it are dropped
		let Some(ordinal) = self.current else {
			return;
		};
		let Some(file) = self.files.get_mut(&ordinal) else {
			return;
		};
		file.phase = file.phase.end();
		self.current = None;
		self.finalize_ready();
	}

	/// Uploads the current file's next data, or holds it until it can be.
	pub(super) fn take_data(&mut self, data: Vec<u8>) {
		// a file's events after the job stopped taking it are dropped
		let Some(ordinal) = self.current else {
			return;
		};
		let Some(file) = self.files.get(&ordinal) else {
			return;
		};
		if file.failed() {
			return;
		}
		let waits = self.dirs.slots[file.parent].created_uuid().is_none()
			|| file.uploading >= CHUNKS_PER_FILE;
		let permit = if waits {
			None
		} else {
			take_memory(&self.output_slot, &self.memory)
		};
		let Some(permit) = permit else {
			self.held = Some(data);
			return;
		};
		self.upload(ordinal, data, permit);
	}

	/// Uploads `data` as the next chunk of file `ordinal`, in the memory `permit` holds.
	pub(super) fn upload(&mut self, ordinal: u64, data: Vec<u8>, permit: OwnedSemaphorePermit) {
		let len = data.len() as u64;
		if let Some(error) = storage_exceeded(self.max_bytes, self.committed + len) {
			self.stop_with(error);
			return;
		}
		self.committed += len;
		let file = self
			.files
			.get_mut(&ordinal)
			.expect("an uploading file is known");
		file.hasher.update_rayon(&data);
		file.written += len;
		file.uploading += 1;
		let index = file.next_index;
		file.next_index += 1;
		let upload = Arc::clone(&file.upload);
		let backend = Arc::clone(&self.backend);
		let op = self.reporter.op();
		self.uploads.push(Box::pin(async move {
			let result = backend.upload_chunk(&upload, index, data).await;
			drop((permit, op));
			(ordinal, len, result)
		}) as MaybeSendBoxFuture<'static, _>);
	}

	pub(super) fn upload_finished(
		&mut self,
		ordinal: u64,
		len: u64,
		result: Result<RemoteFileInfo, Error>,
	) {
		let Some(file) = self.files.get_mut(&ordinal) else {
			return;
		};
		file.uploading -= 1;
		let (failed, dest_uuid) = (file.failed(), file.active.dest_uuid);
		match result {
			Ok(info) => {
				file.info = Some(info);
				if !failed {
					self.reporter.chunk_uploaded(dest_uuid, len);
				}
			}
			Err(error) => {
				let error = Arc::new(error);
				self.note_error(&error);
				if !failed {
					self.fail_file(ordinal, ExtractStage::Upload, error);
				}
			}
		}
		self.finalize_ready();
	}

	/// Reports file `ordinal` as failed; its later data is dropped.
	pub(super) fn fail_file(&mut self, ordinal: u64, stage: ExtractStage, error: Arc<Error>) {
		let retry = self.file_retry(&self.files[&ordinal]);
		let file = self
			.files
			.get_mut(&ordinal)
			.expect("a failed file is known");
		file.phase = FilePhase::Failed {
			ended: match file.phase {
				FilePhase::Receiving => false,
				FilePhase::Ended | FilePhase::Finalizing => true,
				FilePhase::Failed { ended } => ended,
			},
		};
		report_file_failure(&mut self.report, &self.reporter, file, stage, error, retry);
		self.link_target_failed(ordinal);
	}

	/// Where `file`, which failed, is extracted again: `None` for a tar's hard link, whose copy
	/// needs the file it names read in the same pass (see [`ExtractFailure::retry`]).
	fn file_retry<U>(&self, file: &FileSlot<U>) -> Option<ExtractRetry> {
		matches!(file.source, SlotSource::Codec).then(|| self.retry(file.parent))
	}

	/// Registers the files whose data is all up and whose directory exists, as many at once as
	/// other small requests; forgets failed files with nothing left in flight.
	pub(super) fn finalize_ready(&mut self) {
		// a finalize started now would park on the pause holding the job busy, so the job
		// could never go idle and give back its memory: it starts on resume instead
		if self.control.is_pause_requested() {
			return;
		}
		let ready: Vec<u64> = self
			.files
			.iter()
			.filter(|(_, file)| {
				file.uploading == 0
					&& match file.phase {
						FilePhase::Ended => self.dirs.slots[file.parent].created_uuid().is_some(),
						FilePhase::Failed { ended } => ended,
						FilePhase::Receiving | FilePhase::Finalizing => false,
					}
			})
			.map(|(ordinal, _)| *ordinal)
			.collect();
		for ordinal in ready {
			if self.files[&ordinal].failed() {
				self.files.remove(&ordinal);
			} else if self.finalizes.len() < MAX_SMALL_PARALLEL_REQUESTS {
				self.start_finalize(ordinal);
			}
		}
	}

	fn start_finalize(&mut self, ordinal: u64) {
		let file = self.files.get_mut(&ordinal).expect("checked by the caller");
		let top_level = file.parent == ROOT && self.into_destination;
		let parent = self.dirs.slots[file.parent]
			.created_uuid()
			.expect("checked by the caller");
		file.phase = FilePhase::Finalizing;
		let modified = file.modified.unwrap_or_else(Utc::now);
		let completion = UploadCompletion {
			written: file.written,
			num_chunks: file.next_index,
			hash: Blake3Hash::from(file.hasher.finalize()),
			final_times: (modified, modified),
		};
		let upload = Arc::clone(&file.upload);
		// copied, not taken: a finalize a pause stopped is started again with the same info
		let info = file.info.clone().unwrap_or_default();
		let name = file.name.clone();
		let backend = Arc::clone(&self.backend);
		let control = self.control.clone();
		let ops = self.reporter.ops();
		let targets = Arc::clone(&self.targets);
		self.finalizes.push(Box::pin(async move {
			let mut retry = NameRetry::new(NameShape::File, "item");
			let result = finalize_new_file_unless_paused(FinalizeTask {
				backend: &*backend,
				control: &control,
				ops: &ops,
				upload: &upload,
				parent,
				name,
				// a directory the job did not create may hold the name by now
				recheck: top_level.then_some(&mut retry),
				completion,
				info,
				targets: &targets,
			})
			.await;
			(ordinal, result)
		}) as MaybeSendBoxFuture<'static, _>);
	}

	pub(super) fn finalize_finished(&mut self, ordinal: u64, registration: UnlessPaused) {
		let result = match registration {
			UnlessPaused::Ran(result) => result,
			UnlessPaused::Paused => {
				// a pause came before the drive lock: started again on resume
				if let Some(file) = self.files.get_mut(&ordinal) {
					file.phase = FilePhase::Ended;
				}
				return;
			}
		};
		let Some(file) = self.files.remove(&ordinal) else {
			return;
		};
		match result {
			Ok(Finalized {
				file: registered,
				name,
				propagation_errors,
			}) => {
				self.report_propagation(registered.uuid(), propagation_errors);
				if name.as_ref() != file.archive_name() {
					self.renamed(
						file.active.entry,
						file.path,
						&name,
						ExtractRenameReason::DuplicateName,
					);
				}
				let active = ExtractActiveFile {
					name: name.as_ref().to_owned(),
					..file.active
				};
				self.reporter.file_done(&active, file.written);
				self.link_target_registered(
					ordinal,
					file.link_key,
					registered.uuid(),
					file.written,
				);
				self.created_digest = self
					.created_digest
					.wrapping_add(file_digest(active.dest_uuid, file.written));
				if file.parent == ROOT && self.into_destination {
					self.top_level_created(
						ExtractTopLevelKey::Entry {
							id: file.active.entry,
						},
						NonRootItemType::File(Cow::Owned(registered)),
					);
				}
			}
			Err(FinalizeError::Stopped) => self
				.reporter
				.file_abandoned(file.active.dest_uuid, file.bytes()),
			Err(FinalizeError::RegisteredAsVersion {
				file: registered,
				propagation_errors,
			}) => {
				self.link_target_failed(ordinal);
				self.report_propagation(registered.uuid(), propagation_errors);
				let error = Error::custom(
					ErrorKind::InvalidState,
					"the entry was registered as a new version of an existing file",
				);
				let stage = ExtractStage::RegisteredAsVersion {
					existing_file: registered.stable_uuid.into(),
				};
				self.finalize_failed(&file, stage, Arc::new(error));
			}
			Err(FinalizeError::Failed(error)) => {
				self.link_target_failed(ordinal);
				let error = Arc::new(error);
				self.note_error(&error);
				self.finalize_failed(&file, ExtractStage::Finalize, error);
			}
		}
	}

	/// Reports `file`, which could not be registered, as failed at `stage`.
	fn finalize_failed(
		&mut self,
		file: &FileSlot<B::Upload>,
		stage: ExtractStage,
		error: Arc<Error>,
	) {
		let retry = self.file_retry(file);
		report_file_failure(&mut self.report, &self.reporter, file, stage, error, retry);
	}
}

/// Records `file` as failed at `stage`, in `report` and as an event, to be tried again as
/// `retry` says.
fn report_file_failure<U>(
	report: &mut ExtractReport,
	reporter: &Reporter,
	file: &FileSlot<U>,
	stage: ExtractStage,
	error: Arc<Error>,
	retry: Option<ExtractRetry>,
) {
	let failure = ExtractFailure {
		entry: file.active.entry,
		path: file.path.clone(),
		dest_parent: file.active.dest_parent,
		dest_name: file.name.as_ref().to_owned(),
		stage,
		retry,
		error,
	};
	reporter.file_failed(
		Some(file.active.dest_uuid),
		file.bytes(),
		record(&mut report.failures, &mut report.omitted.failures, failure),
	);
}
