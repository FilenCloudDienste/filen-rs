# Compressed-stream fixtures

Single compressed files written by each codec's own tool, for `check_fixtures("streams")` in
`src/fs/archive/extract/codec/tests.rs`, which extracts each one and compares the file it holds
(named after the archive, less its codec extension) and the bytes that belong to no frame with
`manifest.tsv`. The manifest is written from the inputs by `generate.sh`, never from what the SDK
reads.

The inputs are `bin.dat`, 4 KiB of seeded noise, and `text.txt`:

- `two-streams-x86.bin.xz`: an xz stream of `bin.dat` through the x86 branch filter with a
  CRC64 check, then one of `text.txt` with a SHA-256 check.
- `text.txt.lzma`, `text.txt.br`: LZMA-alone and brotli, which carry no checksum, so the
  manifest expects their file to be unchecked.

Streams the crates in the tree write are built by the tests instead
(`streams_of_several_members_or_frames_extract_to_one_file`): gzip and bzip2 members at
different levels, and lz4 and zstd frames with skippable frames among them and one frame
without a checksum.

Run `./generate.sh` to make them again; it lists the tools it needs and the versions they were
made with.
