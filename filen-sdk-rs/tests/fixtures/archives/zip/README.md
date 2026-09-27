# Zip fixtures from real tools

Zips made by the tools people actually use, which the SDK's own writer and the `zip` crate
never produce: methods 9 (Deflate64), 12 (bzip2), 14 (LZMA, with and without an end marker),
95 (XZ) and 98 (PPMd, unsupported), ZipCrypto from three writers (Info-ZIP's with the data
descriptor check byte), WinZip AES as AE-1 (libarchive) and AE-2 (7-Zip), a Finder-style
zip with unflagged UTF-8 names, a symlink and `__MACOSX` entries, streamed data descriptors,
Info-ZIP's zip64 records, and an archive comment followed by trailing bytes.

`src/fs/archive/zip/read/tests.rs` reads each one through the zip reader and compares what
comes out with contents it generates the same way the script below does, so the manifest is
the script itself: the password is `pw` wherever there is one. None of these tools writes an
OS X (19) version-made-by host, so the tests patch that into the Info-ZIP symlink's record.

Made on macOS 26.5 with Info-ZIP Zip 3.0 (`/usr/bin/zip`), `ditto`, bsdtar 3.5.3 (libarchive
3.7.4) and 7-Zip 26.01 (`7zz`, Homebrew). Running the script again gives the same contents,
not the same bytes: encryption salts and headers are random, and `ditto` stamps `__MACOSX`
directories with the current time.

```sh
# Run in an empty directory on macOS; writes the fixtures to ./out.
set -eu
export TZ=UTC
mkdir -p src/sub finder/Café out
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
PY
printf 'bonjour\n' > 'finder/Café/Résumé.txt'
printf 'naive\n' > finder/naïve.txt
ln -s naïve.txt finder/link
xattr -w com.filen.fixture tagged finder/naïve.txt
find src finder -exec touch -h -t 202401020304.05 {} +
O=../out
cd src
for method in Deflate64 BZip2 XZ PPMd; do
	7zz a -tzip -mm=$method $O/$(echo $method | tr '[:upper:]' '[:lower:]').zip hello.txt sub
done
7zz a -tzip -mm=LZMA $O/lzma.zip hello.txt sub
7zz a -tzip -mm=LZMA:eos=off $O/lzma-no-eos.zip hello.txt sub
zip -r -e -P pw $O/zipcrypto-infozip.zip hello.txt sub
7zz a -tzip -mem=ZipCrypto -ppw $O/zipcrypto-7zip.zip hello.txt sub
bsdtar --format zip --options zip:encryption=zipcrypt --passphrase pw -cf $O/zipcrypto-bsdtar.zip hello.txt sub
7zz a -tzip -mem=AES128 -ppw $O/aes128-7zip.zip hello.txt sub
7zz a -tzip -mem=AES256 -ppw $O/aes256-7zip.zip hello.txt sub
bsdtar --format zip --options zip:encryption=aes128 --passphrase pw -cf $O/aes128-bsdtar.zip hello.txt sub
bsdtar --format zip --options zip:encryption=aes256 --passphrase pw -cf $O/aes256-bsdtar.zip hello.txt sub
zip -r - hello.txt sub | cat > $O/descriptor-infozip.zip
cat sub/lines.txt | zip -fz $O/zip64-infozip.zip -
echo "a zip comment" | zip -z $O/comment-trailing.zip hello.txt
printf 'trailing bytes after the comment\n' >> $O/comment-trailing.zip
ln -s hello.txt link
touch -h -t 202401020304.05 link
zip -y $O/symlink-infozip.zip hello.txt link
7zz a -tzip -snl $O/symlink-7zip.zip hello.txt link
cd ..
ditto -c -k --sequesterRsrc --keepParent finder out/finder-ditto.zip
```
