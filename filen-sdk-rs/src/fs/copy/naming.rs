//! "Keep both" naming for copies: every copied item gets a name that is free in its
//! destination directory, `name (1).ext` style.

use std::{borrow::Cow, collections::HashSet};

use crate::fs::name::{EntryNameError, EntryNameErrorKind, MAX_BYTES, ValidatedName, encode_name};

/// The key two names collide on. The server compares names through `hash_name`, which
/// lowercases with [`str::to_lowercase`], so this must use exactly the same folding.
fn collision_key(name: &str) -> String {
	name.to_lowercase()
}

/// A name that passes validation: the name itself when it is valid, otherwise its
/// reversible encoding (legacy clients stored names today's rules reject).
pub(crate) fn validated_name(name: &str) -> Result<ValidatedName, EntryNameError> {
	ValidatedName::try_from(name).or_else(|_| encode_name(name))
}

/// The names taken in one destination directory, compared case-insensitively.
#[derive(Debug, Default)]
pub(crate) struct TakenNames {
	keys: HashSet<String>,
}

impl TakenNames {
	pub(crate) fn new<'a>(names: impl IntoIterator<Item = &'a str>) -> Self {
		Self {
			keys: names.into_iter().map(collision_key).collect(),
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

	/// Picks and takes a free name for an item called `name`: the (validated) name itself when
	/// free, otherwise `stem (n).ext` with the smallest free `n`. A name that already ends in
	/// ` (n)` continues from `n + 1`; a counter that cannot grow any further is treated as part
	/// of the stem, so it gets a counter of its own. Only a file's last extension is kept apart
	/// (`a.tar.gz` → `a.tar (1).gz`); directories have no extension. Candidates are trimmed at
	/// a character boundary to fit the name length limit.
	pub(crate) fn allocate(
		&mut self,
		name: &str,
		is_dir: bool,
	) -> Result<ValidatedName, EntryNameError> {
		let name = validated_name(name)?;
		if self.insert(name.as_ref()) {
			return Ok(name);
		}

		let (stem, ext) = split_extension(name.as_ref(), is_dir);
		let (mut base, mut n) = strip_counter(stem)
			.and_then(|(base, n)| Some((base, n.checked_add(1)?)))
			.unwrap_or((stem, 1));
		loop {
			let candidate = numbered_candidate(base, n, ext)?;
			if self.insert(candidate.as_ref()) {
				return Ok(candidate);
			}
			// Every iteration either returns or skips a taken name, and only finitely many
			// names are taken, so counting up from 1 terminates. Only a continued counter can
			// run out of numbers; the whole stem then starts over at 1.
			(base, n) = match n.checked_add(1) {
				Some(next) => (base, next),
				None => (stem, 1),
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
		let trimmed = truncate_at_char_boundary(&base, budget);
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

fn truncate_at_char_boundary(s: &str, max_bytes: usize) -> &str {
	if s.len() <= max_bytes {
		return s;
	}
	let mut end = max_bytes;
	while !s.is_char_boundary(end) {
		end -= 1;
	}
	&s[..end]
}

#[cfg(test)]
mod tests {
	use super::*;

	fn allocate(taken: &[&str], name: &str, is_dir: bool) -> String {
		let mut names = TakenNames::new(taken.iter().copied());
		names.allocate(name, is_dir).unwrap().into()
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
		// the key is exactly `to_lowercase`, which `hash_name` uses
		assert_eq!(collision_key("İstanbul"), "İstanbul".to_lowercase());
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
		let first: String = names.allocate("a.txt", false).unwrap().into();
		let second: String = names.allocate("a.txt", false).unwrap().into();
		let third: String = names.allocate("A.TXT", false).unwrap().into();
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
		let encoded: String = encode_name("a:b.txt").unwrap().into();
		assert_eq!(allocate(&[], "a:b.txt", false), encoded);
		let mut names = TakenNames::default();
		names.allocate("a:b.txt", false).unwrap();
		let second: String = names.allocate("a:b.txt", false).unwrap().into();
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
	fn empty_names_are_rejected() {
		assert!(TakenNames::default().allocate("", false).is_err());
	}
}
