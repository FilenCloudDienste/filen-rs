#!/usr/bin/env bash
# Point git at the in-repo hooks directory.
# Run once per clone (or worktree):
#
#   ./scripts/git-hooks/install.sh
#
# Uninstall with: git config --unset core.hooksPath
set -eu

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(git -C "$script_dir" rev-parse --show-toplevel)
hooks_rel=$(realpath --relative-to="$repo_root" "$script_dir" 2>/dev/null \
	|| python3 -c "import os,sys; print(os.path.relpath(sys.argv[1], sys.argv[2]))" \
		"$script_dir" "$repo_root")

cd "$repo_root"
chmod +x "$script_dir"/pre-commit "$script_dir"/pre-push
git config core.hooksPath "$hooks_rel"

printf 'Installed: git core.hooksPath -> %s\n' "$hooks_rel"
printf 'Hooks active: pre-commit, pre-push\n'
printf 'Bypass with --no-verify when needed.\n'
