//! Turns the sources of a copy (files, and directories with their recursive listings) into
//! the directories to create, parent first, and the files to copy into them.
//!
//! Nothing below the top level can collide with an existing item, since it lands in a
//! directory the copy creates. Sibling names that collide in the source (case-insensitive
//! duplicates) are renamed rather than dropped, and a directory whose metadata cannot be
//! decrypted is created under its uuid so its readable contents are still copied. A file
//! whose metadata cannot be decrypted has no key to read it with, so it is skipped.

use std::{
	borrow::Cow,
	collections::{HashMap, HashSet, VecDeque},
};

use chrono::{DateTime, Utc};
use filen_macros::js_type;
use filen_types::{api::v3::dir::color::DirColor, fs::Uuid};

use crate::{
	Error, ErrorKind,
	fs::{
		HasName, HasUUID,
		dir::traits::HasDirInfo,
		file::{enums::RemoteFileType, traits::HasFileInfo},
		name::ValidatedName,
	},
};

use super::naming::{SourceName, TakenNames};

/// A source directory, independent of the category it was listed from.
#[derive(Debug, Clone)]
pub(crate) struct SourceDir<D> {
	pub(crate) uuid: Uuid,
	/// `None` when the metadata could not be decrypted.
	pub(crate) name: Option<String>,
	pub(crate) created: Option<DateTime<Utc>>,
	/// Shared-in listings carry no color; they report [`DirColor::Default`].
	pub(crate) color: DirColor<'static>,
	/// Whatever the caller needs to address this directory again (to retry a failed copy).
	pub(crate) handle: D,
}

impl<D> SourceDir<D> {
	/// `handle` takes `dir` once its fields are read, so the handle can own it.
	pub(crate) fn new<T: HasUUID + HasName + HasDirInfo>(
		dir: T,
		color: DirColor<'static>,
		handle: impl FnOnce(T) -> D,
	) -> Self {
		Self {
			uuid: dir.uuid(),
			name: dir.name().map(str::to_owned),
			created: dir.created(),
			color,
			handle: handle(dir),
		}
	}
}

/// An entry of a recursive listing, keyed by the uuid of the directory it is in.
#[derive(Debug, Clone)]
pub(crate) struct Listed<T> {
	pub(crate) parent: Uuid,
	pub(crate) item: T,
}

#[derive(Debug, Clone)]
pub(crate) enum PlanSource<D> {
	File(RemoteFileType<'static>),
	Dir {
		root: SourceDir<D>,
		dirs: Vec<Listed<SourceDir<D>>>,
		files: Vec<Listed<RemoteFileType<'static>>>,
	},
}

#[derive(Debug, Clone)]
pub(crate) struct PlanRequest<D> {
	pub(crate) source: PlanSource<D>,
	/// The existing directory the source is copied into.
	pub(crate) destination: Uuid,
	/// Name to use instead of the source's (still subject to keep-both).
	pub(crate) name: Option<String>,
}

/// Where a planned item goes: an existing directory, or one the plan creates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DestParent {
	Existing(Uuid),
	/// Index into [`CopyPlan::dirs`], always lower than the index of anything placed in it.
	Planned(usize),
}

#[derive(Debug, Clone)]
pub(crate) struct PlannedDir<D> {
	/// Index of the request this directory belongs to.
	pub(crate) request: usize,
	pub(crate) source_uuid: Uuid,
	/// Chosen up front so the item can be reported before it exists.
	pub(crate) dest_uuid: Uuid,
	pub(crate) parent: DestParent,
	pub(crate) name: ValidatedName,
	pub(crate) created: Option<DateTime<Utc>>,
	pub(crate) color: DirColor<'static>,
	/// Source path, `/`-joined, starting with the source's own name.
	pub(crate) source_path: String,
	/// Counts of everything planned below this directory, for reporting when it fails.
	pub(crate) descendant_dirs: u64,
	pub(crate) descendant_files: u64,
	pub(crate) descendant_bytes: u64,
	pub(crate) handle: D,
}

