# ruzstd 0.9.0, vendored

The zstd decoder behind `fs::archive` (tar.zst, .zst and 7z's zstd coder), wired in with
`[patch.crates-io]` in the workspace `Cargo.toml`.

## Why

ruzstd 0.9.0, and its `master` as of 2026-09-13 (`fe37617`), never check what a compressed block
decodes to. RFC 8878 caps a block at 128 KiB (`Block_Maximum_Size`), but ruzstd runs every
sequence of a block into its ring buffer and decodes a literals section's full stated size, so a
crafted frame of a few KiB makes a single block allocate gigabytes, far past the window the SDK
charges its codec budget for, and a block of enough sequences overflows a `u32` sum and panics.

## What is ours

Only `src/` differs from the published crate, by the patch in the commit that follows the one
adding this directory (`git log -- filen-sdk-rs/vendor/ruzstd`):

- a literals section may regenerate at most 128 KiB;
- a block's literals and 3 bytes per sequence (a match copies at least 3) may come to at most
  128 KiB, checked before the sequences are decoded into a buffer of that many;
- a block's literals and matches together may decode to at most 128 KiB, checked before any of
  it is written to the ring;
- Huffman-coded literals stop at their stated count, where a bitstream holding more was decoded
  to its end (up to 8 literals per byte) before the count was checked;
- the per-block sum is counted in `usize`, so it cannot overflow.

`Cargo.toml` is the published (normalized) one, less the dev-dependencies, benches and examples
of the crate's own test suite, which a dependency never builds; those tests and their fixtures
are not vendored. `rustfmt.toml` keeps upstream's default formatting.

## Leaving

Drop this directory and the `[patch.crates-io]` entry once a ruzstd release bounds block sizes.
