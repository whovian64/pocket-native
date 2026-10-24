#!/bin/bash
# probe.sh [game] [frames] [--out dir] [--serial id]
#
# The game is named as scripts/android/install.sh takes it: any part of its
# name, its title id, or none for the only one built or one from a list.
# Pushes "Pocket Native Android" from the output directory (build/ by
# default) to the handheld with adb, probes the game there as
# device/probe.sh does, and pulls the report back into
# <out>/probe-<title id>-android. USB debugging has to be on (GammaOS:
# Settings > System > Developer options).
set -euo pipefail

repo="$(cd "$(dirname "$0")/../.." && pwd)"
out="$repo/build" id="" frames=3600 adb=(adb)
while [ $# -gt 0 ]; do
    case "$1" in
        --out) out="$(realpath -m "$2")"; shift 2 ;;
        --serial) adb=(adb -s "$2"); shift 2 ;;
        -h|--help) sed -n '2,11p' "$0"; exit 0 ;;
        # a number of frames is a short number; a title id is sixteen digits
        *) if [[ "$1" =~ ^[0-9]{1,9}$ ]]; then frames="$1"; else id="$1"; fi; shift ;;
    esac
done
local_dir="$out/Pocket Native Android/pocket-native"
id=$(bash "$repo/scripts/android/pick_game.sh" "$local_dir/games" "$id")
[ -f "$local_dir/games/$id/title.so" ] || { echo "no $id in $local_dir, build it with scripts/android/build.sh first"; exit 2; }
command -v adb >/dev/null || { echo "adb is not installed (Android platform-tools)"; exit 1; }

# /data/local/tmp is the one place the shell user may run programs and load
# libraries from; the card is mounted noexec
remote=/data/local/tmp/pocket-native
"${adb[@]}" wait-for-device
echo "== the handheld: $("${adb[@]}" shell getprop ro.product.model | tr -d '\r'), Android $("${adb[@]}" shell getprop ro.build.version.release | tr -d '\r')"
echo "== pushing, unchanged files are skipped"
"${adb[@]}" shell mkdir -p "$remote"
"${adb[@]}" push --sync "$local_dir/." "$remote/" >/dev/null
"${adb[@]}" shell chmod 755 "$remote/pocket-native" "$remote/probe.sh"

echo "== probing $id for $frames frames"
status=0
"${adb[@]}" shell sh "$remote/probe.sh" "$id" "$frames" || status=$?

report="$out/probe-$id-android"
rm -rf "$report"
mkdir -p "$report"
"${adb[@]}" pull "$remote/games/$id/probe-$id/." "$report/" >/dev/null || true
"${adb[@]}" pull "$remote/games/$id/data/probe-log.txt" "$report/" >/dev/null 2>&1 || true
# what drew it, for reports from a handheld whose driver misbehaves
{
    "${adb[@]}" shell getprop | grep -E 'ro.product.(model|board)|ro.build.(version.release|display.id)|ro.hardware.egl' || true
    "${adb[@]}" shell dumpsys SurfaceFlinger | grep -m2 -E 'GLES|EGL' || true
} | tr -d '\r' > "$report/device.txt"
echo "== report in $report"
exit $status
