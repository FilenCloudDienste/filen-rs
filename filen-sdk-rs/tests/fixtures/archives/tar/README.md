# tar fixtures

Tars written by bsdtar, for `check_fixtures("tar")` in
`src/fs/archive/extract/codec/tests.rs`, which extracts each one and compares what it holds with
`manifest.tsv`. The manifest is written from the inputs (and, for the AppleDouble member, with
Python's `tarfile`) by `../manifest.py`, never from what the SDK reads.

Every tar but `appledouble.tar` holds the tree `source.py` describes: directories, the two files
from `../inputs.py` (`text.txt` with a `SCHILY.xattr.user.fixture` vendor key), a hard link and
a symlink to it, a non-ASCII name, and a 126-byte path, which ustar splits into its prefix
field, GNU tar writes as a `././@LongLink` record and pax as a `path` record. `source.py` writes
it as a pax archive with fixed times and owners; bsdtar reads its entries from there (`@`) and
writes them in each format, so the fixtures are bsdtar's own output without anything of the
machine that made them. `pax.tar` carries bsdtar's vendor keys (`LIBARCHIVE.xattr.*` and
`SCHILY.xattr.*`).

`appledouble.tar` is what bsdtar writes on macOS for a file with an extended attribute: an
AppleDouble `._note.txt` member ahead of the file.

Made on macOS with bsdtar 3.5.3 (libarchive 3.7.4), which runs the zstd and lz4 programs for
`--zstd` and `--lz4`, from this directory:

```sh
python3 ../inputs.py
python3 source.py
for format in ustar pax gnutar; do bsdtar --format "$format" -cf "$format.tar" @source.tar; done
bsdtar --format pax -czf pax.tar.gz @source.tar
bsdtar --format pax -cjf pax.tar.bz2 @source.tar
bsdtar --format pax -cJf pax.tar.xz @source.tar
bsdtar --format pax --lzma -cf pax.tar.lzma @source.tar
bsdtar --format pax --zstd -cf pax.tar.zst @source.tar
bsdtar --format pax --lz4 -cf pax.tar.lz4 @source.tar
mkdir mac
printf 'a note with a macOS extended attribute\n' > mac/note.txt
xattr -w user.fixture 'vendor key' mac/note.txt
touch -t 202601010000 mac/note.txt
bsdtar --format pax --mac-metadata --uid 0 --gid 0 --uname root --gname wheel \
	-cf appledouble.tar -C mac note.txt
rm -r mac source.tar bin.dat text.txt
(cd .. && python3 manifest.py)
```

macOS adds a `com.apple.provenance` attribute to files it writes, so the AppleDouble member
also carries one, as it would from any Mac.
