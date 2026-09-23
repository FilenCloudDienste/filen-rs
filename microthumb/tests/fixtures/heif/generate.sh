#!/usr/bin/env bash
# Regenerates the HEIF fixtures in this directory.
#
# Nothing in the tree encodes HEVC, so unlike the rest of the suite these are
# built once with standard tools and committed. Tiles are 10-bit 4:2:2 HEVC,
# the profile a Fujifilm HIF carries, unless a fixture says otherwise, at a
# quantiser low enough that the solid colours survive for the tests' colour
# checks.
#
# Needs ffmpeg (with libx265), MP4Box (gpac) and exiftool:
#   brew install ffmpeg gpac exiftool
#
# Built with ffmpeg 9.0.2 / x265 4.3, GPAC 26.07, exiftool 13.55.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

# tile OUT W H COLOUR RIGHT BOTTOM
#
# One solid HEVC tile. RIGHT/BOTTOM paint a magenta band over that many
# columns/rows at the tile's far edge: the pixels a grid overhangs its image
# by, which a correct decode crops away and a misplaced one shows. PIX_FMT and
# PROFILE override the 10-bit 4:2:2 default.
tile() {
	local out=$1 w=$2 h=$3 colour=$4 right=$5 bottom=$6
	local vf=null
	if ((right > 0)); then
		vf="drawbox=x=$((w - right)):y=0:w=$right:h=$h:color=magenta:t=fill"
	fi
	if ((bottom > 0)); then
		[[ $vf == null ]] && vf="" || vf="$vf,"
		vf="${vf}drawbox=x=0:y=$((h - bottom)):w=$w:h=$bottom:color=magenta:t=fill"
	fi
	ffmpeg -loglevel error -y -f lavfi -i "color=c=$colour:s=${w}x$h" -vf "$vf" \
		-frames:v 1 -c:v libx265 -pix_fmt "${PIX_FMT:-yuv422p10le}" \
		-profile:v "${PROFILE:-main422-10}" \
		-x265-params log-level=error:qp=4:keyint=1 -f hevc "$out"
}

# jpeg OUT FILTERGRAPH
#
# A baseline 8-bit JPEG, then stripped of its JFIF APP0 so it starts SOI
# straight into its tables — the bare shape of Fujifilm's thumbnail items.
jpeg() {
	local out=$1 graph=$2
	ffmpeg -loglevel error -y -filter_complex "$graph" -frames:v 1 -c:v mjpeg \
		-q:v 5 -pix_fmt yuvj420p "$out"
	exiftool -q -all= -overwrite_original "$out"
}

# grid OUT W H TILE_W TILE_H GRID_EXTRA [MP4BOX_ARGS...]
#
# A 2x2 grid of red / green / blue / yellow tiles (reading order), cropped to
# WxH, with GRID_EXTRA appended to the grid item's options (a rotation or a
# mirror) and any further MP4Box arguments (thumbnail items) after it.
grid() {
	local out=$1 w=$2 h=$3 tw=$4 th=$5 extra=$6
	shift 6
	local right=$((2 * tw - w)) bottom=$((2 * th - h))
	tile "$work/t1.hevc" "$tw" "$th" red 0 0
	tile "$work/t2.hevc" "$tw" "$th" lime "$right" 0
	tile "$work/t3.hevc" "$tw" "$th" blue 0 "$bottom"
	tile "$work/t4.hevc" "$tw" "$th" yellow "$right" "$bottom"
	rm -f "$here/$out"
	MP4Box -quiet \
		-add-image "$work/t1.hevc:id=1:hidden" \
		-add-image "$work/t2.hevc:id=2:hidden" \
		-add-image "$work/t3.hevc:id=3:hidden" \
		-add-image "$work/t4.hevc:id=4:hidden" \
		-add-derived-image ":type=grid:image-grid-size=2x2:ref=dimg,1:ref=dimg,2:ref=dimg,3:ref=dimg,4:image-size=${w}x$h:id=5:primary$extra" \
		"$@" "$here/$out"
}

# Grids the tile path cannot place, which go to the whole-frame decode: one
# with a centred clean-aperture crop (`clap`) taking 10 pixels off every edge,
# and one whose three 64-wide columns overrun its 128-wide image by a whole
# tile, turned half-way so that the overrunning column leads.
grid grid-clap.heic 120 90 64 48 ":clap=100,1,70,1,0,1,0,1"
tile "$work/o1.hevc" 64 48 red 0 0
tile "$work/o2.hevc" 64 48 lime 0 0
tile "$work/o3.hevc" 64 48 magenta 0 0
rm -f "$here/grid-overrun.heic"
MP4Box -quiet \
	-add-image "$work/o1.hevc:id=1:hidden" \
	-add-image "$work/o2.hevc:id=2:hidden" \
	-add-image "$work/o3.hevc:id=3:hidden" \
	-add-derived-image ":type=grid:image-grid-size=1x3:ref=dimg,1:ref=dimg,2:ref=dimg,3:image-size=128x48:id=5:primary:rotation=180" \
	"$here/grid-overrun.heic"

