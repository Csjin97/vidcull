#!/usr/bin/env bash
set -euo pipefail
OUT="${1:?usage: gen-partial-idr-jitter-fixtures.sh <out_dir>}"
mkdir -p "$OUT"
FF=(-loglevel error -y -an -fflags +bitexact -bitexact -c:v libx264 -preset ultrafast -pix_fmt yuv420p -sc_threshold 0)

ffmpeg -f lavfi -i testsrc2=size=640x360:rate=30 -t 60 \
  -g 74 -keyint_min 74 "${FF[@]}" "$OUT/jitter_source.mp4"

ffmpeg -i "$OUT/jitter_source.mp4" -ss 13.7 -t 20 \
  -g 53 -keyint_min 53 "${FF[@]}" "$OUT/jitter_clip.mp4"

ffmpeg -f lavfi -i mandelbrot=size=640x360:rate=30 -t 90 \
  -g 75 -keyint_min 75 "${FF[@]}" "$OUT/fp_adversary.mp4"

echo "wrote: $OUT/jitter_source.mp4 $OUT/jitter_clip.mp4 $OUT/fp_adversary.mp4"
