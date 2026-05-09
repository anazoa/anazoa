#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Build tcpdump for Android using either local sources or downloaded inputs.

Usage:
  build-android-tcpdump.sh \
    [--legacy] \
    [--ndk /path/to/android-ndk] \
    [--libpcap-src /path/to/libpcap-1.10.6] \
    [--tcpdump-src /path/to/tcpdump-4.99.5] \
    [--ndk-version r27d] \
    [--libpcap-version 1.10.6] \
    [--tcpdump-version 4.99.5] \
    [--ndk-url https://...] \
    [--libpcap-url https://...] \
    [--tcpdump-url https://...] \
    [--api 30] \
    [--arch arm64] \
    [--download-dir /tmp/tcpdump-android-downloads] \
    [--work-dir /tmp/tcpdump-android-build] \
    [--prefix /tmp/tcpdump-android-prefix] \
    [--output /path/to/output/tcpdump]

Notes:
  - This script intentionally disables libpcap PACKET_RX_RING support so the
    resulting tcpdump avoids TPACKET mmap capture paths that fail on some
    Android kernels with "can't mmap rx ring: Invalid argument".
  - `--legacy` selects a libpcap/tcpdump pair that still supports Linux
    socket-read fallback when PACKET_RX_RING is disabled:
      libpcap 1.9.1 + tcpdump 4.9.3
  - If local paths are omitted, the script downloads the Android NDK plus the
    libpcap and tcpdump release tarballs into a cache directory.
  - The script assumes a Linux host because it uses the NDK's
    linux-x86_64 toolchain package.
EOF
}

die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

require_cmd() {
  command -v "$1" >/dev/null 2>&1 || die "required command not found: $1"
}

download_file() {
  local url="$1"
  local out="$2"

  mkdir -p "$(dirname "$out")"

  if [[ -f "$out" ]]; then
    printf '==> Reusing cached download: %s\n' "$out" >&2
    return 0
  fi

  if command -v curl >/dev/null 2>&1; then
    curl -fL --retry 3 --retry-delay 1 -o "$out" "$url"
  elif command -v wget >/dev/null 2>&1; then
    wget -O "$out" "$url"
  else
    die "need curl or wget to download $url"
  fi
}

archive_root_name() {
  local archive_name="$1"
  local first_entry=""

  case "$archive_name" in
    *.tar.gz|*.tgz|*.tar.xz|*.tar.bz2)
      first_entry="$(tar -tf "$archive_name" | sed -n '1p')"
      ;;
    *.zip)
      first_entry="$(unzip -Z -1 "$archive_name" | sed -n '1p')"
      ;;
  esac

  if [[ -n "$first_entry" ]]; then
    first_entry="${first_entry%%/*}"
    printf '%s\n' "$first_entry"
    return 0
  fi

  archive_name="${archive_name##*/}"
  archive_name="${archive_name%.tar.gz}"
  archive_name="${archive_name%.tgz}"
  archive_name="${archive_name%.tar.xz}"
  archive_name="${archive_name%.tar.bz2}"
  archive_name="${archive_name%.zip}"
  printf '%s\n' "$archive_name"
}

extract_archive() {
  local archive="$1"
  local dest_dir="$2"
  local root_name

  root_name="$(archive_root_name "$archive")"
  mkdir -p "$dest_dir"

  if [[ -d "$dest_dir/$root_name" ]]; then
    printf '==> Reusing extracted tree: %s\n' "$dest_dir/$root_name" >&2
    printf '%s\n' "$dest_dir/$root_name"
    return 0
  fi

  case "$archive" in
    *.tar.gz|*.tgz|*.tar.xz|*.tar.bz2)
      tar -xf "$archive" -C "$dest_dir"
      ;;
    *.zip)
      unzip -q "$archive" -d "$dest_dir"
      ;;
    *)
      die "unsupported archive format: $archive"
      ;;
  esac

  [[ -d "$dest_dir/$root_name" ]] || die "expected extracted directory not found: $dest_dir/$root_name"
  printf '%s\n' "$dest_dir/$root_name"
}

abspath() {
  local path="$1"
  if [[ -d "$path" ]]; then
    (cd "$path" && pwd -P)
  else
    (cd "$(dirname "$path")" && printf '%s/%s\n' "$(pwd -P)" "$(basename "$path")")
  fi
}

copy_source_tree() {
  local src="$1"
  local dst="$2"
  rm -rf "$dst"
  mkdir -p "$(dirname "$dst")"
  cp -a "$src" "$dst"
}

resolve_input_dir() {
  local name="$1"
  local existing_path="$2"
  local url="$3"
  local downloads_root="$4"
  local extract_root="$5"

  if [[ -n "$existing_path" ]]; then
    [[ -d "$existing_path" ]] || die "$name directory does not exist: $existing_path"
    abspath "$existing_path"
    return 0
  fi

  [[ -n "$url" ]] || die "missing $name path and download URL"

  local archive="$downloads_root/${url##*/}"
  download_file "$url" "$archive" >&2
  extract_archive "$archive" "$extract_root"
}

