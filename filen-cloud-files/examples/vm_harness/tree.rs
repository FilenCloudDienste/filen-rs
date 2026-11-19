//! The deterministic tree the provider serves from the fake drive, and what the readers expect of
//! it.

use std::{collections::HashMap, time::Duration};

use filen_cloud_files::test_support::{FakeDrive, ffi_dir, ffi_file, pattern};
use filen_mobile_native_cache::ffi::FfiNonRootObject;
use filen_types::fs::Uuid;

/// The account root the sync root is registered for.
pub(crate) const ROOT: Uuid = Uuid::from_u128(0xF11E_0000_0000_0000_0000_0000_0000_0001);
/// Every directory of the tree below the root, parents first, with its uuid. `lx` holds what the
/// local-change scenarios change, `rx` what the remote-change scenarios change, `px` what the
/// pin scenarios change and `sx` what the save and restart scenarios change; nothing else in the
/// tree is changed.
pub(crate) const DIRS: &[(&str, u128)] = &[
	("sub", 0x3001),
	("sub\\deeper", 0x3002),
	("emptydir", 0x3003),
	("late", 0x3004),
	("lx", 0x3005),
	("lx\\dst", 0x3006),
	("lx\\dirren", 0x3007),
	("lx\\deldir", 0x3008),
	("rx", 0x3009),
	("rx\\sub", 0x300A),
	// Never listed by any scenario: an on-demand directory the platform never populates.
	("rx\\lazy", 0x300B),
	("rx\\lazy\\deep", 0x300C),
	("rx\\mvdir", 0x300D),
	("rx\\trdir", 0x300E),
	("rx\\trdir\\inner", 0x300F),
	("rx\\cleandir", 0x3010),
	("rx\\attrdir", 0x3011),
	// What the pin scenarios pin and unpin; `lazy` directories are left unlisted until a scenario
	// lists them.
	("px", 0x3012),
	("px\\pa", 0x3013),
	("px\\pa\\sub", 0x3014),
	("px\\pa\\lazy", 0x3015),
	("px\\pr", 0x3016),
	("px\\pr\\sub", 0x3017),
	("px\\pr\\lazy", 0x3018),
	("px\\pu", 0x3019),
	("px\\pu\\sub", 0x301A),
	("sx", 0x301B),
	// Pinned while never listed, nor anything under it: the provider populates it all itself.
	("px\\pn", 0x301C),
	("px\\pn\\deep", 0x301D),
	("px\\pn\\deep\\deeper", 0x301E),
];
/// `emptydir/`, which has no children.
pub(crate) const EMPTYDIR: Uuid = Uuid::from_u128(0x3003);

/// The `Case.txt` / `case.TXT` collision: the lower stable id wins.
const CASE_KEPT: (&str, u128) = ("Case.txt", 0x10);
const CASE_LOST: (&str, u128) = ("case.TXT", 0x11);

/// What the drive does with a file's reads.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Content {
	/// The pattern, declaring its real BLAKE3.
	Plain,
	/// The pattern, declaring a hash it does not have.
	WrongHash,
	/// The pattern, with the server's bytes ending at the offset.
	TruncatedAt(u64),
	/// Never delivered: every read waits until it is cancelled.
	Held,
	/// The pattern, every read delivered after this much real time.
	Delayed(Duration),
}

/// One file of the tree: its path under the root, its size in bytes and its content.
#[derive(Clone, Copy)]
pub(crate) struct TreeFile {
	pub(crate) path: &'static str,
	pub(crate) size: usize,
	pub(crate) content: Content,
}

const fn file(path: &'static str, size: usize, content: Content) -> TreeFile {
	TreeFile {
		path,
		size,
		content,
	}
}