#[derive(Debug, Clone)]
pub(crate) struct PlannedFile {
	/// Index of the request this file belongs to.
	pub(crate) request: usize,
	pub(crate) source: RemoteFileType<'static>,
	/// Chosen up front so the item can be reported before it exists.
	pub(crate) dest_uuid: Uuid,
	pub(crate) parent: DestParent,
	pub(crate) name: ValidatedName,
	pub(crate) size: u64,
	pub(crate) source_path: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PlannedItem {
	Dir(usize),
	File(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PlannedTopLevel {
	/// Index into the requests the plan was built from.
	pub(crate) request: usize,
	pub(crate) item: PlannedItem,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(
	feature = "wasm-full",
	derive(serde::Serialize, tsify::Tsify),
	tsify(into_wasm_abi, large_number_types_as_bigints),
	serde(tag = "type", rename_all = "camelCase")
)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
pub enum SkipReason {
	/// The file's metadata could not be decrypted, so there is no key to read it with.
	UndecryptableFile { uuid: Uuid },
	/// Listed entries whose parent is not reachable from the source directory (a malformed or
	/// cyclic listing).
	Unreachable { count: u64 },
}

#[js_type(export, no_deser)]
pub struct SkippedEntry {
	pub source_path: String,
	pub bytes: u64,
	pub reason: SkipReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(
	feature = "wasm-full",
	derive(serde::Serialize, tsify::Tsify),
	tsify(into_wasm_abi, large_number_types_as_bigints),
	serde(rename_all = "camelCase")
)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
pub enum RenameReason {
	/// A sibling in the source already took the (case-insensitive) name.
	DuplicateName,
	/// The metadata could not be decrypted; the item is named after its uuid.
	Undecryptable,
	/// The name is invalid under today's rules and was encoded.
	InvalidName,
}

#[derive(Debug, Clone)]
pub struct RenamedEntry {
	pub source_uuid: Uuid,
	pub source_path: String,
	pub name: ValidatedName,
	pub reason: RenameReason,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[js_type(export, no_deser, no_default)]
pub struct PlanTotals {
	pub dirs: u64,
	pub files: u64,
	pub bytes: u64,
}

#[derive(Debug)]
pub(crate) struct CopyPlan<D> {
	/// Parent first: every directory comes after the directory it is created in.
	pub(crate) dirs: Vec<PlannedDir<D>>,
	pub(crate) files: Vec<PlannedFile>,
	pub(crate) top_level: Vec<PlannedTopLevel>,
	pub(crate) skipped: Vec<SkippedEntry>,
	/// Renames below the top level. Top-level items are renamed by keep-both as a matter of
	/// course and are reported through [`CopyPlan::top_level`]; a name that changes again while
	/// copying is reported by the job.
	pub(crate) renamed: Vec<RenamedEntry>,
	pub(crate) totals: PlanTotals,
	/// Destinations whose listing had entries with undecryptable names: the top-level names
	/// chosen for them are checked with the server before use, since they may be taken.
	pub(crate) unverified_destinations: HashSet<Uuid>,
}

impl<D> Default for CopyPlan<D> {
	fn default() -> Self {
		Self {
			dirs: Vec::new(),
			files: Vec::new(),
			top_level: Vec::new(),
			skipped: Vec::new(),
			renamed: Vec::new(),
			totals: PlanTotals::default(),
			unverified_destinations: HashSet::new(),
		}
	}
}

/// Builds a [`CopyPlan`]. Every destination must be registered with the names it already holds
/// before the plan is built, so top-level names are chosen against them.
#[derive(Debug, Default)]
pub(crate) struct CopyPlanner {
	destinations: HashMap<Uuid, TakenNames>,
	unverified_destinations: HashSet<Uuid>,
}

impl CopyPlanner {
	pub(crate) fn add_destination<'a>(
		&mut self,
		uuid: Uuid,
		existing_names: impl IntoIterator<Item = &'a str>,
	) {
		self.destinations
			.insert(uuid, TakenNames::new(existing_names));
	}

	/// Records that `uuid`'s listing had entries whose names could not be decrypted, so the
	/// names it holds are not fully known.
	pub(crate) fn mark_unverified(&mut self, uuid: Uuid) {
		self.unverified_destinations.insert(uuid);
	}

	/// Validates every request before planning any: a directory cannot be copied into itself
	/// or one of its descendants.
	pub(crate) fn plan<D>(mut self, requests: Vec<PlanRequest<D>>) -> Result<CopyPlan<D>, Error> {
		for request in &requests {
			if !self.destinations.contains_key(&request.destination) {
				return Err(Error::custom(
					ErrorKind::Internal,
					"copy destination was not registered with the planner",
				));
			}
			if let PlanSource::Dir { root, dirs, .. } = &request.source
				&& (root.uuid == request.destination
					|| dirs.iter().any(|d| d.item.uuid == request.destination))
			{
				return Err(Error::custom(
					ErrorKind::InvalidState,
					"cannot copy a directory into itself or one of its subdirectories",
				));
			}
		}

		let mut plan = CopyPlan {
			unverified_destinations: self.unverified_destinations,
			..CopyPlan::default()
		};
		for (index, request) in requests.into_iter().enumerate() {
			let taken = self
				.destinations
				.get_mut(&request.destination)
				.expect("checked above");
			let parent = DestParent::Existing(request.destination);
			match request.source {
				PlanSource::File(file) => {
					let path = path_segment(file.name(), file.uuid()).into_owned();
					if let Some(file) = plan.plan_file(
						index,
						file,
						parent,
						request.name.as_deref(),
						taken,
						path,
						true,
					) {
						plan.top_level.push(PlannedTopLevel {
							request: index,
							item: PlannedItem::File(file),
						});
					}
				}
				PlanSource::Dir { root, dirs, files } => {
					let dir = plan.plan_tree(
						index,
						root,
						dirs,
						files,
						parent,
						request.name.as_deref(),
						taken,
					);
					plan.top_level.push(PlannedTopLevel {
						request: index,
						item: PlannedItem::Dir(dir),
					});
				}
			}
		}

		plan.fill_descendant_counts();
		plan.totals = PlanTotals {
			dirs: plan.dirs.len() as u64,
			files: plan.files.len() as u64,
			bytes: plan.files.iter().map(|f| f.size).sum(),
		};
		Ok(plan)
	}
}

/// The name an item gets in `taken`: its own (or the preferred one), or its uuid when its
/// metadata could not be decrypted. The reason is `Some` when the name differs from the
/// source's for more than NFC normalization.
fn allocate_name(
	taken: &mut TakenNames,
	uuid: Uuid,
	name: Option<&str>,
	is_dir: bool,
) -> (ValidatedName, Option<RenameReason>) {
	let uuid_name =
		|| ValidatedName::try_from(uuid.to_string().as_str()).expect("a uuid is a valid name");
	let by_uuid = |taken: &mut TakenNames| {
		taken
			.allocate(uuid_name(), is_dir)
			.unwrap_or_else(|_| uuid_name())
	};
	let Some(name) = name else {
		return (by_uuid(taken), Some(RenameReason::Undecryptable));
	};
	// Only an empty name, or one whose encoding exceeds the length limit, fails here.
	let Ok(source_name) = SourceName::parse(name) else {
		return (by_uuid(taken), Some(RenameReason::InvalidName));
	};
	let encoded = matches!(source_name, SourceName::Encoded(_));
	let valid = source_name.into_name();
	// Kept to tell a keep-both rename from the name itself.
	let Ok(allocated) = taken.allocate(valid.clone(), is_dir) else {
		return (by_uuid(taken), Some(RenameReason::InvalidName));
	};
	let reason = if allocated != valid {
		Some(RenameReason::DuplicateName)
	} else if encoded {
		Some(RenameReason::InvalidName)
	} else {
		None
	};
	(allocated, reason)
}

/// An item's segment of a source path: its name, or its uuid when its metadata could not be
/// decrypted.
fn path_segment(name: Option<&str>, uuid: Uuid) -> Cow<'_, str> {
	name.map_or_else(|| Cow::Owned(uuid.to_string()), Cow::Borrowed)
}

impl<D> CopyPlan<D> {
	/// Records a rename worth reporting. At the top level keep-both renames are expected and
	/// visible through the planned name, so only the other reasons are reported there.
	fn note_rename(
		&mut self,
		source_uuid: Uuid,
		source_path: &str,
		name: &ValidatedName,
		reason: Option<RenameReason>,
		top_level: bool,
	) {
		let Some(reason) =
			reason.filter(|reason| !top_level || *reason != RenameReason::DuplicateName)
		else {
			return;
		};
		// the planned item keeps its own path and name
		self.renamed.push(RenamedEntry {
			source_uuid,
			source_path: source_path.to_owned(),
			name: name.clone(),
			reason,
		});
	}

	/// Plans `file` into `parent`, or records it as skipped when it cannot be read.
	#[allow(clippy::too_many_arguments)]
	fn plan_file(
		&mut self,
		request: usize,
		file: RemoteFileType<'static>,
		parent: DestParent,
		preferred: Option<&str>,
		taken: &mut TakenNames,
		source_path: String,
		top_level: bool,
	) -> Option<usize> {
		let source_uuid = file.uuid();
		if file.name().is_none() || file.key().is_none() {
			self.skipped.push(SkippedEntry {
				source_path,
				bytes: file.size(),
				reason: SkipReason::UndecryptableFile { uuid: source_uuid },
			});
			return None;
		}
		let (name, reason) = allocate_name(taken, source_uuid, preferred.or(file.name()), false);
		self.note_rename(source_uuid, &source_path, &name, reason, top_level);
		self.files.push(PlannedFile {
			request,
			size: file.size(),
			source: file,
			dest_uuid: Uuid::new_v4(),
			parent,
			name,
			source_path,
		});
		Some(self.files.len() - 1)
	}

	#[allow(clippy::too_many_arguments)]
	fn push_dir(
		&mut self,
		request: usize,
		dir: SourceDir<D>,
		parent: DestParent,
		preferred: Option<&str>,
		taken: &mut TakenNames,
		source_path: String,
		top_level: bool,
	) -> usize {
		let (name, reason) =
			allocate_name(taken, dir.uuid, preferred.or(dir.name.as_deref()), true);
		self.note_rename(dir.uuid, &source_path, &name, reason, top_level);
		self.dirs.push(PlannedDir {
			request,
			source_uuid: dir.uuid,
			dest_uuid: Uuid::new_v4(),
			parent,
			name,
			created: dir.created,
			color: dir.color,
			source_path,
			descendant_dirs: 0,
			descendant_files: 0,
			descendant_bytes: 0,
			handle: dir.handle,
		});
		self.dirs.len() - 1
	}

	/// Plans a source directory and everything reachable below it, breadth first so parents
	/// always precede their children. Returns the index of the planned root.
	#[allow(clippy::too_many_arguments)]
	fn plan_tree(
		&mut self,
		request: usize,
		root: SourceDir<D>,
		dirs: Vec<Listed<SourceDir<D>>>,
		files: Vec<Listed<RemoteFileType<'static>>>,
		parent: DestParent,
		preferred: Option<&str>,
		taken: &mut TakenNames,
	) -> usize {
		let root_path = path_segment(root.name.as_deref(), root.uuid).into_owned();
		let root_uuid = root.uuid;

		// children by parent uuid, keeping listing order
		let mut child_dirs: HashMap<Uuid, Vec<SourceDir<D>>> = HashMap::new();
		let total_listed_dirs = dirs.len();
		for listed in dirs {
			child_dirs
				.entry(listed.parent)
				.or_default()
				.push(listed.item);
		}
		let mut child_files: HashMap<Uuid, Vec<RemoteFileType<'static>>> = HashMap::new();
		for listed in files {
			child_files
				.entry(listed.parent)
				.or_default()
				.push(listed.item);
		}

		let root_index = self.push_dir(request, root, parent, preferred, taken, root_path, true);
		let mut visited = HashSet::with_capacity(total_listed_dirs + 1);
		visited.insert(root_uuid);
		let mut queue = VecDeque::from([(root_uuid, root_index)]);
		while let Some((source_uuid, planned_index)) = queue.pop_front() {
			let parent = DestParent::Planned(planned_index);
			// owned: `self.dirs` grows while the children are planned
			let path = self.dirs[planned_index].source_path.clone();
			let mut taken = TakenNames::default();
			for dir in child_dirs.remove(&source_uuid).unwrap_or_default() {
				// a uuid listed twice (or a cycle back to an ancestor) is planned once
				if !visited.insert(dir.uuid) {
					continue;
				}
				let dir_uuid = dir.uuid;
				let child_path = format!("{path}/{}", path_segment(dir.name.as_deref(), dir.uuid));
				let index =
					self.push_dir(request, dir, parent, None, &mut taken, child_path, false);
				queue.push_back((dir_uuid, index));
			}
			for file in child_files.remove(&source_uuid).unwrap_or_default() {
				let child_path = format!("{path}/{}", path_segment(file.name(), file.uuid()));
				self.plan_file(request, file, parent, None, &mut taken, child_path, false);
			}
		}

		// whatever was not reached from the root has an orphaned or cyclic parent
		let unreachable_dirs: usize = child_dirs
			.values()
			.flatten()
			.filter(|dir| !visited.contains(&dir.uuid))
			.count();
		let (unreachable_files, unreachable_bytes) = child_files
			.values()
			.flatten()
			.fold((0u64, 0u64), |(count, bytes), file| {
				(count + 1, bytes + file.size())
			});
		let count = unreachable_dirs as u64 + unreachable_files;
		if count > 0 {
			self.skipped.push(SkippedEntry {
				source_path: self.dirs[root_index].source_path.clone(),
				bytes: unreachable_bytes,
				reason: SkipReason::Unreachable { count },
			});
		}
		root_index
	}

	/// Sums each planned directory's descendants. Children always follow their parent, so one
	/// reverse pass accumulates bottom-up.
	fn fill_descendant_counts(&mut self) {
		for file in &self.files {
			if let DestParent::Planned(parent) = file.parent {
				self.dirs[parent].descendant_files += 1;
				self.dirs[parent].descendant_bytes += file.size;
			}
		}
		for index in (0..self.dirs.len()).rev() {
			if let DestParent::Planned(parent) = self.dirs[index].parent {
				let (dirs, files, bytes) = {
					let dir = &self.dirs[index];
					(
						dir.descendant_dirs + 1,
						dir.descendant_files,
						dir.descendant_bytes,
					)
				};
				let parent = &mut self.dirs[parent];
				parent.descendant_dirs += dirs;
				parent.descendant_files += files;
				parent.descendant_bytes += bytes;
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use std::borrow::Cow;

	use chrono::TimeZone;
	use filen_types::crypto::EncryptedString;

	use super::*;
	use crate::{
		consts::CHUNK_SIZE_U64,
		crypto::{file::FileKey, shared::CreateRandom, v3::EncryptionKey},
		fs::{
			file::{
				AnonymousRemoteFile, RemoteFile,
				meta::{DecryptedFileMeta, FileMeta},
			},
			name::encode_name,
		},
	};

	fn dir(name: Option<&str>) -> SourceDir<()> {
		SourceDir {
			uuid: Uuid::new_v4(),
			name: name.map(str::to_owned),
			created: Some(Utc.with_ymd_and_hms(2020, 1, 1, 0, 0, 0).unwrap()),
			color: DirColor::Blue,
			handle: (),
		}
	}

	fn file_with_meta(meta: FileMeta<'static>, size: u64) -> RemoteFileType<'static> {
		let anonymous: AnonymousRemoteFile = RemoteFile::from_meta(
			Uuid::new_v4(),
			(),
			Uuid::new_v4().into(),
			size,
			size.div_ceil(CHUNK_SIZE_U64),
			"de-1",
			"bucket",
			Utc::now(),
			false,
			meta,
		);
		RemoteFileType::File(Cow::Owned(anonymous))
	}

	fn file(name: &str, size: u64) -> RemoteFileType<'static> {
		file_with_meta(
			FileMeta::Decoded(DecryptedFileMeta {
				name: Cow::Owned(name.to_owned()),
				size,
				mime: Cow::Borrowed("application/octet-stream"),
				key: FileKey::V3(EncryptionKey::generate()),
				last_modified: Utc::now(),
				created: None,
				hash: None,
			}),
			size,
		)
	}

	fn undecryptable_file(size: u64) -> RemoteFileType<'static> {
		file_with_meta(
			FileMeta::Encrypted(EncryptedString(Cow::Borrowed("garbage"))),
			size,
		)
	}

	fn listed<T>(parent: &SourceDir<()>, item: T) -> Listed<T> {
		Listed {
			parent: parent.uuid,
			item,
		}
	}

	fn planner(destination: Uuid, existing: &[&str]) -> CopyPlanner {
		let mut planner = CopyPlanner::default();
		planner.add_destination(destination, existing.iter().copied());
		planner
	}

	fn request(source: PlanSource<()>, destination: Uuid) -> PlanRequest<()> {
		PlanRequest {
			source,
			destination,
			name: None,
		}
	}

	fn name(dir_or_file: &ValidatedName) -> &str {
		dir_or_file.as_ref()
	}

	/// Every item comes after the directory it is created in.
	fn assert_parent_first(plan: &CopyPlan<()>) {
		for (index, dir) in plan.dirs.iter().enumerate() {
			if let DestParent::Planned(parent) = dir.parent {
				assert!(parent < index, "dir {index} precedes its parent {parent}");
			}
		}
		for file in &plan.files {
			if let DestParent::Planned(parent) = file.parent {
				assert!(parent < plan.dirs.len());
			}
		}
	}

	#[test]
	fn the_same_source_twice_gets_two_names() {
		let destination = Uuid::new_v4();
		let source = file("a.txt", 10);
		let plan = planner(destination, &[])
			.plan(vec![
				request(PlanSource::File(source.clone()), destination),
				request(PlanSource::File(source), destination),
			])
			.unwrap();
		let names: Vec<&str> = plan.files.iter().map(|f| name(&f.name)).collect();
		assert_eq!(names, ["a.txt", "a (1).txt"]);
		assert_ne!(plan.files[0].dest_uuid, plan.files[1].dest_uuid);
		assert_eq!(plan.top_level.len(), 2);
		assert_eq!(plan.totals.bytes, 20);
	}

	#[test]
	fn top_level_file_gets_a_keep_both_name() {
		let destination = Uuid::new_v4();
		let plan = planner(destination, &["a.txt"])
			.plan(vec![request(
				PlanSource::File(file("a.txt", 10)),
				destination,
			)])
			.unwrap();

		assert_eq!(plan.files.len(), 1);
		assert_eq!(name(&plan.files[0].name), "a (1).txt");
		assert_eq!(plan.files[0].parent, DestParent::Existing(destination));
		assert_eq!(
			plan.top_level,
			vec![PlannedTopLevel {
				request: 0,
				item: PlannedItem::File(0)
			}]
		);
		// a keep-both rename at the top level is expected, not reported
		assert!(plan.renamed.is_empty());
		assert_eq!(
			plan.totals,
			PlanTotals {
				dirs: 0,
				files: 1,
				bytes: 10
			}
		);
	}

	#[test]
	fn destinations_with_hidden_names_are_carried_into_the_plan() {
		let (verified, unverified) = (Uuid::new_v4(), Uuid::new_v4());
		let mut planner = CopyPlanner::default();
		planner.add_destination(verified, std::iter::empty());
		planner.add_destination(unverified, std::iter::empty());
		planner.mark_unverified(unverified);
		let plan = planner
			.plan(vec![
				request(PlanSource::File(file("a", 1)), verified),
				request(PlanSource::File(file("b", 1)), unverified),
			])
			.unwrap();
		assert_eq!(plan.unverified_destinations, HashSet::from([unverified]));
	}

	#[test]
	fn requests_into_one_destination_share_its_names() {
		let destination = Uuid::new_v4();
		let mut second = request(PlanSource::File(file("b.txt", 1)), destination);
		second.name = Some("a.txt".to_string());
		let plan = planner(destination, &[])
			.plan(vec![
				request(PlanSource::File(file("a.txt", 1)), destination),
				second,
			])
			.unwrap();
		let names = plan.files.iter().map(|f| name(&f.name)).collect::<Vec<_>>();
		assert_eq!(names, ["a.txt", "a (1).txt"]);
		assert_eq!(plan.top_level.len(), 2);
	}

	#[test]
	fn tree_is_planned_parent_first_with_metadata_and_counts() {
		let destination = Uuid::new_v4();
		let root = dir(Some("Photos"));
		let year = dir(Some("2020"));
		let plan = planner(destination, &["Photos"])
			.plan(vec![request(
				PlanSource::Dir {
					dirs: vec![listed(&root, year.clone())],
					files: vec![
						listed(&year, file("x.jpg", 5)),
						listed(&root, file("y.txt", 7)),
						listed(&root, file("empty", 0)),
					],
					root: root.clone(),
				},
				destination,
			)])
			.unwrap();
		assert_parent_first(&plan);

		assert_eq!(plan.dirs.len(), 2);
		let photos = &plan.dirs[0];
		assert_eq!(name(&photos.name), "Photos (1)");
		assert_eq!(photos.parent, DestParent::Existing(destination));
		assert_eq!(photos.source_uuid, root.uuid);
		assert_eq!(photos.created, root.created);
		assert_eq!(photos.color, DirColor::Blue);
		assert_eq!(photos.source_path, "Photos");
		assert_eq!(
			(
				photos.descendant_dirs,
				photos.descendant_files,
				photos.descendant_bytes
			),
			(1, 3, 12)
		);

		let year_planned = &plan.dirs[1];
		assert_eq!(name(&year_planned.name), "2020");
		assert_eq!(year_planned.parent, DestParent::Planned(0));
		assert_eq!(year_planned.source_path, "Photos/2020");
		assert_eq!(
			(
				year_planned.descendant_dirs,
				year_planned.descendant_files,
				year_planned.descendant_bytes
			),
			(0, 1, 5)
		);

		let by_name = |n: &str| plan.files.iter().find(|f| name(&f.name) == n).unwrap();
		assert_eq!(by_name("x.jpg").parent, DestParent::Planned(1));
		assert_eq!(by_name("x.jpg").source_path, "Photos/2020/x.jpg");
		assert_eq!(by_name("y.txt").parent, DestParent::Planned(0));
		assert_eq!(by_name("empty").size, 0);

		assert_eq!(
			plan.totals,
			PlanTotals {
				dirs: 2,
				files: 3,
				bytes: 12
			}
		);
		assert_eq!(
			plan.top_level,
			vec![PlannedTopLevel {
				request: 0,
				item: PlannedItem::Dir(0)
			}]
		);
		let mut dest_uuids = plan
			.dirs
			.iter()
			.map(|d| d.dest_uuid)
			.chain(plan.files.iter().map(|f| f.dest_uuid))
			.collect::<Vec<_>>();
		dest_uuids.sort();
		dest_uuids.dedup();
		assert_eq!(dest_uuids.len(), 5, "every planned item gets its own uuid");
	}

	#[test]
	fn copying_a_directory_into_itself_or_a_descendant_is_rejected() {
		let root = dir(Some("a"));
		let child = dir(Some("b"));
		let source = || PlanSource::Dir {
			root: root.clone(),
			dirs: vec![listed(&root, child.clone())],
			files: Vec::new(),
		};
		for destination in [root.uuid, child.uuid] {
			let error = planner(destination, &[])
				.plan(vec![request(source(), destination)])
				.unwrap_err();
			assert_eq!(error.kind(), ErrorKind::InvalidState);
		}
	}

	#[test]
	fn an_invalid_request_rejects_the_whole_plan() {
		let destination = Uuid::new_v4();
		let root = dir(Some("a"));
		let error = planner(destination, &[])
			.plan(vec![
				request(PlanSource::File(file("ok.txt", 1)), destination),
				request(
					PlanSource::Dir {
						root: root.clone(),
						dirs: Vec::new(),
						files: Vec::new(),
					},
					root.uuid,
				),
			])
			.unwrap_err();
		// the second request's destination was never registered
		assert_eq!(error.kind(), ErrorKind::Internal);
	}

	#[test]
	fn case_duplicate_siblings_are_renamed_not_dropped() {
		let destination = Uuid::new_v4();
		let root = dir(Some("root"));
		let clash_dir = dir(Some("x"));
		let plan = planner(destination, &[])
			.plan(vec![request(
				PlanSource::Dir {
					dirs: vec![listed(&root, clash_dir)],
					files: vec![
						listed(&root, file("A.txt", 1)),
						listed(&root, file("a.txt", 2)),
						listed(&root, file("X", 3)),
					],
					root: root.clone(),
				},
				destination,
			)])
			.unwrap();

		let names = plan.files.iter().map(|f| name(&f.name)).collect::<Vec<_>>();
		assert_eq!(names, ["A.txt", "a (1).txt", "X (1)"]);
		assert_eq!(name(&plan.dirs[1].name), "x");
		assert_eq!(plan.renamed.len(), 2);
		assert!(
			plan.renamed
				.iter()
				.all(|r| r.reason == RenameReason::DuplicateName)
		);
		assert_eq!(plan.totals.files, 3, "nothing is dropped");
	}

	#[test]
	fn undecryptable_dir_is_named_by_uuid_and_keeps_its_contents() {
		let destination = Uuid::new_v4();
		let root = dir(Some("root"));
		let hidden = dir(None);
		let plan = planner(destination, &[])
			.plan(vec![request(
				PlanSource::Dir {
					dirs: vec![listed(&root, hidden.clone())],
					files: vec![listed(&hidden, file("inside.txt", 4))],
					root,
				},
				destination,
			)])
			.unwrap();

		assert_eq!(name(&plan.dirs[1].name), hidden.uuid.to_string());
		assert_eq!(plan.files.len(), 1);
		assert_eq!(plan.files[0].parent, DestParent::Planned(1));
		assert_eq!(plan.renamed.len(), 1);
		assert_eq!(plan.renamed[0].reason, RenameReason::Undecryptable);
		assert_eq!(plan.renamed[0].source_uuid, hidden.uuid);
	}

	#[test]
	fn undecryptable_top_level_dir_is_reported_too() {
		let destination = Uuid::new_v4();
		let root = dir(None);
		let plan = planner(destination, &[])
			.plan(vec![request(
				PlanSource::Dir {
					root: root.clone(),
					dirs: Vec::new(),
					files: Vec::new(),
				},
				destination,
			)])
			.unwrap();
		assert_eq!(name(&plan.dirs[0].name), root.uuid.to_string());
		assert_eq!(plan.renamed[0].reason, RenameReason::Undecryptable);
	}

	#[test]
	fn undecryptable_files_are_skipped_and_reported() {
		let destination = Uuid::new_v4();
		let root = dir(Some("root"));
		let hidden = undecryptable_file(9);
		let hidden_uuid = hidden.uuid();
		let top = undecryptable_file(3);
		let top_uuid = top.uuid();
		let plan = planner(destination, &[])
			.plan(vec![
				request(
					PlanSource::Dir {
						dirs: Vec::new(),
						files: vec![listed(&root, hidden)],
						root,
					},
					destination,
				),
				request(PlanSource::File(top), destination),
			])
			.unwrap();

		assert!(plan.files.is_empty());
		assert_eq!(plan.skipped.len(), 2);
		assert_eq!(plan.skipped[0].bytes, 9);
		assert_eq!(
			plan.skipped[0].reason,
			SkipReason::UndecryptableFile { uuid: hidden_uuid }
		);
		assert_eq!(plan.skipped[0].source_path, format!("root/{hidden_uuid}"));
		assert_eq!(
			plan.skipped[1].reason,
			SkipReason::UndecryptableFile { uuid: top_uuid }
		);
		// a skipped top-level file has no planned top-level item
		assert_eq!(plan.top_level.len(), 1);
		assert_eq!(plan.totals.bytes, 0);
	}

	#[test]
	fn unreachable_entries_are_reported_in_one_aggregate() {
		let destination = Uuid::new_v4();
		let root = dir(Some("root"));
		let orphan_parent = dir(Some("gone"));
		let orphan = dir(Some("orphan"));
		let plan = planner(destination, &[])
			.plan(vec![request(
				PlanSource::Dir {
					dirs: vec![listed(&orphan_parent, orphan.clone())],
					files: vec![
						listed(&orphan, file("f", 6)),
						listed(&root, file("kept", 1)),
					],
					root,
				},
				destination,
			)])
			.unwrap();

		assert_eq!(plan.dirs.len(), 1);
		assert_eq!(plan.files.len(), 1);
		assert_eq!(plan.skipped.len(), 1);
		assert_eq!(plan.skipped[0].reason, SkipReason::Unreachable { count: 2 });
		assert_eq!(plan.skipped[0].bytes, 6);
	}

	#[test]
	fn invalid_legacy_names_are_encoded_and_reported() {
		let destination = Uuid::new_v4();
		let root = dir(Some("root"));
		let plan = planner(destination, &[])
			.plan(vec![request(
				PlanSource::Dir {
					dirs: Vec::new(),
					files: vec![listed(&root, file("a:b.txt", 1))],
					root,
				},
				destination,
			)])
			.unwrap();
		let encoded = encode_name("a:b.txt").unwrap();
		assert_eq!(plan.files[0].name, encoded);
		assert_eq!(plan.renamed.len(), 1);
		assert_eq!(plan.renamed[0].reason, RenameReason::InvalidName);
	}

	#[test]
	fn deep_trees_are_planned_iteratively() {
		const DEPTH: usize = 10_000;
		let destination = Uuid::new_v4();
		let root = dir(Some("root"));
		let mut parent = root.clone();
		let mut dirs = Vec::with_capacity(DEPTH);
		for _ in 0..DEPTH {
			let child = dir(Some("d"));
			dirs.push(listed(&parent, child.clone()));
			parent = child;
		}
		let files = vec![listed(&parent, file("leaf", 2))];

		let plan = std::thread::Builder::new()
			.stack_size(256 * 1024)
			.spawn(move || {
				planner(destination, &[])
					.plan(vec![request(
						PlanSource::Dir { root, dirs, files },
						destination,
					)])
					.unwrap()
			})
			.unwrap()
			.join()
			.expect("planning a deep tree must not overflow the stack");

		assert_eq!(plan.dirs.len(), DEPTH + 1);
		assert_parent_first(&plan);
		assert_eq!(plan.dirs[0].descendant_dirs, DEPTH as u64);
		assert_eq!(plan.dirs[0].descendant_bytes, 2);
		assert_eq!(plan.dirs[DEPTH].descendant_files, 1);
	}
}
