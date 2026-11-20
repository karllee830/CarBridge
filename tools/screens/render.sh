#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2026 CarBridge contributors
# SPDX-License-Identifier: AGPL-3.0-only
#
# Renders the head unit state screens of screens.html into the clips CarBridge ships:
#   openwrt/carbridge/files/NAME.h264, NAME.h265                     landscape, 1280x720
#   openwrt/carbridge/files/NAME-portrait.h264, NAME-portrait.h265   portrait, 720x1280
#
# Usage: tools/screens/render.sh [NAME...]
#   With no NAME, renders every screen listed in SCREENS below. Works from any directory.
#   Needs Google Chrome (set CHROME to use another binary) and ffmpeg with libx264 and libx265.
#
# Each clip is 30 frames at 30 fps that the head unit loops: one key frame, then 29 P frames, no B frames.
#   H.264: Constrained Baseline, level 3.1, an access unit delimiter before every frame, BT.709 matrix.
#   H.265: Main, level 3.1, VPS, SPS and PPS before the IDR frame, BT.709 primaries, transfer and matrix.
# An animated screen has 30 rendered frames, a still one a single frame repeated.
set -euo pipefail

TOP=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
PAGE=$TOP/tools/screens/screens.html
OUT=$TOP/openwrt/carbridge/files
CHROME=${CHROME:-"/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"}
SCREENS="unpaired pairing connecting unavailable unavailable-mfi unavailable-bluetooth unavailable-network"
ANIMATED=" pairing connecting "
# orientation:size:file name suffix
LAYOUTS="landscape:1280x720: portrait:720x1280:-portrait"
FRAMES=30

if [[ $# -gt 0 ]]; then
    for name in "$@"; do
        if [[ " $SCREENS " != *" $name "* ]]; then
            echo "unknown screen: $name (known: $SCREENS)" >&2
            exit 1
        fi
    done
    SCREENS="$*"
fi

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

shot() {  # QUERY WxH OUT.png
    "$CHROME" --headless=new --hide-scrollbars --force-device-scale-factor=1 --user-data-dir="$TMP/profile" \
        --window-size="${2/x/,}" --screenshot="$3" "file://$PAGE?$1" >/dev/null 2>&1
    [[ -s $3 ]] || { echo "Chrome made no screenshot for $1" >&2; exit 1; }
}

VF="scale=out_color_matrix=bt709:out_range=tv,format=yuv420p"

for screen in $SCREENS; do
    for layout in $LAYOUTS; do
        IFS=: read -r orientation size suffix <<< "$layout"
        dir=$TMP/$screen$suffix
        mkdir -p "$dir"
        query="screen=$screen&orientation=$orientation"
        if [[ $ANIMATED == *" $screen "* ]]; then
            for ((f = 0; f < FRAMES; f++)); do
                shot "$query&frame=$f" "$size" "$(printf '%s/%02d.png' "$dir" "$f")"
            done
        else
            shot "$query&frame=0" "$size" "$dir/00.png"
            for ((f = 1; f < FRAMES; f++)); do cp "$dir/00.png" "$(printf '%s/%02d.png' "$dir" "$f")"; done
        fi

        name=$screen$suffix
        ffmpeg -v error -y -framerate 30 -i "$dir/%02d.png" -frames:v $FRAMES -vf "$VF" \
            -c:v libx264 -profile:v baseline -level 3.1 -preset veryslow -crf 14 -tune stillimage \
            -g $FRAMES -keyint_min $FRAMES -sc_threshold 0 -bf 0 -x264-params aud=1 \
            -colorspace bt709 -color_range tv -f h264 "$OUT/$name.h264"
        ffmpeg -v error -y -framerate 30 -i "$dir/%02d.png" -frames:v $FRAMES -vf "$VF" \
            -c:v libx265 -profile:v main -preset slow -crf 16 \
            -x265-params "level-idc=3.1:keyint=$FRAMES:min-keyint=$FRAMES:scenecut=0:bframes=0:info=0:log-level=error:colorprim=bt709:transfer=bt709:colormatrix=bt709:range=limited" \
            -f hevc "$OUT/$name.h265"
        echo "$name.h264 $(wc -c < "$OUT/$name.h264") bytes, $name.h265 $(wc -c < "$OUT/$name.h265") bytes"
    done
done
