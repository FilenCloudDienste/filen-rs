//! An in-memory sync root: ordinary items and placeholders by path, a user who changes them from
//! another process (each change records the events the watcher would report, and a placeholder's
//! delete or rename becomes a request the core answers), and the provider's side, [`LocalFs`],
//! recording what the core did as [`LocalOp`]s.

use std::{
	collections::{BTreeMap, HashMap, HashSet},
	fmt,
	path::{Path, PathBuf},
	sync::{
		Arc, Mutex, MutexGuard, PoisonError,
		atomic::{AtomicBool, Ordering},
	},
};

use filen_sdk_rs::thumbnail::{ByteSource, MemSource};
use filen_types::fs::Uuid;

use crate::{
	failure::Failure,
	identity::{IdentityBlob, PlaceholderIdentity},
	names::PlaceholderName,
	os::{
		CheckedHash, DehydrateReason, DehydrateRequest, DehydrateResponder, DeleteRequest,
		DeleteResponder, EntryOutcome, FileSnapshot, FileStamp, IfPinned, LocalEntry, LocalError,
		LocalEvent, LocalFileId, LocalFs, LocalKind, PinAttr, PlaceholderState, RenameRequest,
		RenameResponder, RenameTarget, UnsupportedKind, Updated,
	},
	placeholder::{PlaceholderSpec, SpecKind},
	root_path::{CaseKey, RootPath, RootPathError},
	sealed,
};

/// The prefix [`LocalFs::upload_path`] gives the fake's paths, which [`FakeLocal::read_upload`]
/// resolves.
const UPLOAD_PREFIX: &str = "fake-root:";

/// What the provider did to the fake root, or how a request it answered ended, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalOp {
	/// An ordinary file became the placeholder `identity`.
	Converted {
		path: RootPath,
		identity: PlaceholderIdentity,
		in_sync: bool,
	},
	/// An ordinary directory became the placeholder `identity`.
	ConvertedDir {
		path: RootPath,
		identity: PlaceholderIdentity,
		populated: bool,
	},
	/// A file placeholder got the identity `identity`.
	Updated {
		path: RootPath,
		identity: PlaceholderIdentity,
		in_sync: bool,
	},
	/// A placeholder got the identity `identity` and was marked not in sync.
	Retagged {
		path: RootPath,
		identity: PlaceholderIdentity,
	},
	/// A directory placeholder was marked in sync.
	DirInSync(RootPath),
	/// A directory placeholder was marked populated by the provider.
	DirPopulated(RootPath),
	/// A file placeholder was hydrated whole.
	Hydrated(RootPath),
	/// A partly hydrated file was hashed: reading it would have asked the provider for its data.
	HashedPartial(RootPath),
	/// A file placeholder's bytes on disk were hashed through a handle that refuses no other.
	HashChecked(RootPath),
	/// A clean file placeholder's bytes on disk were removed.
	Dehydrated(RootPath),
	/// A placeholder was given the pin `pin` of its own.
	PinSet { path: RootPath, pin: PinAttr },
	/// The placeholder `identity` was created at `path`.
	PlaceholderCreated {
		path: RootPath,
		identity: PlaceholderIdentity,
	},
	/// The provider renamed `from` to `to`.
	Renamed { from: RootPath, to: RootPath },
	/// A clean file placeholder became the placeholder `identity` of a newer remote version,
	/// dehydrated.
	RemoteUpdated {
		path: RootPath,
		identity: PlaceholderIdentity,
	},
	/// A clean placeholder was removed.
	Removed(RootPath),
	/// A clean placeholder was renamed from `from` to `to`.
	RenamedClean { from: RootPath, to: RootPath },
	/// A directory placeholder got the identity `identity` and was marked in sync.
	DirUpdated {
		path: RootPath,
		identity: PlaceholderIdentity,
	},
	/// A delete request was allowed, or vetoed with the failure given.
	DeleteAnswered {
		path: RootPath,
		answer: Result<(), Failure>,
	},
	/// A rename request was allowed, or vetoed with the failure given.
	RenameAnswered {
		from: RootPath,
		answer: Result<(), Failure>,
	},
	/// A delete responder was dropped unanswered, which vetoes the delete.
	DeleteAbandoned(RootPath),
	/// A dehydration request was allowed or vetoed.
	DehydrateAnswered { path: RootPath, allowed: bool },
	/// A dehydration responder was dropped unanswered, which vetoes the dehydration.
	DehydrateAbandoned(RootPath),
	/// A rename responder was dropped unanswered, which vetoes the rename.
	RenameAbandoned(RootPath),
	/// A placeholder was made an ordinary item.
	Reverted(RootPath),
	/// A file's bytes were copied into the new ordinary file `to`.
	CopiedToPlain { from: RootPath, to: RootPath },
}

/// One item of the fake root, as a test sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FakeEntry {
	/// The path as spelled.
	pub path: RootPath,
	/// Whether it is a directory.
	pub is_dir: bool,
	/// A file's bytes (zeros where a placeholder holds none).
	pub bytes: Vec<u8>,
	/// What the placeholder stands for, `None` for an ordinary item or an identity that does not
	/// decode.
	pub identity: Option<PlaceholderIdentity>,
	/// Whether it is a placeholder.
	pub placeholder: bool,
	/// Whether a placeholder is marked in sync.
	pub in_sync: bool,
	/// A placeholder's pin of its own; [`PinAttr::Unspecified`] for an ordinary item.
	pub pin: PinAttr,
	/// How many of a file placeholder's bytes are on disk; an ordinary file's size.
	pub on_disk: u64,
}

/// The fake sync root. Clones share one state, which every responder holds too.
#[derive(Debug, Clone)]
pub struct FakeLocal(
	// Reason for the Arc: the test, the core's tasks and the responders handed out observe one
	// state.
	Arc<Mutex<LocalState>>,
);

#[derive(Debug)]
struct LocalState {
	nodes: BTreeMap<CaseKey, Node>,
	next_id: u128,
	clock: i64,
	ops: Vec<LocalOp>,
	watch: Vec<LocalEvent>,
	lost_events: bool,
	/// A write the user is in the middle of when the drive next copies the file at the path: the
	/// copy reads the torn bytes, and the write then ends with the final ones.
	torn_copy: Option<(CaseKey, Vec<u8>, Vec<u8>)>,
	/// Where upload paths are real files, written as they are handed out, for a real drive to
	/// read.
	mirror: Option<PathBuf>,
	/// Open settled handles, by file id.
	settled: HashMap<u128, u32>,
	/// Open write handles, by file id.
	writers: HashMap<u128, u32>,
	/// Open read handles that do not share deleting, by file id.
	readers: HashMap<u128, u32>,
	/// Every open read handle, sharing deleting or not, by file id.
	all_readers: HashMap<u128, u32>,
	/// The next dehydrate fails with this.
	fail_dehydrate: Option<LocalError>,
	/// How many dehydrates from now are refused as busy.
	busy_dehydrations: u32,
	/// What the user does to the file at the path while the provider hashes it.
	during_hash: Option<(CaseKey, DuringHash)>,
	/// An ordinary file the user moves over another path while the provider next hashes it through
	/// a settled handle: from, to.
	replace_during_settled_hash: Option<(RootPath, RootPath)>,
	/// An ordinary directory the user renames once the provider opened it to convert it, and before
	/// it does.
	during_dir_convert: Option<DuringDirConvert>,
	/// A pin the user gives an item just as the provider acts on another.
	pin_during: Option<PinDuring>,
	/// Where a hydration takes a file's bytes from, by its stable id.
	source: Option<Source>,
	fail_convert: Option<LocalError>,
	fail_update: Option<LocalError>,
	/// Every update of a file placeholder's identity and in-sync state fails with this, while set.
	fail_file_updates: Option<LocalError>,
	/// While set, a mark in sync of a file placeholder succeeds and leaves it not in sync, raising
	/// a watcher modification, as a platform that acknowledges a mark it then loses would.
	lose_in_sync_marks: bool,
	fail_hydrate: Option<LocalError>,
	/// Fails every hydrate while set, after the one `fail_hydrate` fails.
	fail_hydrations: Option<LocalError>,
	/// Hydrations succeed and leave the file as partly on disk as it was, as the
	/// `ERROR_NOT_A_CLOUD_FILE` race leaves a file.
	stall_hydration: bool,
	fail_rename: Option<LocalError>,
	/// Paths whose next allowed delete or rename fails after the answer.
	fail_completion: HashSet<CaseKey>,
	/// Directories that hold, beside their items, one whose name is not valid Unicode.
	unrepresentable: HashSet<CaseKey>,
	/// Every placeholder the provider creates fails with this, while set.
	fail_placements: Option<LocalError>,
	/// Every placeholder of one of these names the provider creates fails with the error.
	fail_placements_named: HashMap<String, LocalError>,
	/// Paths whose state reads fail with the error, while set.
	fail_inspect: HashMap<CaseKey, LocalError>,
	/// How often the provider read each path's state.
	inspected: HashMap<CaseKey, u32>,
	/// Directories whose listings fail with the error, while set.
	fail_list: HashMap<CaseKey, LocalError>,
	/// Paths whose pin changes fail with the error, while set.
	fail_pin_change: HashMap<CaseKey, LocalError>,
	/// Paths whose reverts to an ordinary item fail with the error, while set: the platform's own
	/// unregister then takes the item, whatever it holds.
	fail_revert: HashMap<CaseKey, LocalError>,
	/// Paths whose copies to an ordinary file fail with the error, while set.
	fail_copy: HashMap<CaseKey, LocalError>,
	/// How many more reverts to plain before a test kills the helper.
	kill_after_reverts: Option<u32>,
	/// Every read of where an open item is now fails with this, while set.
	fail_handle_paths: Option<LocalError>,
	/// Paths at or under which the watcher's events reach no job ([`FakeLocal::swallow_events`]).
	swallowed: Vec<RootPath>,
}

/// What the user does to a file while the provider hashes it.
#[derive(Debug, Clone)]
enum DuringHash {
	/// Writes these bytes.
	Write(Vec<u8>),
	/// Gives it this pin of its own.
	Pin(PinAttr),
}

/// What the user does to an ordinary directory once the provider opened it to convert it.
#[derive(Debug, Clone)]
struct DuringDirConvert {
	/// The directory, renamed to `to`.
	from: RootPath,
	to: RootPath,
	/// Whether a new ordinary directory is then made at `from`.
	refill: bool,
}

/// A pin the user gives the item `target` as the provider acts on the item `on`.
#[derive(Debug, Clone)]
struct PinDuring {
	on: CaseKey,
	act: ProviderAct,
	target: RootPath,
	pin: PinAttr,
}

/// What the provider does that [`PinDuring`] waits for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderAct {
	/// Changes the item's pin, just after.
	PinChange,
	/// Dehydrates the item, just before.
	Dehydrate,
}

