#!/bin/sh
set -eu

export LK_CUSTOM_WEBRTC=livekit/webrtc-sys/libwebrtc/linux-x64-release

cargo build --release
