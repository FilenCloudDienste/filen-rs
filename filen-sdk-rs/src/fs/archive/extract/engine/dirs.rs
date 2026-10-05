//! The directories of an extraction: the folder its entries land in, and the ones their paths
//! imply, planned as the entries name them and created parent first.

use std::{borrow::Cow, collections::VecDeque, sync::Arc};

use chrono::{DateTime, Utc};
use filen_types::{api::v3::dir::color::DirColor, fs::Uuid};
use futures::stream::FuturesUnordered;

use crate::{
	Error, ErrorKind,
	fs::{
		HasName, HasUUID,
		archive::{
			dispose::{DisposalBackend, dir_digest},
			entry_path::joined,
			extract::{
				ExtractRoot,
				report::{
					ArchiveEntryId, ExtractFailure, ExtractPhase, ExtractRenameReason,
					ExtractRetry, ExtractStage, ExtractTopLevelKey,
				},
			},
			format::{ArchiveFormat, extract_folder_name},
			names::{DirId, PathResolver, PlannedDir, ROOT},
		},
		categories::{DirType, NonRootItemType, Normal},
		dir::RemoteDirectory,
		drive_job::{
			dir::{CreatedDirOutcome, DirError, DirTask, create_dir},
			exceeds_limit,
		},
		name::{
			ValidatedName,
			keep_both::{NameShape, TakenNames},
		},
	},
	job::Stopped,
	util::MaybeSendBoxFuture,
};

use super::{Driver, record};

pub(super) enum DirState {
	Planned,
	Creating,
	/// Its uuid alone: a failure's retry builds the directory from the slot, and the root's is
	/// kept once in the driver, so an archive's thousands of directories do not each hold one.
	Created(Uuid),
	Failed(Arc<Error>),
}

/// What the codec reported the archive holds, and what [`Driver::open`] set up for it.
pub(super) struct Opened {
	pub(super) layout: ArchiveFormat,
	/// Where entries go, by the names their paths give.
	pub(super) resolver: PathResolver,
}

impl<B: DisposalBackend> Driver<B> {
	/// What the archive holds, once the codec said so: every entry comes after.
	pub(super) fn opened(&self) -> &Opened {
		self.opened
			.as_ref()
			.expect("entries follow the archive's layout")
	}

	pub(super) fn opened_mut(&mut self) -> &mut Opened {
		self.opened
			.as_mut()
			.expect("entries follow the archive's layout")
	}
}

/// The directories the job plans, by [`DirId`], and the creates it works them off with.
#[derive(Default)]
pub(super) struct Dirs {
	pub(super) slots: Vec<DirSlot>,
	/// Directories planned and neither created nor failed.
	pub(super) uncreated: usize,
	/// Planned directories whose parent is created, waiting for a create to start.
	pub(super) ready: VecDeque<DirId>,
	pub(super) creates:
		FuturesUnordered<MaybeSendBoxFuture<'static, (DirId, Result<CreatedDirOutcome, DirError>)>>,
}

/// A directory of the extraction; [`ROOT`] is the one entries land in.
pub(super) struct DirSlot {
	pub(super) uuid: Uuid,
	place: DirPlace,
	pub(super) state: DirState,
	children: Vec<DirId>,
}

/// Where a directory of the extraction comes from.
enum DirPlace {
	/// [`ROOT`], which exists before the first entry is read: the destination, or the folder the
	/// job created in it. Boxed, so the slots of an archive's thousands of directories stay
	/// small.
	Root(Box<DirType<'static, Normal>>),
	/// A directory an entry's path names.
	Named(NamedDir),
}

/// A directory an entry's path names, planned by the job.
struct NamedDir {
	parent: DirId,
	/// The name it is created under, and once it is, the name it got.
	name: ValidatedName,
	/// The name the archive gave it, when `name` is a keep-both name instead.
	archive_name: Option<ValidatedName>,
	created: DateTime<Utc>,
	/// The entry that named it first.
	entry: ArchiveEntryId,
}

impl NamedDir {
	/// The name the archive gave it.
	fn archive_name(&self) -> &ValidatedName {
		self.archive_name.as_ref().unwrap_or(&self.name)
	}
}

impl DirSlot {
	/// A directory the job plans, creates or fails: never the root, which it starts with.
	fn named(&self) -> &NamedDir {
		match &self.place {
			DirPlace::Named(named) => named,
			DirPlace::Root(_) => unreachable!("the root exists before any entry is read"),
		}
	}

