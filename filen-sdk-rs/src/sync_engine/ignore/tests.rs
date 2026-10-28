use std::collections::BTreeMap;

use super::*;
use crate::ErrorKind;

fn source(text: &str, origin: Origin<'_>) -> IgnoreSource {
	let (source, errors) = IgnoreSource::parse(text, origin).unwrap();
	assert!(errors.is_empty(), "{errors:?}");
	source
}

fn root_rules(text: &str) -> IgnoreRules {
	let mut rules = IgnoreRules::default();
	rules.insert_file(String::new(), source(text, Origin::File { dir: "" }));
	rules
}

fn ignored(rules: &IgnoreRules, rel_path: &str, is_dir: bool) -> bool {
	rules
		.ignored_root(rel_path, is_dir, &mut HashMap::new())
		.is_some()
}

/// The decision hiding `rel_path`, which must be hidden.
fn decision(rules: &IgnoreRules, rel_path: &str) -> IgnoreDecision {
	rules
		.ignored_root(rel_path, F, &mut HashMap::new())
		.unwrap_or_else(|| panic!("{rel_path} is not ignored"))
		.1
}

const F: bool = false;
const D: bool = true;

#[test]
fn gitignore_conformance() {
	#[rustfmt::skip]
	let probes: &[(&str, &str, &str, bool, bool)] = &[
		("1", "months", "months", F, true),
		("2", "months", "amonths", F, false),
		("3", "months", "monthsa", F, false),
		("4", "*.lock", "Cargo.lock", F, true),
		("5", "*.rs", "src/main.rs", F, true),
		("6", "src/*.rs", "src/grep/src/main.rs", F, false),
		("7", "/*.c", "cat-file.c", F, true),
		("8", "/*.c", "mozilla-sha1/sha1.c", F, false),
		("9", "/src/*.rs", "src/grep/src/main.rs", F, false),
		("10", "!src/main.rs\n*.rs", "src/main.rs", F, true),
		("11", "*.rs\n!src/main.rs", "src/main.rs", F, false),
		("12", "foo/", "foo", D, true),
		("13", "foo/", "foo", F, false),
		("14", "**/foo", "src/foo", F, true),
		("15", "**/foo/**", "wat/src/foo/bar/baz", F, true),
		("16", "**/foo/**", "wat/src/afoo/bar/baz", F, false),
		("17", "abc/**", "abc", D, false),
		("18", "a/**/b", "a/x/y/b", F, true),
		("19", "\\!xy", "!xy", F, true),
		("20", "\\#foo", "#foo", F, true),
		("21", "#foo", "#foo", F, false),
		("22", "node_modules/ ", "node_modules", D, true),
		("23", "*.html", "foo.HTML", F, true),
		("24", "path1/*", "path2/path1/foo", F, false),
		("25", "dir/\n!dir/keep", "dir/keep", F, true),
		("25b", "dir\n!dir/keep", "dir/keep", F, true),
		("26", "dir/*\n!dir/keep", "dir/keep", F, false),
	];
	for &(n, patterns, path, is_dir, expect) in probes {
		let rules = root_rules(patterns);
		assert_eq!(
			ignored(&rules, path, is_dir),
			expect,
			"probe {n}: {patterns:?} vs {path:?}"
		);
	}
	// The trap 25/25b guard against: the crate alone re-includes under an excluded parent.
	for patterns in ["dir/\n!dir/keep", "dir\n!dir/keep"] {
		let mut builder = GitignoreBuilder::new(".");
		for line in patterns.lines() {
			builder.add_line(None, line).unwrap();
		}
		let matcher = builder.build().unwrap();
		assert!(
			matcher
				.matched_path_or_any_parents("dir/keep", false)
				.is_whitelist()
		);
	}
}

#[test]
fn nested_file_is_scoped_to_its_directory() {
	let mut rules = IgnoreRules::default();
	rules.insert_file(
		"src".into(),
		source("/build\n*.o", Origin::File { dir: "src" }),
	);
	assert!(ignored(&rules, "src/build", D));
	assert!(!ignored(&rules, "build", D));
	assert!(ignored(&rules, "src/a/b.o", F));
	assert!(!ignored(&rules, "lib/b.o", F));
	assert_eq!(
		rules.decide("src/a/b.o", F),
		Some(IgnoreHit {
			origin: Origin::File { dir: "src" },
			pattern: "*.o"
		})
	);
}

