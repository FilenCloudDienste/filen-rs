#!/usr/bin/env bash
# Regenerates the HEIF fixtures in this directory.
#
# Nothing in the tree encodes HEVC, so unlike the rest of the suite these are
# built once with standard tools and committed. Every tile is 10-bit 4:2:2
# HEVC, the profile a Fujifilm HIF carries, at a quantiser low enough that the
# solid colours survive for the tests' colour checks.
#
# Needs ffmpeg (with libx265) and MP4Box (gpac):
#   brew install ffmpeg gpac
#
# Built with ffmpeg 9.0.2 / x265 4.3 and GPAC 26.07.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

# tile OUT W H COLOUR RIGHT BOTTOM
#
# One solid HEVC tile. RIGHT/BOTTOM paint a magenta band over that many
# columns/rows at the tile's far edge: the pixels a grid overhangs its image
# by, which a correct decode crops away and a misplaced one shows.
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
		-frames:v 1 -c:v libx265 -pix_fmt yuv422p10le -profile:v main422-10 \
		-x265-params log-level=error:qp=4:keyint=1 -f hevc "$out"
}

# grid OUT W H TILE_W TILE_H GRID_EXTRA
#
# A 2x2 grid of red / green / blue / yellow tiles (reading order), cropped to
# WxH, with GRID_EXTRA appended to the grid item's options (a rotation or a
# mirror).
grid() {
	local out=$1 w=$2 h=$3 tw=$4 th=$5 extra=$6
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
		"$here/$out"
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