/// Every file of the tree, in the order they get ids; the root's placeable files first.
pub(crate) const FILES: &[TreeFile] = &[
	file("small.txt", 10_000, Content::Plain),
	file("hello.bin", 300_000, Content::Plain),
	file("big.bin", 5_242_880 + 123, Content::Plain),
	file("exact.bin", 2_097_152, Content::Plain),
	file("empty.txt", 0, Content::Plain),
	file("never.bin", 50_000, Content::Plain),
	file("damaged.bin", 20_000, Content::WrongHash),
	file("damaged-big.bin", 3_145_728 + 5, Content::WrongHash),
	file("truncated.bin", 100_000, Content::TruncatedAt(49_152)),
	file("cancel.bin", 8 * 1_048_576, Content::Held),
	file("openclose.bin", 5 * 1_048_576, Content::Plain),
	file("seq.bin", 2 * 1_048_576 + 524_288, Content::Plain),
	file("heldopen.bin", 2 * 1_048_576 + 4096, Content::Plain),
	// Sizes no other file has, as the checks find a file's requests by its size.
	file("damaged-locked.bin", 3_145_728 + 9, Content::WrongHash),
	file(
		"slow.bin",
		2 * 1_048_576 + 8192,
		Content::Delayed(Duration::from_secs(4)),
	),
	file("abandon.bin", 1_048_576, Content::Held),
	file("par0.bin", 3 * 1_048_576, Content::Plain),
	file("par1.bin", 3 * 1_048_576, Content::Plain),
	file("par2.bin", 3 * 1_048_576, Content::Plain),
	file("par3.bin", 3 * 1_048_576, Content::Plain),
	file("par4.bin", 3 * 1_048_576, Content::Plain),
	file("par5.bin", 3 * 1_048_576, Content::Plain),
	file("par6.bin", 3 * 1_048_576, Content::Plain),
	file("par7.bin", 3 * 1_048_576, Content::Plain),
	file("sub\\inner.bin", 10_000, Content::Plain),
	file("sub\\deeper\\deep.bin", 12_288, Content::Plain),
	file("late\\late.bin", 4_096, Content::Plain),
	// The local-change scenarios' files, each of a size no other file has.
	file("lx\\edit.txt", 7_001, Content::Plain),
	file("lx\\save.txt", 7_002, Content::Plain),
	file("lx\\save2.txt", 7_003, Content::Plain),
	file("lx\\office.txt", 7_004, Content::Plain),
	file("lx\\replace.txt", 7_005, Content::Plain),
	file("lx\\del.txt", 7_006, Content::Plain),
	file("lx\\deloff.txt", 7_007, Content::Plain),
	file("lx\\ren.txt", 7_008, Content::Plain),
	file("lx\\renoff.txt", 7_009, Content::Plain),
	file("lx\\mv.txt", 7_010, Content::Plain),
	file("lx\\mvoff.txt", 7_011, Content::Plain),
	file("lx\\mv2.txt", 7_012, Content::Plain),
	file("lx\\out.txt", 7_013, Content::Plain),
	file("lx\\recycle.txt", 7_014, Content::Plain),
	file("lx\\conflict.txt", 7_015, Content::Plain),
	file("lx\\dirren\\x.bin", 7_016, Content::Plain),
	file("lx\\deldir\\a.bin", 7_017, Content::Plain),
	file("lx\\deldir\\b.bin", 7_018, Content::Plain),
	// The remote-change scenarios' files, each of a size no other file has.
	file("rx\\dehyd.txt", 8_001, Content::Plain),
	file("rx\\hyd.txt", 8_002, Content::Plain),
	file("rx\\mod.txt", 8_003, Content::Plain),
	file("rx\\holdr.txt", 8_004, Content::Plain),
	file("rx\\holdw.txt", 8_005, Content::Plain),
	file("rx\\holdww.txt", 8_006, Content::Plain),
	file("rx\\ren.txt", 8_007, Content::Plain),
	file("rx\\case.txt", 8_008, Content::Plain),
	file("rx\\mv.txt", 8_009, Content::Plain),
	file("rx\\mvlazy.txt", 8_010, Content::Plain),
	file("rx\\out.txt", 8_011, Content::Plain),
	file("rx\\trash.txt", 8_012, Content::Plain),
	file("rx\\trashh.txt", 8_013, Content::Plain),
	file("rx\\trashm.txt", 8_014, Content::Plain),
	file("rx\\restore.txt", 8_015, Content::Plain),
	file("rx\\perm.txt", 8_016, Content::Plain),
	file("rx\\both.txt", 8_017, Content::Plain),
	file("rx\\mvdir\\h1.bin", 8_018, Content::Plain),
	file("rx\\mvdir\\h2.bin", 8_019, Content::Plain),
	file("rx\\trdir\\a.bin", 8_020, Content::Plain),
	file("rx\\trdir\\b.bin", 8_021, Content::Plain),
	file("rx\\trdir\\m.bin", 8_022, Content::Plain),
	file("rx\\trdir\\inner\\i.bin", 8_023, Content::Plain),
	file("rx\\cleandir\\c1.bin", 8_024, Content::Plain),
	file("rx\\cleandir\\c2.bin", 8_025, Content::Plain),
	file("rx\\stop1.txt", 8_026, Content::Plain),
	file("rx\\stop2.txt", 8_027, Content::Plain),
	file("rx\\stop3.txt", 8_028, Content::Plain),
	file("rx\\holdd.txt", 8_029, Content::Plain),
	file("rx\\offdel.txt", 8_030, Content::Plain),
	file("rx\\quiet.txt", 8_031, Content::Plain),
	file("rx\\ro.txt", 8_032, Content::Plain),
	file("rx\\attrdir\\f.bin", 8_033, Content::Plain),
	// Read in 4096-byte steps, so no single request covers it: the hydration job finds it damaged.
	file("damaged-seq.bin", 3_145_728 + 17, Content::WrongHash),
	// The pin scenarios' files, each of a size no other file has.
	file("px\\p1.bin", 2_097_152 + 1, Content::Plain),
	file("px\\pa\\a.bin", 9_001, Content::Plain),
	file("px\\pa\\sub\\b.bin", 9_002, Content::Plain),
	file("px\\pa\\lazy\\c.bin", 9_003, Content::Plain),
	file("px\\pr\\a.bin", 9_004, Content::Plain),
	file("px\\pr\\sub\\b.bin", 9_005, Content::Plain),
	file("px\\pr\\lazy\\c.bin", 9_006, Content::Plain),
	file("px\\pin4.bin", 9_007, Content::Plain),
	file("px\\free.bin", 9_008, Content::Plain),
	file("px\\keep.bin", 9_009, Content::Plain),
	file("px\\udh.bin", 9_010, Content::Plain),
	file("px\\udha.bin", 9_011, Content::Plain),
	file("px\\pdam.bin", 3_145_728 + 13, Content::WrongHash),
	file("px\\pu\\a.bin", 9_012, Content::Plain),
	file("px\\pu\\sub\\b.bin", 9_013, Content::Plain),
	file("px\\pu\\kept.bin", 9_014, Content::Plain),
	file("px\\uheld.bin", 9_015, Content::Plain),
	// The save and restart scenarios' files, each of a size no other file has.
	file("sx\\copy.bin", 32 * 1_048_576 + 11, Content::Plain),
	file("sx\\save.txt", 10_001, Content::Plain),
	file("sx\\over.txt", 10_002, Content::Plain),
	file("sx\\repl.txt", 10_003, Content::Plain),
	file("sx\\conf.txt", 10_004, Content::Plain),
	file("sx\\f1.txt", 10_005, Content::Plain),
	file("sx\\pend.txt", 10_006, Content::Plain),
	file("px\\pn\\a.bin", 9_016, Content::Plain),
	file("px\\pn\\deep\\b.bin", 9_017, Content::Plain),
	file("px\\pn\\deep\\deeper\\c.bin", 9_018, Content::Plain),
];

