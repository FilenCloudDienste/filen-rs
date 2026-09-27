//! The public compress API on [`Client`]: lists and plans the sources, checks the archive can be
//! written and extracted again, and runs the job.

use std::sync::Arc;

use filen_types::fs::Uuid;

use crate::{
	Error, ErrorKind,
	auth::Client,
	fs::{
		HasUUID,
		archive::{
			limits::{MAX_ARCHIVE_PATH_BYTES, MAX_ARCHIVE_PATH_DEPTH},
			worker,
		},
		categories::{DirType, Normal},
		drive_job::{
			backend::ClientBackend,
			listing::{ItemSource, ListingBytes, ScanError, watch_listing},
			plan::{
				DestParent, ItemPlan, ItemPlanner, PlanRequest, PlanSource, RenameReason,
				RenamedEntry,
			},
		},
		file::traits::HasFileInfo,
		name::ValidatedName,
	},
	job::JobControl,
	util::MaybeArc,
};

use super::{
	CompressFormat,
	codec::{ArchiveEntry, CompressJob, compress, tar_size},
	engine::{CompressTask, Source, run_compress},
	report::{CompressCallback, CompressFailed, CompressPhase, CompressReport, Reporter},
};

#[derive(Debug, Clone)]
pub struct CompressConfig {
	pub format: CompressFormat,
	/// Storage still free on the account, if the caller knows it. A bare tar's size is known
	/// before anything is written, and one that would reach this is refused up front with
	/// [`ErrorKind::MaxStorageReached`], its size in the report's `needed_bytes`; a compressed
	/// archive is refused as soon as its written bytes would reach it, leaving nothing behind.
	pub max_bytes: Option<u64>,
}

impl Client {
	/// Compresses `sources` into a new archive called `name` in `destination`. Nothing can
	/// encode on the server (items are end-to-end encrypted), so every source is downloaded and
	/// encoded here, and the archive is encrypted and uploaded as one new file.
	///
	/// `name` is the archive's whole name and has to carry the format's extension (see
	/// [`CompressFormat::extension`]); a name the destination holds gets the next keep-both name
	/// (`photos (1).tar.gz`). Sources may be the user's own items, shared ones or linked ones.
	/// Undecryptable files are skipped and names that collide inside the archive renamed, both
	/// reported; a path longer or deeper than an archive may hold fails the job before anything
	/// is written, so every archive the SDK writes can be extracted by it again.
	///
	/// The archive only becomes visible once all of it is uploaded, so a job that ends early
	/// leaves nothing behind. It reports its progress to `callback` and can be paused, resumed
	/// and cancelled through `control`.
	pub async fn compress_items(
		self: Arc<Self>,
		sources: Vec<ItemSource>,
		destination: DirType<'static, Normal>,
		name: ValidatedName,
		config: CompressConfig,
		callback: impl CompressCallback,
		control: JobControl,
	) -> Result<CompressReport, CompressFailed> {
		let reporter = Reporter::new(callback);
		let archives = self.client().state().archives().clone();
		let refuse = |report: CompressReport, phase, error: Error| {
			reporter.finish_unstarted(phase, report.totals);
			CompressFailed {
				report: CompressReport {
					counts: reporter.counts(),
					..report
				},
				error: Arc::new(error),
			}
		};
		let checked = config
			.format
			.check_name(name.as_ref())
			.and_then(|extension_len| {
				let memory = config.format.encoder_memory()?;
				if memory > archives.codec_mem_budget {
					return Err(Error::custom(
						ErrorKind::InsufficientMemory,
						format!(
							"this format needs {memory} bytes of codec memory, over the {} allowed",
							archives.codec_mem_budget
						),
					));
				}
				Ok(extension_len)
			});
		let extension_len = match checked {
			Ok(extension_len) => extension_len,
			Err(error) => {
				return Err(refuse(
					CompressReport::default(),
					CompressPhase::Failed,
					error,
				));
			}
		};

		let plan = match self.plan_sources(sources, &reporter, &control).await {
			Ok(plan) => plan,
			Err(ScanError::Stopped) => {
				let error = Error::custom(ErrorKind::Cancelled, "compression cancelled");
				return Err(refuse(
					CompressReport::default(),
					CompressPhase::Cancelled,
					error,
				));
			}
			Err(ScanError::Failed(error)) => {
				return Err(refuse(
					CompressReport::default(),
					CompressPhase::Failed,
					error,
				));
			}
		};
		let mut report = CompressReport {
			skipped: plan.skipped.clone(),
			renamed: plan.renamed.clone(),
			totals: plan.totals,
			..CompressReport::default()
		};
		report.renamed.extend(top_level_renames(&plan));
		let (entries, sources) = match archive_entries(&plan, config.format) {
			Ok(entries) => entries,
			Err(error) => return Err(refuse(report, CompressPhase::Failed, error)),
		};
		if let (CompressFormat::Tar { compression: None }, Some(max)) =
			(config.format, config.max_bytes)
		{
			let needed = tar_size(&entries);
			if needed >= max {
				report.needed_bytes = Some(needed);
				let error = Error::custom(
					ErrorKind::MaxStorageReached,
					format!("the archive needs {needed} bytes but only {max} are free"),
				);
				return Err(refuse(report, CompressPhase::Failed, error));
			}
		}
		reporter.set_plan(plan.totals, &report.skipped, &report.renamed);

		let job = CompressJob {
			format: config.format,
			entries,
		};
		run_compress(CompressTask {
			backend: Arc::new(ClientBackend::new(self)),
			control,
			reporter,
			destination,
			name,
			extension_len,
			sources,
			max_bytes: config.max_bytes,
			config: archives,
			start: Box::new(move || worker::start(move |port| compress(&port, job))),
			report,
		})
		.await
	}

