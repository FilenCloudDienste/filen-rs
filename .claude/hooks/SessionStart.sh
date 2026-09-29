#!/bin/sh
# SessionStart hook: install the repository's git hooks and report the tools
# they need that are missing, so a session finds out now rather than at its
# first commit. Whatever it prints is shown to the session; it never fails it.

# Found from this script, not the cwd: the hook also runs on resume and
# compaction, when the session may be working in another directory.
root=$(git -C "$(dirname "$0")" rev-parse --show-toplevel 2>/dev/null) || exit 0

hooks_path=$(git -C "$root" config core.hooksPath)
if [ -z "$hooks_path" ]; then
	bash "$root/scripts/git-hooks/install.sh" >/dev/null 2>&1 ||
		echo "Could not install the git hooks; run ./scripts/git-hooks/install.sh (required, see CLAUDE.md)."
elif [ "$hooks_path" != "scripts/git-hooks" ]; then
	echo "core.hooksPath is '$hooks_path', not scripts/git-hooks: this repository's required hooks are not running (see CLAUDE.md, Git Hooks)."
fi

missing=
command -v taplo >/dev/null 2>&1 || missing="$missing taplo (cargo install taplo-cli --version 0.10.0 --locked);"
command -v sqlfluff >/dev/null 2>&1 || missing="$missing sqlfluff, for staged .sql files (pip install sqlfluff);"
# From the root, so rustup reads the toolchain rust-toolchain.toml pins.
(cd "$root" && rustup target list --installed 2>/dev/null) | grep -q '^wasm32-unknown-unknown$' ||
	missing="$missing the wasm32 target (rustup target add wasm32-unknown-unknown);"
if [ "$(uname -s)" = "Darwin" ] && [ ! -x /opt/homebrew/opt/llvm/bin/clang ]; then
	missing="$missing brew LLVM (brew install llvm);"
fi
if [ -n "$missing" ]; then
	echo "The pre-commit hook will stop on missing tools:$missing"
fi

exit 0
