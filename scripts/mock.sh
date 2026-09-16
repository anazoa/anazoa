#!/bin/sh
set -eux

export LK_CUSTOM_WEBRTC=$(pwd)/livekit/webrtc-sys/libwebrtc/linux-x64-release

# export LK_DEBUG_WEBRTC=true
# export LK_CUSTOM_WEBRTC=$(pwd)/livekit/webrtc-sys/libwebrtc/linux-x64-debug

# webrtc-sys (libwebrtc m150+) compiles against libwebrtc's hermetic libc++ and
# needs a clang matching the toolchain libwebrtc was built with (clang 21+). The
# WebRTC checkout ships exactly that compiler; honour a pre-set CC/CXX otherwise.
CR_CLANG="$(pwd)/livekit/webrtc-sys/libwebrtc/src/third_party/llvm-build/Release+Asserts/bin"
if [ -x "$CR_CLANG/clang++" ]; then
    export CC="${CC:-$CR_CLANG/clang}"
    export CXX="${CXX:-$CR_CLANG/clang++}"
fi

# cargo clean -p webrtc-sys --release
cargo build --release
sudo target/release/mock test --media test/sample.opus --keep-logs

# Disconnects and reconnects the caller mid-test (via reconnect-host, see
# mock/src/bin/reconnect_host.rs) and places a second call — reproduces
# Android's nativeStart/nativeStop reuse of process-global statics
# (TUNNEL_STATE, SHUTDOWN_TX, ...), which the plain run above never touches
# since each peer there is a single-session, fresh process.
sudo target/release/mock test --media test/sample.opus --keep-logs --reconnect

# The netem is applied to both veth interfaces symmetrically right after they
# come up, before TURN and anazoa start — so packet loss hits everything:
# DTLS handshake, ICE keepalives, VP9/Opus RTP, and the OneMe TLS connect
# (which is what the harness's wait-for-idle before answer/call is for).
# Covers fragment-reassembly expiry and duplicate-fragment handling in
# protozoa::tunnel that a clean link never exercises.
#
# Other expressions worth trying by hand:
#   mild, should adapt slowly:                 --netem "loss 1%"
#   aggressive, should trigger rapid downscale: --netem "loss 5%"
#   milder loss with RTT (affects GCC probing): --netem "loss 2% delay 30ms"
sudo target/release/mock test --media test/sample.opus --keep-logs --netem "loss 5% delay 30ms"
