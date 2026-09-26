//! "Keep both" naming: an item written into a directory gets a name that is free there,
//! `name (1).ext` style, instead of replacing (or, on the server, versioning) what is already
//! there. Used wherever the SDK creates items from other items: copies, compressed archives and
//! extracted entries.

use std::{
	borrow::Cow,
	collections::{HashMap, HashSet},
};

use super::{EntryNameError, EntryNameErrorKind, MAX_BYTES, ValidatedName, encode_name};

/// The key two names collide on. The server compares names through `hash_name`, which
/// lowercases with [`str::to_lowercase`], so this must use exactly the same folding.
fn collision_key(name: &str) -> String {
	name.to_lowercase()
}

/// A source item's name made valid: the name itself when it is valid, otherwise its reversible
/// encoding (legacy clients stored names today's rules reject).
#[derive(Debug, PartialEq)]
pub(crate) enum SourceName {
	Valid(ValidatedName),
	Encoded(ValidatedName),
}

impl SourceName {
	pub(crate) fn parse(name: &str) -> Result<Self, EntryNameError> {
		match ValidatedName::try_from(name) {
			Ok(valid) => Ok(Self::Valid(valid)),
			Err(_) => encode_name(name).map(Self::Encoded),
		}
	}

	pub(crate) fn into_name(self) -> ValidatedName {
		match self {
			Self::Valid(name) | Self::Encoded(name) => name,
		}
	}
}

/// The names taken in one destination directory, compared case-insensitively.
#[derive(Debug, Default)]
pub(crate) struct TakenNames {
	keys: HashSet<String>,
	/// Per `(stem, extension)` collision key, a counter below which every candidate is known to
	/// be taken, so the `k`-th duplicate of one name does not retry the `k - 1` before it.
	/// Names are only ever added, so a candidate once seen taken stays taken.
	next_counter: HashMap<(String, String), u64>,
}

impl TakenNames {
	pub(crate) fn new<'a>(names: impl IntoIterator<Item = &'a str>) -> Self {
		Self {
			keys: names.into_iter().map(collision_key).collect(),
			next_counter: HashMap::new(),
		}
	}

	#[cfg(test)]
	pub(crate) fn contains(&self, name: &str) -> bool {
		self.keys.contains(&collision_key(name))
	}

	/// Marks `name` as taken; returns false if it already was.
	pub(crate) fn insert(&mut self, name: &str) -> bool {
		self.keys.insert(collision_key(name))
	}

	/// Picks and takes a free name for an item called `name`: the name itself when free,
	/// otherwise `stem (n).ext` with the smallest free `n`. A name that already ends in
	/// ` (n)` continues from `n + 1`; a counter that cannot grow any further is treated as part
	/// of the stem, so it gets a counter of its own. Only a file's last extension is kept apart
	/// (`a.tar.gz` → `a.tar (1).gz`); directories have no extension. Candidates are trimmed at
	/// a character boundary to fit the name length limit.
	pub(crate) fn allocate(
		&mut self,
		name: ValidatedName,
		is_dir: bool,
	) -> Result<ValidatedName, EntryNameError> {
		if self.insert(name.as_ref()) {
			return Ok(name);
		}

		let (stem, ext) = split_extension(name.as_ref(), is_dir);
		let (mut base, mut n) = strip_counter(stem)
			.and_then(|(base, n)| Some((base, n.checked_add(1)?)))
			.unwrap_or((stem, 1));
		let mut key = (collision_key(base), collision_key(ext));
		let known_taken_below = self.next_counter.get(&key).copied().unwrap_or(1);
		// the hint may only be raised when every counter below this start is known taken
		let mut contiguous = n <= known_taken_below;
		n = n.max(known_taken_below);
		loop {
			let candidate = numbered_candidate(base, n, ext)?;
			if self.insert(candidate.as_ref()) {
				if contiguous && let Some(next) = n.checked_add(1) {
					self.next_counter.insert(key, next);
				}
				return Ok(candidate);
			}
			// Every iteration either returns or skips a taken name, and only finitely many
			// names are taken, so counting up terminates. Only a continued counter can run out
			// of numbers; the whole stem then starts over at 1, with its own hint.
			(base, n) = match n.checked_add(1) {
				Some(next) => (base, next),
				None => {
					key = (collision_key(stem), key.1);
					let start = self.next_counter.get(&key).copied().unwrap_or(1);
					contiguous = true;
					(stem, start)
				}
			};
		}
	}
}

