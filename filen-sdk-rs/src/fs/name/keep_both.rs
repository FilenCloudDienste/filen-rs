//! "Keep both" naming: an item written into a directory gets a name that is free there,
//! `name (1).ext` style, instead of replacing (or, on the server, versioning) what is already
//! there. Used wherever the SDK creates items from other items: copies, compressed archives and
//! extracted entries.

use std::borrow::Cow;

use super::{EntryNameError, EntryNameErrorKind, MAX_BYTES, ValidatedName, encode_name};
use crate::util::{SeededMap, SeededSet};

/// The key two names collide on. The server compares names through `hash_name`, which
/// lowercases with [`str::to_lowercase`], so this must use exactly the same folding.
pub(crate) fn collision_key(name: &str) -> String {
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

/// Which part of a name keep-both numbers, and which it keeps apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NameShape {
	/// A directory: the whole name is numbered (`docs` → `docs (1)`).
	Dir,
	/// A file: its last extension is kept apart (`a.txt` → `a (1).txt`).
	File,
	/// A file whose last `len` bytes are one extension, kept apart whole (`a.tar.gz` →
	/// `a (1).tar.gz`, where [`NameShape::File`] would give `a.tar (1).gz`).
	#[cfg(any(
		not(all(target_family = "wasm", target_os = "unknown")),
		feature = "wasm-full"
	))]
	FileWithExtension { len: usize },
}

/// The names taken in one destination directory, compared case-insensitively.
#[derive(Debug, Default)]
pub(crate) struct TakenNames {
	keys: SeededSet<String>,
	/// Per [`CounterKey`], a counter below which every candidate of that suffix length is known
	/// to be taken, so the `k`-th duplicate of one name (or of its case variants) does not retry
	/// the `k - 1` before it. Names are only ever added, so a candidate once seen taken stays
	/// taken.
	next_counter: SeededMap<CounterKey, u64>,
	/// Numbered candidates built so far, the work an allocation costs.
	#[cfg(test)]
	candidates_built: u64,
}

/// What a numbered candidate's hint is kept under: the collision keys of the base and extension
/// it is built from, as trimmed to fit, and the length of its ` (n)` suffix, which decides how
/// much is trimmed. Names with one key get colliding candidates at every counter of that length
/// (the suffix separates the parts with a space and a parenthesis, so neither lowercases
/// differently next to it). Keying on the untrimmed name instead would let two case variants
/// share a hint although a lowercase that changes a name's length in bytes trims them at
/// different characters, so their candidates need not collide.
type CounterKey = (String, String, usize);