#[test]
fn deeper_file_beats_shallower() {
	let mut rules = root_rules("*.log");
	rules.insert_file("src".into(), source("!*.log", Origin::File { dir: "src" }));
	assert!(!ignored(&rules, "src/a.log", F));
	assert!(!ignored(&rules, "src/deep/a.log", F));
	assert!(ignored(&rules, "b.log", F));
}

#[test]
fn nested_whitelist_cannot_reinclude_under_an_ignored_directory() {
	let mut rules = root_rules("dir/");
	rules.insert_file("dir".into(), source("!keep", Origin::File { dir: "dir" }));
	assert!(rules.decide("dir/keep", F).is_none());
	assert!(ignored(&rules, "dir/keep", F));
}

#[test]
fn file_beats_user_beats_defaults() {
	let mut rules = IgnoreRules::new(Some(source("*.psd\n!.DS_Store", Origin::User)));
	assert!(ignored(&rules, "a.psd", F));
	assert!(!ignored(&rules, "a/.DS_Store", F));
	assert_eq!(
		rules.decide("a.psd", F).map(|hit| hit.origin),
		Some(Origin::User)
	);
	rules.insert_file(String::new(), source("!*.psd", Origin::File { dir: "" }));
	assert!(!ignored(&rules, "a.psd", F));

	let bare = IgnoreRules::default();
	assert_eq!(
		bare.decide("a/.DS_Store", F),
		Some(IgnoreHit {
			origin: Origin::Default,
			pattern: ".DS_Store"
		})
	);
}

#[test]
fn matching_is_case_insensitive() {
	let rules = IgnoreRules::new(Some(source("*.JPG\nÄ*", Origin::User)));
	assert!(ignored(&rules, "img.jpg", F));
	assert!(ignored(&rules, "ä.txt", F));
	assert!(ignored(&rules, "Ä.TXT", F));
	assert_eq!(
		rules.decide("img.jpg", F).map(|hit| hit.pattern),
		Some("*.JPG")
	);
}

#[test]
fn nfd_pattern_matches_nfc_path() {
	let rules = root_rules("e\u{301}t\u{e9}.txt");
	assert!(ignored(&rules, "\u{e9}t\u{e9}.txt", F));
}

#[test]
fn brace_alternation_and_double_star_pins() {
	// Divergence from git, which reads braces literally: globset alternates.
	let rules = root_rules("*.{jpg,png}");
	assert!(ignored(&rules, "a.png", F));
	assert!(!ignored(&rules, "a.{jpg,png}", F));
	assert!(ignored(&root_rules("\\{a\\}"), "{a}", F));
	// `**` away from a separator is two `*`, as in git.
	let rules = root_rules("a**b");
	assert!(ignored(&rules, "axyzb", F));
	assert!(!ignored(&rules, "ax/yb", F));
}

#[test]
fn bad_line_is_reported_and_the_rest_applies() {
	let text = "\u{feff}*.log\r\n[z-a]\r\n!keep.log\r\n";
	let (source, errors) = IgnoreSource::parse(text, Origin::File { dir: "src" }).unwrap();
	assert_eq!(errors.len(), 1);
	assert_eq!(errors[0].line, Some(2));
	assert!(
		errors[0].to_string().starts_with("src/.filenignore:2: "),
		"{}",
		errors[0]
	);
	let mut rules = IgnoreRules::default();
	rules.insert_file("src".into(), source);
	assert!(ignored(&rules, "src/a.log", F));
	assert!(!ignored(&rules, "src/keep.log", F));

	let (_, errors) = IgnoreSource::parse("{a", Origin::User).unwrap();
	assert!(
		errors[0]
			.to_string()
			.starts_with("user ignore patterns:1: ")
	);
}