/// `(stem, ext)` with `ext` including its dot. A leading dot (`.bashrc`) is part of the stem,
/// not an extension.
fn split_extension(name: &str, is_dir: bool) -> (&str, &str) {
	if is_dir {
		return (name, "");
	}
	match name.rfind('.') {
		Some(dot) if dot > 0 => name.split_at(dot),
		_ => (name, ""),
	}
}

/// Splits a trailing ` (n)` off `stem`, when there is a non-empty base before it.
fn strip_counter(stem: &str) -> Option<(&str, u64)> {
	let inner = stem.strip_suffix(')')?;
	let open = inner.rfind(" (")?;
	let digits = &inner[open + 2..];
	if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
		return None;
	}
	let base = &inner[..open];
	if base.is_empty() {
		return None;
	}
	Some((base, digits.parse().ok()?))
}

/// `base (n)ext`, with `base` (and, if even that is not enough, `ext`) trimmed at a character
/// boundary so the result fits [`MAX_BYTES`].
fn numbered_candidate(base: &str, n: u64, ext: &str) -> Result<ValidatedName, EntryNameError> {
	let suffix = format!(" ({n})");
	// An extension so long that no base character fits is folded into the base, so trimming
	// eats into it instead of producing an empty base.
	let (base, ext) = if ext.len() + suffix.len() >= MAX_BYTES {
		(Cow::Owned(format!("{base}{ext}")), "")
	} else {
		(Cow::Borrowed(base), ext)
	};
	let mut budget = MAX_BYTES - suffix.len() - ext.len();
	loop {
		let trimmed = &base[..base.floor_char_boundary(budget)];
		let candidate = format!("{trimmed}{suffix}{ext}");
		match ValidatedName::try_from(candidate.as_str()) {
			// NFC normalization can lengthen a name slightly; trim further and retry.
			Err(EntryNameError {
				kind: EntryNameErrorKind::TooLong { .. },
				..
			}) if budget > 1 => budget -= 1,
			Err(_) => return encode_name(&candidate),
			Ok(name) => return Ok(name),
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn source_name(name: &str) -> ValidatedName {
		SourceName::parse(name).unwrap().into_name()
	}

	fn allocate(taken: &[&str], name: &str, is_dir: bool) -> String {
		let mut names = TakenNames::new(taken.iter().copied());
		names.allocate(source_name(name), is_dir).unwrap().into()
	}

	#[test]
	fn free_name_is_kept() {
		assert_eq!(allocate(&["other.txt"], "a.txt", false), "a.txt");
		assert_eq!(allocate(&[], "dir", true), "dir");
	}

	#[test]
	fn clashing_name_gets_the_next_free_counter() {
		assert_eq!(allocate(&["a.txt"], "a.txt", false), "a (1).txt");
		assert_eq!(
			allocate(&["a.txt", "a (1).txt"], "a.txt", false),
			"a (2).txt"
		);
		assert_eq!(allocate(&["docs"], "docs", true), "docs (1)");
	}

	#[test]
	fn collisions_are_case_insensitive_like_the_server() {
		assert_eq!(allocate(&["A.TXT"], "a.txt", false), "a (1).txt");
		assert_eq!(
			allocate(&["ÄRGER.txt"], "ärger.txt", false),
			"ärger (1).txt"
		);
		assert_eq!(
			allocate(&["a (1).TXT", "a.txt"], "A.txt", false),
			"A (2).txt"
		);
		// the key is exactly `to_lowercase`, which `hash_name` uses: it maps `İ` to `i` plus a
		// combining dot, where a simple case fold would give a plain `i`
		assert_eq!(collision_key("İstanbul"), "i\u{307}stanbul");
	}

	#[test]
	fn existing_counter_is_continued() {
		assert_eq!(allocate(&["a (1).txt"], "a (1).txt", false), "a (2).txt");
		assert_eq!(allocate(&["photos (9)"], "photos (9)", true), "photos (10)");
		// only a well-formed ` (digits)` counts as a counter
		assert_eq!(
			allocate(&["a (x).txt"], "a (x).txt", false),
			"a (x) (1).txt"
		);
		assert_eq!(allocate(&["(1).txt"], "(1).txt", false), "(1) (1).txt");
		assert_eq!(allocate(&["a(1).txt"], "a(1).txt", false), "a(1) (1).txt");
	}

	#[test]
	fn a_counter_that_cannot_grow_becomes_part_of_the_stem() {
		let max = format!("a ({}).txt", u64::MAX);
		assert_eq!(
			allocate(&[max.as_str()], &max, false),
			format!("a ({}) (1).txt", u64::MAX)
		);
		let below_max = format!("a ({}).txt", u64::MAX - 1);
		assert_eq!(
			allocate(&[below_max.as_str(), max.as_str()], &below_max, false),
			format!("a ({}) (1).txt", u64::MAX - 1)
		);
	}

	#[test]
	fn only_a_files_last_extension_is_kept_apart() {
		assert_eq!(allocate(&["a.tar.gz"], "a.tar.gz", false), "a.tar (1).gz");
		assert_eq!(allocate(&["Makefile"], "Makefile", false), "Makefile (1)");
		assert_eq!(allocate(&[".bashrc"], ".bashrc", false), ".bashrc (1)");
		assert_eq!(allocate(&["my.folder"], "my.folder", true), "my.folder (1)");
	}

	#[test]
	fn each_allocation_takes_its_name() {
		let mut names = TakenNames::default();
		let first: String = names.allocate(source_name("a.txt"), false).unwrap().into();
		let second: String = names.allocate(source_name("a.txt"), false).unwrap().into();
		let third: String = names.allocate(source_name("A.TXT"), false).unwrap().into();
		assert_eq!(
			[first.as_str(), second.as_str(), third.as_str()],
			["a.txt", "a (1).txt", "A (2).TXT"]
		);
		assert!(names.contains("A (1).TXT"));
	}

	#[test]
	fn names_are_nfc_normalized_before_comparison() {
		let decomposed = "e\u{301}.txt";
		let composed = "\u{e9}.txt";
		assert_eq!(allocate(&[composed], decomposed, false), "\u{e9} (1).txt");
	}

	#[test]
	fn invalid_legacy_names_are_encoded() {
		let encoded = encode_name("a:b.txt").unwrap();
		assert_eq!(
			SourceName::parse("a:b.txt").unwrap(),
			SourceName::Encoded(encoded.clone())
		);
		assert_eq!(
			SourceName::parse("a.txt").unwrap(),
			SourceName::Valid(ValidatedName::try_from("a.txt").unwrap())
		);
		assert_eq!(allocate(&[], "a:b.txt", false), String::from(encoded));
		let mut names = TakenNames::default();
		names.allocate(source_name("a:b.txt"), false).unwrap();
		let second: String = names
			.allocate(source_name("a:b.txt"), false)
			.unwrap()
			.into();
		assert!(second.ends_with(" (1).txt"), "{second}");
		assert!(ValidatedName::try_from(second.as_str()).is_ok());
	}

	#[test]
	fn long_names_are_trimmed_to_the_limit_at_char_boundaries() {
		let long = format!("{}.txt", "é".repeat(125)); // 250 + 4 bytes
		assert_eq!(long.len(), 254);
		let renamed = allocate(&[long.as_str()], &long, false);
		assert!(renamed.len() <= MAX_BYTES, "{} bytes", renamed.len());
		assert!(renamed.ends_with(" (1).txt"), "{renamed}");
		assert!(ValidatedName::try_from(renamed.as_str()).is_ok());
	}

	#[test]
	fn a_huge_extension_is_trimmed_rather_than_emptying_the_base() {
		let long = format!("a.{}", "x".repeat(253));
		assert_eq!(long.len(), MAX_BYTES);
		let renamed = allocate(&[long.as_str()], &long, false);
		assert!(renamed.len() <= MAX_BYTES, "{} bytes", renamed.len());
		assert!(renamed.starts_with("a."), "{renamed}");
		assert!(renamed.ends_with(" (1)"), "{renamed}");
	}

	#[test]
	fn many_duplicates_are_allocated_in_linear_time() {
		// names from an archive are attacker-chosen: the k-th duplicate must not retry every
		// counter before it, or 20k duplicates of one name (and of its case variants) would
		// take hundreds of millions of candidate checks
		let mut names = TakenNames::default();
		let start = std::time::Instant::now();
		let mut last = String::new();
		for i in 0..20_000 {
			let spelling = if i % 2 == 0 {
				"report.pdf"
			} else {
				"REPORT.pdf"
			};
			last = names.allocate(source_name(spelling), false).unwrap().into();
		}
		assert_eq!(last, "REPORT (19999).pdf");
		assert!(
			start.elapsed() < std::time::Duration::from_secs(1),
			"{:?}",
			start.elapsed()
		);
		// the hint never skips a free counter, even one below a counter taken out of order
		let mut names = TakenNames::new(["a.txt", "a (2).txt"]);
		let first: String = names.allocate(source_name("a.txt"), false).unwrap().into();
		let second: String = names.allocate(source_name("a.txt"), false).unwrap().into();
		assert_eq!(
			[first.as_str(), second.as_str()],
			["a (1).txt", "a (3).txt"]
		);
	}

	#[test]
	fn empty_names_are_rejected() {
		assert!(SourceName::parse("").is_err());
	}
}
