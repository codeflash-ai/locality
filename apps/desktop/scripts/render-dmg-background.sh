#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
OUTPUT="${ROOT}/apps/desktop/src-tauri/assets/dmg-background.png"
TMPDIR="$(mktemp -d)"

cleanup() {
  rm -rf "${TMPDIR}"
}
trap cleanup EXIT

if ! command -v magick >/dev/null 2>&1; then
  echo "ImageMagick 'magick' is required to render the DMG background." >&2
  exit 1
fi

mkdir -p "$(dirname "${OUTPUT}")"

TEXT_COLOR="#111827"
NOTE_COLOR="#374151"
ORANGE="#f5a623"

line1_a="${TMPDIR}/line1-a.png"
line1_b="${TMPDIR}/line1-b.png"
line1_c="${TMPDIR}/line1-c.png"
line2="${TMPDIR}/line2.png"
base="${TMPDIR}/base.png"
composed="${TMPDIR}/composed.png"

magick -background none -fill "${TEXT_COLOR}" -font .SF-Compact-Medium -pointsize 25 label:'To install, ' "${line1_a}"
magick -background none -fill "${TEXT_COLOR}" -font .SF-Compact-Medium-Italic -pointsize 25 label:'drag' "${line1_b}"
magick -background none -fill "${TEXT_COLOR}" -font .SF-Compact-Medium -pointsize 25 label:'Locality' "${line1_c}"
magick -background none -fill "${TEXT_COLOR}" -font .SF-Compact-Medium -pointsize 25 label:'to Applications' "${line2}"

line1_a_width="$(magick identify -format '%w' "${line1_a}")"
line1_b_width="$(magick identify -format '%w' "${line1_b}")"
line1_c_width="$(magick identify -format '%w' "${line1_c}")"
line1_height="$(magick identify -format '%h' "${line1_a}")"
line2_width="$(magick identify -format '%w' "${line2}")"
line1_gap=8
line1_width="$((line1_a_width + line1_b_width + line1_gap + line1_c_width))"
line1_x="$(((760 - line1_width) / 2))"
line1_y=48
line2_x="$(((760 - line2_width) / 2))"
line2_y="$((line1_y + line1_height + 4))"

magick -size 760x440 xc:'#fafafa' \
  -stroke "${ORANGE}" -strokewidth 4 -fill none -draw 'path "M 290 270 C 313 294 347 292 364 269 C 381 292 416 291 437 266"' \
  -stroke "${ORANGE}" -strokewidth 4 -fill none -draw 'path "M 419 262 L 439 265 L 428 283"' \
  -stroke none \
  -fill "${NOTE_COLOR}" -font .SF-Compact-Regular -pointsize 15 -gravity south \
  -annotate +0+71 'Then open Locality from Applications.' \
  "${base}"

magick "${base}" \
  "${line1_a}" -geometry "+${line1_x}+${line1_y}" -composite \
  "${line1_b}" -geometry "+$((line1_x + line1_a_width))+${line1_y}" -composite \
  "${line1_c}" -geometry "+$((line1_x + line1_a_width + line1_b_width + line1_gap))+${line1_y}" -composite \
  "${line2}" -geometry "+${line2_x}+${line2_y}" -composite \
  -depth 8 "PNG32:${OUTPUT}"

echo "Rendered ${OUTPUT}"
