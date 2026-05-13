#!/bin/sh
set -eu

filename=$1
metafile=/tmp/meta-$$.txt
copy=/tmp/copy-$$.opus

ffmpeg -hide_banner -loglevel error -i $filename \
  -af "silencedetect=noise=-40dB:duration=0.1,ametadata=print:file=-" \
  -f null /dev/null \
| awk '
    BEGIN { prev=0; first=1; n=0; print ";FFMETADATA1"; printf "vadmap=" }
    /lavfi\.silence_start=/ { split($0, a, "="); sil_s=a[2] }
    /lavfi\.silence_end=/   {
        split($0, a, "="); end_t=a[2]
        if (sil_s+0 > prev+0) {
            printf "%s%d:%d", (first ? "" : ","), int(prev*50), int(sil_s*50)
            first=0; n++
        }
        prev=end_t
    }
    END { print ""; print n " fragments detected" > "/dev/stderr" }' \
> $metafile

ffmpeg -hide_banner -loglevel error \
  -i $filename \
  -i $metafile \
  -map_metadata 1 \
  -c:a copy $copy

rm $metafile
mv $copy $filename
