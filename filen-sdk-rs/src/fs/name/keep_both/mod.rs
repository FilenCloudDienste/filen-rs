//! "Keep both" naming: an item written into a directory gets a name that is free there,
//! `name (1).ext` style, instead of replacing (or, on the server, versioning) what is already
//! there. Used wherever the SDK creates items from other items: copies, compressed archives and
//! extracted entries.

use std::{borrow::Cow, collections::BTreeMap};

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
	#[cfg(feature = "archive")]
	FileWithExtension { len: usize },
}

/// The names taken in one destination directory, compared case-insensitively.
#[derive(Debug, Default)]
pub(crate) struct TakenNames {
	keys: SeededSet<String>,
	/// Per [`CounterKey`], the runs of counters whose candidates are all known to be taken, as
	/// `first -> end` (exclusive), disjoint and never touching. The `k`-th duplicate of one name
	/// (or of its case variants) jumps over a run instead of retrying the `k - 1` counters before
	/// it, wherever its walk starts: a name that already ends in ` (n)` starts partway through a
	/// counter length. Names are only ever added, so a candidate once seen taken stays taken, and
	/// a run never covers a free one.
	taken_runs: SeededMap<CounterKey, BTreeMap<u64, u64>>,
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
		self.allocate_counting(name, shape, &mut 0)
	}

	/// [`allocate`](Self::allocate), adding the numbered candidates it builds, the work it costs,
	/// to `built`.
	fn allocate_counting(
		&mut self,
		name: ValidatedName,
		shape: NameShape,
		built: &mut u64,
	) -> Result<ValidatedName, EntryNameError> {
		if self.insert(name.as_ref()) {
			return Ok(name);
		}

		let (stem, ext) = split_extension(name.as_ref(), shape);
		let (mut base, mut n) = strip_counter(stem)
			.and_then(|(base, n)| Some((base, n.checked_add(1)?)))
			.unwrap_or((stem, 1));
		// every counter from `from` below `n` was seen taken, under the key `walked`
		let (mut from, mut walked) = (n, None);
		loop {
			let (candidate, key) = numbered_candidate(base, n, ext)?;
			*built += 1;
			if key != walked {
				// a new counter length (or base): what the walk found under the last is kept
				if let Some(walked) = walked.take()
					&& from < n
				{
					add_run(self.taken_runs.entry(walked).or_default(), from, n);
				}
				// a copy: `key` is still looked up and its run recorded below
				(from, walked) = (n, key.clone());
			}
			if let Some(end) = key
				.as_ref()
				.and_then(|key| self.taken_runs.get(key))
				.and_then(|runs| runs.range(..=n).next_back())
				.map(|(_, &end)| end)
				.filter(|&end| end > n)
			{
				n = end;
				continue;
			}
			if self.insert(candidate.as_ref()) {
				if let Some(key) = key
					&& let Some(end) = n.checked_add(1)
				{
					add_run(self.taken_runs.entry(key).or_default(), from, end);
				}
				return Ok(candidate);
			}
			// Every iteration either returns, skips a taken name, or jumps past a run, and only
			// finitely many names are taken, so counting up terminates. Only a continued counter
			// can run out of numbers; the whole stem then starts over at 1.
			(base, n) = match n.checked_add(1) {
				Some(next) => (base, next),
				None => (stem, 1),
			};
		}
	}
}

/// Records that the candidates of every counter in `first..end` are taken, merged with the runs
/// it overlaps or touches, so a lookup finds the whole run from any counter inside it.
fn add_run(runs: &mut BTreeMap<u64, u64>, mut first: u64, mut end: u64) {
	if let Some((&before, &before_end)) = runs.range(..first).next_back()
		&& before_end >= first
	{
		first = before;
		end = end.max(before_end);
	}
	while let Some((&start, &run_end)) = runs.range(first..=end).next() {
		runs.remove(&start);
		end = end.max(run_end);
	}
	runs.insert(first, end);
}

/// `(stem, ext)` with `ext` including its dot. A leading dot (`.bashrc`) is part of the stem,
/// not an extension.
fn split_extension(name: &str, shape: NameShape) -> (&str, &str) {
	match shape {
		NameShape::Dir => (name, ""),
		#[cfg(feature = "archive")]
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
	let (base, digits) = inner.rsplit_once(" (")?;
	if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
		return None;
	}
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
	// The base keeps at least its first character: an extension that leaves no room for it is
	// folded into the base, so trimming eats into the extension instead. A candidate starting
	// with the suffix's space would have to be encoded, which lengthens it past the limit.
	let (base, ext, mut budget) = match MAX_BYTES
		.checked_sub(suffix.len() + ext.len())
		.filter(|&budget| base.floor_char_boundary(budget) > 0)
	{
		Some(budget) => (Cow::Borrowed(base), ext, budget),
		None => (
			Cow::Owned(format!("{base}{ext}")),
			"",
			MAX_BYTES - suffix.len(),
		),
	};
	loop {
		// floor_char_boundary always returns a char boundary.
		#[allow(clippy::string_slice)]
		let trimmed = &base[..base.floor_char_boundary(budget)];
		let candidate = format!("{trimmed}{suffix}{ext}");
		let too_long = match ValidatedName::try_from(candidate.as_str()) {
			Ok(name) => {
				let key = (collision_key(trimmed), collision_key(ext), suffix.len());
				return Ok((name, Some(key)));
			}
			Err(
				error @ EntryNameError {
					kind: EntryNameErrorKind::TooLong { .. },
					..
				},
			) => error,
			Err(_) => match encode_name(&candidate) {
				Ok(name) => return Ok((name, None)),
				Err(
					error @ EntryNameError {
						kind: EntryNameErrorKind::TooLong { .. },
						..
					},
				) => error,
				Err(error) => return Err(error),
			},
		};
		// NFC normalization or encoding lengthened it: trim further, keeping the first character
		budget = trimmed.len() - 1;
		if base.floor_char_boundary(budget) == 0 {
			return Err(too_long);
		}
	}
}

#[cfg(test)]
mod tests;