#[derive(Debug, Clone)]
struct Node {
	path: RootPath,
	id: u128,
	kind: NodeKind,
	placeholder: Option<FakePlaceholder>,
	unsupported: Option<UnsupportedKind>,
}

#[derive(Debug, Clone)]
enum NodeKind {
	File {
		bytes: Vec<u8>,
		created: i64,
		modified: i64,
		changed: i64,
	},
	Dir {
		created: i64,
	},
}

#[derive(Debug, Clone)]
struct FakePlaceholder {
	identity: IdentityBlob,
	in_sync: bool,
	on_disk: u64,
	modified_bytes: u64,
	/// A directory's: whether the platform populated it. A file's is always `true`.
	populated: bool,
	/// The pin of its own.
	pin: PinAttr,
}

impl FakePlaceholder {
	/// A placeholder of `identity`, in sync, holding no byte written since.
	fn new(identity: PlaceholderIdentity, on_disk: u64, populated: bool) -> Self {
		Self {
			identity: identity.encode(),
			in_sync: true,
			on_disk,
			modified_bytes: 0,
			populated,
			pin: PinAttr::Unspecified,
		}
	}
}

impl Node {
	fn size(&self) -> u64 {
		match &self.kind {
			NodeKind::File { bytes, .. } => u64::try_from(bytes.len()).expect("fits a u64"),
			NodeKind::Dir { .. } => 0,
		}
	}

	fn local_kind(&self) -> LocalKind {
		match &self.kind {
			NodeKind::File {
				created,
				modified,
				changed,
				..
			} => LocalKind::File(FileStamp {
				size: self.size(),
				created: *created,
				modified: *modified,
				changed: *changed,
			}),
			NodeKind::Dir { created } => LocalKind::Dir { created: *created },
		}
	}

	/// The placeholder state of a placeholder node.
	fn placeholder_state(&self, placeholder: &FakePlaceholder) -> PlaceholderState {
		PlaceholderState::new(
			placeholder.in_sync,
			match self.kind {
				NodeKind::File { .. } => placeholder.on_disk < self.size(),
				NodeKind::Dir { .. } => !placeholder.populated,
			},
			placeholder.on_disk,
			placeholder.modified_bytes,
			placeholder.pin,
		)
	}

	/// The file placeholder node as it is now.
	fn snapshot(&self) -> Result<FileSnapshot, LocalError> {
		let (LocalKind::File(stamp), Some(placeholder)) = (self.local_kind(), &self.placeholder)
		else {
			return Err(LocalError::NotFound);
		};
		Ok(FileSnapshot {
			id: LocalFileId::new(self.id),
			stamp,
			state: self.placeholder_state(placeholder),
		})
	}

	fn view(&self) -> FakeEntry {
		FakeEntry {
			path: self.path.clone(),
			is_dir: matches!(self.kind, NodeKind::Dir { .. }),
			bytes: match &self.kind {
				NodeKind::File { bytes, .. } => bytes.clone(),
				NodeKind::Dir { .. } => Vec::new(),
			},
			identity: self
				.placeholder
				.as_ref()
				.and_then(|placeholder| PlaceholderIdentity::decode(&placeholder.identity).ok()),
			placeholder: self.placeholder.is_some(),
			in_sync: self
				.placeholder
				.as_ref()
				.is_some_and(|placeholder| placeholder.in_sync),
			pin: self
				.placeholder
				.as_ref()
				.map_or(PinAttr::Unspecified, |placeholder| placeholder.pin),
			on_disk: self
				.placeholder
				.as_ref()
				.map_or(self.size(), |placeholder| placeholder.on_disk),
		}
	}
}

impl LocalState {
	fn tick(&mut self) -> i64 {
		self.clock += 1;
		self.clock
	}

	fn get(&self, path: &RootPath) -> Option<&Node> {
		self.nodes.get(&path.case_key())
	}

	fn get_mut(&mut self, path: &RootPath) -> Option<&mut Node> {
		self.nodes.get_mut(&path.case_key())
	}

	fn by_id_mut(&mut self, id: u128) -> Option<&mut Node> {
		self.nodes.values_mut().find(|node| node.id == id)
	}

	/// The error [`FakeLocal::fail_revert`] set for the path the item `id` is at, if any.
	fn revert_refused(&self, id: u128) -> Result<(), LocalError> {
		match self
			.by_id(id)
			.and_then(|node| self.fail_revert.get(&node.path.case_key()))
		{
			Some(error) => Err(*error),
			None => Ok(()),
		}
	}

	fn by_id(&self, id: u128) -> Option<&Node> {
		self.nodes.values().find(|node| node.id == id)
	}

	fn assert_parent_dir(&self, path: &RootPath) {
		let parent = path.parent().expect("not the root");
		assert!(
			self.get(&parent)
				.is_some_and(|node| matches!(node.kind, NodeKind::Dir { .. })),
			"{parent} is not a directory"
		);
	}

	fn insert(&mut self, path: RootPath, kind: NodeKind, placeholder: Option<FakePlaceholder>) {
		self.assert_parent_dir(&path);
		assert!(self.get(&path).is_none(), "{path} exists");
		self.next_id += 1;
		let id = self.next_id;
		self.nodes.insert(
			path.case_key(),
			Node {
				path,
				id,
				kind,
				placeholder,
				unsupported: None,
			},
		);
	}

	/// Removes `path` and everything under it.
	fn remove_tree(&mut self, path: &RootPath) {
		self.nodes.retain(|_, node| !node.path.starts_with(path));
	}

	/// Moves `from` and everything under it to `to`.
	fn move_tree(&mut self, from: &RootPath, to: &RootPath) {
		let moved: Vec<Node> = self
			.nodes
			.values()
			.filter(|node| node.path.starts_with(from))
			.cloned()
			.collect();
		self.remove_tree(from);
		for mut node in moved {
			node.path = node.path.rebase(from, to).expect("under `from`");
			self.nodes.insert(node.path.case_key(), node);
		}
	}

	fn taken(&self, from: &RootPath, to: &RootPath) -> bool {
		from.case_key() != to.case_key() && self.get(to).is_some()
	}

	/// Whether a user's handle on `path` or an item under it refuses a delete or a rename: a
	/// writer, or a reader that does not share deleting.
	fn refuses_delete(&self, path: &RootPath) -> bool {
		self.nodes.values().any(|node| {
			node.path.starts_with(path)
				&& (self.writers.get(&node.id).is_some_and(|open| *open > 0)
					|| self.readers.get(&node.id).is_some_and(|open| *open > 0))
		})
	}

	/// The clean placeholder `expect` at `path`: in sync, and for a file with no byte written
	/// since. [`LocalError::NotFound`] when another item is there.
	fn clean(
		&mut self,
		path: &RootPath,
		expect: PlaceholderIdentity,
	) -> Result<&mut Node, LocalError> {
		let node = self.get_mut(path).ok_or(LocalError::NotFound)?;
		let placeholder = node.placeholder.as_ref().ok_or(LocalError::NotFound)?;
		if PlaceholderIdentity::decode(&placeholder.identity).ok() != Some(expect) {
			return Err(LocalError::NotFound);
		}
		if !placeholder.in_sync || placeholder.modified_bytes > 0 {
			return Err(LocalError::Modified);
		}
		Ok(node)
	}
}

/// Where a hydration takes a file's bytes from: the drive's content of a stable id.
#[derive(Clone)]
struct Source(Arc<dyn Fn(Uuid) -> Vec<u8> + Send + Sync>);

impl fmt::Debug for Source {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("Source")
	}
}

/// The stable id of the file `placeholder` stands for, if it stands for one.
fn held_stable(placeholder: &FakePlaceholder) -> Option<Uuid> {
	match PlaceholderIdentity::decode(&placeholder.identity) {
		Ok(PlaceholderIdentity::File { stable, .. }) => Some(stable),
		_ => None,
	}
}

/// Fills the file `node`, a placeholder of the file `stable`, with what `source` gives for it, as
/// a hydration does: only when that is as long as the file, which keeps its bytes otherwise.
fn fill_from(node: &mut Node, stable: Option<Uuid>, source: Option<Source>) {
	let size = node.size();
	let filled = source
		.zip(stable)
		.map(|(Source(source), stable)| source(stable));
	if let (Some(filled), NodeKind::File { bytes, .. }) = (filled, &mut node.kind)
		&& u64::try_from(filled.len()).ok() == Some(size)
	{
		*bytes = filled;
	}
}

/// What a user's delete or rename did.
#[derive(Debug)]
pub enum UserChange<R> {
	/// The item was ordinary: done, with no callback.
	Done,
	/// The item is a placeholder: the platform asks the provider first.
	Asked(R),
}

impl<R> UserChange<R> {
	/// The request, panicking on an ordinary item's change.
	pub fn asked(self) -> R {
		match self {
			Self::Asked(request) => request,
			Self::Done => panic!("the change raised no request"),
		}
	}
}

impl FakeLocal {
	/// A sync root registered as the placeholder `root`, holding nothing and not populated yet:
	/// until [`FakeLocal::populate`] or [`FakeLocal::set_populated`], the remote side leaves it
	/// alone.
	pub fn new(root: PlaceholderIdentity) -> Self {
		let mut nodes = BTreeMap::new();
		nodes.insert(
			RootPath::root().case_key(),
			Node {
				path: RootPath::root(),
				id: 1,
				kind: NodeKind::Dir { created: 0 },
				placeholder: Some(FakePlaceholder::new(root, 0, false)),
				unsupported: None,
			},
		);
		Self(Arc::new(Mutex::new(LocalState {
			nodes,
			next_id: 1,
			clock: 0,
			ops: Vec::new(),
			watch: Vec::new(),
			lost_events: false,
			torn_copy: None,
			mirror: None,
			settled: HashMap::new(),
			writers: HashMap::new(),
			readers: HashMap::new(),
			all_readers: HashMap::new(),
			fail_dehydrate: None,
			busy_dehydrations: 0,
			during_hash: None,
			replace_during_settled_hash: None,
			during_dir_convert: None,
			pin_during: None,
			source: None,
			fail_convert: None,
			fail_update: None,
			fail_file_updates: None,
			lose_in_sync_marks: false,
			fail_hydrate: None,
			fail_hydrations: None,
			stall_hydration: false,
			fail_rename: None,
			fail_completion: HashSet::new(),
			unrepresentable: HashSet::new(),
			fail_placements: None,
			fail_placements_named: HashMap::new(),
			fail_inspect: HashMap::new(),
			inspected: HashMap::new(),
			fail_list: HashMap::new(),
			fail_pin_change: HashMap::new(),
			fail_revert: HashMap::new(),
			fail_copy: HashMap::new(),
			kill_after_reverts: None,
			fail_handle_paths: None,
			swallowed: Vec::new(),
		})))
	}