patch_disable_packet_ring() {
  local config_h="$1"

  [[ -f "$config_h" ]] || die "missing config.h: $config_h"

  if grep -q '^#define HAVE_PACKET_RING 1$' "$config_h"; then
    perl -0pi -e 's/^#define HAVE_PACKET_RING 1$/\/\* #undef HAVE_PACKET_RING \*\//m' "$config_h"
  fi

  if grep -q '^#define HAVE_TPACKET_STATS 1$' "$config_h"; then
    perl -0pi -e 's/^#define HAVE_TPACKET_STATS 1$/\/\* #undef HAVE_TPACKET_STATS \*\//m' "$config_h"
  fi

  if grep -q '^#define PCAP_SUPPORT_PACKET_RING 1$' "$config_h"; then
    perl -0pi -e 's/^#define PCAP_SUPPORT_PACKET_RING 1$/\/\* #undef PCAP_SUPPORT_PACKET_RING \*\//m' "$config_h"
  fi
}

ARCH="arm64"
API="30"
WORK_DIR=""
PREFIX=""
OUTPUT=""
DOWNLOAD_DIR=""
NDK=""
LIBPCAP_SRC=""
TCPDUMP_SRC=""
NDK_VERSION="r27d"
LIBPCAP_VERSION="1.10.6"
TCPDUMP_VERSION="4.99.5"
NDK_URL=""
LIBPCAP_URL=""
TCPDUMP_URL=""
LEGACY_PRESET="0"
BASE_CFLAGS="-fPIE -O2"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --legacy)
      LEGACY_PRESET="1"
      shift
      ;;
    --ndk)
      NDK="${2:-}"
      shift 2
      ;;
    --libpcap-src)
      LIBPCAP_SRC="${2:-}"
      shift 2
      ;;
    --tcpdump-src)
      TCPDUMP_SRC="${2:-}"
      shift 2
      ;;
    --ndk-version)
      NDK_VERSION="${2:-}"
      shift 2
      ;;
    --libpcap-version)
      LIBPCAP_VERSION="${2:-}"
      shift 2
      ;;
    --tcpdump-version)
      TCPDUMP_VERSION="${2:-}"
      shift 2
      ;;
    --ndk-url)
      NDK_URL="${2:-}"
      shift 2
      ;;
    --libpcap-url)
      LIBPCAP_URL="${2:-}"
      shift 2
      ;;
    --tcpdump-url)
      TCPDUMP_URL="${2:-}"
      shift 2
      ;;
    --api)
      API="${2:-}"
      shift 2
      ;;
    --arch)
      ARCH="${2:-}"
      shift 2
      ;;
    --download-dir)
      DOWNLOAD_DIR="${2:-}"
      shift 2
      ;;
    --work-dir)
      WORK_DIR="${2:-}"
      shift 2
      ;;
    --prefix)
      PREFIX="${2:-}"
      shift 2
      ;;
    --output)
      OUTPUT="${2:-}"
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      die "unknown argument: $1"
      ;;
  esac
done

case "$ARCH" in
  arm64|aarch64)
    TARGET_TRIPLE="aarch64-linux-android"
    ;;
  armv7|arm)
    TARGET_TRIPLE="armv7a-linux-androideabi"
    ;;
  x86)
    TARGET_TRIPLE="i686-linux-android"
    ;;
  x86_64|amd64)
    TARGET_TRIPLE="x86_64-linux-android"
    ;;
  *)
    die "unsupported arch: $ARCH"
    ;;
esac

require_cmd perl
require_cmd make
require_cmd cp

if [[ "$LEGACY_PRESET" == "1" ]]; then
  LIBPCAP_VERSION="1.9.1"
  TCPDUMP_VERSION="4.9.3"
  BASE_CFLAGS="$BASE_CFLAGS -std=gnu89"
fi

if [[ -z "$WORK_DIR" ]]; then
  WORK_DIR="$(pwd)/out/tcpdump-android-build-${ARCH}-api${API}"
fi
mkdir -p "$WORK_DIR"
WORK_DIR="$(abspath "$WORK_DIR")"

if [[ -z "$DOWNLOAD_DIR" ]]; then
  DOWNLOAD_DIR="$WORK_DIR/downloads"
fi
if [[ -z "$PREFIX" ]]; then
  PREFIX="$WORK_DIR/prefix"
fi
if [[ -z "$OUTPUT" ]]; then
  OUTPUT="$WORK_DIR/dist/tcpdump"
fi

DOWNLOAD_DIR="$(abspath "$DOWNLOAD_DIR")"
PREFIX="$(abspath "$PREFIX")"
OUTPUT="$(abspath "$OUTPUT")"

