#!/bin/bash
# install.sh [game] [--out dir] [--serial id] [--no-start] [--internal | --sd <card>] [--log]
#
# Puts a game scripts/android/build.sh made on the handheld with adb, the game
# named by any part of its name ("pokemon x") or its title id, or with none
# named the only one built, or one picked from a list: installs
# its app, pushes the system font and the profiles to the app's files folder
# and the game to the app's own folder on the SD card, or with no card (or
# --internal) beside the rest (later pushes skip what has not changed), and
# starts it. --sd names the card, as /storage lists it, where there are two.
# USB debugging has to be on. The app's log is then in
# /sdcard/Android/data/<package>/files/data/log.txt, which --log pulls, with
# the hints of where the game ran code the recompiler missed, for the next
# build to recompile.
set -euo pipefail

repo="$(cd "$(dirname "$0")/../.." && pwd)"
out="$repo/build" id="" adb=(adb) start=1 log_only=0 card="" internal=0
while [ $# -gt 0 ]; do
    case "$1" in
        --out) out="$(realpath -m "$2")"; shift 2 ;;
        --serial) adb=(adb -s "$2"); shift 2 ;;
        --no-start) start=0; shift ;;
        --log) log_only=1; shift ;;
        --internal) internal=1; shift ;;
        --sd) card="$2"; shift 2 ;;
        -h|--help) sed -n '2,14p' "$0"; exit 0 ;;
        *) id="$1"; shift ;;
    esac
done
pn="$out/Pocket Native Android/pocket-native"
id=$(bash "$repo/scripts/android/pick_game.sh" "$pn/games" "$id")
game="$pn/games/$id"
name=$(cat "$game/name.txt" 2>/dev/null || echo "$id")
[ -f "$game/pocket-native.apk" ] || { echo "no app for $id in $game, build it with scripts/android/build.sh first"; exit 2; }
command -v adb >/dev/null || { echo "adb is not installed (Android platform-tools)"; exit 1; }
package=$(cat "$game/package.txt")
files="/sdcard/Android/data/$package/files"
"${adb[@]}" wait-for-device

if [ $log_only = 1 ]; then
    "${adb[@]}" pull "$files/data/log.txt" "$out/log-$id.txt" >/dev/null && echo "== log in $out/log-$id.txt"
    "${adb[@]}" pull "$files/data/panic.txt" "$out/panic-$id.txt" >/dev/null 2>&1 && echo "== and a panic, in $out/panic-$id.txt"
    # where the game ran code the recompiler missed, which the next build
    # recompiles: the build container's HOME is the work folder, where
    # 3dsrecomp looks for <title id>.hints
    hints="$out/.work-android/.local/share/3dsrecomp"
    mkdir -p "$hints"
    if "${adb[@]}" pull "$files/libtitle.hints" "$hints/$id.hints.new" >/dev/null 2>&1; then
        # what the handheld found, added to what earlier pulls found
        { cat "$hints/$id.hints.new"; cat "$hints/$id.hints" 2>/dev/null || true; } | { grep -v '^#' || true; } | sort -u > "$hints/$id.hints.merged"
        mv "$hints/$id.hints.merged" "$hints/$id.hints"
        rm -f "$hints/$id.hints.new"
        echo "== $(wc -l < "$hints/$id.hints") places the game ran uncompiled code; the next build of it recompiles them"
    fi
    exit 0
fi

echo "== installing $name ($package)"
# copied whole before installing, the streamed install can stall
"${adb[@]}" install --no-streaming -r "$game/pocket-native.apk" >/dev/null
"${adb[@]}" shell mkdir -p "$files/sysdata" "$files/titles"
"${adb[@]}" push --sync "$game/sysdata/." "$files/sysdata/" >/dev/null
"${adb[@]}" push --sync "$pn/titles/." "$files/titles/" >/dev/null

# the game, the big one, on the SD card when there is one: a card is what
# /storage lists besides the built-in storage, its name its volume id
if [ $internal = 0 ] && [ -z "$card" ]; then
    card=$("${adb[@]}" shell ls /storage | tr -d '\r' | grep -v -x -e emulated -e self | head -1 || true)
fi
target="$files"
if [ $internal = 0 ] && [ -n "$card" ]; then
    target="/storage/$card/Android/data/$package/files"
    if ! "${adb[@]}" shell ls -d "$target" >/dev/null 2>&1; then
        # the app makes its own folder on the card the first time it starts
        "${adb[@]}" shell am start -W -n "$package/android.app.NativeActivity" >/dev/null 2>&1 || true
        sleep 2
        "${adb[@]}" shell am force-stop "$package" >/dev/null 2>&1 || true
        "${adb[@]}" shell mkdir -p "$target"
    fi
    echo "== pushing the game to the SD card ($card), unchanged files are skipped"
else
    echo "== pushing the game to the built-in storage, unchanged files are skipped"
fi
"${adb[@]}" push --sync "$game/game.3ds" "$target/game.3ds" >/dev/null
if [ "$target" != "$files" ]; then
    # a copy from before there was a card, which the card's now stands for
    "${adb[@]}" shell rm -f "$files/game.3ds"
fi
if [ $start = 1 ]; then
    echo "== starting it"
    "${adb[@]}" shell am start -n "$package/android.app.NativeActivity" >/dev/null
fi
echo "== done; scripts/android/install.sh \"$name\" --log pulls its log"
