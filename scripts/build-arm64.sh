#!/bin/sh
set -eu

export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc
export PKG_CONFIG_SYSROOT_DIR=/
export PKG_CONFIG_PATH=/usr/lib/aarch64-linux-gnu/pkgconfig
export LK_CUSTOM_WEBRTC=webrtc-sys/libwebrtc/linux-arm64-release

cargo build --release --target aarch64-unknown-linux-gnu