/// Names in the root the core must skip, as Windows cannot hold them.
pub(crate) const UNPLACEABLE: [&str; 3] = ["bad:name.txt", "trailing.", "CON.txt"];

/// The size of the file at `path`, as the tree serves it.
pub(crate) fn size_of(path: &str) -> Option<usize> {
	FILES
		.iter()
		.find(|file| file.path == path)
		.map(|file| file.size)
}

/// The names a listing of the root must show, sorted.
pub(crate) fn expected_root() -> Vec<String> {
	let mut names: Vec<String> = FILES
		.iter()
		.filter(|file| !file.path.contains('\\'))
		.map(|file| file.path.to_owned())
		.chain(
			DIRS.iter()
				.map(|(path, _)| *path)
				.filter(|path| !path.contains('\\'))
				.chain([CASE_KEPT.0])
				.map(str::to_owned),
		)
		.collect();
	names.sort();
	names
}

/// The names the core must report skipped in the root.
pub(crate) fn expected_skipped() -> Vec<&'static str> {
	let mut names = UNPLACEABLE.to_vec();
	names.push(CASE_LOST.0);
	names
}

/// The listed file `name`, its content added to `drive` under the stable id `stable`.
fn add(
	drive: &FakeDrive,
	stable: u128,
	name: &str,
	size: usize,
	content: Content,
) -> FfiNonRootObject {
	let (stable, version) = (Uuid::from_u128(stable), Uuid::from_u128(stable + 0x10_0000));
	drive.add_file(stable, version, &pattern(size));
	match content {
		Content::Plain => {}
		Content::WrongHash => drive.declare(stable, Some([0; 32])),
		Content::TruncatedAt(offset) => drive.truncate_at(stable, offset),
		Content::Held => drive.hold_file(stable),
		Content::Delayed(delay) => drive.delay_file(stable, delay),
	}
	FfiNonRootObject::File(ffi_file(
		stable,
		version,
		name,
		i64::try_from(size).expect("a tree file is a few MiB"),
	))
}