/// The user level is all or nothing, unlike a `.filenignore`: one bad line refuses the whole text,
/// naming it, so what the engine stores is exactly what applies.
#[test]
fn a_bad_line_refuses_the_whole_user_level() {
	let error = parse_user_ignore("*.psd\n[z-a]\n!keep.psd\n")
		.expect_err("a bad line refuses the whole text");
	assert_eq!(error.line, Some(2));
	assert!(
		error.to_string().starts_with("user ignore patterns:2: "),
		"{error}"
	);

	let rules = IgnoreRules::new(Some(
		parse_user_ignore("*.psd\n!keep.psd\n").expect("a clean text compiles"),
	));
	assert!(ignored(&rules, "a.psd", F));
	assert!(!ignored(&rules, "keep.psd", F));
}

#[test]
fn memo_serves_later_paths_under_an_ignored_directory() {
	let rules = root_rules("build/");
	let mut memo = HashMap::new();
	let by_root = IgnoreDecision {
		level: IgnoreLevel::File { dir: String::new() },
		pattern: "build/".to_string(),
	};
	assert_eq!(
		rules.ignored_root("build/a/b", F, &mut memo),
		Some(("build", by_root.clone()))
	);
	assert_eq!(memo.get("build"), Some(&Some(by_root.clone())));
	assert_eq!(
		rules.ignored_root("build/c", F, &mut memo),
		Some(("build", by_root))
	);
	assert_eq!(rules.ignored_root("src/c", F, &mut memo), None);
	assert_eq!(memo.get("src"), Some(&None));
	assert_eq!(rules.ignored_root("", D, &mut memo), None);
}

/// A path an ancestor directory's rule hides is reported at that ancestor, so the line that comes
/// back is the line for the reported path — the leaf's own name matched nothing.
#[test]
fn an_ancestor_rule_comes_back_with_the_ancestor() {
	let rules = root_rules("*.log\nbuild/");
	// A leaf the `*.log` line also matches, so consulting the leaf first would come back with a
	// different path AND a different line.
	assert_eq!(
		rules.ignored_root("build/deep/a.log", F, &mut HashMap::new()),
		Some((
			"build",
			IgnoreDecision {
				level: IgnoreLevel::File { dir: String::new() },
				pattern: "build/".to_string(),
			}
		))
	);
}

/// The line comes back as the user wrote it, not as the case-folded text the matcher holds.
#[test]
fn the_reported_line_is_the_one_as_written() {
	let rules = IgnoreRules::new(Some(source("Ä*\n*.JPG", Origin::User)));
	assert_eq!(decision(&rules, "ä.txt").pattern, "Ä*");
	assert_eq!(decision(&rules, "sub/IMG.jpg").pattern, "*.JPG");
}

/// A line that escapes its trailing space keeps it. Trimming it off would report a dangling `\`,
/// which is not a line anyone wrote and not one that would match if pasted back.
#[test]
fn an_escaped_trailing_space_is_reported_with_the_space() {
	let rules = root_rules("node_modules\\ ");
	assert_eq!(decision(&rules, "node_modules ").pattern, "node_modules\\ ");
}

/// The line rides beside the level, never inside it: two paths one source hides with different
/// lines keep equal levels, which is what the rule that keeps a deleted directory compares.
#[test]
fn two_decisions_differing_only_in_the_line_keep_equal_levels() {
	let rules = root_rules("*.log\n*.tmp");
	let one = decision(&rules, "a.log");
	let other = decision(&rules, "b.tmp");
	assert_ne!(one.pattern, other.pattern);
	assert_eq!(one.level, other.level);
	assert_ne!(one, other);
}