impl TakenNames {
	pub(crate) fn new<'a>(names: impl IntoIterator<Item = &'a str>) -> Self {
		Self {
			keys: names.into_iter().map(collision_key).collect(),
			..Self::default()
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
	/// of the stem, so it gets a counter of its own. What is kept apart from the counter depends
	/// on the name's [`NameShape`]. Candidates are trimmed at a character boundary to fit the
	/// name length limit.
	pub(crate) fn allocate(
		&mut self,
		name: ValidatedName,
		shape: NameShape,
	) -> Result<ValidatedName, EntryNameError> {
		if self.insert(name.as_ref()) {
			return Ok(name);
		}

		let (stem, ext) = split_extension(name.as_ref(), shape);
		let (mut base, mut n) = strip_counter(stem)
			.and_then(|(base, n)| Some((base, n.checked_add(1)?)))
			.unwrap_or((stem, 1));
		// the hint may only be raised when every counter of this length below `n` is known taken
		let mut contiguous = first_of_its_length(n);
		loop {
			let (candidate, key) = numbered_candidate(base, n, ext)?;
			#[cfg(test)]
			{
				self.candidates_built += 1;
			}
			if let Some(known_taken_below) = key.as_ref().and_then(|key| self.next_counter.get(key))
				&& *known_taken_below >= n
			{
				contiguous = true;
				if *known_taken_below > n {
					n = *known_taken_below;
					continue;
				}
			}
			if self.insert(candidate.as_ref()) {
				if contiguous
					&& let Some(key) = key
					&& let Some(next) = n.checked_add(1)
				{
					self.next_counter.insert(key, next);
				}
				return Ok(candidate);
			}
			// Every iteration either returns, skips a taken name, or jumps forward, and only
			// finitely many names are taken, so counting up terminates. Only a continued counter
			// can run out of numbers; the whole stem then starts over at 1.
			(base, n) = match n.checked_add(1) {
				Some(next) => {
					contiguous |= first_of_its_length(next);
					(base, next)
				}
				None => {
					contiguous = true;
					(stem, 1)
				}
			};
		}
	}
}

/// Whether `n` is the smallest counter of its number of digits.
fn first_of_its_length(n: u64) -> bool {
	n.checked_ilog10()
		.is_some_and(|digits| 10u64.pow(digits) == n)
}

/// `(stem, ext)` with `ext` including its dot. A leading dot (`.bashrc`) is part of the stem,
/// not an extension.
fn split_extension(name: &str, shape: NameShape) -> (&str, &str) {
	match shape {
		NameShape::Dir => (name, ""),
		#[cfg(any(
			not(all(target_family = "wasm", target_os = "unknown")),
			feature = "wasm-full"
		))]
		NameShape::FileWithExtension { len }
			if len < name.len() && name.is_char_boundary(name.len() - len) =>
		{
			name.split_at(name.len() - len)
		}
		_ => match name.rfind('.') {
			Some(dot) if dot > 0 => name.split_at(dot),
			_ => (name, ""),
		},
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
/// boundary so the result fits [`MAX_BYTES`]; and the key of its hint, which a candidate that had
/// to be encoded has none of, since it is not built from its parts.
fn numbered_candidate(
	base: &str,
	n: u64,
	ext: &str,
) -> Result<(ValidatedName, Option<CounterKey>), EntryNameError> {
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
			Err(_) => return Ok((encode_name(&candidate)?, None)),
			Ok(name) => {
				let key = (collision_key(trimmed), collision_key(ext), suffix.len());
				return Ok((name, Some(key)));
			}
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
		let shape = if is_dir {
			NameShape::Dir
		} else {
			NameShape::File
		};
		let mut names = TakenNames::new(taken.iter().copied());
		names.allocate(source_name(name), shape).unwrap().into()
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
	fn a_stem_that_ran_out_of_counters_keeps_counting_where_it_left_off() {
		let below_max = format!("a ({}).txt", u64::MAX - 1);
		let max = format!("a ({}).txt", u64::MAX);
		let numbered = format!("a ({}) (1).txt", u64::MAX - 1);
		let mut names =
			TakenNames::new(["a.txt", below_max.as_str(), max.as_str(), numbered.as_str()]);
		let allocated = [below_max.as_str(), below_max.as_str(), "a.txt"]
			.map(|name| String::from(names.allocate(source_name(name), NameShape::File).unwrap()));
		assert_eq!(
			allocated,
			[
				format!("a ({}) (2).txt", u64::MAX - 1),
				format!("a ({}) (3).txt", u64::MAX - 1),
				// the stem's counters are its own: the plain name's first one is still free
				"a (1).txt".to_owned(),
			]
		);
	}

	#[test]
	fn a_case_variant_trimmed_elsewhere_still_gets_the_smallest_free_counter() {
		// `İ` lowercases to `i` and a combining dot, so these two names collide although one is
		// 255 bytes and the other 170: numbered, only the first is trimmed, and the two `(1)`s
		// no longer collide
		let decomposed = "i\u{307}".repeat(85);
		let composed = "\u{130}".repeat(85);
		assert_eq!(collision_key(&composed), decomposed);
		let mut names = TakenNames::new([decomposed.as_str()]);
		let allocated = [&decomposed, &composed]
			.map(|name| String::from(names.allocate(source_name(name), NameShape::Dir).unwrap()));
		assert_eq!(
			allocated,
			[
				format!("{}i (1)", "i\u{307}".repeat(83)),
				format!("{composed} (1)"),
			]
		);
	}

	#[test]
	fn a_known_compound_extension_is_kept_apart_whole() {
		let mut names = TakenNames::new(["photos.tar.gz"]);
		let shape = NameShape::FileWithExtension {
			len: ".tar.gz".len(),
		};
		let name: String = names
			.allocate(source_name("photos.tar.gz"), shape)
			.unwrap()
			.into();
		assert_eq!(name, "photos (1).tar.gz");
		// an extension as long as the whole name falls back to the last one
		let mut names = TakenNames::new(["x.gz"]);
		let name: String = names
			.allocate(source_name("x.gz"), NameShape::FileWithExtension { len: 4 })
			.unwrap()
			.into();
		assert_eq!(name, "x (1).gz");
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
		let first: String = names
			.allocate(source_name("a.txt"), NameShape::File)
			.unwrap()
			.into();
		let second: String = names
			.allocate(source_name("a.txt"), NameShape::File)
			.unwrap()
			.into();
		let third: String = names
			.allocate(source_name("A.TXT"), NameShape::File)
			.unwrap()
			.into();
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
		names
			.allocate(source_name("a:b.txt"), NameShape::File)
			.unwrap();
		let second: String = names
			.allocate(source_name("a:b.txt"), NameShape::File)
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
		const DUPLICATES: u64 = 20_000;
		// hints are kept per counter length, so a duplicate builds one candidate for each length
		// (1 to 5 digits here) on its way to the free one
		const MOST_BUILT_EACH: u64 = DUPLICATES.ilog10() as u64 + 2;
		let mut names = TakenNames::default();
		let mut last = String::new();
		for i in 0..DUPLICATES {
			let spelling = if i % 2 == 0 {
				"report.pdf"
			} else {
				"REPORT.pdf"
			};
			last = names
				.allocate(source_name(spelling), NameShape::File)
				.unwrap()
				.into();
		}
		assert_eq!(last, "REPORT (19999).pdf");
		assert!(names.candidates_built <= MOST_BUILT_EACH * DUPLICATES);
		// and so must one long enough that every candidate is trimmed
		names.candidates_built = 0;
		let long = "x".repeat(250);
		let long_upper = long.to_uppercase();
		for i in 0..DUPLICATES {
			let spelling = if i % 2 == 0 { &long } else { &long_upper };
			last = names
				.allocate(source_name(&format!("{spelling}.pdf")), NameShape::File)
				.unwrap()
				.into();
		}
		assert_eq!(last, format!("{} (19999).pdf", &long_upper[..243]));
		assert!(names.candidates_built <= MOST_BUILT_EACH * DUPLICATES);
		// the hint never skips a free counter, even one below a counter taken out of order
		let mut names = TakenNames::new(["a.txt", "a (2).txt"]);
		let first: String = names
			.allocate(source_name("a.txt"), NameShape::File)
			.unwrap()
			.into();
		let second: String = names
			.allocate(source_name("a.txt"), NameShape::File)
			.unwrap()
			.into();
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
