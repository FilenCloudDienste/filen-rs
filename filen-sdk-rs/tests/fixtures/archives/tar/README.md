# tar fixtures

Tars written by bsdtar, for `check_fixtures("tar")` in
`src/fs/archive/extract/codec/tests.rs`, which extracts each one and compares what it holds with
`manifest.tsv`. The manifest is written from the inputs (and, for the AppleDouble member, with
Python's `tarfile`) by `generate.sh`, never from what the SDK reads.

Every tar but `appledouble.tar` holds the same tree: directories, `bin.dat` (4 KiB of seeded
noise) and `text.txt` (with a `SCHILY.xattr.user.fixture` vendor key), a hard link and a symlink
to it, a non-ASCII name, and a 126-byte path, which ustar splits into its prefix field, GNU tar
writes as a `././@LongLink` record and pax as a `path` record. The script writes the tree as a
pax archive with fixed times and owners; bsdtar reads its entries from there (`@`) and writes
them in each format, so the fixtures are bsdtar's own output without anything of the machine
that made them. `pax.tar` carries bsdtar's vendor keys (`LIBARCHIVE.xattr.*` and
`SCHILY.xattr.*`).

The manifest expects the files of `pax.tar.lzma` and `pax.tar.zst` to be unchecked: LZMA-alone
carries no checksum, and libarchive writes zstd frames without one.

`appledouble.tar` is what bsdtar writes on macOS for a file with an extended attribute: an
AppleDouble `._note.txt` member ahead of the file. Whether macOS also gives the file a
`com.apple.provenance` attribute depends on the process that writes it; the committed member
carries one, so a rerun may write a smaller `._note.txt`, and the manifest with it.

Run `./generate.sh` on macOS to make them again; it lists the tools it needs and the versions
they were made with.
