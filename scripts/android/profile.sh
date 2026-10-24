#!/bin/bash
# profile.sh [game] [--out dir] [--serial id]
#
# Pulls the CPU profile a game wrote on the handheld, with
# ZAKURO_HOST_PROFILE=from,frames in its data/env.txt and the game quit
# with FN + Start, and prints where each thread spent its time, by function:
# the engine's own, and the game's recompiled ones by their 3DS address.
# The game is named as install.sh takes it.
set -euo pipefail

repo="$(cd "$(dirname "$0")/../.." && pwd)"
out="$repo/build" what="" adb=(adb)
while [ $# -gt 0 ]; do
    case "$1" in
        --out) out="$(realpath -m "$2")"; shift 2 ;;
        --serial) adb=(adb -s "$2"); shift 2 ;;
        -h|--help) sed -n '2,9p' "$0"; exit 0 ;;
        *) what="$1"; shift ;;
    esac
done
pn="$out/Pocket Native Android/pocket-native"
id=$(bash "$repo/scripts/android/pick_game.sh" "$pn/games" "$what")
package=$(cat "$pn/games/$id/package.txt")
profile="$out/profile-$id.txt"
"${adb[@]}" pull "/sdcard/Android/data/$package/files/data/zakuro-host-profile.txt" "$profile" >/dev/null \
    || { echo "no profile on the handheld: put ZAKURO_HOST_PROFILE=600,36000 in data/env.txt, play, and quit with FN + Start"; exit 1; }
engine=$(command -v podman || command -v docker) || { echo "this needs podman or docker"; exit 1; }
image=pocket-native-android:$(sha256sum "$repo/scripts/android/Containerfile" | cut -c1-12)
user=(--user "$(id -u):$(id -g)")
[ "$(basename "$engine")" = podman ] && user=(--userns=keep-id)
"$engine" run --rm "${user[@]}" -v "$repo:/src:ro" -v "$out/.work-android:/work:ro" -v "$pn/games/$id:/game:ro" \
    -v "$out:/out:ro" "$image" python3 /src/tools/symbolize_profile.py "/out/$(basename "$profile")" /game/pocket-native.apk \
    /opt/ndk/toolchains/llvm/prebuilt/linux-x86_64/bin/llvm-symbolizer \
    "libpocket_native.so=/work/target/aarch64-linux-android/release/libpocket_native.so" \
    "libtitle.so=/work/recomp-android/$id/$id.so" | tee "$out/profile-$id-summary.txt"
echo "== also in $out/profile-$id-summary.txt"
