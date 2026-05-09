#!/bin/sh

ffmpeg \
  -i caller-session.webm -i calltaker-session.webm \
  -filter_complex "
    [0:v][1:v]hstack=inputs=2[v];
    [0:a][1:a]amerge=inputs=2,pan=stereo|c0<c0|c1<c1[a]
  " \
  -map "[v]" -map "[a]" \
  -f nut pipe:1 | ffplay -