/// The stable id the file at `path` is served under.
pub(crate) fn stable_of(path: &str) -> Option<Uuid> {
	(0x1000_u128..)
		.zip(FILES)
		.find(|(_, file)| file.path == path)
		.map(|(n, _)| Uuid::from_u128(n))
}

/// The uuid of the directory `path`.
pub(crate) fn dir_uuid(path: &str) -> Option<Uuid> {
	DIRS.iter()
		.find(|(dir, _)| *dir == path)
		.map(|(_, uuid)| Uuid::from_u128(*uuid))
}

/// The cache id the directory `path` is listed under: the account root for `""`.
pub(crate) fn dir_id(path: &str) -> Option<String> {
	if path.is_empty() {
		return Some(ROOT.to_string());
	}
	DIRS.iter()
		.find(|(dir, _)| *dir == path)
		.map(|(_, uuid)| format!("stable/{}", Uuid::from_u128(*uuid)))
}

/// The directory `path` lives in, `""` for the root, and its name.
fn split(path: &str) -> (&str, &str) {
	path.rsplit_once('\\').unwrap_or(("", path))
}

/// The uuid text the children of the directory `path` name as their parent.
fn parent_uuid(path: &str) -> String {
	let id = dir_id(path).expect("a directory of the tree");
	id.strip_prefix("stable/").unwrap_or(&id).to_owned()
}

/// `child` with its parent set to the directory `dir`, as the cache lists it.
fn with_parent(mut child: FfiNonRootObject, dir: &str) -> FfiNonRootObject {
	match &mut child {
		FfiNonRootObject::File(file) => file.parent = parent_uuid(dir),
		FfiNonRootObject::Dir(listed) => listed.parent = parent_uuid(dir),
	}
	child
}

/// The drive serving the tree.
pub(crate) fn build() -> FakeDrive {
	let drive = FakeDrive::new(ROOT);
	let mut listings: HashMap<&str, Vec<FfiNonRootObject>> = HashMap::new();
	for (n, entry) in (0x1000_u128..).zip(FILES) {
		let (dir, name) = split(entry.path);
		let child = add(&drive, n, name, entry.size, entry.content);
		listings.entry(dir).or_default().push(child);
	}
	let root = listings.entry("").or_default();
	for (n, name) in (0x1100_u128..).zip(UNPLACEABLE) {
		root.push(add(&drive, n, name, 100, Content::Plain));
	}
	for (name, stable) in [CASE_LOST, CASE_KEPT] {
		root.push(add(&drive, stable, name, 100, Content::Plain));
	}
	for &(path, uuid) in DIRS {
		let (dir, name) = split(path);
		listings
			.entry(dir)
			.or_default()
			.push(FfiNonRootObject::Dir(ffi_dir(Uuid::from_u128(uuid), name)));
		listings.entry(path).or_default();
	}
	for (dir, children) in listings {
		let children = children
			.into_iter()
			.map(|child| with_parent(child, dir))
			.collect();
		drive.list(&dir_id(dir).expect("a directory of the tree"), children);
	}
	drive
}

/// The bytes a `bump` gives the file of `size` bytes: not the pattern.
pub(crate) fn bumped(size: usize) -> Vec<u8> {
	pattern(size).into_iter().map(|byte| byte ^ 0x5A).collect()
}

/// The bytes of a file of `size` bytes an `add` lists: neither the pattern nor `bumped`.
pub(crate) fn added(size: usize) -> Vec<u8> {
	pattern(size).into_iter().map(|byte| byte ^ 0xA5).collect()
}
