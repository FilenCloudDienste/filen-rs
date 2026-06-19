#!/bin/bash
# Prepare a filen-js release: bump the version (major|minor|patch) across the four files a release
# touches, commit "chore: release filen-js@X.Y.Z" on main, and create the annotated tag
# filen-js@X.Y.Z.
#
# It does NOT push. Pushing the tag is what triggers the npm-publish workflow, so that is left to
# you:
#     git push origin main && git push origin filen-js@X.Y.Z
#
# Usage:
#     scripts/release.sh <major|minor|patch> [--dry-run]
set -euo pipefail

# This script lives in scripts/, so the repo root is one level up.
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

CARGO_TOML="filen-sdk-rs/Cargo.toml"
WEB_DIR="filen-sdk-rs/web"

usage() {
	echo "usage: $0 <major|minor|patch> [--dry-run]" >&2
	exit 2
}

BUMP="${1:-}"
DRY_RUN="${2:-}"
case "$BUMP" in
	major | minor | patch) ;;
	*) usage ;;
esac
if [ -n "$DRY_RUN" ] && [ "$DRY_RUN" != "--dry-run" ]; then
	usage
fi

# --- preconditions ---
command -v npm >/dev/null || {
	echo "error: npm is required (it bumps web/package.json + package-lock.json)" >&2
	exit 1
}

BRANCH="$(git rev-parse --abbrev-ref HEAD)"
if [ "$BRANCH" != "main" ]; then
	echo "error: releases are cut from main, but you are on '$BRANCH'" >&2
	exit 1
fi

# Untracked files are fine (they won't be committed); reject uncommitted changes to tracked files.
if ! git diff --quiet || ! git diff --cached --quiet; then
	echo "error: working tree has uncommitted changes to tracked files — commit or stash first" >&2
	exit 1
fi

# --- current version (the package's own version line in Cargo.toml) ---
OLD="$(grep -m1 '^version = ' "$CARGO_TOML" | sed -E 's/^version = "([^"]+)"/\1/')"
if ! [[ "$OLD" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
	echo "error: could not parse a X.Y.Z version from $CARGO_TOML (got '$OLD')" >&2
	exit 1
fi

IFS='.' read -r MAJOR MINOR PATCH <<<"$OLD"
case "$BUMP" in
	major)
		MAJOR=$((MAJOR + 1))
		MINOR=0
		PATCH=0
		;;
	minor)
		MINOR=$((MINOR + 1))
		PATCH=0
		;;
	patch)
		PATCH=$((PATCH + 1))
		;;
esac
NEW="$MAJOR.$MINOR.$PATCH"
TAG="filen-js@$NEW"

echo "filen-js release: $OLD -> $NEW ($BUMP bump), tag $TAG"

if git rev-parse -q --verify "refs/tags/$TAG" >/dev/null; then
	echo "error: tag $TAG already exists" >&2
	exit 1
fi

if [ "$DRY_RUN" = "--dry-run" ]; then
	echo "(dry run) would bump $CARGO_TOML, Cargo.lock, $WEB_DIR/package.json and"
	echo "(dry run) $WEB_DIR/package-lock.json, commit 'chore: release $TAG', and tag $TAG (no push)."
	exit 0
fi

# --- bump versions ---
# Cargo.toml: the package version is the only line that starts with `version = `.
OLD_ESC="${OLD//./\\.}"
tmp="$(mktemp)"
sed "s/^version = \"$OLD_ESC\"/version = \"$NEW\"/" "$CARGO_TOML" >"$tmp"
mv "$tmp" "$CARGO_TOML"

# Cargo.lock: sync the workspace member's locked version to the new Cargo.toml.
cargo update -p filen-sdk-rs

# web/package.json + web/package-lock.json (both version fields) — npm keeps them in sync.
(cd "$WEB_DIR" && npm version "$NEW" --no-git-tag-version >/dev/null)

# --- commit + annotated tag (no push) ---
git add "$CARGO_TOML" Cargo.lock "$WEB_DIR/package.json" "$WEB_DIR/package-lock.json"
echo "staged:"
git --no-pager diff --cached --stat
git commit -m "chore: release $TAG"
git tag -a "$TAG" -m "$TAG"

echo
echo "Prepared $TAG locally (commit $(git rev-parse --short HEAD), annotated tag $TAG). Nothing pushed."
echo "To publish (the tag push triggers the npm-publish workflow):"
echo "    git push origin main && git push origin $TAG"
