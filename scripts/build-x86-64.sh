#!/bin/sh
set -eu

REPO_ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
export LK_CUSTOM_WEBRTC="$REPO_ROOT/livekit/webrtc-sys/libwebrtc/linux-x64-release"

# webrtc-sys (libwebrtc m150+) compiles against libwebrtc's hermetic libc++ and
# needs a clang matching the toolchain libwebrtc was built with (clang 21+). The
# WebRTC checkout ships exactly that compiler; honour a pre-set CC/CXX otherwise.
CR_CLANG="$REPO_ROOT/livekit/webrtc-sys/libwebrtc/src/third_party/llvm-build/Release+Asserts/bin"
if [ -x "$CR_CLANG/clang++" ]; then
    export CC="${CC:-$CR_CLANG/clang}"
    export CXX="${CXX:-$CR_CLANG/clang++}"
fi

cargo build --release