	fn state(&self) -> MutexGuard<'_, LocalState> {
		self.0.lock().unwrap_or_else(PoisonError::into_inner)
	}

	// ---- set-up ----

	/// A file placeholder for `identity` at `path`, in sync, holding `bytes`; `hydrated` says
	/// whether they are on disk.
	pub fn place_file(&self, path: &RootPath, identity: PlaceholderIdentity, bytes: &[u8]) {
		self.place_file_with(path, identity, bytes, true);
	}

	/// As [`FakeLocal::place_file`], hydrated or not.
	pub fn place_file_with(
		&self,
		path: &RootPath,
		identity: PlaceholderIdentity,
		bytes: &[u8],
		hydrated: bool,
	) {
		let mut state = self.state();
		let now = state.tick();
		let size = u64::try_from(bytes.len()).expect("fits a u64");
		state.insert(
			path.clone(),
			NodeKind::File {
				bytes: bytes.to_vec(),
				created: now,
				modified: now,
				changed: now,
			},
			Some(FakePlaceholder::new(
				identity,
				if hydrated { size } else { 0 },
				true,
			)),
		);
	}

	/// A directory placeholder for `identity` at `path`, in sync and populated.
	pub fn place_dir(&self, path: &RootPath, identity: PlaceholderIdentity) {
		self.place_dir_with(path, identity, true);
	}

	/// A directory placeholder for `identity` at `path`, in sync, which the platform has not
	/// populated yet.
	pub fn place_dir_unpopulated(&self, path: &RootPath, identity: PlaceholderIdentity) {
		self.place_dir_with(path, identity, false);
	}

	fn place_dir_with(&self, path: &RootPath, identity: PlaceholderIdentity, populated: bool) {
		let mut state = self.state();
		let now = state.tick();
		state.insert(
			path.clone(),
			NodeKind::Dir { created: now },
			Some(FakePlaceholder::new(identity, 0, populated)),
		);
	}

	/// Marks the directory placeholder at `path` populated or not, with no other change.
	pub fn set_populated(&self, path: &RootPath, populated: bool) {
		let mut state = self.state();
		let node = state.get_mut(path).expect("the directory exists");
		node.placeholder.as_mut().expect("a placeholder").populated = populated;
	}

	/// Populates the directory placeholder `dir` with `specs`, as the platform does with a
	/// `FETCH_PLACEHOLDERS` answer: a name already taken keeps what holds it.
	pub fn populate(&self, dir: &RootPath, specs: &[PlaceholderSpec]) -> Vec<EntryOutcome> {
		let outcomes = specs
			.iter()
			.map(|spec| match self.place_spec(dir, spec, false) {
				Ok(()) => EntryOutcome::Created,
				Err(_) => EntryOutcome::Exists,
			})
			.collect();
		self.set_populated(dir, true);
		self.state().watch.push(LocalEvent::Modified(dir.clone()));
		outcomes
	}

	/// Makes the file placeholder at `path` hold only `on_disk` of its bytes locally.
	pub fn set_on_disk(&self, path: &RootPath, on_disk: u64) {
		let mut state = self.state();
		let node = state.get_mut(path).expect("the item exists");
		node.placeholder.as_mut().expect("a placeholder").on_disk = on_disk;
	}

	/// Makes the item at `path` a symbolic link.
	pub fn make_symlink(&self, path: &RootPath) {
		self.state()
			.get_mut(path)
			.expect("the item exists")
			.unsupported = Some(UnsupportedKind::Symlink);
	}

	/// Fails the next convert with `error`.
	pub fn fail_convert(&self, error: LocalError) {
		self.state().fail_convert = Some(error);
	}

	/// Fails the next identity update with `error`.
	pub fn fail_update(&self, error: LocalError) {
		self.state().fail_update = Some(error);
	}

	/// Fails every update of a file placeholder's identity and in-sync state with `error` from now
	/// on, as a mark in sync the platform keeps refusing does, or none with `None`.
	pub fn fail_file_updates(&self, error: Option<LocalError>) {
		self.state().fail_file_updates = error;
	}

	/// Makes every later mark in sync of a file placeholder succeed and leave it not in sync,
	/// raising a watcher modification; `false` marks them again.
	pub fn lose_in_sync_marks(&self, lose: bool) {
		self.state().lose_in_sync_marks = lose;
	}

	/// Fails the next dehydrate with `error`.
	pub fn fail_dehydrate(&self, error: LocalError) {
		self.state().fail_dehydrate = Some(error);
	}

	/// Refuses the next `count` dehydrates as busy, as another process holding the file does.
	pub fn busy_dehydrations(&self, count: u32) {
		self.state().busy_dehydrations = count;
	}

	/// Makes the user write `bytes` to the file `path` while the provider's next hash of it runs.
	pub fn edit_during_hash(&self, path: &RootPath, bytes: &[u8]) {
		self.state().during_hash = Some((path.case_key(), DuringHash::Write(bytes.to_vec())));
	}

	/// Makes the user move the ordinary file `from` over `to` ([`FakeLocal::user_replace`]) while
	/// the provider's next hash of it through a settled handle runs: the handle shares deleting, so
	/// the rename goes through, and the handle follows the file.
	pub fn replace_during_settled_hash(&self, from: &RootPath, to: &RootPath) {
		self.state().replace_during_settled_hash = Some((from.clone(), to.clone()));
	}

	/// Makes the user rename the ordinary directory `from` to `to` ([`FakeLocal::user_rename`]) once
	/// the provider opened it to convert it and checked it is the directory it expects, and before
	/// the convert: the handle shares deleting, so the rename goes through, and the handle follows
	/// the directory.
	pub fn rename_during_dir_convert(&self, from: &RootPath, to: &RootPath) {
		self.state().during_dir_convert = Some(DuringDirConvert {
			from: from.clone(),
			to: to.clone(),
			refill: false,
		});
	}

	/// As [`FakeLocal::rename_during_dir_convert`], then makes a new ordinary directory at `from`.
	pub fn replace_during_dir_convert(&self, from: &RootPath, to: &RootPath) {
		self.state().during_dir_convert = Some(DuringDirConvert {
			from: from.clone(),
			to: to.clone(),
			refill: true,
		});
	}

	/// Runs what the user does to the directory `path` once the provider opened it to convert it.
	fn run_during_dir_convert(&self, path: &RootPath) {
		let during = self
			.state()
			.during_dir_convert
			.take_if(|during| during.from.case_key() == path.case_key());
		if let Some(during) = during {
			assert!(matches!(
				self.user_rename(&during.from, &during.to),
				Ok(UserChange::Done)
			));
			if during.refill {
				self.user_mkdir(&during.from);
			}
		}
	}

	/// Makes the user give the file `path` the pin `pin` while the provider's next hash of it runs.
	pub fn pin_during_hash(&self, path: &RootPath, pin: PinAttr) {
		self.state().during_hash = Some((path.case_key(), DuringHash::Pin(pin)));
	}

	/// Makes the user give `target` the pin `pin` as the provider next does `act` on `on`, as a
	/// user racing the provider does.
	pub fn pin_during(&self, on: &RootPath, act: ProviderAct, target: &RootPath, pin: PinAttr) {
		self.state().pin_during = Some(PinDuring {
			on: on.case_key(),
			act,
			target: target.clone(),
			pin,
		});
	}

	/// Fills the file placeholder `path` with `bytes`, all on disk, as a reader's fetches through
	/// the dispatcher fill it, its in-sync state left alone.
	pub fn downloaded(&self, path: &RootPath, bytes: &[u8]) {
		let mut state = self.state();
		let node = state.get_mut(path).expect("the item exists");
		let NodeKind::File { bytes: held, .. } = &mut node.kind else {
			panic!("{path} is not a file");
		};
		assert_eq!(held.len(), bytes.len(), "a download fills the file's size");
		*held = bytes.to_vec();
		node.placeholder.as_mut().expect("a placeholder").on_disk =
			u64::try_from(bytes.len()).expect("fits a u64");
		state.watch.push(LocalEvent::Modified(path.clone()));
	}

	/// Makes every hydration fill a file placeholder with the bytes `source` gives for its stable
	/// id, as the platform fills it with what the provider transfers; the bytes a placeholder was
	/// placed with stay otherwise (a created one holds zeros).
	pub fn hydrate_from(&self, source: impl Fn(Uuid) -> Vec<u8> + Send + Sync + 'static) {
		self.state().source = Some(Source(Arc::new(source)));
	}

	/// Fails the next hydrate with `error`.
	pub fn fail_hydrate(&self, error: LocalError) {
		self.state().fail_hydrate = Some(error);
	}

	/// Fails every hydrate with `error` from now on, whoever asks for it, or none with `None`.
	pub fn fail_hydrations(&self, error: Option<LocalError>) {
		self.state().fail_hydrations = error;
	}

	/// Puts an item whose name is not valid Unicode in the directory `dir`: listing it yields
	/// [`RootPathError::NotUnicode`] beside its other items.
	pub fn add_unrepresentable(&self, dir: &RootPath) {
		self.state().unrepresentable.insert(dir.case_key());
	}

	/// Fails every placeholder the provider creates from now on with `error`, or none with `None`.
	pub fn fail_placements(&self, error: Option<LocalError>) {
		self.state().fail_placements = error;
	}

	/// Fails every placeholder named `name` the provider creates from now on with `error`.
	pub fn fail_placements_named(&self, name: &str, error: LocalError) {
		self.state()
			.fail_placements_named
			.insert(name.to_owned(), error);
	}

	/// Makes every read of `path`'s state fail with `error` while it is set; `None` clears it.
	pub fn fail_inspect(&self, path: &RootPath, error: Option<LocalError>) {
		let mut state = self.state();
		match error {
			Some(error) => state.fail_inspect.insert(path.case_key(), error),
			None => state.fail_inspect.remove(&path.case_key()),
		};
	}

	/// How often the provider read the state of `path`, whatever was there.
	pub fn inspections(&self, path: &RootPath) -> u32 {
		self.state()
			.inspected
			.get(&path.case_key())
			.copied()
			.unwrap_or(0)
	}

	/// Makes every listing of the directory `path` fail with `error` while it is set; `None` clears
	/// it.
	pub fn fail_list_dir(&self, path: &RootPath, error: Option<LocalError>) {
		let mut state = self.state();
		match error {
			Some(error) => state.fail_list.insert(path.case_key(), error),
			None => state.fail_list.remove(&path.case_key()),
		};
	}

	/// Makes every pin change of `path` fail with `error` while it is set; `None` clears it.
	pub fn fail_pin_change(&self, path: &RootPath, error: Option<LocalError>) {
		let mut state = self.state();
		match error {
			Some(error) => state.fail_pin_change.insert(path.case_key(), error),
			None => state.fail_pin_change.remove(&path.case_key()),
		};
	}

	/// Makes every revert of `path` to an ordinary item fail with `error` while it is set, as an
	/// item in a state the platform cannot revert; `None` clears it. The platform's unregister takes
	/// such an item, whatever it holds: the worst it could do, as what it does was not measured.
	pub fn fail_revert(&self, path: &RootPath, error: Option<LocalError>) {
		let mut state = self.state();
		match error {
			Some(error) => state.fail_revert.insert(path.case_key(), error),
			None => state.fail_revert.remove(&path.case_key()),
		};
	}

	/// Makes every copy of `path` to an ordinary file fail with `error` while it is set; `None`
	/// clears it.
	pub fn fail_copy(&self, path: &RootPath, error: Option<LocalError>) {
		let mut state = self.state();
		match error {
			Some(error) => state.fail_copy.insert(path.case_key(), error),
			None => state.fail_copy.remove(&path.case_key()),
		};
	}

	/// Makes every later hydration succeed without bringing the missing bytes.
	pub fn stall_hydration(&self) {
		self.state().stall_hydration = true;
	}

	/// Fails the provider's next rename with `error`.
	pub fn fail_rename(&self, error: LocalError) {
		self.state().fail_rename = Some(error);
	}

	/// Makes the next allowed delete or rename of `path` fail after the provider allowed it, as a
	/// local error after the acknowledgement would.
	pub fn fail_completion(&self, path: &RootPath) {
		self.state().fail_completion.insert(path.case_key());
	}

	// ---- the user, in another process ----

	/// Creates the ordinary file `path` holding `bytes`.
	pub fn user_create(&self, path: &RootPath, bytes: &[u8]) {
		let mut state = self.state();
		let now = state.tick();
		state.insert(
			path.clone(),
			NodeKind::File {
				bytes: bytes.to_vec(),
				created: now,
				modified: now,
				changed: now,
			},
			None,
		);
		state.watch.push(LocalEvent::Created(path.clone()));
		state.watch.push(LocalEvent::Modified(path.clone()));
	}

	/// Creates the ordinary directory `path`.
	pub fn user_mkdir(&self, path: &RootPath) {
		let mut state = self.state();
		let now = state.tick();
		state.insert(path.clone(), NodeKind::Dir { created: now }, None);
		state.watch.push(LocalEvent::Created(path.clone()));
	}

	/// Replaces the bytes of the file `path`, as a write through a handle opened for writing does:
	/// a placeholder is no longer in sync. Refused while the provider holds it settled.
	pub fn user_write(&self, path: &RootPath, bytes: &[u8]) -> Result<(), LocalError> {
		self.write(path, bytes, false)
	}

	/// Changes the attributes of the placeholder `path`, which clears its in-sync state and writes
	/// no byte.
	pub fn user_touch(&self, path: &RootPath) {
		let mut state = self.state();
		let node = state.get_mut(path).expect("the item exists");
		node.placeholder.as_mut().expect("a placeholder").in_sync = false;
		state.watch.push(LocalEvent::Modified(path.clone()));
	}

	/// Sets the pin of the placeholder `path`'s own, as `attrib +P` / `+U` / `-P -U` does: measured
	/// to leave its in-sync state and data alone, to raise no callback and one watcher
	/// modification.
	pub fn user_pin(&self, path: &RootPath, pin: PinAttr) {
		let mut state = self.state();
		let node = state.get_mut(path).expect("the item exists");
		node.placeholder.as_mut().expect("a placeholder").pin = pin;
		state.watch.push(LocalEvent::Modified(path.clone()));
	}

	/// Appends `more` to the file `path` through a handle opened for appending only, which was
	/// measured to leave a placeholder marked in sync while counting the bytes as modified.
	pub fn user_append_only(&self, path: &RootPath, more: &[u8]) -> Result<(), LocalError> {
		let mut bytes = self.entry(path).expect("the file exists").bytes;
		bytes.extend_from_slice(more);
		self.write(path, &bytes, true)
	}

	fn write(&self, path: &RootPath, new: &[u8], append_only: bool) -> Result<(), LocalError> {
		let mut state = self.state();
		let now = state.tick();
		let node = state.get(path).ok_or(LocalError::NotFound)?;
		let id = node.id;
		if state.settled.get(&id).is_some_and(|open| *open > 0) {
			return Err(LocalError::Busy);
		}
		let node = state.get_mut(path).expect("found above");
		let size = node.size();
		let NodeKind::File {
			bytes,
			modified,
			changed,
			..
		} = &mut node.kind
		else {
			panic!("{path} is not a file");
		};
		let old_len = u64::try_from(bytes.len()).expect("fits a u64");
		*bytes = new.to_vec();
		*modified = now;
		*changed = now;
		let new_len = u64::try_from(new.len()).expect("fits a u64");
		if let Some(placeholder) = &mut node.placeholder {
			assert_eq!(
				placeholder.on_disk, size,
				"a write to {path} hydrates it first; hydrate it in the test"
			);
			placeholder.on_disk = new_len;
			if append_only {
				placeholder.modified_bytes += new_len.saturating_sub(old_len);
			} else {
				placeholder.in_sync = false;
				placeholder.modified_bytes = new_len;
			}
		}
		state.watch.push(LocalEvent::Modified(path.clone()));
		Ok(())
	}

	/// Opens the file `path` for reading until the guard drops, sharing deleting or not: a reader
	/// that does not refuses the provider's deletes and renames of it.
	pub fn user_open_for_read(&self, path: &RootPath, share_delete: bool) -> FakeReader {
		let mut state = self.state();
		let id = state.get(path).expect("the file exists").id;
		if !share_delete {
			*state.readers.entry(id).or_default() += 1;
		}
		*state.all_readers.entry(id).or_default() += 1;
		FakeReader {
			local: self.clone(),
			id,
			counted: !share_delete,
		}
	}

	/// Opens the file `path` for writing until the guard drops.
	pub fn user_open_for_write(&self, path: &RootPath) -> FakeWriter {
		let mut state = self.state();
		let id = state.get(path).expect("the file exists").id;
		*state.writers.entry(id).or_default() += 1;
		FakeWriter {
			local: self.clone(),
			id,
			path: path.clone(),
		}
	}

	/// Deletes `path`: an ordinary item at once, a placeholder once the provider allows it.
	pub fn user_delete(&self, path: &RootPath) -> UserChange<(DeleteRequest, FakeDelete)> {
		let mut state = self.state();
		let node = state.get(path).expect("the item exists");
		match &node.placeholder {
			None => {
				state.remove_tree(path);
				state.watch.push(LocalEvent::Removed(path.clone()));
				UserChange::Done
			}
			Some(placeholder) => {
				let request = DeleteRequest::new(
					placeholder.identity.clone(),
					path.clone(),
					matches!(node.kind, NodeKind::Dir { .. }),
					false,
				);
				UserChange::Asked((
					request,
					FakeDelete {
						local: Some(self.clone()),
						path: path.clone(),
						removes: true,
					},
				))
			}
		}
	}

	/// Asks to dehydrate the placeholder `path` from another process, as `CfDehydratePlaceholder`
	/// does: the platform asks the provider first, and the bytes go once the provider dehydrates the
	/// file itself.
	pub fn user_dehydrate(&self, path: &RootPath) -> UserChange<(DehydrateRequest, FakeDehydrate)> {
		let state = self.state();
		let node = state.get(path).expect("the item exists");
		match &node.placeholder {
			None => UserChange::Done,
			Some(placeholder) => UserChange::Asked((
				DehydrateRequest::new(
					placeholder.identity.clone(),
					path.clone(),
					false,
					DehydrateReason::UserManual,
				),
				FakeDehydrate {
					local: Some(self.clone()),
					path: path.clone(),
				},
			)),
		}
	}

	/// A responder for a delete request a test builds itself, of the item at `path`: allowing it
	/// records the answer and removes nothing, as allowing an undelete does.
	pub fn delete_responder(&self, path: &RootPath) -> FakeDelete {
		FakeDelete {
			local: Some(self.clone()),
			path: path.clone(),
			removes: false,
		}
	}

	/// Renames `from` to `to`, refused when `to` is taken: an ordinary item at once, a placeholder
	/// once the provider allows it.
	pub fn user_rename(
		&self,
		from: &RootPath,
		to: &RootPath,
	) -> Result<UserChange<(RenameRequest, FakeRename)>, LocalError> {
		if self.state().taken(from, to) {
			return Err(LocalError::Exists);
		}
		self.user_rename_replacing(from, to)
	}

	/// Renames `from` to `to`, replacing what `to` holds, as `MoveFileEx(REPLACE_EXISTING)` does
	/// and as `ReplaceFileW` renames the replaced file onto the backup name it reserved: an
	/// ordinary item at once, a placeholder once the provider allows it, which was measured to ask
	/// nothing of the item replaced.
	pub fn user_rename_replacing(
		&self,
		from: &RootPath,
		to: &RootPath,
	) -> Result<UserChange<(RenameRequest, FakeRename)>, LocalError> {
		let mut state = self.state();
		state.assert_parent_dir(to);
		let node = state.get(from).ok_or(LocalError::NotFound)?;
		match &node.placeholder {
			None => {
				state.move_tree(from, to);
				state.watch.push(LocalEvent::Renamed {
					from: from.clone(),
					to: to.clone(),
				});
				Ok(UserChange::Done)
			}
			Some(placeholder) => Ok(UserChange::Asked((
				RenameRequest::new(
					placeholder.identity.clone(),
					from.clone(),
					RenameTarget::InRoot(to.clone()),
					matches!(node.kind, NodeKind::Dir { .. }),
				),
				FakeRename {
					local: Some(self.clone()),
					from: from.clone(),
					to: Some(to.clone()),
				},
			))),
		}
	}

	/// Moves `path` out of the root (another folder, or the recycle bin).
	pub fn user_move_out(&self, path: &RootPath) -> UserChange<(RenameRequest, FakeRename)> {
		let mut state = self.state();
		let node = state.get(path).expect("the item exists");
		match &node.placeholder {
			None => {
				state.remove_tree(path);
				state.watch.push(LocalEvent::Removed(path.clone()));
				UserChange::Done
			}
			Some(placeholder) => UserChange::Asked((
				RenameRequest::new(
					placeholder.identity.clone(),
					path.clone(),
					RenameTarget::OutOfRoot,
					matches!(node.kind, NodeKind::Dir { .. }),
				),
				FakeRename {
					local: Some(self.clone()),
					from: path.clone(),
					to: None,
				},
			)),
		}
	}

	/// Moves the ordinary file `from` over `to`, replacing what is there with no callback, as
	/// `MoveFileEx(REPLACE_EXISTING)` was measured to over a placeholder.
	pub fn user_replace(&self, from: &RootPath, to: &RootPath) {
		let mut state = self.state();
		assert!(
			state
				.get(from)
				.expect("the file exists")
				.placeholder
				.is_none(),
			"only an ordinary file replaces without a callback"
		);
		state.remove_tree(to);
		state.move_tree(from, to);
		state.watch.push(LocalEvent::Renamed {
			from: from.clone(),
			to: to.clone(),
		});
	}

	/// Removes the item at `path` and everything under it with no event, as a change the watcher
	/// missed.
	pub fn vanish(&self, path: &RootPath) {
		self.state().remove_tree(path);
	}

	/// Unregisters the root as Windows was measured to: every file placeholder not all on disk is
	/// deleted, and every other placeholder becomes an ordinary item. An item whose revert fails
	/// ([`FakeLocal::fail_revert`]) is deleted too, whatever it holds.
	pub fn unregister_root(&self) {
		let mut state = self.state();
		let unrevertable = std::mem::take(&mut state.fail_revert);
		state.nodes.retain(|key, node| {
			!unrevertable.contains_key(key)
				&& !matches!(
					(&node.kind, &node.placeholder),
					(NodeKind::File { bytes, .. }, Some(placeholder))
						if placeholder.on_disk < u64::try_from(bytes.len()).expect("fits a u64")
				)
		});
		for node in state.nodes.values_mut() {
			node.placeholder = None;
		}
	}

	/// Removes every empty ordinary directory, deepest first, as the helper does after an
	/// unregister: `None` when nothing is left in the root, else how many items other than
	/// directories are.
	pub fn remove_empty_dirs(&self) -> Option<u64> {
		let mut state = self.state();
		loop {
			let empty: Vec<CaseKey> = state
				.nodes
				.values()
				.filter(|node| {
					!node.path.is_root()
						&& node.placeholder.is_none()
						&& matches!(node.kind, NodeKind::Dir { .. })
						&& !state
							.nodes
							.values()
							.any(|child| child.path.parent().as_ref() == Some(&node.path))
				})
				.map(|node| node.path.case_key())
				.collect();
			if empty.is_empty() {
				let left: Vec<_> = state
					.nodes
					.values()
					.filter(|node| !node.path.is_root())
					.collect();
				if left.is_empty() {
					return None;
				}
				let items = left
					.iter()
					.filter(|node| !matches!(node.kind, NodeKind::Dir { .. }))
					.count();
				return Some(u64::try_from(items).unwrap_or(u64::MAX));
			}
			for key in empty {
				state.nodes.remove(&key);
			}
		}
	}

	/// Takes every item out of the root, as renaming the root moves them: what it held.
	pub fn take_all(&self) -> Vec<FakeEntry> {
		let mut state = self.state();
		let taken = state
			.nodes
			.values()
			.filter(|node| !node.path.is_root())
			.map(Node::view)
			.collect();
		state.nodes.retain(|_, node| node.path.is_root());
		taken
	}

	/// Kills the helper, by a panic, once the provider made `count` more kept items ordinary.
	pub fn kill_after_reverts(&self, count: u32) {
		self.state().kill_after_reverts = Some(count);
	}

	/// Every watcher event since the last call, in order; `[Rescan]` after
	/// [`FakeLocal::lose_watch_events`].
	pub fn take_watch_events(&self) -> Vec<LocalEvent> {
		let mut state = self.state();
		let mut events = std::mem::take(&mut state.watch);
		if std::mem::take(&mut state.lost_events) {
			return vec![LocalEvent::Rescan];
		}
		let swallowed = |path: &RootPath| {
			state
				.swallowed
				.iter()
				.any(|under| path.case_key() == under.case_key() || path.starts_with(under))
		};
		events.retain(|event| match event {
			LocalEvent::Created(path)
			| LocalEvent::Modified(path)
			| LocalEvent::Removed(path)
			| LocalEvent::Closed(path) => !swallowed(path),
			LocalEvent::Renamed { to, .. } => !swallowed(to),
			LocalEvent::Rescan => true,
		});
		events
	}

	/// Makes every watcher event naming a path at or under `path` (a rename by its target) reach no
	/// job, until [`FakeLocal::stop_swallowing`]: as when each job such an event brought looked in
	/// the instant the item there was a placeholder of another item's identity, found it in sync,
	/// and stopped.
	pub fn swallow_events(&self, path: &RootPath) {
		self.state().swallowed.push(path.clone());
	}

	/// Lets every watcher event through again.
	pub fn stop_swallowing(&self) {
		self.state().swallowed.clear();
	}

	/// Makes every read of where an open item is now fail with `error`, while set.
	pub fn fail_handle_paths(&self, error: Option<LocalError>) {
		self.state().fail_handle_paths = error;
	}

	/// Makes the watcher lose its events: the next take reports a rescan instead.
	pub fn lose_watch_events(&self) {
		self.state().lost_events = true;
	}

	// ---- inspection ----

	/// The item at `path`, if any.
	pub fn entry(&self, path: &RootPath) -> Option<FakeEntry> {
		self.state().get(path).map(Node::view)
	}

	/// Every item, the root excluded, in path order.
	pub fn entries(&self) -> Vec<FakeEntry> {
		self.state()
			.nodes
			.values()
			.filter(|node| !node.path.is_root())
			.map(Node::view)
			.collect()
	}

	/// What the provider did, in order.
	pub fn ops(&self) -> Vec<LocalOp> {
		self.state().ops.clone()
	}

	/// Makes every upload path a real file under `dir`, holding the file's bytes as they are when
	/// the path is handed out, so the real cache can upload from it.
	pub fn mirror_uploads(&self, dir: PathBuf) {
		self.state().mirror = Some(dir);
	}

	/// Makes the drive's next copy of the file `path` read `torn`, as a copy taken in the middle of
	/// the user's write does, the write then ending with `final_bytes`.
	pub fn tear_next_copy(&self, path: &RootPath, torn: &[u8], final_bytes: &[u8]) {
		self.state().torn_copy = Some((path.case_key(), torn.to_vec(), final_bytes.to_vec()));
	}

	/// Writes `bytes` as a new ordinary file at an upload path the fake handed out, as the drive
	/// copies an edit out of its cache there, the watcher seeing it as any new file; `false` when
	/// something is there already.
	pub fn write_new_upload(&self, upload_path: &Path, bytes: &[u8]) -> bool {
		let Some(path) = upload_path
			.to_str()
			.and_then(|text| text.strip_prefix(UPLOAD_PREFIX))
			.and_then(|text| {
				RootPath::parse(text.split('\\').filter(|part| !part.is_empty())).ok()
			})
		else {
			return false;
		};
		if self.state().get(&path).is_some() {
			return false;
		}
		self.user_create(&path, bytes);
		true
	}

	/// The bytes at an upload path the fake handed out, as the drive reads them; `None` when the
	/// file is gone.
	pub fn read_upload(&self, upload_path: &Path) -> Option<Vec<u8>> {
		let text = upload_path.to_str()?.strip_prefix(UPLOAD_PREFIX)?;
		let path = RootPath::parse(text.split('\\').filter(|part| !part.is_empty())).ok()?;
		let torn = {
			let mut state = self.state();
			match &state.torn_copy {
				Some((key, ..)) if *key == path.case_key() => state.torn_copy.take(),
				_ => None,
			}
		};
		if let Some((_, torn, final_bytes)) = torn {
			self.write(&path, &final_bytes, false).ok()?;
			return Some(torn);
		}
		match self.state().get(&path)?.kind.clone() {
			NodeKind::File { bytes, .. } => Some(bytes),
			NodeKind::Dir { .. } => None,
		}
	}

	/// Creates the placeholder `spec` in `parent`, dehydrated and in sync; `record` makes it a
	/// provider operation the test sees in [`FakeLocal::ops`].
	fn place_spec(
		&self,
		parent: &RootPath,
		spec: &PlaceholderSpec,
		record: bool,
	) -> Result<(), LocalError> {
		let mut state = self.state();
		if let Some(error) = state.fail_placements.filter(|_| record) {
			return Err(error);
		}
		if let Some(error) = state
			.fail_placements_named
			.get(spec.name().as_str())
			.filter(|_| record)
		{
			return Err(*error);
		}
		let path = parent.join(spec.name().clone());
		if state.get(&path).is_some() {
			return Err(LocalError::Exists);
		}
		let now = state.tick();
		let is_file = matches!(spec.kind(), SpecKind::File { .. });
		let kind = match spec.kind() {
			SpecKind::File { size } => NodeKind::File {
				bytes: vec![0; usize::try_from(size.get()).expect("a fake file fits memory")],
				created: now,
				modified: now,
				changed: now,
			},
			SpecKind::Dir => NodeKind::Dir { created: now },
		};
		state.insert(
			path.clone(),
			kind,
			Some(FakePlaceholder::new(spec.identity(), 0, is_file)),
		);
		if record {
			state.ops.push(LocalOp::PlaceholderCreated {
				path,
				identity: spec.identity(),
			});
		}
		Ok(())
	}

	/// Where the item `id` an open handle holds is now: `None` once it is gone, or the error
	/// [`FakeLocal::fail_handle_paths`] set.
	fn handle_path(&self, id: u128) -> Result<Option<RootPath>, LocalError> {
		let state = self.state();
		if let Some(error) = state.fail_handle_paths {
			return Err(error);
		}
		Ok(state.by_id(id).map(|node| node.path.clone()))
	}

	fn settled_node<T>(
		&self,
		file: &FakeSettled,
		read: impl FnOnce(&mut Node) -> Result<T, LocalError>,
	) -> Result<T, LocalError> {
		let mut state = self.state();
		let node = state.by_id_mut(file.id).ok_or(LocalError::NotFound)?;
		read(node)
	}
}