#[test]
fn every_default_matches_its_example_and_misses_a_near_miss() {
	#[rustfmt::skip]
	let cases: &[(&str, bool, &str, bool)] = &[
		("a/.DS_Store", F, "a/DS_Store", F),
		("._photo.jpg", F, "_photo.jpg", F),
		(".Spotlight-V100", D, ".Spotlight-V100", F),
		(".fseventsd", D, ".fseventsd", F),
		(".TemporaryItems", D, ".TemporaryItems", F),
		(".DocumentRevisions-V100", D, ".DocumentRevisions-V100", F),
		(".Trashes", D, ".Trashes", F),
		("a/Thumbs.db", F, "a/Thumbs.dbx", F),
		("ehthumbs.db", F, "myehthumbs.db", F),
		("ehthumbs_vista.db", F, "ehthumbs_vista.db.bak", F),
		("desktop.ini", F, "desktop.ini.txt", F),
		("$RECYCLE.BIN", D, "$RECYCLE.BIN", F),
		("System Volume Information", D, "System Volume Information", F),
		("~$budget.xlsx", F, "budget~$.xlsx", F),
		(".~lock.doc.odt#", F, ".~lock.doc.odt", F),
		("a.swp", F, "a.swpx", F),
		("a.swo", F, "swo", F),
		("a.swn", F, "a.sw", F),
		(".#notes.org", F, "#notes.org", F),
		("x.crdownload", F, "x.crdownload.txt", F),
	];
	assert_eq!(cases.len(), DEFAULT_IGNORE_PATTERNS.len());
	let rules = IgnoreRules::default();
	for &(hit, hit_dir, miss, miss_dir) in cases {
		assert!(rules.decide(hit, hit_dir).is_some(), "{hit}");
		assert!(rules.decide(miss, miss_dir).is_none(), "{miss}");
	}
}

fn rule_node(rel_path: &str, size: u64) -> RemoteNode {
	RemoteNode {
		rel_path: rel_path.to_owned(),
		kind: NodeKind::File,
		remote_uuid: Uuid::new_v4(),
		stable_uuid: None,
		content_hash: None,
		size,
		modified_millis: 0,
	}
}

fn rule_view(nodes: Vec<RemoteNode>, held: &[&str]) -> RemoteView {
	RemoteView {
		nodes: nodes
			.into_iter()
			.map(|node| (node.rel_path.clone(), node))
			.collect(),
		has_collisions: false,
		held_paths: held.iter().map(|path| (*path).to_owned()).collect(),
		skipped: Vec::new(),
		ignored: BTreeMap::new(),
		ignored_default_untracked: 0,
	}
}

/// The remote rules `mode` reads from `view`, where a download serves `bodies` by path (a path
/// without one fails) and `on_disk` lists the directories holding a local rule file. Also returns
/// the paths downloaded, in order.
async fn load(
	mode: SyncMode,
	view: &RemoteView,
	on_disk: &[&str],
	bodies: &HashMap<&str, Vec<u8>>,
	cached: &HashMap<Uuid, Arc<str>>,
) -> (RemoteRules, Vec<String>) {
	load_tracked(mode, view, on_disk, &[], bodies, cached).await
}

/// [`load`], where `tracked` lists the directories whose rule file has a baseline row.
async fn load_tracked(
	mode: SyncMode,
	view: &RemoteView,
	on_disk: &[&str],
	tracked: &[&str],
	bodies: &HashMap<&str, Vec<u8>>,
	cached: &HashMap<Uuid, Arc<str>>,
) -> (RemoteRules, Vec<String>) {
	// A whole-read view: its backing never consults a baseline, so an empty one is exact.
	let no_baseline = Baseline::default();
	let paths: HashMap<Uuid, String> = view
		.nodes
		.of(&no_baseline)
		.iter()
		.map(|(_, node)| (node.remote_uuid, node.rel_path.clone()))
		.collect();
	let mut fetched = Vec::new();
	let rules = load_remote_rules(
		mode,
		RuleCandidates {
			view,
			baseline: &no_baseline,
			paths: rule_file_paths(view, &no_baseline),
		},
		None,
		|dir| on_disk.contains(&dir),
		|dir| tracked.contains(&dir),
		cached,
		|uuid| {
			let path = paths[&uuid].as_str();
			fetched.push(path.to_owned());
			std::future::ready(
				bodies
					.get(path)
					.cloned()
					.ok_or_else(|| Error::custom(ErrorKind::IO, "offline")),
			)
		},
	)
	.await;
	(rules, fetched)
}

fn remote_ignored(rules: &RemoteRules, rel_path: &str) -> bool {
	ignored(&rules.rules, rel_path, F)
}

