#!/usr/bin/env bash
# Regenerates the compressed-stream fixtures in this directory and their manifest.tsv (see
# README.md).
#
# Only the streams nothing in the tree writes are made here, with each codec's own tool: an xz
# stream through the x86 branch filter, LZMA-alone, and brotli. The codec tests build the
# streams of several gzip or bzip2 members and of lz4 and zstd frames themselves. The inputs are
# bin.dat, 4 KiB of seeded noise, and text.txt; the manifest is written from them, never from
# what the SDK reads.
#
# Needs xz and brotli (brew install xz brotli) and python3.
#
# Built on macOS with xz 5.8 and brotli 1.2.
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

# an xz stream through the x86 branch filter with a CRC64 check, then a plain one with a
# SHA-256 check
xz --x86 --lzma2=preset=6 -c bin.dat > a.xz
xz --check=sha256 -c text.txt > b.xz
cat a.xz b.xz > "$here/two-streams-x86.bin.xz"
xz --format=lzma -c text.txt > "$here/text.txt.lzma"
brotli -c text.txt > "$here/text.txt.br"

# what each stream extracts to, told from the inputs: a file named after the stream, less its
# codec extension, and none checked where the stream carries no checksum (brotli and
# LZMA-alone never do)
python3 - "$here" <<'PY'
import os
import sys
import zlib

here = sys.argv[1]
binary, text = open("bin.dat", "rb").read(), open("text.txt", "rb").read()


def file(archive, path, data):
    return f"{archive}\tfile\t{path}\t{len(data)}\t{zlib.crc32(data):08x}"


rows = [
    file("text.txt.br", "text.txt", text),
    "text.txt.br\tunchecked\t\t1",
    file("text.txt.lzma", "text.txt", text),
    "text.txt.lzma\tunchecked\t\t1",
    file("two-streams-x86.bin.xz", "two-streams-x86.bin", binary + text),
]
with open(os.path.join(here, "manifest.tsv"), "w") as out:
    out.write("# archive\tkind\tpath\tsize or target\tcrc32\n")
    out.write("".join(row + "\n" for row in rows))
PY