/// A write handle a test holds open.
#[derive(Debug)]
pub struct FakeWriter {
	local: FakeLocal,
	id: u128,
	path: RootPath,
}

impl Drop for FakeWriter {
	/// Raises a watcher modification and, for a placeholder, the platform's close notification.
	fn drop(&mut self) {
		let mut state = self.local.state();
		if let Some(open) = state.writers.get_mut(&self.id) {
			*open -= 1;
		}
		let placeholder = state
			.by_id(self.id)
			.is_some_and(|node| node.placeholder.is_some());
		state.watch.push(LocalEvent::Modified(self.path.clone()));
		if placeholder {
			state.watch.push(LocalEvent::Closed(self.path.clone()));
		}
	}
}

/// A read handle a test holds open.
#[derive(Debug)]
pub struct FakeReader {
	local: FakeLocal,
	id: u128,
	/// Whether it refuses deletes, and so is counted.
	counted: bool,
}

impl Drop for FakeReader {
	fn drop(&mut self) {
		let mut state = self.local.state();
		if let Some(open) = state.all_readers.get_mut(&self.id) {
			*open -= 1;
		}
		if !self.counted {
			return;
		}
		if let Some(open) = state.readers.get_mut(&self.id) {
			*open -= 1;
		}
	}
}

/// A file the provider holds settled.
#[derive(Debug)]
pub struct FakeSettled {
	local: FakeLocal,
	id: u128,
}

