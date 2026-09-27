"""Writes each directory's manifest.tsv: what every fixture there extracts to, told from the
inputs (and, for the AppleDouble member, Python's tarfile), never from the SDK's own reading.
Run from this directory once the fixtures are made."""

import os
import tarfile
import zlib

from inputs import binary, text

LONG_DIR = "tree/a-directory-name-of-sixty-characters-for-the-long-path-cases"
LONG = LONG_DIR + "/a-file-name-of-sixty-characters-for-the-long-path-cases.txt"


def file(path, data):
    return ("file", path, str(len(data)), f"{zlib.crc32(data):08x}")


def write(directory, rows):
    with open(os.path.join(directory, "manifest.tsv"), "w") as out:
        out.write("# archive\tkind\tpath\tsize or target\tcrc32\n")
        for row in rows:
            out.write("\t".join(row) + "\n")


BINARY, TEXT = binary(), text()

write("7z", [
    (archive,) + row
    for archive in sorted(name for name in os.listdir("7z") if name.endswith(".7z"))
    for row in (file("bin.dat", BINARY), file("text.txt", TEXT))
])

# the tree tar/source.py writes
TREE = [
    ("dir", "tree"),
    ("dir", "tree/dir"),
    file("tree/dir/text.txt", TEXT),
    file("tree/dir/bin.dat", BINARY),
    ("hardlink", "tree/dir/hard", "tree/dir/text.txt"),
    ("symlink", "tree/dir/link", "text.txt"),
    file("tree/café.txt", "café\n".encode()),
    ("dir", LONG_DIR),
    file(LONG, b"deep\n"),
]
rows = [
    (archive,) + row
    for archive in sorted(name for name in os.listdir("tar") if ".tar" in name and name != "appledouble.tar")
    for row in TREE
]
with tarfile.open("tar/appledouble.tar") as tar:
    rows += [("appledouble.tar",) + file(member.name, tar.extractfile(member).read()) for member in tar]
write("tar", rows)

BOTH = BINARY + TEXT
write("streams", [
    ("frames.bin.zst",) + file("frames.bin", BOTH),
    ("frames.bin.zst", "unaccounted", "", "42"),
    ("skippable-frames.bin.lz4",) + file("skippable-frames.bin", BOTH),
    ("skippable-frames.bin.lz4", "unaccounted", "", "20"),
    ("text.txt.br",) + file("text.txt", TEXT),
    ("text.txt.lzma",) + file("text.txt", TEXT),
    ("two-members.bin.bz2",) + file("two-members.bin", BOTH),
    ("two-members.bin.gz",) + file("two-members.bin", BOTH),
    ("two-streams-x86.bin.xz",) + file("two-streams-x86.bin", BOTH),
])