	fn named_mut(&mut self) -> &mut NamedDir {
		match &mut self.place {
			DirPlace::Named(named) => named,
			DirPlace::Root(_) => unreachable!("the root exists before any entry is read"),
		}
	}

	pub(super) fn created_uuid(&self) -> Option<Uuid> {
		match &self.state {
			DirState::Created(uuid) => Some(*uuid),
			_ => None,
		}
	}
}

impl<B: DisposalBackend> Driver<B> {
	/// Sets up where entries go, once the codec has told what the archive holds.
	pub(super) async fn open(&mut self, layout: ArchiveFormat) -> Result<(), Stopped> {
		let destination = self.destination.uuid();
		let listed = self
			.control
			.until_stopping(self.backend.list_dir_names(&self.destination))
			.await?;
		let targets = self
			.control
			.until_stopping(self.backend.connected_targets(destination))
			.await?;
		let (listed, targets) = match (listed, targets) {
			(Ok(listed), Ok(targets)) => (listed, targets),
			(Err(error), _) | (_, Err(error)) => return Err(self.stop_with(error)),
		};
		self.targets = Arc::new(targets);
		self.unverified = listed.unverified;
		let new_folder = match (&self.root, layout) {
			(
				ExtractRoot::NewFolder { name },
				ArchiveFormat::Tar { .. } | ArchiveFormat::Zip | ArchiveFormat::SevenZ,
			) => Some(name.clone()),
			(_, ArchiveFormat::Single { .. }) | (ExtractRoot::Destination, _) => None,
		};
		let (root, resolver) = match new_folder {
			None => {
				self.into_destination = true;
				let resolver = PathResolver::new(listed.names.iter().map(String::as_str));
				(self.destination.clone(), resolver)
			}
			Some(name) => {
				let wanted = match name {
					Some(name) => name,
					None => self.default_folder_name(),
				};
				let mut taken = TakenNames::new(listed.names.iter().map(String::as_str));
				let name = match taken.allocate(wanted, NameShape::Dir) {
					Ok(name) => name,
					Err(error) => return Err(self.stop_with(error.into())),
				};
				let folder = self.create_root(name).await?;
				let resolver = PathResolver::new(std::iter::empty());
				(DirType::Dir(Cow::Owned(folder)), resolver)
			}
		};
		self.dirs.slots.push(DirSlot {
			uuid: root.uuid(),
			state: DirState::Created(root.uuid()),
			place: DirPlace::Root(Box::new(root)),
			children: Vec::new(),
		});
		self.opened = Some(Opened { layout, resolver });
		self.reporter.set_phase(ExtractPhase::Extracting);
		Ok(())
	}

	fn default_folder_name(&self) -> ValidatedName {
		extract_folder_name(self.archive.name())
	}

	/// Creates the folder entries are extracted into.
	async fn create_root(&mut self, name: ValidatedName) -> Result<RemoteDirectory, Stopped> {
		loop {
			// No entry is read yet, so only prefetched chunks are in flight: waited out holding
			// nothing, as between entries.
			if self.control.is_pause_requested() {
				self.pause().await?;
			}
			if self.control.is_stopping() {
				self.reporter.wind_down(&self.control);
				return Err(Stopped);
			}
			let task = DirTask {
				backend: Arc::clone(&self.backend),
				control: self.control.clone(),
				ops: self.reporter.ops(),
				targets: Arc::clone(&self.targets),
				parent: self.destination.uuid(),
				uuid: Uuid::new_v4(),
				// a copy: a create that never started is tried again under the same name
				name: name.clone(),
				created: Utc::now(),
				color: DirColor::Default,
				top_level: true,
				verify_name: self.unverified,
				subject: "item",
			};
			match create_dir(task).await {
				Ok(outcome) => {
					let dir = outcome.dir;
					self.report_propagation(dir.uuid(), outcome.propagation_errors);
					// handed over before the update that counts it
					self.top_level_created(
						ExtractTopLevelKey::Root,
						NonRootItemType::Dir(Cow::Owned(dir.clone())),
					);
					self.reporter.dir_created(
						dir.uuid(),
						self.destination.uuid(),
						outcome.name.as_ref(),
					);
					self.created_digest = self.created_digest.wrapping_add(dir_digest(dir.uuid()));
					return Ok(dir);
				}
				Err(DirError::NotStarted) => {}
				Err(DirError::Failed(error)) => return Err(self.stop_with(error)),
			}
		}
	}