impl Drop for FakeSettled {
	fn drop(&mut self) {
		if let Some(open) = self.local.state().settled.get_mut(&self.id) {
			*open -= 1;
		}
	}
}

/// A directory the provider holds open to convert it: it follows the directory through a rename.
#[derive(Debug)]
pub struct FakeDir {
	id: u128,
}

/// The answer to one fake `NOTIFY_DELETE`.
#[derive(Debug)]
pub struct FakeDelete {
	/// `None` once answered.
	local: Option<FakeLocal>,
	path: RootPath,
	/// Whether allowing it deletes the item.
	removes: bool,
}

impl sealed::Sealed for FakeDelete {}

impl DeleteResponder for FakeDelete {
	fn allow(mut self) {
		let local = self.local.take().expect("a responder is answered once");
		let mut state = local.state();
		state.ops.push(LocalOp::DeleteAnswered {
			path: self.path.clone(),
			answer: Ok(()),
		});
		if self.removes && !state.fail_completion.remove(&self.path.case_key()) {
			state.remove_tree(&self.path);
			state.watch.push(LocalEvent::Removed(self.path.clone()));
		}
	}

	fn veto(mut self, failure: Failure) {
		let local = self.local.take().expect("a responder is answered once");
		local.state().ops.push(LocalOp::DeleteAnswered {
			path: self.path.clone(),
			answer: Err(failure),
		});
	}
}

