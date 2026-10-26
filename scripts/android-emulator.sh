#!/bin/sh
set -eu

# Runs a headless x86_64 Android emulator for testing the app.
# Usage:
#   scripts/android-emulator.sh             # adb on localhost only
#   scripts/android-emulator.sh --lan ADDR  # also relay adb on ADDR:5555
#
# The APK is arm64-only; the API 34 google_apis x86_64 image runs it
# through its built-in ARM translation. Install the SDK packages once:
#   sdkmanager "emulator" "system-images;android-34;google_apis;x86_64"
#
# --lan exposes the emulator's adbd, which has no authentication: anyone
# who can reach ADDR:5555 gets full control of the emulator. From the other
# host: `adb connect ADDR:5555`, then `scrcpy` for the screen.

REPO_ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)

lan_addr=
while [ $# -gt 0 ]; do
    case "$1" in
        --lan)
            lan_addr="${2:?--lan needs an address}"
            shift 2
            ;;
        *)
            echo "Usage: $0 [--lan ADDR]" >&2
            exit 1
            ;;
    esac
done

export ANDROID_SDK_ROOT="${ANDROID_SDK_ROOT:-$REPO_ROOT/.scratch/android-sdk}"
export ANDROID_AVD_HOME="${ANDROID_AVD_HOME:-$REPO_ROOT/.scratch/avd}"
AVD_NAME=anazoa
SYSTEM_IMAGE="system-images;android-34;google_apis;x86_64"

if [ ! -d "$ANDROID_AVD_HOME/$AVD_NAME.avd" ]; then
    mkdir -p "$ANDROID_AVD_HOME"
    echo no | "$ANDROID_SDK_ROOT/cmdline-tools/latest/bin/avdmanager" create avd \
        -n "$AVD_NAME" -k "$SYSTEM_IMAGE" -d pixel_6
    # The default AVD has no hardware keyboard, a small data partition, and
    # 2 GB RAM / 4 cores, which swaps heavily once the app is running.
    sed -i -e '/^hw.keyboard=/d' -e '/^disk.dataPartition.size=/d' \
        -e '/^hw.ramSize=/d' -e '/^hw.cpu.ncore=/d' "$ANDROID_AVD_HOME/$AVD_NAME.avd/config.ini"
    cat >> "$ANDROID_AVD_HOME/$AVD_NAME.avd/config.ini" <<EOF
hw.keyboard=yes
disk.dataPartition.size=8G
hw.ramSize=4096
hw.cpu.ncore=8
EOF
fi

# Rendering is software-only (swiftshader), where the launcher's gesture
# animations can stall and leave a half-finished transition on screen.
# Turn animations off and use 3-button navigation once the device boots.
# Both settings persist on the data partition; reapplying is harmless.
(
    adb="$ANDROID_SDK_ROOT/platform-tools/adb -s emulator-5554"
    $adb wait-for-device
    until [ "$($adb shell getprop sys.boot_completed 2>/dev/null | tr -d '\r')" = 1 ]; do
        sleep 2
    done
    for scale in window_animation_scale transition_animation_scale animator_duration_scale; do
        $adb shell settings put global "$scale" 0
    done
    $adb shell cmd overlay enable-exclusive --category \
        com.android.internal.systemui.navbar.threebutton
) &

relay_pid=
if [ -n "$lan_addr" ]; then
    # The emulator binds adb to 127.0.0.1:5555 only; binding the same port on
    # a specific LAN address doesn't conflict with it.
    socat "TCP-LISTEN:5555,bind=$lan_addr,fork,reuseaddr" TCP:127.0.0.1:5555 &
    relay_pid=$!
    trap 'kill "$relay_pid" 2>/dev/null || true' EXIT INT TERM
fi

"$ANDROID_SDK_ROOT/emulator/emulator" -avd "$AVD_NAME" -port 5554 \
    -no-window -no-audio -no-boot-anim -no-snapshot -gpu swiftshader_indirect