	/// Lists every source directory and plans the archive's entries under a root of its own.
	async fn plan_sources(
		&self,
		sources: Vec<ItemSource>,
		reporter: &MaybeArc<Reporter>,
		control: &JobControl,
	) -> Result<ItemPlan<crate::fs::drive_job::listing::ItemSourceDir>, ScanError> {
		let sources_total = sources
			.iter()
			.filter(|source| matches!(source, ItemSource::Dir(_)))
			.count() as u64;
		let bytes = ListingBytes::default();
		let ops = reporter.ops();
		let mut sources_done = 0;
		let report = |sources_done| reporter.set_scan(bytes.scan(sources_done, sources_total));
		report(sources_done);

		// the archive's root: nothing is in it yet, and nothing is listed for it
		let root = Uuid::new_v4();
		let mut planner = ItemPlanner::default();
		planner.add_destination(root, std::iter::empty());
		let mut requests = Vec::with_capacity(sources.len());
		for source in sources {
			let source = match source {
				ItemSource::File(file) => PlanSource::File(file),
				ItemSource::Dir(dir) => {
					reporter.checkpoint(control).await?;
					bytes.next_source();
					let listing = self.list_item_source(dir, &bytes);
					let source = watch_listing(listing, &ops, control, || report(sources_done))
						.await?
						.map_err(ScanError::Failed)?;
					sources_done += 1;
					report(sources_done);
					source
				}
			};
			requests.push(PlanRequest {
				source,
				destination: root,
				name: None,
			});
		}
		planner.plan(requests).map_err(ScanError::Failed)
	}
}

/// Top-level items the planner gave keep-both names, since two sources had the same name.
fn top_level_renames<D>(plan: &ItemPlan<D>) -> Vec<RenamedEntry> {
	plan.top_level
		.iter()
		.filter_map(|top| {
			let (source_uuid, source_path, name) = match top.item {
				crate::fs::drive_job::plan::PlannedItem::Dir(index) => {
					let dir = &plan.dirs[index];
					(dir.source_uuid, &dir.source_path, &dir.name)
				}
				crate::fs::drive_job::plan::PlannedItem::File(index) => {
					let file = &plan.files[index];
					(file.source.uuid(), &file.source_path, &file.name)
				}
			};
			(source_path != name.as_ref()).then(|| RenamedEntry {
				source_uuid,
				source_path: source_path.clone(),
				name: name.clone(),
				reason: RenameReason::DuplicateName,
			})
		})
		.collect()
}