impl Drop for FakeDelete {
	fn drop(&mut self) {
		if let Some(local) = self.local.take() {
			local
				.state()
				.ops
				.push(LocalOp::DeleteAbandoned(self.path.clone()));
		}
	}
}

/// The answer to one fake `NOTIFY_DEHYDRATE`.
#[derive(Debug)]
pub struct FakeDehydrate {
	/// `None` once answered.
	local: Option<FakeLocal>,
	path: RootPath,
}

impl FakeDehydrate {
	fn answer(&mut self, allowed: bool) {
		let local = self.local.take().expect("a responder is answered once");
		local.state().ops.push(LocalOp::DehydrateAnswered {
			path: self.path.clone(),
			allowed,
		});
	}
}

impl sealed::Sealed for FakeDehydrate {}

impl DehydrateResponder for FakeDehydrate {
	fn allow(mut self) {
		self.answer(true);
	}

	fn veto(mut self) {
		self.answer(false);
	}
}

impl Drop for FakeDehydrate {
	fn drop(&mut self) {
		if let Some(local) = self.local.take() {
			local
				.state()
				.ops
				.push(LocalOp::DehydrateAbandoned(self.path.clone()));
		}
	}
}

/// The answer to one fake `NOTIFY_RENAME`.
#[derive(Debug)]
pub struct FakeRename {
	/// `None` once answered.
	local: Option<FakeLocal>,
	from: RootPath,
	/// `None` for a move out of the root.
	to: Option<RootPath>,
}

impl sealed::Sealed for FakeRename {}

impl RenameResponder for FakeRename {
	fn allow(mut self) {
		let local = self.local.take().expect("a responder is answered once");
		let mut state = local.state();
		state.ops.push(LocalOp::RenameAnswered {
			from: self.from.clone(),
			answer: Ok(()),
		});
		if state.fail_completion.remove(&self.from.case_key()) {
			return;
		}
		match &self.to {
			Some(to) => {
				state.move_tree(&self.from, to);
				// A rename inside the root was measured to clear the placeholder's in-sync state.
				if let Some(placeholder) =
					state.get_mut(to).and_then(|node| node.placeholder.as_mut())
				{
					placeholder.in_sync = false;
				}
				state.watch.push(LocalEvent::Renamed {
					from: self.from.clone(),
					to: to.clone(),
				});
			}
			None => {
				state.remove_tree(&self.from);
				state.watch.push(LocalEvent::Removed(self.from.clone()));
			}
		}
	}

	fn veto(mut self, failure: Failure) {
		let local = self.local.take().expect("a responder is answered once");
		local.state().ops.push(LocalOp::RenameAnswered {
			from: self.from.clone(),
			answer: Err(failure),
		});
	}
}

impl Drop for FakeRename {
	fn drop(&mut self) {
		if let Some(local) = self.local.take() {
			local
				.state()
				.ops
				.push(LocalOp::RenameAbandoned(self.from.clone()));
		}
	}
}

impl sealed::Sealed for FakeLocal {}

impl LocalFs for FakeLocal {
	fn revert_to_plain(&self, path: &RootPath, expect: LocalFileId) -> Result<(), LocalError> {
		let mut state = self.state();
		if let Some(error) = state.fail_revert.get(&path.case_key()) {
			return Err(*error);
		}
		let node = state
			.get_mut(path)
			.filter(|node| node.id == expect.get())
			.ok_or(LocalError::NotFound)?;
		node.placeholder = None;
		state.ops.push(LocalOp::Reverted(path.clone()));
		if let Some(left) = state.kill_after_reverts.as_mut() {
			*left = left.saturating_sub(1);
			if *left == 0 {
				state.kill_after_reverts = None;
				drop(state);
				panic!("the fake root killed the helper after a revert");
			}
		}
		Ok(())
	}

	fn copy_to_plain(
		&self,
		path: &RootPath,
		expect: LocalFileId,
		to: &RootPath,
	) -> Result<(), LocalError> {
		let mut state = self.state();
		if let Some(error) = state.fail_copy.get(&path.case_key()) {
			return Err(*error);
		}
		let node = state
			.get(path)
			.filter(|node| node.id == expect.get())
			.ok_or(LocalError::NotFound)?;
		let NodeKind::File { bytes, .. } = &node.kind else {
			return Err(LocalError::NotFound);
		};
		if node
			.placeholder
			.as_ref()
			.is_some_and(|placeholder| placeholder.on_disk < node.size())
		{
			return Err(LocalError::NotFound);
		}
		if state.get(to).is_some() {
			return Err(LocalError::Exists);
		}
		let bytes = bytes.clone();
		let now = state.tick();
		state.insert(
			to.clone(),
			NodeKind::File {
				bytes,
				created: now,
				modified: now,
				changed: now,
			},
			None,
		);
		state.ops.push(LocalOp::CopiedToPlain {
			from: path.clone(),
			to: to.clone(),
		});
		Ok(())
	}

	fn open_on_disk(
		&self,
		path: &RootPath,
		expect: LocalFileId,
	) -> Result<Box<dyn ByteSource>, LocalError> {
		let state = self.state();
		let node = state
			.get(path)
			.filter(|node| node.id == expect.get())
			.ok_or(LocalError::NotFound)?;
		let NodeKind::File { bytes, .. } = &node.kind else {
			return Err(LocalError::NotFound);
		};
		if node
			.placeholder
			.as_ref()
			.is_some_and(|placeholder| placeholder.on_disk < node.size())
		{
			return Err(LocalError::NotFound);
		}
		Ok(Box::new(MemSource(bytes.clone())))
	}

	type Settled = FakeSettled;

	type Dir = FakeDir;

	fn inspect(&self, path: &RootPath) -> Result<LocalEntry, LocalError> {
		let mut state = self.state();
		*state.inspected.entry(path.case_key()).or_default() += 1;
		if let Some(error) = state.fail_inspect.get(&path.case_key()) {
			return Err(*error);
		}
		let Some(node) = state.get(path) else {
			return Ok(LocalEntry::Missing);
		};
		if let Some(kind) = node.unsupported {
			return Ok(LocalEntry::Unsupported(kind));
		}
		let id = LocalFileId::new(node.id);
		Ok(match &node.placeholder {
			None => LocalEntry::Plain {
				kind: node.local_kind(),
				id,
			},
			Some(placeholder) => LocalEntry::Placeholder {
				kind: node.local_kind(),
				id,
				identity: PlaceholderIdentity::decode(&placeholder.identity),
				state: node.placeholder_state(placeholder),
			},
		})
	}

	fn list_dir(
		&self,
		path: &RootPath,
	) -> Result<Vec<Result<RootPath, RootPathError>>, LocalError> {
		let state = self.state();
		if let Some(error) = state.fail_list.get(&path.case_key()) {
			return Err(*error);
		}
		state.get(path).ok_or(LocalError::NotFound)?;
		let mut listed: Vec<Result<RootPath, RootPathError>> = state
			.nodes
			.values()
			.filter(|node| node.path.parent().as_ref() == Some(path))
			.map(|node| Ok(node.path.clone()))
			.collect();
		if state.unrepresentable.contains(&path.case_key()) {
			listed.push(Err(RootPathError::NotUnicode));
		}
		Ok(listed)
	}

	fn open_settled(&self, path: &RootPath) -> Result<FakeSettled, LocalError> {
		let mut state = self.state();
		let node = state.get(path).ok_or(LocalError::NotFound)?;
		if matches!(node.kind, NodeKind::Dir { .. }) {
			return Err(LocalError::NotFound);
		}
		let id = node.id;
		if state.writers.get(&id).is_some_and(|open| *open > 0) {
			return Err(LocalError::Busy);
		}
		// As measured on Windows: the provider's own open of a placeholder for reading that refuses
		// writers fetches all of its data the file does not hold before it returns.
		let source = state.source.clone();
		if let Some(node) = state.by_id_mut(id)
			&& let size = node.size()
			&& let Some(placeholder) = node.placeholder.as_mut()
			&& placeholder.on_disk < size
		{
			placeholder.on_disk = size;
			let stable = held_stable(placeholder);
			fill_from(node, stable, source);
			let path = node.path.clone();
			state.ops.push(LocalOp::Hydrated(path));
		}
		*state.settled.entry(id).or_default() += 1;
		Ok(FakeSettled {
			local: self.clone(),
			id,
		})
	}

	fn settled_stamp(&self, file: &FakeSettled) -> Result<(LocalFileId, FileStamp), LocalError> {
		self.settled_node(file, |node| match node.local_kind() {
			LocalKind::File(stamp) => Ok((LocalFileId::new(node.id), stamp)),
			LocalKind::Dir { .. } => Err(LocalError::NotFound),
		})
	}

