#!/usr/bin/env bash
set -euo pipefail

input="${1:-/tmp/herolocality.m4v}"
output="${2:-/tmp/herolocality-onboarding.mp4}"

ffmpeg -y -i "${input}" \
  -map 0:v:0 -an -sn \
  -vf "scale='min(960,iw)':-2:flags=lanczos,fps=30,format=yuv420p" \
  -c:v libx264 \
  -preset slow \
  -crf 21 \
  -profile:v main \
  -level 3.1 \
  -tag:v avc1 \
  -movflags +faststart \
  "${output}"

ffprobe -v error \
  -select_streams v:0 \
  -show_entries stream=codec_name,profile,width,height,pix_fmt,avg_frame_rate:format=duration,size,format_name \
  -of default=noprint_wrappers=1 \
  "${output}"
