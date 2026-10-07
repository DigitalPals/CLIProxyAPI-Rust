#!/bin/sh
# Renders the app icons in ui/icons from the bracketed fuse mark (assets/icon.svg) with
# headless Chromium. The mark sits on the page colour, snapped to its 8-unit grid so the
# edges stay sharp. Maskable icons keep the mark well inside the 80% safe zone.
#
#   scripts/render-icons.sh            (needs chromium or google-chrome on PATH)
set -eu
cd "$(dirname "$0")/.."
browser=$(command -v chromium || command -v chromium-browser || command -v google-chrome)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# name size cell: the mark is 6 x 7 cells of 8 units, so its pixel size is 6c x 7c.
render() {
  name=$1 size=$2 cell=$3
  w=$((cell * 6)) h=$((cell * 7))
  x=$(((size - w) / 2)) y=$(((size - h) / 2))
  cat > "$work/$name.svg" <<SVG
<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 $size $size" width="$size" height="$size">
<rect width="$size" height="$size" fill="#0b0b0a"/>
<g transform="translate($x $y) scale($cell) scale(0.125)">
<path fill="#F3B52F" d="M0 0H16V8H8V48H16V56H0ZM48 0H32V8H40V48H32V56H48Z"/>
<path fill="#F4F4F5" d="M16 24H32V32H16Z"/>
</g>
</svg>
SVG
  "$browser" --headless --disable-gpu --hide-scrollbars --force-device-scale-factor=1 \
    --window-size="$size,$size" --screenshot="$PWD/ui/icons/$name.png" "file://$work/$name.svg" 2>/dev/null
}

render icon-192 192 14
render icon-512 512 36
render maskable-512 512 30
render apple-touch-icon 180 12