#[tokio::test]
async fn a_mode_that_only_pushes_reads_no_remote_rules() {
	let view = rule_view(vec![rule_node(FILENIGNORE, 2)], &[".filenignore"]);
	let bodies = HashMap::from([(FILENIGNORE, b"*\n".to_vec())]);
	for mode in [SyncMode::LocalToRemote, SyncMode::LocalBackup] {
		let (rules, fetched) = load(mode, &view, &[], &bodies, &HashMap::new()).await;
		assert!(fetched.is_empty(), "{mode:?}: {fetched:?}");
		assert!(!remote_ignored(&rules, "a.txt"), "{mode:?}");
		assert!(
			rules.blocked.is_empty() && rules.errors.is_empty(),
			"{mode:?}"
		);
	}
}

/// A mode that only pushes takes its rules from disk, but a synced rule file the disk has lost (its
/// row still stands) is read from the remote until the loss propagates: a directory deleted
/// locally is still read with the rules that keep its ignored remote content.
#[tokio::test]
async fn a_mode_that_only_pushes_reads_a_synced_rule_file_the_disk_has_lost() {
	let view = rule_view(
		vec![
			rule_node(FILENIGNORE, 2),
			rule_node("proj/.filenignore", 14),
		],
		&[],
	);
	let bodies = HashMap::from([
		(FILENIGNORE, b"*\n".to_vec()),
		("proj/.filenignore", b"node_modules/\n".to_vec()),
	]);
	for mode in [SyncMode::LocalToRemote, SyncMode::LocalBackup] {
		let (rules, fetched) =
			load_tracked(mode, &view, &[""], &["", "proj"], &bodies, &HashMap::new()).await;
		assert_eq!(fetched, vec!["proj/.filenignore"], "{mode:?}");
		assert!(remote_ignored(&rules, "proj/node_modules/m.js"), "{mode:?}");
		assert!(!remote_ignored(&rules, "a.txt"), "{mode:?}");
		assert!(
			rules.blocked.is_empty() && rules.errors.is_empty(),
			"{mode:?}"
		);
	}
}

/// A pulling mode does not report a rule file held back under a directory the rules already
/// ignore: nothing under that directory syncs anyway.
#[tokio::test]
async fn a_held_rule_file_under_an_ignored_directory_is_not_reported() {
	let view = rule_view(vec![rule_node(FILENIGNORE, 7)], &["cache/sub/.filenignore"]);
	let bodies = HashMap::from([(FILENIGNORE, b"cache/\n".to_vec())]);
	for mode in [SyncMode::RemoteToLocal, SyncMode::RemoteBackup] {
		let (rules, _) = load(mode, &view, &[], &bodies, &HashMap::new()).await;
		assert!(
			rules.errors.is_empty() && rules.blocked.is_empty(),
			"{mode:?}: {:?}",
			rules.errors
		);
	}
}

/// A pulling mode reads its rules shallowest first, and never reads a file whose directory the
/// shallower rules already ignore: git never looks inside an ignored directory either.
#[tokio::test]
async fn a_mode_that_pulls_reads_shallowest_first_and_skips_ignored_directories() {
	let view = rule_view(
		vec![
			rule_node("src/deep/.filenignore", 9),
			rule_node("cache/.filenignore", 3),
			rule_node("src/.filenignore", 4),
			rule_node(FILENIGNORE, 7),
		],
		&[],
	);
	let bodies = HashMap::from([
		(FILENIGNORE, b"cache/\n".to_vec()),
		("cache/.filenignore", b"!x\n".to_vec()),
		("src/.filenignore", b"*.o\n".to_vec()),
		("src/deep/.filenignore", b"!keep.o\n".to_vec()),
	]);
	for mode in [SyncMode::RemoteToLocal, SyncMode::RemoteBackup] {
		let (rules, fetched) = load(mode, &view, &[""], &bodies, &HashMap::new()).await;
		assert_eq!(
			fetched,
			vec![".filenignore", "src/.filenignore", "src/deep/.filenignore"],
			"{mode:?}"
		);
		assert!(remote_ignored(&rules, "cache/x"), "{mode:?}");
		assert!(remote_ignored(&rules, "src/a.o"), "{mode:?}");
		assert!(!remote_ignored(&rules, "src/deep/keep.o"), "{mode:?}");
		assert!(
			rules.blocked.is_empty() && rules.errors.is_empty(),
			"{mode:?}"
		);
		assert_eq!(rules.bodies.len(), 3, "{mode:?}");
	}
}

