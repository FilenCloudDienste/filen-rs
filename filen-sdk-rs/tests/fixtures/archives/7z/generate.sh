#!/usr/bin/env bash
# Regenerates the 7z fixtures in this directory and their manifest.tsv (see README.md).
#
# Nothing in the tree writes 7-Zip's filters and methods as 7-Zip does, so these are made once
# with 7-Zip itself and committed. Every archive holds the same two files: bin.dat, 4 KiB of
# seeded noise in which the branch filters find calls to rewrite, and text.txt, both dated so
# that the archives come out the same byte for byte. The manifest is written from those inputs,
# never from what the SDK reads.
#
# Needs 7-Zip (`7zz`: brew install sevenzip), p7zip with its zstd plugin (`7z`) for
# method-zstd.7z, which 7-Zip does not write, and python3.
#
# Built on macOS with 7-Zip 26.01 and p7zip 17.05.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
cd "$work"

# bin.dat and text.txt, dated 2025-12-31 23:00 UTC
python3 - <<'PY'
import os
import random

MODIFIED = 1767222000
rng = random.Random(7)
inputs = {
    "bin.dat": bytes(rng.getrandbits(8) for _ in range(4096)),
    "text.txt": "".join(
        f"line {i}: the quick brown fox jumps over the lazy dog\n" for i in range(40)
    ).encode(),
}
for name, data in inputs.items():
    with open(name, "wb") as out:
        out.write(data)
    os.utime(name, (MODIFIED, MODIFIED))
PY

# Writes filter-bcj.7z filter-bcj2.7z filter-arm.7z filter-armt.7z filter-arm64.7z filter-ppc.7z
# filter-sparc.7z filter-ia64.7z filter-riscv.7z filter-delta-4.7z.
for filter in BCJ BCJ2 ARM ARMT ARM64 PPC SPARC IA64 RISCV Delta:4; do
	7zz a "filter-$(echo "$filter" | tr 'A-Z:' 'a-z-').7z" -mf="$filter" bin.dat text.txt
done
# Writes method-lzma.7z method-deflate.7z method-deflate64.7z method-bzip2.7z method-ppmd.7z.
for method in LZMA Deflate Deflate64 BZip2 PPMd; do
	7zz a "method-$(echo "$method" | tr 'A-Z' 'a-z').7z" -m0="$method" bin.dat text.txt
done
7zz a encrypted.7z -p'fixture password' bin.dat text.txt
7zz a encrypted-headers.7z -p'fixture password' -mhe=on bin.dat text.txt
7zz a non-solid.7z -ms=off bin.dat text.txt
7z a method-zstd.7z -m0=zstd bin.dat text.txt
mv ./*.7z "$here"

# what each archive extracts to: both files, told from the inputs
python3 - "$here" <<'PY'
import os
import sys
import zlib

here = sys.argv[1]
with open(os.path.join(here, "manifest.tsv"), "w") as out:
    out.write("# archive\tkind\tpath\tsize or target\tcrc32\n")
    for archive in sorted(name for name in os.listdir(here) if name.endswith(".7z")):
        for name in ("bin.dat", "text.txt"):
            data = open(name, "rb").read()
            out.write(f"{archive}\tfile\t{name}\t{len(data)}\t{zlib.crc32(data):08x}\n")
PY
