# How `pins.rs` was built

The samples come from [raw.pixls.us](https://raw.pixls.us), the CC0 sample
library maintained for darktable/RawTherapee. It replaced rawsamples.ch, which
was CC BY-NC-SA.

Regenerating the table is a curation job, not something to automate into the
test run — the whole point of the pin table is that adding a sample is a
deliberate, reviewed act. The procedure that produced the current table:

1. **Fetch the index.** The site's file list is DataTables-driven, so it is not
   in the page HTML; it comes from `https://raw.pixls.us/json/getrepository.php?set=all`
   (2016 rows at the time of writing). Each row carries make, model, variant,
   size, a licence link, a download link and a published sha256.

2. **Filter to CC0.** Exactly two licence strings appear across the library:
   `publicdomain/zero` (CC0 1.0) and `licenses/by-nc-sa/4.0`. 146 rows are
   CC BY-NC-SA and are excluded. That the field discriminates is what makes it
   usable as provenance — it is not a constant.

3. **Select.** Ten per format, smallest first, one per camera model. Smallest
   first keeps the cache small and is harmless here: container structure, not
   pixel count, is what these tests exercise. One per model buys vendor and
   generation spread, which is what actually varies the IFD layout.

4. **Check size before committing to the picks.** `Content-Length` on each
   candidate, summed, against a ~2 GB ceiling. The current set is
   1,046,599,886 bytes (0.97 GiB) for 100 files, so no format had to be
   trimmed.

5. **Download once and verify against the published sha256.** A supply-chain
   double-check: the pinned BLAKE3 is computed from bytes that already matched
   the hash the library publishes independently.

6. **Sniff the container magic and compare it to the extension.** This is
   worth doing rather than trusting the file name. Two candidates were dropped
   because the two disagreed:
   - `Canon - PowerShot S2 IS - 10bit 10bit DNG, CHDK ver. 1.0.0-1504 (4:3).CR2`
     — a CHDK-produced DNG wearing a `.CR2` extension.
   - `Samsung - SM-G973U - 16bit 16bit (2.1132075471698).dng` — a plain JPEG
     (`FFD8FFE0 ... JFIF`) wearing a `.dng` extension.

   Both would have quietly mischaracterised their format. The `container`
   column records what the bytes actually say.

7. **Emit the table**, sorted by format then size.

## Adding a sample

Pin the exact URL, the BLAKE3 of the bytes you verified, the exact length, and
the container magic you observed. Confirm the licence on the library's own row
for that file. `pins_are_well_formed` and `all_fixtures_are_cc0` run without
needing the bytes and will catch the mechanical mistakes.

## Cache location

`../.fixture-cache/raw` relative to the crate, i.e. the workspace root —
outside `target/`, so `cargo clean` does not cost a 1 GiB re-download.
Gitignored; the samples are never committed. Override with
`MICROTHUMB_RAW_FIXTURE_DIR`. `MICROTHUMB_RAW_FIXTURES=offline` uses only what
is already cached.

# How `heif_pins.rs` was built

The HEIF files come from the test corpora of open-source projects, not from a
sample library: no library like raw.pixls.us collects HEIF (its index holds
none — it is a RAW library). This repository is AGPL-3.0, so a file is usable
when the project that commits it licenses it under AGPL-3.0 or anything
compatible with it: GPL-2.0-or-later, LGPL, MIT, BSD, Apache-2.0, CC0, CC-BY,
CC-BY-SA. A file with no licence at all (an attachment on an issue) or under
non-commercial terms is not.

1. **Search.** Photo managers (immich, PhotoPrism, Nextcloud, memories,
   LibrePhotos, Lychee), image libraries (libheif, pillow_heif, libvips,
   libavif, ImageMagick, kimageformats), metadata libraries (Exiv2, exiftool,
   metadata-extractor, exif-samples, nom-exif) and raw/camera tools, plus
   repository and code search, for camera- or phone-original `.hif`, `.heic`,
   `.heif` and `.avif` files.

2. **Check the licence where the file lives.** The repository's LICENSE covers
   its committed test data unless a README beside the files says otherwise —
   and some do: PhotoPrism's `assets/` is under separate terms, and
   pillow_heif's `heif_other/nokia/` is Nokia's. Both were excluded. Every
   pinned file's licence is the document at the same commit as the file,
   recorded as `licence_source`.

3. **Check the content.** Make, model and software from the EXIF; the item
   structure from the `meta` box. Converted and encoder-generated files were
   dropped wherever a device original covered the same case; the one exception
   is the AVIF, where none exists (its EXIF is a phone's, its bitstream a
   re-encode).

4. **Pin to a commit.** `raw.githubusercontent.com/<owner>/<repo>/<commit>/...`,
   so the bytes cannot move under the pin, with the length and BLAKE3 taken from
   a fresh download.

The set covers Sony and Canon HIF, a Sony portrait whose JPEG thumbnail
inherits the primary's rotation, two iPhone generations, a Samsung motion photo
and a phone AVIF, about 11 MB in all.

**No Fujifilm HIF is pinned, because none could be found under a usable
licence.** The real ones in circulation are issue attachments with no licence.
The camera-JPEG path Fujifilm files take is pinned by the synthetic
`tests/fixtures/heif/fuji*.heic` instead; a licensed Fujifilm file belongs here
as soon as one exists.

`heif_pins_are_well_formed_and_usably_licensed` checks the table without the
bytes: the commit in every URL, the licence document at that same commit, a
licence from the usable list, and a baseline row for each file.
