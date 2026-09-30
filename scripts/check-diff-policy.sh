#!/usr/bin/env bash
# Blocks changes that need the maintainer's approval before they land (see CLAUDE.md,
# Contributing): [patch]/[replace]/[source] sections, vendored sources, git dependencies
# not pinned to a rev or not under the Enduriel or FilenCloudDienste GitHub accounts,
# zeroize, unsafe code, an allow of clippy::cast_possible_truncation, and binary files that
# no generate.sh or README.md next to them names. Only the diff is checked, so what is
# already on main never trips it.
#
#   scripts/check-diff-policy.sh --cached              staged changes (pre-commit)
#   scripts/check-diff-policy.sh origin/main...HEAD    a whole branch (CI, house-review)
#
# A change the maintainer has approved is committed with SKIP_DIFF_POLICY=1, which only
# the pre-commit hook reads: Branch Lint still reports it, in its last step, so the PR
# shows what was approved.

set -u

range=${1:---cached}
case $range in
--cached)
	# Concluding a conflicted merge stages everything the other side changed. That was
	# checked where it was committed, and CI checks the merged branch against its target.
	git rev-parse -q --verify MERGE_HEAD >/dev/null && exit 0
	old=HEAD new= # an empty rev before ":path" names the index
	;;
*...*) old=$(git merge-base "${range%%...*}" "${range##*...}") new=${range##*...} ;;
*..*) old=${range%%..*} new=${range##*..} ;;
*)
	printf 'usage: %s [--cached | <base>...<tip> | <base>..<tip>]\n' "$0" >&2
	exit 2
	;;
esac

status=0
block() {
	printf 'diff-policy: %s\n' "$1" >&2
	status=1
}
# The first lines of a list, and how many more there are.
first() {
	head -n 20 <<<"$1"
	awk 'NR > 20 { n++ } END { if (n) printf "... and %d more\n", n }' <<<"$1"
}
# Added (or changed) lines of the diff, limited to the given pathspecs. -M keeps a moved
# file from counting as all-new lines.
added() {
	git diff -M -U0 "$range" -- "$@" | grep -E '^\+' | grep -vE '^\+\+\+ '
}

added '*Cargo.toml' '*.cargo/config.toml' '*.cargo/config' | grep -qE '^\+[[:space:]]*(\[(patch|replace|source)|replace-with)' &&
	block 'a [patch], [replace] or [source] section needs maintainer approval; depend on a maintained fork by git rev instead.'

