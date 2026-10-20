//! Backslashes in names, matched alike on every platform.
//!
//! A backslash is an ordinary character in a Filen name: only `/` separates the components of a
//! root-relative path. globset disagrees off unix: `Candidate` rewrites every byte
//! `std::path::is_separator` accepts to `/` before matching, backslash included, so on Windows
//! `bad\x` reached the matcher as the two components `bad/x`. A rule naming the whole directory
//! (`bad*/`, or `bad\\x` written to match the backslash) then missed it, and a rule naming only
//! `x` hid it.
//!
//! So the matcher never sees a backslash on either side. The path has each one replaced with
//! [`STAND_IN`]; the pattern has each glob token that matches a literal backslash (an escaped
//! `\\`, a class naming or spanning it) changed to match the stand-in instead. Both are done on
//! every platform, so all of them match the same way. One line reads differently than it did on
//! any platform: `dir\\/`, refused before because `ignore` took its second backslash for the
//! escape of a `/`, now hides a directory named `dir\`, as in git.

use std::{borrow::Cow, str::Chars};

use super::collision_key;

/// The character a backslash in a path is matched as. It is an ASCII capital, which no
/// [`collision_key`] holds (both a path and a pattern pass through one first, and no
/// `char::to_lowercase` yields an ASCII capital), so no character of a real name or pattern can be
/// taken for a backslash. And it is one byte, as the backslash is: globset matches bytes, so `?`
/// and a class each take one.
pub(super) const STAND_IN: char = 'B';
const STAND_IN_STR: &str = "B";
const BELOW_STAND_IN: char = 'A';
const ABOVE_STAND_IN: char = 'C';

/// The folded form of `rel_path` the matcher sees.
pub(super) fn path_key(rel_path: &str) -> String {
	let key = collision_key(rel_path);
	if key.contains('\\') {
		key.replace('\\', STAND_IN_STR)
	} else {
		key
	}
}

/// `line`, already folded with [`collision_key`], rewritten so that each glob token matching a
/// backslash matches [`STAND_IN`] instead. Everything else is left as written, including what
/// `ignore` reads off the line before globbing it (a comment, a leading `!` or `/`, a trailing `/`,
/// an escaped trailing space).
pub(super) fn pattern_line(line: &str) -> Cow<'_, str> {
	// A class may span the stand-in without naming a backslash, so `[` is a reason to look too.
	if line.starts_with('#') || !line.contains(['\\', '[']) {
		return Cow::Borrowed(line);
	}
	// `ignore` keeps trailing whitespace only after a `\ `, and trims it otherwise.
	let keeps_space = line.ends_with("\\ ");
	let body = if keeps_space { line } else { line.trim_end() };
	let mut out = String::with_capacity(body.len());
	let mut chars = body.chars();
	// globset stops reading classes after an unclosed `[`: from there on `[` is a literal.
	let mut reads_classes = true;
	while let Some(c) = chars.next() {
		match c {
			'\\' => match chars.next() {
				Some('\\') => out.push(STAND_IN),
				Some(escaped) => {
					out.push('\\');
					out.push(escaped);
				}
				// A dangling escape: globset refuses the line, as it did before.
				None => out.push('\\'),
			},
			'[' if reads_classes => {
				let text = chars.as_str();
				match Class::parse(&mut chars) {
					Some(class) => {
						let consumed = text.len() - chars.as_str().len();
						let (written, _) = text.split_at(consumed);
						out.push('[');
						match class.rewritten() {
							Some(rewritten) => out.push_str(&rewritten),
							None => out.push_str(written),
						}
					}
					None => {
						reads_classes = false;
						chars = text.chars();
						out.push('[');
					}
				}
			}
			c => out.push(c),
		}
	}
	// A space kept by `\\ ` (an escaped backslash, then the space) now follows the stand-in, and
	// `ignore` would trim it: escape it, which globs to the same literal space.
	if keeps_space && !out.ends_with("\\ ") && out.ends_with(' ') {
		out.pop();
		out.push_str("\\ ");
	}
	Cow::Owned(out)
}