	fn hash(&self, file: &mut FakeSettled) -> Result<[u8; 32], LocalError> {
		let mut state = self.state();
		let node = state.by_id(file.id).ok_or(LocalError::NotFound)?;
		let partial = node
			.placeholder
			.as_ref()
			.is_some_and(|placeholder| placeholder.on_disk < node.size());
		let path = node.path.clone();
		let NodeKind::File { bytes, .. } = &node.kind else {
			return Err(LocalError::NotFound);
		};
		let hash = *blake3::hash(bytes).as_bytes();
		if partial {
			state.ops.push(LocalOp::HashedPartial(path.clone()));
		}
		let replace = state
			.replace_during_settled_hash
			.take_if(|(from, _)| from.case_key() == path.case_key());
		drop(state);
		if let Some((from, to)) = replace {
			self.user_replace(&from, &to);
		}
		Ok(hash)
	}

	fn upload_path(&self, path: &RootPath) -> PathBuf {
		let state = self.state();
		let Some(mirror) = &state.mirror else {
			return PathBuf::from(format!("{UPLOAD_PREFIX}{path}"));
		};
		let real = path
			.components()
			.iter()
			.fold(mirror.clone(), |at, name| at.join(name.as_str()));
		if let Some(NodeKind::File { bytes, .. }) = state.get(path).map(|node| &node.kind) {
			if let Some(parent) = real.parent() {
				std::fs::create_dir_all(parent).expect("the mirror directory is writable");
			}
			std::fs::write(&real, bytes).expect("the mirror directory is writable");
		}
		real
	}

	fn convert_file(
		&self,
		file: &FakeSettled,
		identity: PlaceholderIdentity,
		in_sync: bool,
	) -> Result<(), LocalError> {
		let mut state = self.state();
		if let Some(error) = state.fail_convert.take() {
			return Err(error);
		}
		let node = state.by_id_mut(file.id).ok_or(LocalError::NotFound)?;
		if node.placeholder.is_some() {
			// cfapi refuses to convert a placeholder (0x8007017C).
			return Err(LocalError::Os(crate::os::OsError::Rejected { code: -1 }));
		}
		let size = node.size();
		node.placeholder = Some(FakePlaceholder {
			identity: identity.encode(),
			in_sync,
			on_disk: size,
			modified_bytes: if in_sync { 0 } else { size },
			populated: true,
			pin: PinAttr::Unspecified,
		});
		let path = node.path.clone();
		state.ops.push(LocalOp::Converted {
			path: path.clone(),
			identity,
			in_sync,
		});
		state.watch.push(LocalEvent::Modified(path));
		Ok(())
	}

	/// Fails as [`FakeLocal::fail_revert`] makes a revert of the path the file is at fail.
	fn revert_settled(&self, file: &FakeSettled) -> Result<(), LocalError> {
		let mut state = self.state();
		state.revert_refused(file.id)?;
		let node = state.by_id_mut(file.id).ok_or(LocalError::NotFound)?;
		if node
			.placeholder
			.as_ref()
			.is_none_or(|placeholder| placeholder.on_disk < node.size())
		{
			// cfapi refuses an ordinary file, and a file not all on disk.
			return Err(LocalError::Os(crate::os::OsError::Rejected { code: -1 }));
		}
		node.placeholder = None;
		let path = node.path.clone();
		state.ops.push(LocalOp::Reverted(path));
		Ok(())
	}

	/// The file's path by its id, which follows it through a rename; `None` once it is gone.
	fn settled_path(&self, file: &FakeSettled) -> Result<Option<RootPath>, LocalError> {
		self.handle_path(file.id)
	}

	fn update_file(
		&self,
		file: &FakeSettled,
		identity: PlaceholderIdentity,
		in_sync: bool,
	) -> Result<(), LocalError> {
		let mut state = self.state();
		if let Some(error) = state.fail_update.take().or(state.fail_file_updates) {
			return Err(error);
		}
		let lose = in_sync && state.lose_in_sync_marks;
		let node = state.by_id_mut(file.id).ok_or(LocalError::NotFound)?;
		let placeholder = node.placeholder.as_mut().ok_or(LocalError::NotFound)?;
		placeholder.identity = identity.encode();
		placeholder.in_sync = in_sync && !lose;
		if in_sync && !lose {
			placeholder.modified_bytes = 0;
		}
		let path = node.path.clone();
		state.ops.push(LocalOp::Updated {
			path: path.clone(),
			identity,
			in_sync,
		});
		if lose {
			state.watch.push(LocalEvent::Modified(path));
		}
		Ok(())
	}

	fn mark_partial_in_sync(
		&self,
		path: &RootPath,
		expect: LocalFileId,
		identity: PlaceholderIdentity,
	) -> Result<(), LocalError> {
		let mut state = self.state();
		if let Some(error) = state.fail_update.take().or(state.fail_file_updates) {
			return Err(error);
		}
		let lose = state.lose_in_sync_marks;
		let node = state
			.get_mut(path)
			.filter(|node| node.id == expect.get() && matches!(node.kind, NodeKind::File { .. }))
			.ok_or(LocalError::NotFound)?;
		let size = node.size();
		let placeholder = node.placeholder.as_mut().ok_or(LocalError::NotFound)?;
		if placeholder.identity != identity.encode() {
			return Err(LocalError::NotFound);
		}
		if placeholder.modified_bytes > 0 || placeholder.on_disk >= size {
			return Err(LocalError::Modified);
		}
		placeholder.in_sync = !lose;
		state.ops.push(LocalOp::Updated {
			path: path.clone(),
			identity,
			in_sync: true,
		});
		if lose {
			state.watch.push(LocalEvent::Modified(path.clone()));
		}
		Ok(())
	}

	fn retag_unsynced(
		&self,
		path: &RootPath,
		expect: LocalFileId,
		identity: PlaceholderIdentity,
	) -> Result<(), LocalError> {
		let mut state = self.state();
		if let Some(error) = state.fail_update.take() {
			return Err(error);
		}
		let node = state
			.get_mut(path)
			.filter(|node| node.id == expect.get())
			.ok_or(LocalError::NotFound)?;
		let placeholder = node.placeholder.as_mut().ok_or(LocalError::NotFound)?;
		placeholder.identity = identity.encode();
		placeholder.in_sync = false;
		state.ops.push(LocalOp::Retagged {
			path: path.clone(),
			identity,
		});
		Ok(())
	}

	/// What the user does once the provider opened the directory to convert it
	/// ([`FakeLocal::rename_during_dir_convert`]) runs as this returns.
	fn open_dir(&self, path: &RootPath, expect: LocalFileId) -> Result<FakeDir, LocalError> {
		self.state()
			.get(path)
			.filter(|node| node.id == expect.get() && matches!(node.kind, NodeKind::Dir { .. }))
			.ok_or(LocalError::NotFound)?;
		self.run_during_dir_convert(path);
		Ok(FakeDir { id: expect.get() })
	}

	fn convert_dir(
		&self,
		dir: &FakeDir,
		identity: PlaceholderIdentity,
		populated: bool,
	) -> Result<(), LocalError> {
		let mut state = self.state();
		if let Some(error) = state.fail_convert.take() {
			return Err(error);
		}
		let node = state.by_id_mut(dir.id).ok_or(LocalError::NotFound)?;
		node.placeholder = Some(FakePlaceholder::new(identity, 0, populated));
		let path = node.path.clone();
		state.ops.push(LocalOp::ConvertedDir {
			path: path.clone(),
			identity,
			populated,
		});
		state.watch.push(LocalEvent::Modified(path));
		Ok(())
	}

	/// Fails as [`FakeLocal::fail_revert`] makes a revert of the path the directory is at fail.
	fn revert_dir(&self, dir: &FakeDir) -> Result<(), LocalError> {
		let mut state = self.state();
		state.revert_refused(dir.id)?;
		let node = state.by_id_mut(dir.id).ok_or(LocalError::NotFound)?;
		if node.placeholder.take().is_none() {
			// cfapi refuses an ordinary directory.
			return Err(LocalError::Os(crate::os::OsError::Rejected { code: -1 }));
		}
		let path = node.path.clone();
		state.ops.push(LocalOp::Reverted(path));
		Ok(())
	}

	/// The directory's path by its id, as for a file.
	fn dir_path(&self, dir: &FakeDir) -> Result<Option<RootPath>, LocalError> {
		self.handle_path(dir.id)
	}

	fn update_open_dir(
		&self,
		dir: &FakeDir,
		identity: PlaceholderIdentity,
	) -> Result<(), LocalError> {
		let mut state = self.state();
		if let Some(error) = state.fail_update.take() {
			return Err(error);
		}
		let node = state.by_id_mut(dir.id).ok_or(LocalError::NotFound)?;
		let placeholder = node.placeholder.as_mut().ok_or(LocalError::NotFound)?;
		placeholder.identity = identity.encode();
		placeholder.in_sync = true;
		let path = node.path.clone();
		state.ops.push(LocalOp::DirUpdated {
			path: path.clone(),
			identity,
		});
		state.watch.push(LocalEvent::Modified(path));
		Ok(())
	}

	fn set_dir_in_sync(&self, path: &RootPath, expect: LocalFileId) -> Result<(), LocalError> {
		let mut state = self.state();
		let node = state
			.get_mut(path)
			.filter(|node| node.id == expect.get())
			.ok_or(LocalError::NotFound)?;
		node.placeholder
			.as_mut()
			.ok_or(LocalError::NotFound)?
			.in_sync = true;
		state.ops.push(LocalOp::DirInSync(path.clone()));
		Ok(())
	}

	/// As measured for a directory's metadata update: one watcher modification of it.
	fn set_dir_populated(
		&self,
		path: &RootPath,
		expect: LocalFileId,
		identity: PlaceholderIdentity,
	) -> Result<(), LocalError> {
		let mut state = self.state();
		let node = state
			.get_mut(path)
			.filter(|node| node.id == expect.get() && matches!(node.kind, NodeKind::Dir { .. }))
			.ok_or(LocalError::NotFound)?;
		let placeholder = node.placeholder.as_mut().ok_or(LocalError::NotFound)?;
		if PlaceholderIdentity::decode(&placeholder.identity).ok() != Some(identity) {
			return Err(LocalError::NotFound);
		}
		placeholder.populated = true;
		state.ops.push(LocalOp::DirPopulated(path.clone()));
		state.watch.push(LocalEvent::Modified(path.clone()));
		Ok(())
	}

	fn hydrate(&self, path: &RootPath, expect: LocalFileId) -> Result<(), LocalError> {
		let mut state = self.state();
		if let Some(error) = state.fail_hydrate.take().or(state.fail_hydrations) {
			return Err(error);
		}
		let stall = state.stall_hydration;
		let source = state.source.clone();
		let node = state
			.get_mut(path)
			.filter(|node| node.id == expect.get())
			.ok_or(LocalError::NotFound)?;
		let size = node.size();
		let placeholder = node.placeholder.as_mut().ok_or(LocalError::NotFound)?;
		let stable = held_stable(placeholder);
		if !stall {
			placeholder.on_disk = size;
			fill_from(node, stable, source);
		}
		state.ops.push(LocalOp::Hydrated(path.clone()));
		state.watch.push(LocalEvent::Modified(path.clone()));
		Ok(())
	}

