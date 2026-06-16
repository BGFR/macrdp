#!/bin/bash
# Generate an AppIcon.icns from a square (ideally 1024×1024) PNG.
# Usage: make-icns.sh <source.png> <out.icns>
set -euo pipefail

SRC="${1:?usage: make-icns.sh <source.png> <out.icns>}"
OUT="${2:?usage: make-icns.sh <source.png> <out.icns>}"
[ -f "$SRC" ] || { echo "no such icon source: $SRC" >&2; exit 1; }

WORK="$(mktemp -d)"
ICONSET="$WORK/AppIcon.iconset"
mkdir -p "$ICONSET"
trap 'rm -rf "$WORK"' EXIT

# Standard macOS iconset: 16/32/128/256/512 at 1x and 2x.
for sz in 16 32 128 256 512; do
    sips -z "$sz" "$sz" "$SRC" --out "$ICONSET/icon_${sz}x${sz}.png" >/dev/null
    sips -z "$((sz * 2))" "$((sz * 2))" "$SRC" --out "$ICONSET/icon_${sz}x${sz}@2x.png" >/dev/null
done

iconutil -c icns "$ICONSET" -o "$OUT"
echo "wrote $OUT"