# Every crate inherits the workspace lints: [lints] workspace = true, or the dotted
# lints.workspace = true before the first table.
nolints=
while read -r path; do
	git show "$new:$path" 2>/dev/null | awk '
		{ s = $0; gsub(/[[:space:]]/, "", s) }
		s ~ /^\[/ { t = s; sub(/^\[+/, "", t); sub(/\].*/, "", t) }
		t == "package" { p = 1 }
		t == "lints" && s ~ /^workspace=true/ || t == "" && s ~ /^lints(\.|=\{)workspace=true/ { l = 1 }
		END { exit !(p && !l) }
	' && nolints="$nolints$path"$'\n'
done < <(git diff --no-renames "$range" --name-only --diff-filter=AM -- '*Cargo.toml')
[ -n "$nolints" ] &&
	block "a crate must inherit the workspace lints; add [lints] workspace = true to its Cargo.toml:
$(first "${nolints%$'\n'}")"

git diff -M "$range" --name-only --diff-filter=A | grep -qE '(^|/)vendor/' &&
	block 'vendored sources (vendor/) need maintainer approval.'

# Added manifest lines with a git key, quoted or not; a string may be in either quotes.
gitdeps=$(added '*Cargo.toml' | grep -E "(^|[^[:alnum:]_-])git[\"']?[[:space:]]*=")

# A rev after a # is in a comment, and pins nothing.
unpinned=$(grep -vE "^[^#]*(^|[^[:alnum:]_-])rev[[:space:]]*=[[:space:]]*[\"'][0-9a-f]{40}[\"']" <<<"$gitdeps")
[ -n "$unpinned" ] &&
	block "a git dependency must be pinned in one inline table, { git = \"...\", rev = \"<full commit hash>\" }, not to a branch or tag:
$(first "$unpinned")"

# The pinned commit has to stay fetchable, so it lives in a fork the maintainer controls.
# Every git key on the line must hold such a URL: one in a comment vouches for nothing.
foreign=$(awk '{
	l = tolower($0)
	gsub(/(^|[^[:alnum:]_-])git["\047]?[[:space:]]*=[[:space:]]*["\047](https:\/\/github\.com\/|ssh:\/\/git@github\.com\/|git@github\.com:)(enduriel|filenclouddienste)\/[[:alnum:]_.-]+\/?["\047]/, "", l)
} l ~ /(^|[^[:alnum:]_-])git["\047]?[[:space:]]*=/' <<<"$gitdeps")
[ -n "$foreign" ] &&
	block "a git dependency from another account needs maintainer approval; fork it under github.com/Enduriel or github.com/FilenCloudDienste and pin its rev there:
$(first "$foreign")"

added '*.rs' '*.toml' | grep -qi 'zeroize' &&
	block 'zeroize: wiping secrets from memory was rejected by the maintainer; give secret types a redacting Debug instead.'

# Line comments are cut first: a comment may name an unsafe fn without adding one.
unsafe_lines=$(added '*.rs' | sed -E 's#(^\+|[[:space:]])//.*#\1#' |
	grep -E '^\+(.*[^[:alnum:]_])?unsafe[[:space:]]*(\{|\(|(fn|impl|extern|trait)([^[:alnum:]_]|$))')
[ -n "$unsafe_lines" ] &&
	block "added or changed unsafe code needs maintainer approval:
$(first "$unsafe_lines")"

# clippy::cast_possible_truncation has no exceptions (see the workspace lints in Cargo.toml),
# and an allow of clippy::pedantic would switch it off too. Any line naming either is caught,
# so an attribute list spread over several lines cannot slip past.
cast_allows=$(added '*.rs' | sed -E 's#(^\+|[[:space:]])//.*#\1#' |
	grep -E 'cast_possible_truncation|clippy::pedantic')
[ -n "$cast_allows" ] &&
	block "clippy::cast_possible_truncation is never allowed, nor clippy::pedantic around it; rewrite the cast with T::try_from, or num_traits::ToPrimitive from a float:
$(first "$cast_allows")"

# A binary file, new or changed, is allowed where a generate.sh or README.md in its own
# directory names it, saying how it is made or where it comes from. Without rename
# detection, a moved binary is checked as new at its new path.
binaries=
while IFS=$'\t' read -r adds _dels path; do
	[ "$adds" = - ] || continue
	dir=$(dirname "$path")/
	dir=${dir#./} # a root-level file's docs are README.md, not ./README.md
	for doc in generate.sh README.md; do
		git show "$new:$dir$doc" 2>/dev/null
	done | grep -qF "$(basename "$path")" && continue
	binaries="$binaries$path"$'\n'
done < <(git diff --no-renames "$range" --numstat --diff-filter=AM)
[ -n "$binaries" ] &&
	block "binary files need maintainer approval; generate them in the test, or name each one in a generate.sh, or a README.md with its source URL, next to it:
$(first "${binaries%$'\n'}")"

# New crates, and new majors of existing ones, are not blocked (a version bump of an
# existing dependency can pull some in), but each one is a dependency the PR has to
# justify. A version is keyed by its semver-compatible part: 1.2.3 is 1, 0.2.3 is 0.2 and
# 0.0.3 is 0.0.3.
lock_majors() {
	git show "$1:Cargo.lock" 2>/dev/null | awk -F'"' '
		/^name = "/ { n = $2 }
		/^version = "/ {
			sub(/[-+].*/, "", $2)
			split($2, v, ".")
			if (v[1] + 0) print n, v[1]
			else if (v[2] + 0) print n, "0." v[2]
			else print n, "0.0." v[3]
		}
	' | sort -u
}
new_crates=$(comm -13 <(lock_majors "$old") <(lock_majors "$new"))
[ -n "$new_crates" ] &&
	printf 'diff-policy: new packages or major versions in Cargo.lock; name the reason for each in the PR:\n%s\n' "$new_crates" >&2

# Lint suppressions are not blocked either, but each needs a comment naming its tradeoff,
# and the reviewer reads every one, so all of them are listed.
allows=$(git diff -M -U0 --no-prefix "$range" -- '*.rs' | awk '
	/^\+\+\+ / { f = substr($0, 5); next }
	sub(/^\+[[:space:]]*/, "") && /^(#!?\[(.*[^[:alnum:]_])?)?(allow|expect)[[:space:]]*\(/ { print f ": " $0 }
')
[ -n "$allows" ] &&
	printf 'diff-policy: added #[allow]/#[expect] lint suppressions; each needs a comment naming its tradeoff, and the reviewer reads every one:\n%s\n' "$allows" >&2

exit "$status"