/// The archive's entries (directories parent first, then files) and the files the codec reads,
/// with every path checked against what extracting accepts.
fn archive_entries<D>(
	plan: &ItemPlan<D>,
	format: CompressFormat,
) -> Result<(Vec<ArchiveEntry>, Vec<Source>), Error> {
	if matches!(format, CompressFormat::Single { .. })
		&& (plan.files.len() != 1 || !plan.dirs.is_empty())
	{
		return Err(Error::custom(
			ErrorKind::InvalidState,
			"a single compressed file is made of exactly one file",
		));
	}
	let mut dir_paths: Vec<String> = Vec::with_capacity(plan.dirs.len());
	let path_in = |parent: DestParent, name: &ValidatedName, dir_paths: &[String]| match parent {
		DestParent::Existing(_) => name.as_ref().to_owned(),
		DestParent::Planned(index) => format!("{}/{}", dir_paths[index], name.as_ref()),
	};
	let mut entries = Vec::with_capacity(plan.dirs.len() + plan.files.len());
	for dir in &plan.dirs {
		let path = path_in(dir.parent, &dir.name, &dir_paths);
		// a directory's stored path ends in a `/`
		check_path(&path, 1)?;
		dir_paths.push(path.clone());
		entries.push(ArchiveEntry::Dir {
			path,
			modified: dir.created,
		});
	}
	let mut sources = Vec::with_capacity(plan.files.len());
	for (index, file) in plan.files.iter().enumerate() {
		let path = path_in(file.parent, &file.name, &dir_paths);
		check_path(&path, 0)?;
		entries.push(ArchiveEntry::File {
			source: u32::try_from(index).map_err(|_| {
				Error::custom(ErrorKind::InvalidState, "too many files for one archive")
			})?,
			path: path.clone(),
			size: file.size,
			modified: file.source.last_modified(),
		});
		sources.push(Source {
			file: file.source.clone(),
			path,
		});
	}
	Ok((entries, sources))
}

/// Refuses a path extracting would skip, with `extra` bytes stored after it.
fn check_path(path: &str, extra: usize) -> Result<(), Error> {
	if path.len() + extra > MAX_ARCHIVE_PATH_BYTES {
		return Err(Error::custom(
			ErrorKind::InvalidName,
			format!("a path in the archive is longer than {MAX_ARCHIVE_PATH_BYTES} bytes"),
		));
	}
	if path.split('/').count() > MAX_ARCHIVE_PATH_DEPTH {
		return Err(Error::custom(
			ErrorKind::InvalidName,
			format!("a path in the archive is more than {MAX_ARCHIVE_PATH_DEPTH} levels deep"),
		));
	}
	Ok(())
}

#[cfg(test)]
mod tests {
	use std::borrow::Cow;

	use super::*;
	use crate::{
		crypto::{file::FileKey, shared::CreateRandom, v3::EncryptionKey},
		fs::{
			archive::compress::{CompressUpdate, report::CompressCallback},
			drive_job::plan::{Listed, SourceDir},
			file::{
				AnonymousRemoteFile, RemoteFile,
				enums::RemoteFileType,
				meta::{DecryptedFileMeta, FileMeta},
			},
		},
	};

	#[expect(dead_code, reason = "only named by the compile-time check below")]
	struct Ignore;

	impl CompressCallback for Ignore {
		fn on_archive_created(&self, _: crate::fs::file::RemoteFile) {}
		fn on_update(&self, _: CompressUpdate) {}
	}

	/// The bindings run compression on the SDK's multi-threaded runtime, which needs a `Send`
	/// future.
	fn _compress_future_is_send(client: Arc<Client>, destination: DirType<'static, Normal>) {
		fn assert_send<T: Send>(_: T) {}
		assert_send(client.compress_items(
			Vec::new(),
			destination,
			ValidatedName::try_from("a.tar").unwrap(),
			CompressConfig {
				format: CompressFormat::Tar { compression: None },
				max_bytes: None,
			},
			Ignore,
			JobControl::default(),
		));
	}

	fn source_dir(name: &str) -> SourceDir<()> {
		SourceDir {
			uuid: Uuid::new_v4(),
			name: Some(name.to_owned()),
			created: None,
			color: filen_types::api::v3::dir::color::DirColor::Default,
			handle: (),
		}
	}

