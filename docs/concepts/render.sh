#!/bin/sh
# Renders every src/NN-*.html mockup to a 2x PNG next to this script.
# Canvas size is read from the `body { width: Wpx; height: Hpx; }` rule in each file.
set -e
cd "$(dirname "$0")"
CHROME="/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
for f in src/[0-9]*.html; do
  name=$(basename "$f" .html)
  w=$(sed -n 's/.*body { width: \([0-9]*\)px; height: \([0-9]*\)px.*/\1/p' "$f")
  h=$(sed -n 's/.*body { width: \([0-9]*\)px; height: \([0-9]*\)px.*/\2/p' "$f")
  "$CHROME" --headless=new --disable-gpu --hide-scrollbars --force-device-scale-factor=2 \
    --window-size="$w,$h" --screenshot="$PWD/$name.png" "file://$PWD/$f" >/dev/null 2>&1
  echo "$name.png (${w}x${h} @2x)"
done