/// A two-way pair takes a directory's rules from the copy on disk when there is one (the scan
/// reads it), and reads the remote copy only where there is none, under ignored directories too.
#[tokio::test]
async fn a_two_way_pair_reads_a_remote_rule_file_only_where_none_is_on_disk() {
	let view = rule_view(
		vec![
			rule_node(FILENIGNORE, 7),
			rule_node("cache/.filenignore", 3),
			rule_node("src/.filenignore", 4),
		],
		&[],
	);
	let bodies = HashMap::from([
		(FILENIGNORE, b"cache/\n".to_vec()),
		("cache/.filenignore", b"!x\n".to_vec()),
		("src/.filenignore", b"*.o\n".to_vec()),
	]);
	let (rules, mut fetched) = load(SyncMode::TwoWay, &view, &[""], &bodies, &HashMap::new()).await;
	fetched.sort();
	assert_eq!(fetched, vec!["cache/.filenignore", "src/.filenignore"]);
	assert!(
		!remote_ignored(&rules, "cache/y"),
		"the root rules are the scan's to read"
	);
	assert!(remote_ignored(&rules, "src/a.o"));

	let (rules, fetched) = load(SyncMode::TwoWay, &view, &[], &bodies, &HashMap::new()).await;
	assert_eq!(fetched.len(), 3, "{fetched:?}");
	assert!(remote_ignored(&rules, "cache/y"));
}

/// Rules that cannot be read block their directory for the pass, each with its own report line.
/// A bad line only reports, and the rest of that file applies.
#[tokio::test]
async fn an_unusable_remote_rule_file_blocks_its_directory() {
	let too_large = usize::try_from(MAX_RULE_FILE_BYTES).unwrap() + 1;
	let view = rule_view(
		vec![
			rule_node("offline/.filenignore", 4),
			rule_node("huge/.filenignore", MAX_RULE_FILE_BYTES + 1),
			rule_node("binary/.filenignore", 1),
			rule_node("understated/.filenignore", 4),
			rule_node("fine/.filenignore", 6),
			rule_node("badline/.filenignore", 12),
		],
		&["twice/.filenignore"],
	);
	let bodies = HashMap::from([
		("huge/.filenignore", b"*\n".to_vec()),
		("binary/.filenignore", vec![0xff]),
		("understated/.filenignore", vec![b'#'; too_large]),
		("fine/.filenignore", b"*.tmp\n".to_vec()),
		("badline/.filenignore", b"[z-a]\n*.bak\n".to_vec()),
	]);
	let (rules, fetched) = load(
		SyncMode::RemoteToLocal,
		&view,
		&[],
		&bodies,
		&HashMap::new(),
	)
	.await;
	assert!(
		!fetched.iter().any(|path| path == "huge/.filenignore"),
		"an oversized file is not downloaded: {fetched:?}"
	);
	let blocked = ["binary", "huge", "offline", "twice", "understated"];
	assert_eq!(
		rules.blocked,
		blocked.iter().map(|dir| (*dir).to_owned()).collect()
	);
	assert_eq!(rules.errors.len(), 6, "{:?}", rules.errors);
	for dir in blocked.iter().chain(&["badline"]) {
		let prefix = format!("remote {dir}/.filenignore");
		assert!(
			rules.errors.iter().any(|line| line.starts_with(&prefix)),
			"{prefix}: {:?}",
			rules.errors
		);
	}
	assert!(remote_ignored(&rules, "fine/a.tmp"));
	assert!(remote_ignored(&rules, "badline/a.bak"));

	// On a two-way pair a copy on disk governs, so the held remote one blocks nothing.
	let held = rule_view(Vec::new(), &["twice/.filenignore"]);
	let (rules, _) = load(
		SyncMode::TwoWay,
		&held,
		&["twice"],
		&bodies,
		&HashMap::new(),
	)
	.await;
	assert!(rules.blocked.is_empty(), "{:?}", rules.blocked);
}