	fn file(name: &str, size: u64) -> RemoteFileType<'static> {
		let anonymous: AnonymousRemoteFile = RemoteFile::from_meta(
			Uuid::new_v4(),
			(),
			Uuid::new_v4().into(),
			size,
			size.div_ceil(crate::consts::CHUNK_SIZE_U64),
			"de-1",
			"bucket",
			chrono::Utc::now(),
			false,
			FileMeta::Decoded(DecryptedFileMeta {
				name: Cow::Owned(name.to_owned()),
				size,
				mime: Cow::Borrowed("text/plain"),
				key: FileKey::V3(EncryptionKey::generate()),
				last_modified: chrono::Utc::now(),
				created: None,
				hash: None,
			}),
		);
		RemoteFileType::File(Cow::Owned(anonymous))
	}

	/// A plan of `Photos/` (with `a.jpg` and `2024/b.jpg`) and a top-level file also called
	/// `Photos`, into an archive root.
	fn plan() -> ItemPlan<()> {
		let root = Uuid::new_v4();
		let photos = source_dir("Photos");
		let year = source_dir("2024");
		let mut planner = ItemPlanner::default();
		planner.add_destination(root, std::iter::empty());
		let listed = |parent: &SourceDir<()>, item| Listed {
			parent: parent.uuid,
			item,
		};
		planner
			.plan(vec![
				PlanRequest {
					source: PlanSource::Dir {
						root: photos.clone(),
						dirs: vec![listed(&photos, year.clone())],
						files: vec![
							Listed {
								parent: photos.uuid,
								item: file("a.jpg", 3),
							},
							Listed {
								parent: year.uuid,
								item: file("b.jpg", 4),
							},
						],
					},
					destination: root,
					name: None,
				},
				PlanRequest {
					source: PlanSource::File(file("Photos", 5)),
					destination: root,
					name: None,
				},
			])
			.unwrap()
	}

	#[test]
	fn a_plan_becomes_entries_with_joined_paths() {
		let plan = plan();
		let (entries, sources) =
			archive_entries(&plan, CompressFormat::Tar { compression: None }).unwrap();
		let paths: Vec<(&str, Option<u64>)> = entries
			.iter()
			.map(|entry| match entry {
				ArchiveEntry::Dir { path, .. } => (path.as_str(), None),
				ArchiveEntry::File { path, size, .. } => (path.as_str(), Some(*size)),
			})
			.collect();
		let mut files: Vec<_> = paths
			.iter()
			.filter(|(_, size)| size.is_some())
			.copied()
			.collect();
		files.sort();
		assert_eq!(&paths[..2], [("Photos", None), ("Photos/2024", None)]);
		assert_eq!(
			files,
			[
				("Photos (1)", Some(5)),
				("Photos/2024/b.jpg", Some(4)),
				("Photos/a.jpg", Some(3)),
			]
		);
		assert_eq!(sources.len(), 3);
		// the source numbers point at the sources in order
		for entry in &entries {
			if let ArchiveEntry::File { source, path, .. } = entry {
				assert_eq!(&sources[*source as usize].path, path);
			}
		}
		let renamed = top_level_renames(&plan);
		assert_eq!(renamed.len(), 1);
		assert_eq!(
			(renamed[0].source_path.as_str(), renamed[0].name.as_ref()),
			("Photos", "Photos (1)")
		);

		assert_eq!(
			archive_entries(
				&plan,
				CompressFormat::Single {
					compression: crate::fs::archive::compress::Compression {
						codec: crate::fs::archive::format::StreamCodec::Gzip,
						level: None,
					},
				},
			)
			.unwrap_err()
			.kind(),
			ErrorKind::InvalidState,
			"a single compressed file is exactly one file"
		);
	}

	#[test]
	fn paths_extracting_would_skip_are_refused() {
		let at_cap = "a".repeat(MAX_ARCHIVE_PATH_BYTES);
		assert!(check_path(&at_cap, 0).is_ok());
		assert_eq!(
			check_path(&at_cap, 1).unwrap_err().kind(),
			ErrorKind::InvalidName,
			"a directory's trailing slash counts"
		);
		let deep = vec!["d"; MAX_ARCHIVE_PATH_DEPTH].join("/");
		assert!(check_path(&deep, 0).is_ok());
		assert!(check_path(&format!("{deep}/x"), 0).is_err());
	}
}