# Tile placement: 64x48 tiles cropped to 120x90 overhang the image by 8
# columns and 6 rows. As coded that overhang is on the right and bottom; each
# transform below carries it to the left and/or top of the displayed image.
# (MP4Box takes `rotation` in degrees, counter-clockwise.)
grid grid-irot0.heic 120 90 64 48 ""
grid grid-irot1.heic 120 90 64 48 ":rotation=90"
grid grid-irot2.heic 120 90 64 48 ":rotation=180"
grid grid-irot3.heic 120 90 64 48 ":rotation=270"
grid grid-imir0.heic 120 90 64 48 ":mirror-axis=vertical"
grid grid-imir1.heic 120 90 64 48 ":mirror-axis=horizontal"

# The same grid in 10-bit 4:2:0: deeper samples, but chroma subsampled both
# ways, which costs barely more to decode than 8 bits.
PIX_FMT=yuv420p10le PROFILE=main10 grid grid-10bit-420.heic 120 90 64 48 ""

# The same grid in 8-bit 4:2:0, as an iPhone writes its HEICs: the decode
# charge follows the bit depth.
PIX_FMT=yuv420p PROFILE=main grid grid-8bit.heic 120 90 64 48 ""

# A container that lies: four tiles declared 64x64 whose bitstreams decode at
# 1024x1024.
tile "$work/big.hevc" 1024 1024 red 0 0
rm -f "$here/lying-tiles.heic"
MP4Box -quiet \
	-add-image "$work/big.hevc:id=1:hidden:image-size=64x64" \
	-add-image "$work/big.hevc:id=2:hidden:image-size=64x64" \
	-add-image "$work/big.hevc:id=3:hidden:image-size=64x64" \
	-add-image "$work/big.hevc:id=4:hidden:image-size=64x64" \
	-add-derived-image ":type=grid:image-grid-size=2x2:ref=dimg,1:ref=dimg,2:ref=dimg,3:ref=dimg,4:image-size=128x128:id=5:primary" \
	"$here/lying-tiles.heic"

# Fujifilm-shaped: the HEVC grid plus three JPEG thumbnail items, as an X-series
# HIF carries them — a large one in the frame's own 3:2 aspect, a 4:3 one
# letterboxed onto it, and a tiny 4:3 stamp. The large one's quadrants are
# white / black / cyan / orange (reading order), so a test can tell which
# image served and which way up.
jpeg "$work/large.jpg" "color=white:s=384x256[a];color=black:s=384x256[b];color=cyan:s=384x256[c];color=orange:s=384x256[d];[a][b]hstack[t];[c][d]hstack[u];[t][u]vstack"
jpeg "$work/letterbox.jpg" "color=gray:s=640x426,pad=640:480:0:27:black"
jpeg "$work/stamp.jpg" "color=gray:s=160x120"
large="$work/large.jpg:id=6:type=jpeg:image-size=768x512:ref=thmb,5"
letterbox="$work/letterbox.jpg:id=7:type=jpeg:image-size=640x480:ref=thmb,5"
stamp="$work/stamp.jpg:id=8:type=jpeg:image-size=160x120:ref=thmb,5"

grid fuji.heic 120 80 64 44 "" -add-item "$large" -add-item "$letterbox" -add-item "$stamp"
# A portrait GFX shot: the grid rotated, its thumbnail stored sideways and
# carrying no orientation of its own.
grid fuji-irot1.heic 120 80 64 44 ":rotation=90" -add-item "$large" -add-item "$letterbox" -add-item "$stamp"
# Only JPEGs a thumbnail must not come from: the letterboxed one and the stamp.
grid jpeg-thumbs-unusable.heic 120 80 64 44 "" -add-item "$letterbox" -add-item "$stamp"

# An HEVC thumbnail item, as Apple, Sony and Canon write them.
ffmpeg -loglevel error -y -f lavfi -i "color=c=purple:s=160x106" -frames:v 1 \
	-c:v libx265 -pix_fmt yuv422p10le -profile:v main422-10 \
	-x265-params log-level=error:qp=4:keyint=1 -f hevc "$work/hevc-thumb.hevc"
grid hevc-thumb.heic 120 80 64 44 "" -add-image "$work/hevc-thumb.hevc:id=6:ref=thmb,5"