	/// The directory `segments` names, planning the ones not seen before; `Err` once the job
	/// ended.
	pub(super) fn resolve_dirs(
		&mut self,
		segments: &[ValidatedName],
		entry: ArchiveEntryId,
		modified: Option<DateTime<Utc>>,
	) -> Result<DirId, Stopped> {
		let mut planned = Vec::new();
		let resolved = self
			.opened_mut()
			.resolver
			.resolve_dirs(segments, &mut planned);
		let dir = match resolved {
			Ok(dir) => dir,
			Err(error) => return Err(self.stop_with(error.into())),
		};
		for PlannedDir {
			id,
			parent,
			name,
			archive_name,
		} in planned
		{
			self.count_item()?;
			// every planned directory is kept for the whole job, and one entry can imply 256:
			// they are capped like members, before they cost more
			if exceeds_limit(self.dirs.slots.len() as u64, self.config.max_members) {
				return Err(self.stop_with(Error::custom(
					ErrorKind::ArchiveTooLarge,
					format!(
						"the archive names more than {} directories",
						self.config.max_members
					),
				)));
			}
			// the resolver numbers directories in the order it plans them, parents first
			let next = self.dirs.slots.len();
			let Some(parent_slot) = self.dirs.slots.get_mut(parent).filter(|_| id == next) else {
				return Err(self.stop_with(Error::custom(
					ErrorKind::Internal,
					"a directory was planned out of order",
				)));
			};
			parent_slot.children.push(id);
			let state = match &parent_slot.state {
				DirState::Failed(error) => DirState::Failed(Arc::clone(error)),
				DirState::Created(_) => {
					self.dirs.ready.push_back(id);
					DirState::Planned
				}
				DirState::Planned | DirState::Creating => DirState::Planned,
			};
			match state {
				DirState::Failed(_) => self.reporter.dir_failed(None),
				_ => self.dirs.uncreated += 1,
			}
			self.dirs.slots.push(DirSlot {
				uuid: Uuid::new_v4(),
				place: DirPlace::Named(NamedDir {
					parent,
					name,
					archive_name,
					// Filen directories keep a creation time only; the archive's modification
					// time is the closest it has
					created: if id == dir {
						modified.unwrap_or_else(Utc::now)
					} else {
						Utc::now()
					},
					entry,
				}),
				state,
				children: Vec::new(),
			});
		}
		Ok(dir)
	}

	pub(super) fn start_dir(&mut self, dir: DirId) {
		let slot = &self.dirs.slots[dir];
		let named = slot.named();
		let parent = self.dirs.slots[named.parent]
			.created_uuid()
			.expect("a directory is only created once its parent exists");
		let top_level = named.parent == ROOT && self.into_destination;
		let task = DirTask {
			backend: Arc::clone(&self.backend),
			control: self.control.clone(),
			ops: self.reporter.ops(),
			targets: Arc::clone(&self.targets),
			parent,
			uuid: slot.uuid,
			// a copy: the slot keeps its name for the entries below it
			name: named.name.clone(),
			created: named.created,
			color: DirColor::Default,
			top_level,
			verify_name: top_level && self.unverified,
			subject: "item",
		};
		self.dirs.slots[dir].state = DirState::Creating;
		self.dirs
			.creates
			.push(Box::pin(async move { (dir, create_dir(task).await) }));
	}

	pub(super) fn dir_finished(&mut self, dir: DirId, result: Result<CreatedDirOutcome, DirError>) {
		let named = self.dirs.slots[dir].named();
		let (parent_dir, entry) = (named.parent, named.entry);
		let parent = self.dirs.slots[parent_dir].uuid;
		match result {
			Ok(CreatedDirOutcome {
				dir: created,
				name,
				color_error: _,
				propagation_errors,
			}) => {
				let uuid = created.uuid();
				self.report_propagation(uuid, propagation_errors);
				// one record, with the name it got in the end (a keep-both name the resolver
				// picked, then possibly another the destination turned out to need)
				if name != *self.dirs.slots[dir].named().archive_name() {
					let path = self.archive_path(dir);
					self.renamed(entry, path, &name, ExtractRenameReason::DuplicateName);
				}
				// handed over before the update that counts it
				if parent_dir == ROOT && self.into_destination {
					self.top_level_created(
						ExtractTopLevelKey::Entry { id: entry },
						NonRootItemType::Dir(Cow::Owned(created)),
					);
				}
				self.reporter.dir_created(uuid, parent, name.as_ref());
				// the name a retry into it finds it under; the archive's stays where it was
				let named = self.dirs.slots[dir].named_mut();
				if name != named.name {
					let planned = std::mem::replace(&mut named.name, name);
					named.archive_name.get_or_insert(planned);
				}
				self.created_digest = self.created_digest.wrapping_add(dir_digest(uuid));
				self.dirs.slots[dir].state = DirState::Created(uuid);
				self.dirs.uncreated -= 1;
				self.dirs
					.ready
					.extend(self.dirs.slots[dir].children.iter().copied());
				self.finalize_ready();
			}
			// tried again once the pause is over
			Err(DirError::NotStarted) => {
				self.dirs.slots[dir].state = DirState::Planned;
				self.dirs.ready.push_front(dir);
			}
			Err(DirError::Failed(error)) => {
				let error = Arc::new(error);
				self.note_error(&error);
				let failure = ExtractFailure {
					entry,
					path: self.archive_path(dir),
					dest_parent: parent,
					dest_name: self.dirs.slots[dir].named().name.as_ref().to_owned(),
					stage: ExtractStage::CreateDirectory,
					retry: Some(self.retry(parent_dir)),
					error: Arc::clone(&error),
				};
				self.reporter.dir_failed(record(
					&mut self.report.failures,
					&mut self.report.omitted.failures,
					failure,
				));
				self.fail_subtree(dir, &error);
			}
		}
	}