	fn hash_checked(
		&self,
		path: &RootPath,
		expect: LocalFileId,
		stop: &AtomicBool,
	) -> Result<CheckedHash, LocalError> {
		let (before, during) = {
			let mut state = self.state();
			let node = state.get(path).ok_or(LocalError::NotFound)?;
			let before = node.snapshot()?;
			if before.id != expect {
				return Err(LocalError::NotFound);
			}
			if before.state.on_disk() < before.stamp.size {
				return Err(LocalError::NotFound);
			}
			if stop.load(Ordering::Relaxed) {
				return Err(LocalError::Busy);
			}
			let during = state
				.during_hash
				.take_if(|(key, _)| *key == path.case_key())
				.map(|(_, during)| during);
			(before, during)
		};
		match during {
			Some(DuringHash::Write(bytes)) => self.write(path, &bytes, false)?,
			Some(DuringHash::Pin(pin)) => self.user_pin(path, pin),
			None => {}
		}
		let mut state = self.state();
		let node = state.get(path).ok_or(LocalError::NotFound)?;
		let NodeKind::File { bytes, .. } = &node.kind else {
			return Err(LocalError::NotFound);
		};
		let hash = *blake3::hash(bytes).as_bytes();
		let after = node.snapshot()?;
		state.ops.push(LocalOp::HashChecked(path.clone()));
		Ok(CheckedHash {
			before,
			hash,
			after,
		})
	}

	/// As measured for `CfSetPinState`: no callback, one watcher modification, its data and in-sync
	/// state left alone.
	fn swap_pin(
		&self,
		path: &RootPath,
		expect: LocalFileId,
		from: PinAttr,
		to: PinAttr,
	) -> Result<bool, LocalError> {
		let mut state = self.state();
		if let Some(error) = state.fail_pin_change.get(&path.case_key()) {
			return Err(*error);
		}
		let node = state
			.get_mut(path)
			.filter(|node| node.id == expect.get())
			.ok_or(LocalError::NotFound)?;
		let placeholder = node.placeholder.as_mut().ok_or(LocalError::NotFound)?;
		if placeholder.pin != from {
			return Ok(false);
		}
		placeholder.pin = to;
		state.ops.push(LocalOp::PinSet {
			path: path.clone(),
			pin: to,
		});
		state.watch.push(LocalEvent::Modified(path.clone()));
		if let Some(during) = state
			.pin_during
			.take_if(|during| during.act == ProviderAct::PinChange && during.on == path.case_key())
		{
			let node = state.get_mut(&during.target).expect("the target exists");
			node.placeholder.as_mut().expect("a placeholder").pin = during.pin;
			state.watch.push(LocalEvent::Modified(during.target));
		}
		Ok(true)
	}

	/// As measured for `CfDehydratePlaceholder` through a checked handle: refused while any other
	/// handle holds the file, when it is not clean, and when it is pinned unless `if_pinned`
	/// overrides the pin, which it keeps. Raises a watcher modification. It fetches nothing of a
	/// file partly on disk: the Windows layer's handle reads no data.
	fn dehydrate_clean(
		&self,
		path: &RootPath,
		expect: PlaceholderIdentity,
		if_pinned: IfPinned,
	) -> Result<(), LocalError> {
		let mut state = self.state();
		if let Some(during) = state
			.pin_during
			.take_if(|during| during.act == ProviderAct::Dehydrate && during.on == path.case_key())
		{
			let node = state.get_mut(&during.target).expect("the target exists");
			node.placeholder.as_mut().expect("a placeholder").pin = during.pin;
			state.watch.push(LocalEvent::Modified(during.target));
		}
		if let Some(error) = state.fail_dehydrate.take() {
			return Err(error);
		}
		if state.busy_dehydrations > 0 {
			state.busy_dehydrations -= 1;
			return Err(LocalError::Busy);
		}
		let node = state.clean(path, expect)?;
		let id = node.id;
		if if_pinned == IfPinned::Refuse
			&& node
				.placeholder
				.as_ref()
				.is_some_and(|placeholder| placeholder.pin == PinAttr::Pinned)
		{
			return Err(LocalError::Pinned);
		}
		let held = |open: &HashMap<u128, u32>| open.get(&id).is_some_and(|open| *open > 0);
		if held(&state.writers) || held(&state.all_readers) || held(&state.settled) {
			return Err(LocalError::Busy);
		}
		let node = state.get_mut(path).expect("found above");
		node.placeholder.as_mut().expect("found above").on_disk = 0;
		state.ops.push(LocalOp::Dehydrated(path.clone()));
		state.watch.push(LocalEvent::Modified(path.clone()));
		Ok(())
	}

	/// Creates the placeholder `spec` in `parent`, a directory one not populated; measured to raise
	/// no watcher event of its own.
	fn create_placeholder(
		&self,
		parent: &RootPath,
		spec: &PlaceholderSpec,
	) -> Result<(), LocalError> {
		self.place_spec(parent, spec, true)
	}

	/// As measured: the update dehydrates the file, refused while a writer holds it or a reader that
	/// does not share deleting (the Windows layer's handle takes part in share checks as a delete
	/// does), or while any other handle holds it with bytes on disk (the Windows layer dehydrates
	/// those first through `CfDehydratePlaceholder`, which refuses them), and raises a watcher
	/// modification. It fetches nothing: the Windows layer's handle reads no data.
	fn update_from_remote(
		&self,
		path: &RootPath,
		expect: PlaceholderIdentity,
		to: &PlaceholderSpec,
	) -> Result<Updated, LocalError> {
		let mut state = self.state();
		if let Some(error) = state.fail_update.take() {
			return Err(error);
		}
		let SpecKind::File { size } = to.kind() else {
			return Err(LocalError::Os(crate::os::OsError::Rejected { code: -1 }));
		};
		let now = state.tick();
		let node = state.clean(path, expect)?;
		let id = node.id;
		let on_disk = node
			.placeholder
			.as_ref()
			.map_or(0, |placeholder| placeholder.on_disk);
		let held = |open: &HashMap<u128, u32>| open.get(&id).is_some_and(|open| *open > 0);
		if held(&state.writers)
			|| held(&state.readers)
			|| on_disk > 0 && (held(&state.all_readers) || held(&state.settled))
		{
			return Err(LocalError::Busy);
		}
		let node = state.get_mut(path).expect("found above");
		let NodeKind::File {
			bytes,
			modified,
			changed,
			..
		} = &mut node.kind
		else {
			return Err(LocalError::NotFound);
		};
		*bytes = vec![0; usize::try_from(size.get()).expect("a fake file fits memory")];
		*modified = now;
		*changed = now;
		let placeholder = node.placeholder.as_mut().expect("found above");
		placeholder.identity = to.identity().encode();
		placeholder.on_disk = 0;
		let was_pinned = placeholder.pin == PinAttr::Pinned;
		state.ops.push(LocalOp::RemoteUpdated {
			path: path.clone(),
			identity: to.identity(),
		});
		state.watch.push(LocalEvent::Modified(path.clone()));
		Ok(Updated { was_pinned })
	}

	fn remove_clean(&self, path: &RootPath, expect: PlaceholderIdentity) -> Result<(), LocalError> {
		let mut state = self.state();
		let is_dir = matches!(state.clean(path, expect)?.kind, NodeKind::Dir { .. });
		if is_dir
			&& state
				.nodes
				.values()
				.any(|node| node.path.parent().as_ref() == Some(path))
		{
			return Err(LocalError::Exists);
		}
		if state.refuses_delete(path) {
			return Err(LocalError::Busy);
		}
		state.remove_tree(path);
		state.ops.push(LocalOp::Removed(path.clone()));
		state.watch.push(LocalEvent::Removed(path.clone()));
		Ok(())
	}

	fn rename_clean(
		&self,
		from: &RootPath,
		to: &RootPath,
		expect: PlaceholderIdentity,
	) -> Result<(), LocalError> {
		let mut state = self.state();
		if let Some(error) = state.fail_rename.take() {
			return Err(error);
		}
		state.clean(from, expect)?;
		if state.taken(from, to) {
			return Err(LocalError::Exists);
		}
		let Some(parent) = to.parent() else {
			return Err(LocalError::NotFound);
		};
		if !state
			.get(&parent)
			.is_some_and(|node| matches!(node.kind, NodeKind::Dir { .. }))
		{
			return Err(LocalError::NotFound);
		}
		if state.refuses_delete(from) {
			return Err(LocalError::Busy);
		}
		state.move_tree(from, to);
		state.ops.push(LocalOp::RenamedClean {
			from: from.clone(),
			to: to.clone(),
		});
		state.watch.push(LocalEvent::Renamed {
			from: from.clone(),
			to: to.clone(),
		});
		Ok(())
	}

	fn update_dir(
		&self,
		path: &RootPath,
		expect: LocalFileId,
		identity: PlaceholderIdentity,
	) -> Result<(), LocalError> {
		let mut state = self.state();
		if let Some(error) = state.fail_update.take() {
			return Err(error);
		}
		let node = state
			.get_mut(path)
			.filter(|node| node.id == expect.get() && matches!(node.kind, NodeKind::Dir { .. }))
			.ok_or(LocalError::NotFound)?;
		let placeholder = node.placeholder.as_mut().ok_or(LocalError::NotFound)?;
		placeholder.identity = identity.encode();
		placeholder.in_sync = true;
		state.ops.push(LocalOp::DirUpdated {
			path: path.clone(),
			identity,
		});
		state.watch.push(LocalEvent::Modified(path.clone()));
		Ok(())
	}

	fn rename_no_replace(&self, from: &RootPath, to: &RootPath) -> Result<(), LocalError> {
		let mut state = self.state();
		if let Some(error) = state.fail_rename.take() {
			return Err(error);
		}
		if state.get(from).is_none() {
			return Err(LocalError::NotFound);
		}
		if state.taken(from, to) {
			return Err(LocalError::Exists);
		}
		state.move_tree(from, to);
		state.ops.push(LocalOp::Renamed {
			from: from.clone(),
			to: to.clone(),
		});
		state.watch.push(LocalEvent::Renamed {
			from: from.clone(),
			to: to.clone(),
		});
		Ok(())
	}
}

/// The path `text`, components separated by `\`: a test helper.
pub fn root_path(text: &str) -> RootPath {
	RootPath::parse(text.split('\\').filter(|part| !part.is_empty())).expect("a valid path")
}

/// The name `text`: a test helper.
pub fn name(text: &str) -> PlaceholderName {
	PlaceholderName::parse(text).expect("a valid name")
}
