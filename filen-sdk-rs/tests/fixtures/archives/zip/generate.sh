#!/usr/bin/env bash
# Regenerates the zip fixtures in this directory (see README.md).
#
# Zips made by the tools people actually use, which the SDK's own writer and the `zip` crate
# never produce. The tests generate the fixtures' contents the same way this script does, so the
# script is the manifest: the password is `pw` wherever there is one. Running it again gives the
# same contents, not the same bytes: encryption salts and headers are random, `ditto` stamps
# `__MACOSX` directories with the current time, and Info-ZIP stamps the entry it reads from stdin
# (the one in zip64-infozip.zip) with it too.
#
# Needs macOS (for `ditto` and `xattr`), Info-ZIP Zip (`/usr/bin/zip`), bsdtar (the system one),
# 7-Zip (`7zz`: brew install sevenzip) and, for zstd, which 7-Zip does not write into a zip,
# CPython 3.14's `zipfile` over libzstd (`python3.14`: brew install python@3.14).
#
# Built on macOS 26.5 with Info-ZIP Zip 3.0, bsdtar 3.5.3 (libarchive 3.7.4), 7-Zip 26.01 and
# CPython 3.14 (frames without a content checksum).
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
cd "$work"
export TZ=UTC
# written apart and moved in at the end: zip and 7zz add to an archive that is already there
out="$work/out"

mkdir -p src/sub finder/Café "$out"
python3 - <<'PY'
def noise(seed, n):
    out = bytearray()
    for _ in range(n):
        seed = (seed * 1103515245 + 12345) & 0x7FFFFFFF
        out.append((seed >> 16) & 0xFF)
    return bytes(out)


open("src/hello.txt", "wb").write(b"hello from a real zip tool\n")
# the repeat is 35000 bytes back: past deflate's 32 KiB window, inside deflate64's 64 KiB
open("src/sub/far.bin", "wb").write(noise(1, 1000) + bytes(34000) + noise(1, 1000))
open("src/sub/lines.txt", "wb").write(b"".join(b"line %d\n" % i for i in range(500)))
# a decomposed (NFD) name, as macOS may store one
open("finder/Cafe\u0301 NFD.txt", "wb").write(b"decomposed\n")
PY
printf 'bonjour\n' > 'finder/Café/Résumé.txt'
printf 'naive\n' > finder/naïve.txt
ln -s naïve.txt finder/link
xattr -w com.filen.fixture tagged finder/naïve.txt
find src finder -exec touch -h -t 202401020304.05 {} +

cd src
# Writes deflate64.zip xz.zip ppmd.zip.
for method in Deflate64 XZ PPMd; do
	7zz a -tzip -mm=$method "$out/$(echo $method | tr '[:upper:]' '[:lower:]').zip" hello.txt sub
done
7zz a -tzip -mm=LZMA "$out/lzma.zip" hello.txt sub
python3.14 -c 'import sys, zipfile
with zipfile.ZipFile(sys.argv[1], "w", zipfile.ZIP_ZSTANDARD) as z:
    for path in sys.argv[2:]:
        z.write(path)' "$out/zstd-python.zip" hello.txt sub sub/far.bin sub/lines.txt
7zz a -tzip -mm=LZMA:eos=off "$out/lzma-no-eos.zip" hello.txt sub
zip -r -e -P pw "$out/zipcrypto-infozip.zip" hello.txt sub
7zz a -tzip -mem=ZipCrypto -ppw "$out/zipcrypto-7zip.zip" hello.txt sub
bsdtar --format zip --options zip:encryption=zipcrypt --passphrase pw \
	-cf "$out/zipcrypto-bsdtar.zip" hello.txt sub
7zz a -tzip -mem=AES128 -ppw "$out/aes128-7zip.zip" hello.txt sub
7zz a -tzip -mem=AES256 -ppw "$out/aes256-7zip.zip" hello.txt sub
bsdtar --format zip --options zip:encryption=aes128 --passphrase pw \
	-cf "$out/aes128-bsdtar.zip" hello.txt sub
bsdtar --format zip --options zip:encryption=aes256 --passphrase pw \
	-cf "$out/aes256-bsdtar.zip" hello.txt sub
zip -r - hello.txt sub | cat > "$out/descriptor-infozip.zip"
cat sub/lines.txt | zip -fz "$out/zip64-infozip.zip" -
cd ..
ditto -c -k --sequesterRsrc --keepParent finder "$out/finder-ditto.zip"
mv "$out"/*.zip "$here"
