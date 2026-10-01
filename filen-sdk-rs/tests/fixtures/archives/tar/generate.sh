#!/usr/bin/env bash
# Regenerates the tar fixtures in this directory and their manifest.tsv (see README.md).
#
# Tars as bsdtar writes them, in each format and through each codec, and the AppleDouble member
# it writes on macOS: interop the `tar` crate cannot stand in for. Every tar but
# appledouble.tar holds the tree written below, from bin.dat (4 KiB of seeded noise) and
# text.txt. The manifest is written from those inputs (and, for the AppleDouble member, with
# Python's `tarfile`), never from what the SDK reads.
#
# Needs macOS (for `xattr` and bsdtar's --mac-metadata), bsdtar (the system one), the zstd and
# lz4 programs it runs for --zstd and --lz4 (brew install zstd lz4) and python3.
#
# Built on macOS with bsdtar 3.5.3 (libarchive 3.7.4).
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
cd "$work"

# bin.dat and text.txt, dated 2025-12-31 23:00 UTC, and source.tar: the tree every fixture but
# appledouble.tar holds, as a pax archive with fixed times and owners, which bsdtar reads its
# entries from (`@source.tar`) so the fixtures hold nothing of the machine that made them
python3 - <<'PY'
import io
import os
import random
import tarfile

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

LONG_DIR = "tree/a-directory-name-of-sixty-characters-for-the-long-path-cases"
LONG = LONG_DIR + "/a-file-name-of-sixty-characters-for-the-long-path-cases.txt"


def add(tar, name, kind, data=b"", link="", pax=None):
    info = tarfile.TarInfo(name)
    info.type, info.size, info.linkname = kind, len(data), link
    info.mtime, info.uname, info.gname = 1767225600, "root", "wheel"
    info.mode = 0o755 if kind == tarfile.DIRTYPE else 0o644
    info.pax_headers = pax or {}
    tar.addfile(info, io.BytesIO(data) if data else None)


with tarfile.open("source.tar", "w", format=tarfile.PAX_FORMAT) as tar:
    add(tar, "tree", tarfile.DIRTYPE)
    add(tar, "tree/dir", tarfile.DIRTYPE)
    add(tar, "tree/dir/text.txt", tarfile.REGTYPE, inputs["text.txt"],
        pax={"SCHILY.xattr.user.fixture": "vendor key"})
    add(tar, "tree/dir/bin.dat", tarfile.REGTYPE, inputs["bin.dat"])
    add(tar, "tree/dir/hard", tarfile.LNKTYPE, link="tree/dir/text.txt")
    add(tar, "tree/dir/link", tarfile.SYMTYPE, link="text.txt")
    add(tar, "tree/café.txt", tarfile.REGTYPE, "café\n".encode())
    add(tar, LONG_DIR, tarfile.DIRTYPE)
    add(tar, LONG, tarfile.REGTYPE, b"deep\n")
PY

# Writes ustar.tar pax.tar gnutar.tar.
for format in ustar pax gnutar; do bsdtar --format "$format" -cf "$here/$format.tar" @source.tar; done
bsdtar --format pax -czf "$here/pax.tar.gz" @source.tar
bsdtar --format pax -cjf "$here/pax.tar.bz2" @source.tar
bsdtar --format pax -cJf "$here/pax.tar.xz" @source.tar
bsdtar --format pax --lzma -cf "$here/pax.tar.lzma" @source.tar
bsdtar --format pax --zstd -cf "$here/pax.tar.zst" @source.tar
bsdtar --format pax --lz4 -cf "$here/pax.tar.lz4" @source.tar

# a file with an extended attribute, which bsdtar writes as an AppleDouble `._note.txt` member
# ahead of the file (macOS may also add a com.apple.provenance attribute to it, depending on the
# process writing it)
mkdir mac
printf 'a note with a macOS extended attribute\n' > mac/note.txt
xattr -w user.fixture 'vendor key' mac/note.txt
touch -t 202601010000 mac/note.txt
bsdtar --format pax --mac-metadata --uid 0 --gid 0 --uname root --gname wheel \
	-cf "$here/appledouble.tar" -C mac note.txt

# what each tar extracts to, told from the inputs
python3 - "$here" <<'PY'
import os
import sys
import tarfile
import zlib

here = sys.argv[1]
LONG_DIR = "tree/a-directory-name-of-sixty-characters-for-the-long-path-cases"
LONG = LONG_DIR + "/a-file-name-of-sixty-characters-for-the-long-path-cases.txt"


def file(path, data):
    return ("file", path, str(len(data)), f"{zlib.crc32(data):08x}")


TREE = [
    ("dir", "tree"),
    ("dir", "tree/dir"),
    file("tree/dir/text.txt", open("text.txt", "rb").read()),
    file("tree/dir/bin.dat", open("bin.dat", "rb").read()),
    ("hardlink", "tree/dir/hard", "tree/dir/text.txt"),
    ("symlink", "tree/dir/link", "text.txt"),
    file("tree/café.txt", "café\n".encode()),
    ("dir", LONG_DIR),
    file(LONG, b"deep\n"),
]
rows = [
    (archive,) + row
    for archive in sorted(
        name for name in os.listdir(here) if ".tar" in name and name != "appledouble.tar"
    )
    for row in TREE
]
# none of the four files is checked where the stream carries no checksum: LZMA-alone has none,
# and libarchive writes zstd without one
rows += [("pax.tar.lzma", "unchecked", "", "4"), ("pax.tar.zst", "unchecked", "", "4")]
with tarfile.open(os.path.join(here, "appledouble.tar")) as tar:
    rows += [
        ("appledouble.tar",) + file(member.name, tar.extractfile(member).read())
        for member in tar
    ]
with open(os.path.join(here, "manifest.tsv"), "w") as out:
    out.write("# archive\tkind\tpath\tsize or target\tcrc32\n")
    out.write("".join("\t".join(row) + "\n" for row in rows))
PY
