#!/usr/bin/env bash
# Generates a random tree of folders and files for exercising the sync engine.
#
#   scripts/gen-sync-tree.sh [-n entries] [-s size] [-d depth] [-p percent] [-j jobs] [-S seed] OUT
#
#   -n  total entries, folders + files (default 1000000)
#   -s  upper bound on the summed file sizes: bytes or K/M/G/T, binary (default 10G)
#   -d  maximum folder nesting below OUT (default 12)
#   -p  percentage of the entries that are folders (default 10)
#   -j  parallel file writers (default: online CPUs)
#   -S  seed: the same seed gives the same tree and sizes (default: random)
#
# OUT must not exist or must be empty. File sizes follow a heavy-tailed (Lomax) distribution
# scaled so they sum to just under -s: mostly small files, a few large ones, some empty.
# Files up to 64 KiB are written with shell builtins from a random base64 pool, so most files
# cost no fork; larger ones are read from /dev/urandom.

# Run as `sh gen-sync-tree.sh`, dash would get this far without $RANDOM, [[ or arrays.
[ -n "${BASH_VERSION:-}" ] || exec bash "$0" "$@"

set -euo pipefail
export LC_ALL=C

die() {
	echo "gen-sync-tree: $*" >&2
	exit 1
}

# Sets variable $1 to $2 in base 10: bash arithmetic would read a leading zero as octal, awk
# as decimal. At most 18 digits, so the value cannot wrap 64-bit arithmetic. Both parsers
# assign instead of echoing: a die inside a nested $(...) would not stop the script.
uint() {
	[[ $2 =~ ^[0-9]{1,18}$ ]] || die "$3 must be a non-negative integer of at most 18 digits: $2"
	printf -v "$1" %d $((10#$2))
}

parse_size() {
	local digits=${2%%[!0-9]*} num mult
	uint num "$digits" -s
	case ${2#"$digits"} in
	'' | B) mult=1 ;;
	K | KiB | k) mult=1024 ;;
	M | MiB) mult=1048576 ;;
	G | GiB) mult=1073741824 ;;
	T | TiB) mult=1099511627776 ;;
	*) die "bad size unit: $2" ;;
	esac
	((num <= 9223372036854775807 / mult)) || die "size too large: $2"
	printf -v "$1" %d $((num * mult))
}

entries=1000000 budget=$((10 << 30)) depth=12 dir_pct=10 seed=$RANDOM$RANDOM
jobs=$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 4)
while getopts 'n:s:d:p:j:S:h' opt; do
	case $opt in
	n) uint entries "$OPTARG" -n ;;
	s) parse_size budget "$OPTARG" ;;
	d) uint depth "$OPTARG" -d ;;
	p) uint dir_pct "$OPTARG" -p ;;
	j) uint jobs "$OPTARG" -j ;;
	S) uint seed "$OPTARG" -S ;;
	*) sed -n '2,/^$/s/^# \{0,1\}//p' "$0" >&2 && exit 2 ;;
	esac
done
shift $((OPTIND - 1))
(($# == 1)) || die "expected one output directory (-h for usage)"
((dir_pct <= 100)) || die "-p must be 0..100"
# awk holds one manifest open per writer, and macOS defaults to 256 open files.
((jobs >= 1 && jobs <= 128)) || die "-j must be 1..128"
out=$1

# Split so entries * dir_pct cannot overflow for an 18-digit -n.
dirs=$((entries / 100 * dir_pct + (entries % 100 * dir_pct + 50) / 100))
((dirs == 0 || depth > 0)) || die "-d 0 leaves no room for $dirs folders"

mkdir -p "$out"
[[ -z $(ls -A "$out") ]] || die "$out is not empty"
# Available is the field before Capacity ("88%"); the device name may hold spaces.
free=$(df -Pk "$out" | awk 'NR == 2 { for (i = 2; i <= NF; i++) if ($i ~ /^[0-9]+%$/) { printf "%.0f", $(i - 1) * 1024; exit } }')
[[ -n $free ]] || die "cannot read free space of $out from df"
((free > budget)) || die "$out has $free bytes free, -s asks for $budget"

pool_size=65536
tmp=$(mktemp -d)
pids=()
# Background writers ignore SIGINT, so Ctrl-C or a kill would leave them filling the disk.
trap 'kill ${pids[@]+"${pids[@]}"} 2>/dev/null || true; rm -rf "$tmp"' EXIT

SECONDS=0
# One pass plans the whole tree: folders in creation order (parents first) to dirs, and
# "size pool-offset path" lines for the files, round-robin across one manifest per writer.
# A folder's parent is drawn uniformly from the folders not yet at the depth limit; a
# file's from all folders, OUT included.
awk -v n="$entries" -v nd="$dirs" -v maxd="$depth" -v budget="$budget" -v seed="$seed" \
	-v jobs="$jobs" -v pool="$pool_size" -v tmp="$tmp" '
BEGIN {
	srand(seed)
	path[0] = "."; lvl[0] = 0; nel = 0
	if (maxd > 0) el[nel++] = 0
	for (i = 1; i <= nd; i++) {
		p = el[int(rand() * nel)]
		path[i] = path[p] "/d" i; lvl[i] = lvl[p] + 1
		if (lvl[i] > deepest) deepest = lvl[i]
		if (lvl[i] < maxd) el[nel++] = i
		print path[i] > (tmp "/dirs")
	}
	nf = n - nd
	for (i = 1; i <= nf; i++) {
		# Lomax, shape 1.5: mean 2, median ~0.6, max ~1e4 over a million draws
		w[i] = (1 - rand()) ^ (-1 / 1.5) - 1
		total += w[i]
	}
	scale = total > 0 ? budget / total : 0
	for (i = 1; i <= nf; i++) {
		size = int(w[i] * scale)
		bytes += size
		off = size <= pool ? int(rand() * (pool - size + 1)) : -1
		printf "%.0f %d %s/f%d.dat\n", size, off, path[int(rand() * (nd + 1))], i \
			> (tmp "/files." (i % jobs))
	}
	printf "planned %d folders (deepest %d), %d files, %.0f bytes\n", nd, deepest, nf, bytes \
		> "/dev/stderr"
}'
echo "plan: ${SECONDS}s" >&2

CDPATH= cd -- "$out"
# Parents precede children in dirs, so one serial mkdir per batch needs no -p.
[[ ! -s $tmp/dirs ]] || xargs mkdir <"$tmp/dirs"
echo "folders: ${SECONDS}s" >&2

# base64 of 48 KiB is exactly 64 KiB of NUL-free text a builtin printf can slice.
pool=$(head -c $((pool_size / 4 * 3)) /dev/urandom | base64 | tr -d '\n')
((${#pool} == pool_size)) || die "random pool is ${#pool} bytes, expected $pool_size"

write_files() {
	local size off path
	while read -r size off path; do
		if ((off >= 0)); then
			printf '%s' "${pool:off:size}" >"$path"
		else
			head -c "$size" /dev/urandom >"$path"
		fi
	done <"$1"
}

for m in "$tmp"/files.*; do
	[[ -e $m ]] || continue
	write_files "$m" &
	pids+=($!)
done
failed=0
for pid in ${pids[@]+"${pids[@]}"}; do wait "$pid" || failed=1; done
pids=() # reaped: the EXIT trap must not kill whatever reuses their pids
((failed == 0)) || die "a file writer failed"
echo "files: ${SECONDS}s" >&2