#[tokio::test]
async fn a_cached_body_is_not_downloaded_again() {
	let root = rule_node(FILENIGNORE, 6);
	let sub = rule_node("sub/.filenignore", 6);
	let stale = Uuid::new_v4();
	let cached = HashMap::from([
		(root.remote_uuid, Arc::from("*.log\n")),
		(stale, Arc::from("*\n")),
	]);
	let view = rule_view(vec![root.clone(), sub.clone()], &[]);
	let bodies = HashMap::from([("sub/.filenignore", b"*.tmp\n".to_vec())]);
	let (rules, fetched) = load(SyncMode::RemoteToLocal, &view, &[], &bodies, &cached).await;
	assert_eq!(fetched, vec!["sub/.filenignore"]);
	assert!(remote_ignored(&rules, "a.log"));
	assert!(remote_ignored(&rules, "sub/a.tmp"));
	let mut kept: Vec<Uuid> = rules.bodies.keys().copied().collect();
	kept.sort();
	let mut expected = vec![root.remote_uuid, sub.remote_uuid];
	expected.sort();
	assert_eq!(kept, expected, "only the bodies this pass read are kept");
}

/// The rules a migrated `excludeDotFiles` pair is given, as the pair root's local file.
fn local_root_rules(text: &str) -> IgnoreRules {
	let mut rules = IgnoreRules::default();
	rules.insert_local_root(source(text, Origin::File { dir: "" }));
	rules
}

#[test]
fn a_local_root_rule_file_hides_what_it_names_as_the_root_level() {
	let rules = local_root_rules(".*\n!/.filenignore\n");
	assert!(rules.has_files());
	assert_eq!(
		decision(&rules, ".env.local"),
		IgnoreDecision {
			level: IgnoreLevel::File { dir: String::new() },
			pattern: ".*".to_owned(),
		}
	);
	assert!(ignored(&rules, "sub/.hidden", F));
	assert!(!ignored(&rules, "sub/visible.txt", F));
}

/// Its own path is out of the sync whatever its lines say, `!/.filenignore` included, and only as
/// a file at the root: a directory by that name, or a nested rule file, is an ordinary item.
#[test]
fn a_local_root_rule_file_keeps_itself_out_of_the_sync() {
	let rules = local_root_rules("*.tmp\n!/.filenignore\n");
	assert_eq!(
		rules.decide(FILENIGNORE, F),
		Some(IgnoreHit {
			origin: Origin::Default,
			pattern: FILENIGNORE,
		})
	);
	assert!(rules.decide(FILENIGNORE, D).is_none());
	assert!(rules.decide("sub/.filenignore", F).is_none());

	let without = root_rules("*.tmp\n");
	assert!(without.decide(FILENIGNORE, F).is_none());
}

/// The local root file reads as the remote root file's text with its own lines appended: it decides
/// before the remote root file and the user level, and a nested rule file still decides before it.
#[test]
fn a_local_root_rule_file_sits_between_the_nested_files_and_the_remote_root_file() {
	let mut rules = IgnoreRules::new(Some(source("!.user-kept\n", Origin::User)));
	rules.insert_file(
		String::new(),
		source("*.log\n!.remote-kept\n", Origin::File { dir: "" }),
	);
	rules.insert_file(
		"sub".into(),
		source("!.nested-kept\n", Origin::File { dir: "sub" }),
	);
	rules.insert_local_root(source(".*\n", Origin::File { dir: "" }));

	assert!(
		ignored(&rules, ".remote-kept", F),
		"local beats the remote root file"
	);
	assert!(
		ignored(&rules, ".user-kept", F),
		"local beats the user level"
	);
	assert!(
		ignored(&rules, "a.log", F),
		"the remote root file still applies"
	);
	assert!(
		!ignored(&rules, "sub/.nested-kept", F),
		"a nested file beats it"
	);
	assert!(ignored(&rules, "sub/.other", F));
}

