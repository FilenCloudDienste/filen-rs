# 7z fixtures

Archives written by 7-Zip itself, for `check_fixtures("7z")` in
`src/fs/archive/extract/codec/tests.rs`, which extracts each one and compares what it holds with
`manifest.tsv`. The manifest is written from the inputs by `generate.sh`, never from what the
SDK reads.

Each archive holds the same two files: `bin.dat`, 4 KiB of seeded noise in which the branch
filters find calls to rewrite, and `text.txt`, both dated so that the archives come out the same
byte for byte: one per filter (`filter-*.7z`), one per method (`method-*.7z`), encrypted with
and without encrypted headers, and one not solid.

`7zz l -slt <archive>` shows each one's coders (`Method`): the filters sit in front of LZMA2,
BCJ2 feeds its four streams from LZMA2 and two LZMA coders, and `04F71101` is zstd. The
`encrypted` archives use the password `fixture password`, the test's `FIXTURE_PASSWORD`.

Run `./generate.sh` to make them again; it lists the tools it needs and the versions they were
made with.
