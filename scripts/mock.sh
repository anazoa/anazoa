#!/bin/sh
set -eux

export LK_CUSTOM_WEBRTC=livekit/webrtc-sys/libwebrtc/linux-x64-release

# cargo clean -p webrtc-sys --release
cargo build --release
sudo target/release/mock test --media test/sample.opus --keep-logs

# The netem is applied to both veth interfaces symmetrically right after they
# come up, before TURN and anazoa start — so packet loss hits everything:
# DTLS handshake, ICE keepalives, and VP9/Opus RTP.
#
# mild, should adapt slowly
# --netem "loss 1%"
#
# aggressive, should trigger rapid downscale
# --netem "loss 5%"
#
# adds RTT which affects GCC's probing
# --netem "loss 2% delay 30ms"