BUILD_ROOT="$WORK_DIR/build"
SRC_ROOT="$WORK_DIR/src"
LIBPCAP_BUILD="$BUILD_ROOT/$(basename "$LIBPCAP_SRC")"
TCPDUMP_BUILD="$BUILD_ROOT/$(basename "$TCPDUMP_SRC")"

mkdir -p "$BUILD_ROOT" "$SRC_ROOT" "$DOWNLOAD_DIR" "$PREFIX" "$(dirname "$OUTPUT")"

if [[ -z "$NDK_URL" ]]; then
  NDK_URL="https://dl.google.com/android/repository/android-ndk-${NDK_VERSION}-linux.zip"
fi
if [[ -z "$LIBPCAP_URL" ]]; then
  LIBPCAP_URL="https://www.tcpdump.org/release/libpcap-${LIBPCAP_VERSION}.tar.gz"
fi
if [[ -z "$TCPDUMP_URL" ]]; then
  TCPDUMP_URL="https://www.tcpdump.org/release/tcpdump-${TCPDUMP_VERSION}.tar.gz"
fi

if [[ -n "$NDK" ]]; then
  NDK="$(abspath "$NDK")"
  [[ -d "$NDK" ]] || die "NDK path does not exist: $NDK"
else
  NDK="$(resolve_input_dir "NDK" "" "$NDK_URL" "$DOWNLOAD_DIR" "$SRC_ROOT")"
fi

LIBPCAP_SRC="$(resolve_input_dir "libpcap" "$LIBPCAP_SRC" "$LIBPCAP_URL" "$DOWNLOAD_DIR" "$SRC_ROOT")"
TCPDUMP_SRC="$(resolve_input_dir "tcpdump" "$TCPDUMP_SRC" "$TCPDUMP_URL" "$DOWNLOAD_DIR" "$SRC_ROOT")"

TOOLCHAIN="$NDK/toolchains/llvm/prebuilt/linux-x86_64"
[[ -d "$TOOLCHAIN" ]] || die "expected Linux toolchain not found: $TOOLCHAIN"

LIBPCAP_BUILD="$BUILD_ROOT/$(basename "$LIBPCAP_SRC")"
TCPDUMP_BUILD="$BUILD_ROOT/$(basename "$TCPDUMP_SRC")"

copy_source_tree "$LIBPCAP_SRC" "$LIBPCAP_BUILD"
copy_source_tree "$TCPDUMP_SRC" "$TCPDUMP_BUILD"

export CC="$TOOLCHAIN/bin/${TARGET_TRIPLE}${API}-clang"
export CXX="$TOOLCHAIN/bin/${TARGET_TRIPLE}${API}-clang++"
export AR="$TOOLCHAIN/bin/llvm-ar"
export RANLIB="$TOOLCHAIN/bin/llvm-ranlib"
export STRIP="$TOOLCHAIN/bin/llvm-strip"
export LD="$TOOLCHAIN/bin/ld"
export CFLAGS="$BASE_CFLAGS"
export CXXFLAGS="$CFLAGS"
export CPPFLAGS=""
export LDFLAGS="-pie"

[[ -x "$CC" ]] || die "compiler not found: $CC"

printf '==> Building libpcap for %s (API %s)\n' "$TARGET_TRIPLE" "$API"
(
  cd "$LIBPCAP_BUILD"
  ./configure \
    --host="$TARGET_TRIPLE" \
    --with-pcap=linux \
    --disable-shared \
    --enable-static \
    --prefix="$PREFIX"

  patch_disable_packet_ring "$LIBPCAP_BUILD/config.h"

  make -j"$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 4)"
  make install
)

printf '==> Building tcpdump for %s (API %s)\n' "$TARGET_TRIPLE" "$API"
(
  cd "$TCPDUMP_BUILD"
  ./configure \
    --host="$TARGET_TRIPLE" \
    --disable-shared \
    --enable-static \
    --without-crypto \
    --prefix="$PREFIX" \
    CPPFLAGS="-I$PREFIX/include" \
    LDFLAGS="-pie -L$PREFIX/lib" \
    LIBS="$PREFIX/lib/libpcap.a"

  make -j"$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 4)"
)

cp "$TCPDUMP_BUILD/tcpdump" "$OUTPUT"
"$STRIP" "$OUTPUT" || true

cat <<EOF
==> Done
Binary: $OUTPUT
Downloaded inputs cache: $DOWNLOAD_DIR

Suggested deploy:
  adb push "$OUTPUT" /data/local/tmp/tcpdump
  adb shell su -c 'chmod 755 /data/local/tmp/tcpdump'
  adb shell su -c '/data/local/tmp/tcpdump -i any -w /data/local/tmp/test.pcap'
EOF