/// A bracket class, read the way globset reads it.
struct Class {
	negated: bool,
	/// Inclusive ranges, a single character as a range of one.
	ranges: Vec<(char, char)>,
}

impl Class {
	/// Reads the class after its `[`, through the closing `]`. `None` when it is never closed.
	fn parse(chars: &mut Chars<'_>) -> Option<Self> {
		let negated = matches!(chars.clone().next(), Some('!' | '^'));
		if negated {
			chars.next();
		}
		let mut ranges: Vec<(char, char)> = Vec::new();
		let mut first = true;
		let mut in_range = false;
		loop {
			let c = chars.next()?;
			if c == ']' && !first {
				break;
			}
			if c == '-' && !first && !in_range {
				in_range = true;
			} else if let Some(last) = ranges.last_mut().filter(|_| in_range) {
				// `in_range` is only set after a first range.
				last.1 = c;
				in_range = false;
			} else {
				ranges.push((c, c));
			}
			first = false;
		}
		// A trailing `-` is a literal one.
		if in_range {
			ranges.push(('-', '-'));
		}
		Some(Self { negated, ranges })
	}

	fn covers(&self, c: char) -> bool {
		self.ranges.iter().any(|&(low, high)| low <= c && c <= high)
	}

	/// The class after its `[`, matching the stand-in exactly when it matches a backslash, or `None`
	/// when it already does (or is a reversed range globset refuses), so it is kept as written.
	fn rewritten(mut self) -> Option<String> {
		if self.ranges.iter().any(|&(low, high)| high < low) {
			return None;
		}
		match (self.covers('\\'), self.covers(STAND_IN)) {
			(true, false) => self.ranges.push((STAND_IN, STAND_IN)),
			(false, true) => take_out(&mut self.ranges, STAND_IN, BELOW_STAND_IN, ABOVE_STAND_IN),
			(true, true) | (false, false) => return None,
		}
		self.written()
	}

	/// The class written back so globset reads exactly these ranges: `]` and `-` are literals only
	/// first (or `-` last), and a leading `!` or `^` would negate it. `None` when no order works,
	/// which takes a class of nothing but `!` and `^`, one this rewrite never builds.
	fn written(mut self) -> Option<String> {
		let bracket = self.covers(']');
		let dash = self.covers('-');
		take_out(&mut self.ranges, ']', '\\', '^');
		take_out(&mut self.ranges, '-', ',', '.');
		if !self.negated && !bracket && !dash {
			let starts_plain = |&(low, _): &(char, char)| !matches!(low, '!' | '^');
			match self.ranges.iter().position(starts_plain) {
				Some(i) => self.ranges.swap(0, i),
				None => {
					// Every range starts with `!` or `^`: lead with the rest of a longer one.
					let longer = self.ranges.iter_mut().find(|(low, high)| low < high)?;
					let (low, high) = *longer;
					*longer = (low, low);
					let next = if low == '!' { '"' } else { '_' };
					self.ranges.insert(0, (next, high));
				}
			}
		}
		let mut out = String::new();
		if self.negated {
			out.push('!');
		}
		if bracket {
			out.push(']');
		} else if dash {
			out.push('-');
		}
		for (low, high) in self.ranges {
			out.push(low);
			if low < high {
				out.push('-');
				out.push(high);
			}
		}
		if bracket && dash {
			out.push('-');
		}
		out.push(']');
		Some(out)
	}
}

/// Removes `c` from every range in `ranges`; `below` and `above` are the characters next to it.
fn take_out(ranges: &mut Vec<(char, char)>, c: char, below: char, above: char) {
	let mut kept = Vec::with_capacity(ranges.len() + 1);
	for &(low, high) in ranges.iter() {
		if c < low || high < c {
			kept.push((low, high));
			continue;
		}
		if low < c {
			kept.push((low, below));
		}
		if c < high {
			kept.push((above, high));
		}
	}
	*ranges = kept;
}
