#!/bin/sh
set -eu

# Rebuilds libanazoa_tun.so for aarch64-android and the debug APK.
# Usage:
#   scripts/build-android.sh              # rebuild Rust .so + APK
#   scripts/build-android.sh --skip-rust  # only rebuild the APK (Kotlin/res changed, Rust didn't)
#
# Used both for local dev and by .github/workflows/build.yml's `android` job
# — CI sets ANDROID_NDK_HOME/ANDROID_SDK_ROOT itself (from nttld/setup-ndk
# and android-actions/setup-android) rather than relying on the .scratch/
# fallbacks below, which only exist on a machine that set them up by hand.

REPO_ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
ANDROID_DIR="$REPO_ROOT/android"

skip_rust=0
if [ "${1:-}" = "--skip-rust" ]; then
    skip_rust=1
fi

if [ "$skip_rust" = 0 ]; then
    ANDROID_NDK_HOME="${ANDROID_NDK_HOME:-$REPO_ROOT/.scratch/ndk/android-ndk-r27c}"
    TOOLCHAIN="$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/linux-x86_64"
    if [ ! -d "$ANDROID_NDK_HOME" ]; then
        echo "Error: Android NDK not found at $ANDROID_NDK_HOME (set ANDROID_NDK_HOME, or re-download android-ndk-r27c to .scratch/ndk)." >&2
        exit 1
    fi

    export ANDROID_NDK_HOME
    export LK_CUSTOM_WEBRTC="$REPO_ROOT/livekit/webrtc-sys/libwebrtc/android-arm64-release"
    export PATH="$TOOLCHAIN/bin:$PATH"
    export CC_aarch64_linux_android="$TOOLCHAIN/bin/aarch64-linux-android23-clang"
    export CXX_aarch64_linux_android="$TOOLCHAIN/bin/aarch64-linux-android23-clang++"
    export AR_aarch64_linux_android="$TOOLCHAIN/bin/llvm-ar"
    export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$TOOLCHAIN/bin/aarch64-linux-android23-clang"

    (cd "$REPO_ROOT" && cargo build -p anazoa-tun --target aarch64-linux-android --lib --release)

    mkdir -p "$ANDROID_DIR/app/src/main/jniLibs/arm64-v8a"
    cp "$REPO_ROOT/target/aarch64-linux-android/release/libanazoa_tun.so" \
        "$ANDROID_DIR/app/src/main/jniLibs/arm64-v8a/libanazoa_tun.so"

    # NDK's C++ runtime is dynamically linked by default and Android doesn't
    # ship it system-wide (apps are namespace-isolated from the platform's
    # private copy) — every app depending on it has to bundle its own.
    # Without this, libanazoa_tun.so fails to dlopen with "library
    # libc++_shared.so not found", which manifests as NoClassDefFoundError
    # everywhere TunnelNative is touched.
    cp "$TOOLCHAIN/sysroot/usr/lib/aarch64-linux-android/libc++_shared.so" \
        "$ANDROID_DIR/app/src/main/jniLibs/arm64-v8a/libc++_shared.so"

    # android/app/libs/libwebrtc.jar is committed (so Kotlin-only --skip-rust
    # builds work on a fresh clone without the full libwebrtc-android
    # toolchain), but it must stay in lockstep with the .so above — a
    # mismatched org.webrtc.* Java API against a newer/older native library
    # fails at runtime (UnsatisfiedLinkError/NoSuchMethodError), not at build
    # time. Whenever a fresh jar is available next to the .a we just linked
    # against, prefer it over the committed one so the two can never drift.
    LIBWEBRTC_JAR="$LK_CUSTOM_WEBRTC/libwebrtc.jar"
    if [ -f "$LIBWEBRTC_JAR" ]; then
        cp "$LIBWEBRTC_JAR" "$ANDROID_DIR/app/libs/libwebrtc.jar"
    fi
fi

export ANDROID_SDK_ROOT="${ANDROID_SDK_ROOT:-$REPO_ROOT/.scratch/android-sdk}"
(cd "$ANDROID_DIR" && ./gradlew :app:assembleDebug --no-daemon --quiet)

echo "APK: $ANDROID_DIR/app/build/outputs/apk/debug/app-debug.apk"