	/// Marks `root`'s planned subdirectories failed, and fails the files waiting in them.
	fn fail_subtree(&mut self, root: DirId, error: &Arc<Error>) {
		let mut stack = vec![root];
		while let Some(dir) = stack.pop() {
			if dir != root {
				self.reporter.dir_failed(None);
			}
			if matches!(
				self.dirs.slots[dir].state,
				DirState::Planned | DirState::Creating
			) {
				self.dirs.uncreated -= 1;
			}
			self.dirs.slots[dir].state = DirState::Failed(Arc::clone(error));
			stack.extend(self.dirs.slots[dir].children.iter().copied());
		}
		self.dirs
			.ready
			.retain(|dir| !matches!(self.dirs.slots[*dir].state, DirState::Failed(_)));
		let waiting: Vec<u64> = self
			.files
			.iter()
			.filter(|(_, file)| {
				!file.failed() && matches!(self.dirs.slots[file.parent].state, DirState::Failed(_))
			})
			.map(|(ordinal, _)| *ordinal)
			.collect();
		for ordinal in waiting {
			self.fail_file(ordinal, ExtractStage::CreateDirectory, Arc::clone(error));
		}
	}

	/// A directory's path in the archive, as drive names.
	fn archive_path(&self, mut dir: DirId) -> String {
		let mut names = Vec::new();
		while let DirPlace::Named(named) = &self.dirs.slots[dir].place {
			names.push(named.archive_name());
			dir = named.parent;
		}
		joined(self.base.iter().chain(names.into_iter().rev()))
	}

	/// A directory's path in the archive: the base, then the names of the directories below
	/// it.
	fn archive_names(&self, mut dir: DirId) -> Vec<ValidatedName> {
		let mut names = Vec::new();
		while let DirPlace::Named(named) = &self.dirs.slots[dir].place {
			names.push(named.archive_name().clone());
			dir = named.parent;
		}
		names.extend(self.base.iter().rev().cloned());
		names.reverse();
		names
	}

	/// Where an entry of `dir` that failed is extracted again: the nearest directory there is,
	/// `dir` itself unless it failed too. The root always is.
	pub(super) fn retry(&self, mut dir: DirId) -> ExtractRetry {
		// the root is created before any entry is read
		while !matches!(self.dirs.slots[dir].state, DirState::Created(_)) {
			dir = self.dirs.slots[dir].named().parent;
		}
		ExtractRetry {
			destination: self.created_dir(dir),
			// with the base of a partial extraction: that is where it is in the archive
			base: self.archive_names(dir),
		}
	}

	/// Created directory `dir`, as the drive holds it: the root as the job has it, any other
	/// built from its slot, the way the job created it.
	fn created_dir(&self, dir: DirId) -> DirType<'static, Normal> {
		let slot = &self.dirs.slots[dir];
		match &slot.place {
			DirPlace::Root(root) => DirType::clone(root),
			// a copy: the slot keeps its name for the entries below it
			DirPlace::Named(named) => DirType::Dir(Cow::Owned(RemoteDirectory::new_from_parts(
				slot.uuid,
				RemoteDirectory::make_meta(named.name.clone(), named.created),
				self.dirs.slots[named.parent].uuid.into(),
				named.created,
			))),
		}
	}
}
