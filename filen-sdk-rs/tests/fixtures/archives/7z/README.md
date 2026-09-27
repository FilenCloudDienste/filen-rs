# 7z fixtures

Archives written by 7-Zip itself, for `check_fixtures("7z")` in
`src/fs/archive/extract/codec/tests.rs`, which extracts each one and compares what it holds with
`manifest.tsv`. The manifest is written from the inputs by `../manifest.py`, never from what the
SDK reads.

Each archive holds the same two files from `../inputs.py`: `bin.dat`, 4 KiB of seeded noise in
which the branch filters find calls to rewrite, and `text.txt`, both dated so that the archives
come out the same byte for byte. Made on macOS with 7-Zip 26.01
(`7zz`) and, for zstd, p7zip 17.05 with its zstd plugin (`7z`), from this directory:

```sh
python3 ../inputs.py
for filter in BCJ BCJ2 ARM ARMT ARM64 PPC SPARC IA64 RISCV Delta:4; do
	7zz a "filter-$(echo "$filter" | tr 'A-Z:' 'a-z-').7z" -mf="$filter" bin.dat text.txt
done
for method in LZMA Deflate Deflate64 BZip2 PPMd; do
	7zz a "method-$(echo "$method" | tr 'A-Z' 'a-z').7z" -m0="$method" bin.dat text.txt
done
7zz a encrypted.7z -p'fixture password' bin.dat text.txt
7zz a encrypted-headers.7z -p'fixture password' -mhe=on bin.dat text.txt
7zz a non-solid.7z -ms=off bin.dat text.txt
7z a method-zstd.7z -m0=zstd bin.dat text.txt
rm bin.dat text.txt
(cd .. && python3 manifest.py)
```

`7zz l -slt <archive>` shows each one's coders (`Method`): the filters sit in front of LZMA2,
BCJ2 feeds its four streams from LZMA2 and two LZMA coders, and `04F71101` is zstd. The
`encrypted` archives use the password `fixture password`, the test's `FIXTURE_PASSWORD`.
