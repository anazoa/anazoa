#!/bin/sh
# Generates the synthetic "speech" track the mock tests use in CI, where the
# real test/sample.opus (a LibriVox recording, not committed) isn't
# available. Run as:
#
#   ./scripts/gen-mock-media.sh mock/fixtures/media.opus
#
# Pink-noise bursts (2s on, 1s off) tagged with a VAD map by vad.sh, so the
# media source sees speech fragments like it would in a real recording. It
# loops over those fragments, so a minute of audio covers any test length.
#
# Requires: ffmpeg with libopus
set -eu

out=$1

ffmpeg -hide_banner -loglevel error -y -f lavfi \
    -i "anoisesrc=color=pink:sample_rate=48000:amplitude=0.3:duration=60" \
    -af "volume='if(lt(mod(t,3),2),1,0)':eval=frame" \
    -ac 1 -c:a libopus -b:a 32k "$out"
"$(dirname -- "$0")/vad.sh" "$out"
