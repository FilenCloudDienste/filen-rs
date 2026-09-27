"""Writes source.tar: the tree every tar fixture but appledouble.tar holds, as a pax archive with
fixed times and owners, which bsdtar reads its entries from (`@source.tar`). Run in this directory
after `python3 ../inputs.py`."""

import io
import tarfile

LONG_DIR = "tree/a-directory-name-of-sixty-characters-for-the-long-path-cases"
LONG = LONG_DIR + "/a-file-name-of-sixty-characters-for-the-long-path-cases.txt"
MODIFIED = 1767225600


def add(tar, name, kind, data=b"", link="", pax=None):
    info = tarfile.TarInfo(name)
    info.type, info.size, info.linkname = kind, len(data), link
    info.mtime, info.uname, info.gname = MODIFIED, "root", "wheel"
    info.mode = 0o755 if kind == tarfile.DIRTYPE else 0o644
    info.pax_headers = pax or {}
    tar.addfile(info, io.BytesIO(data) if data else None)


with tarfile.open("source.tar", "w", format=tarfile.PAX_FORMAT) as tar:
    add(tar, "tree", tarfile.DIRTYPE)
    add(tar, "tree/dir", tarfile.DIRTYPE)
    add(tar, "tree/dir/text.txt", tarfile.REGTYPE, open("text.txt", "rb").read(),
        pax={"SCHILY.xattr.user.fixture": "vendor key"})
    add(tar, "tree/dir/bin.dat", tarfile.REGTYPE, open("bin.dat", "rb").read())
    add(tar, "tree/dir/hard", tarfile.LNKTYPE, link="tree/dir/text.txt")
    add(tar, "tree/dir/link", tarfile.SYMTYPE, link="text.txt")
    add(tar, "tree/café.txt", tarfile.REGTYPE, "café\n".encode())
    add(tar, LONG_DIR, tarfile.DIRTYPE)
    add(tar, LONG, tarfile.REGTYPE, b"deep\n")