#[test]
fn defaults_parse_cleanly() {
	let (_, errors) =
		IgnoreSource::parse(&DEFAULT_IGNORE_PATTERNS.join("\n"), Origin::Default).unwrap();
	assert!(errors.is_empty(), "{errors:?}");
}

// A backslash is one character of a Filen name on every platform. Off unix, globset reads it as a
// separator (its `Candidate` rewrites every `is_separator` byte to `/`), so without the stand-in
// the matcher saw `bad\x` as `bad/x` on Windows: the rules below that hide it missed it there, and
// the ones that miss it hid it.

#[test]
fn a_key_reaches_the_matcher_without_a_backslash_in_it() {
	assert_eq!(backslash::path_key("Bad\\X/file.txt"), "badBx/file.txt");
	assert_eq!(backslash::pattern_line("bad\\\\x"), "badBx");
}

#[test]
fn no_folded_name_or_line_holds_the_backslash_stand_in() {
	let mut buf = [0; 4];
	for c in (0..=u32::from(char::MAX)).filter_map(char::from_u32) {
		assert!(
			!collision_key(c.encode_utf8(&mut buf)).contains(backslash::STAND_IN),
			"{c:?}"
		);
	}
}

#[test]
fn a_directory_rule_hides_a_directory_whose_name_holds_a_backslash() {
	let rules = root_rules("bad*/");
	assert!(ignored(&rules, "bad\\x", D));
	assert!(
		!ignored(&rules, "bad\\x", F),
		"the rule is for directories only"
	);
	let (root, _) = rules
		.ignored_root("bad\\x/file.txt", F, &mut HashMap::new())
		.unwrap();
	assert_eq!(root, "bad\\x");
	let rules = root_rules("bad\\\\/");
	assert!(ignored(&rules, "bad\\", D));
	assert!(!ignored(&rules, "bad\\", F));
}

#[test]
fn a_rule_naming_only_what_follows_a_backslash_misses_the_name() {
	for pattern in ["x", "x/", "/x", "bad/x", "bad/", "bad/*"] {
		let rules = root_rules(pattern);
		assert!(!ignored(&rules, "bad\\x", D), "{pattern:?}");
		assert!(!ignored(&rules, "bad\\x/file.txt", F), "{pattern:?}");
	}
}

#[test]
fn a_backslash_written_into_a_rule_matches_the_backslash_in_a_name() {
	for pattern in [
		"bad\\\\x",
		"BAD\\\\X",
		"/bad\\\\x",
		"bad\\\\x/",
		"bad?x",
		"bad[\\]x",
		"bad[\\\\]x",
		"bad[!a]x",
		"bad[ -~]x",
		"bad[ -\u{3000}]x",
		"bad[!a-z]x",
		"bad{\\\\,y}x",
	] {
		let rules = root_rules(pattern);
		assert!(ignored(&rules, "bad\\x", D), "{pattern:?}");
	}
	let rules = root_rules("bad\\\\x");
	assert_eq!(decision(&rules, "bad\\x").pattern, "bad\\\\x");
}

#[test]
fn a_rule_without_a_backslash_where_the_name_has_one_misses_it() {
	for pattern in [
		"bad\\x",
		"bad\\\\\\\\x",
		"bad[!\\]x",
		"bad[a-z]x",
		"bad[0-[]x",
		"badBx",
		"bad\\\\x/",
	] {
		let rules = root_rules(pattern);
		assert!(!ignored(&rules, "bad\\x", F), "{pattern:?}");
	}
}

#[test]
fn an_escaped_backslash_before_a_trailing_space_keeps_the_space() {
	let rules = root_rules("bad\\\\ ");
	assert!(ignored(&rules, "bad\\ ", F));
	assert!(!ignored(&rules, "bad\\", F));
}

#[test]
fn a_class_naming_a_backslash_keeps_its_bracket_and_dash_literal() {
	assert_eq!(backslash::pattern_line("bad[]\\-]x"), "bad[]\\B-]x");
	let rules = root_rules("bad[]\\-]x");
	for name in ["bad\\x", "bad]x", "bad-x"] {
		assert!(ignored(&rules, name, F), "{name:?}");
	}
	assert!(!ignored(&rules, "badbx", F));
}
