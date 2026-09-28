# ruzstd 0.9.0, vendored

The zstd decoder behind `fs::archive` (tar.zst, .zst and 7z's zstd coder), wired in with
`[patch.crates-io]` in the workspace `Cargo.toml`.

## Why

ruzstd 0.9.0, and its `master` as of 2026-09-13 (`fe37617`), never check what a compressed block
decodes to. RFC 8878 caps a block at 128 KiB (`Block_Maximum_Size`), but ruzstd runs every
sequence of a block into its ring buffer and decodes a literals section's full stated size, so a
crafted frame of a few KiB makes a single block allocate gigabytes, far past the window the SDK
charges its codec budget for, and a block of enough sequences overflows a `u32` sum and panics.

## Where it comes from

The published crate: `ruzstd-0.9.0.crate`, sha256
`a252f5e20f038fe7b4ea53e073e65398d652c864cc162fc77c56c2f13717b888` (the checksum in
`Cargo.lock`), cut from upstream commit `f833802b674e6b9360a259d25c20940e25a54e79`
(`.cargo_vcs_info.json`, path `ruzstd`).

## What is ours

`src/` differs from the published crate by the patch in the commits after the one adding this
directory (`git log -- filen-sdk-rs/vendor/ruzstd`):

- a literals section may regenerate at most 128 KiB;
- a block's literals and 3 bytes per sequence (a match copies at least 3) may come to at most
  128 KiB, checked before the sequences are decoded into a buffer of that many;
- a block's literals and matches together may decode to at most 128 KiB, checked before any of
  it is written to the ring;
- Huffman-coded literals stop at their stated count, where a bitstream holding more was decoded
  to its end (up to 8 literals per byte) before the count was checked;
- the per-block sum is counted in `usize`, so it cannot overflow.

Besides `src/`: `Cargo.toml` is the published (normalized) one, less the dev-dependencies,
benches and examples of the crate's own test suite, which a dependency never builds; those
tests' fixtures, the benches, the examples, `Cargo.toml.orig`, `Cargo.lock` and
`.cargo_vcs_info.json` are left out. `src/tests` is that suite as published, kept so `src/`
stays the crate's less the patch; without its fixtures and dev-dependencies it does not build,
and nothing builds it (it is `#[cfg(test)]`, and only the SDK's own tests run). `rustfmt.toml`
(keeping upstream's default formatting) and this file are ours. The patch's tests live in the
SDK (`fs/archive/decode/tests.rs`).

## Building elsewhere

`[patch.crates-io]` applies only in the workspace whose root `Cargo.toml` holds it: a workspace
that depends on `filen-sdk-rs` from outside this one has to repeat the entry, pointing at this
directory. Without it the SDK does not build: `fs/archive/decode/zstd.rs` names the error the
patch adds (`DecompressBlockError::DecompressedSizeTooLarge`) in a constant for that purpose,
so an unpatched ruzstd fails the build rather than decode unbounded blocks.

## Leaving

Drop this directory and the `[patch.crates-io]` entry once a ruzstd release bounds block sizes,
and with them the build guard in `fs/archive/decode/zstd.rs`, or make it name what that release
reports for a block past 128 KiB.
