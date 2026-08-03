#!/usr/bin/env bash
set -euo pipefail
OUT="${1:?usage: gen-partial-idr-gate-fixtures.sh <out_dir>}"
mkdir -p "$OUT"
FF=(-loglevel error -y -an -fflags +bitexact -bitexact -c:v libx264 -preset ultrafast -pix_fmt yuv420p -sc_threshold 0)

ffmpeg -f lavfi -i testsrc2=size=640x360:rate=30 -t 60 \
  -g 30 -keyint_min 30 "${FF[@]}" "$OUT/synth_g30_source.mp4"

ffmpeg -i "$OUT/synth_g30_source.mp4" -ss 10 -t 30 \
  -g 250 -keyint_min 250 "${FF[@]}" "$OUT/synth_g250_clip.mp4"

echo "wrote: $OUT/synth_g30_source.mp4 $OUT/synth_g250_clip.mp4"
