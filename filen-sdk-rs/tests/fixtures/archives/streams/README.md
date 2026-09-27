# Compressed-stream fixtures

Single compressed files written by each codec's own tool, for `check_fixtures("streams")` in
`src/fs/archive/extract/codec/tests.rs`, which extracts each one and compares the file it holds
(named after the archive, less its codec extension) and the bytes that belong to no frame with
`manifest.tsv`. The manifest is written from the inputs by `../manifest.py`, never from what the
SDK reads.

The inputs are `bin.dat` and `text.txt` from `../inputs.py`. Each `.bin.*` fixture is several
members, streams or frames one after another, holding both files:

- `two-members.bin.gz`, `two-members.bin.bz2`: two members at different levels.
- `two-streams-x86.bin.xz`: an xz stream through the x86 branch filter with a CRC64 check, then
  a plain one with a SHA-256 check.
- `skippable-frames.bin.lz4`: a skippable frame, then a frame of linked 64 KiB blocks stating
  its size, then one without a content checksum.
- `frames.bin.zst`: skippable frames around a level-19 frame and a frame without a checksum.

The skippable frames (20 and 42 bytes) are written by `skippable.py`, since neither CLI writes
them, and are the unaccounted bytes the manifest expects. The manifest also expects a file to be
unchecked where its stream carries no checksum: brotli and LZMA-alone never do, and the second
lz4 and zstd frames are written without one. Made on macOS with xz 5.8, gzip
(Apple's), bzip2 1.0.8, brotli 1.2, lz4 1.10 and zstd 1.5.7, from this directory:

```sh
python3 ../inputs.py
xz --x86 --lzma2=preset=6 -c bin.dat > a.xz
xz --check=sha256 -c text.txt > b.xz
cat a.xz b.xz > two-streams-x86.bin.xz
xz --format=lzma -c text.txt > text.txt.lzma
gzip -n -c bin.dat > a.gz
gzip -n -9 -c text.txt > b.gz
cat a.gz b.gz > two-members.bin.gz
bzip2 -c bin.dat > a.bz2
bzip2 -1 -c text.txt > b.bz2
cat a.bz2 b.bz2 > two-members.bin.bz2
brotli -c text.txt > text.txt.br
python3 skippable.py 0x184D2A50 'lz4 metadata' > s1
lz4 -q -BD -B4 --content-size -c bin.dat > a.lz4
lz4 -q --no-frame-crc -c text.txt > b.lz4
cat s1 a.lz4 b.lz4 > skippable-frames.bin.lz4
python3 skippable.py 0x184D2A5F 'zstd metadata' > s2
zstd -q -19 -c bin.dat > a.zst
zstd -q --no-check -c text.txt > b.zst
cat s2 a.zst s2 b.zst > frames.bin.zst
rm a.* b.* s1 s2 bin.dat text.txt
(cd .. && python3 manifest.py)
```
