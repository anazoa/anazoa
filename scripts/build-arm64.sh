#!/bin/sh
set -eu

# Cross-compile for aarch64 from an x86_64 host. Prerequisites:
#   sudo dpkg --add-architecture arm64 && sudo apt update
#   sudo apt install gcc-aarch64-linux-gnu g++-aarch64-linux-gnu libopus-dev:arm64
#   rustup target add aarch64-unknown-linux-gnu

REPO_ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
export LK_CUSTOM_WEBRTC="$REPO_ROOT/livekit/webrtc-sys/libwebrtc/linux-arm64-release"

export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc
export PKG_CONFIG_ALLOW_CROSS=1
export PKG_CONFIG_SYSROOT_DIR=/
export PKG_CONFIG_PATH=/usr/lib/aarch64-linux-gnu/pkgconfig

# webrtc-sys (libwebrtc m150+) compiles its cxx bridge against libwebrtc's
# hermetic libc++ and needs a clang matching the toolchain libwebrtc was built
# with (clang 21+). The WebRTC checkout ships exactly that compiler and it
# cross-compiles to aarch64 via --target; honour a pre-set CC/CXX otherwise.
CR_CLANG="$REPO_ROOT/livekit/webrtc-sys/libwebrtc/src/third_party/llvm-build/Release+Asserts/bin"
if [ -x "$CR_CLANG/clang++" ]; then
    export CC="${CC:-$CR_CLANG/clang}"
    export CXX="${CXX:-$CR_CLANG/clang++}"
fi

cargo build --release --target aarch64-unknown-linux-gnu
