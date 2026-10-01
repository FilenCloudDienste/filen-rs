# Zip fixtures from real tools

Zips made by the tools people actually use, which the SDK's own writer and the `zip` crate
never produce: methods 9 (Deflate64), 14 (LZMA, with and without an end marker), 93 (zstd), 95
(XZ) and 98 (PPMd, unsupported), ZipCrypto from three writers (Info-ZIP's with the data
descriptor check byte), WinZip AES as AE-1 (libarchive) and AE-2 (7-Zip), a Finder-style zip
with unflagged UTF-8 names, a symlink and `__MACOSX` entries, streamed data descriptors, and
Info-ZIP's zip64 records.

`src/fs/archive/zip/read/tests/fixtures.rs` reads each one through the zip reader and compares
what comes out with contents it generates the same way `generate.sh` does, so the script is the
manifest: the password is `pw` wherever there is one. What the crates in the tree can write is
built by the tests instead: bzip2 entries (the SDK's writer), symlinks as Info-ZIP and 7-Zip
store them, and a comment followed by trailing bytes.

Run `./generate.sh` on macOS to make them again; it lists the tools it needs and the versions
they were made with, and why a second run gives the same contents but not the same bytes.
