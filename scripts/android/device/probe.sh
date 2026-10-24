#!/system/bin/sh
# probe.sh <title id> [frames]: runs a game Pocket Native was built with for
# Android as fast as it goes, without a display or a gamepad, from an adb
# shell on the handheld, and leaves the report in games/<title id>/probe-<title id>.
# Drawing goes to an offscreen EGL surface on the handheld's own GPU driver,
# so this tests the engine, fastmem, the title's library and the GLES renderer
# on Android before there is an app around them.
dir="$(cd "$(dirname "$0")" && pwd)"
game="$dir/games/$1"
[ -f "$game/title.so" ] || { echo "no game $1 in $dir/games"; exit 2; }
data="$game/data"
mkdir -p "$data" "$dir/config"

export ZAKURO_FAST=1 ZAKURO_DATA="$data" ZAKURO_RECOMPILED="$game/title.so" ZAKURO_SHADER_MANIFEST="$data/shaders.txt"
export POCKET_NATIVE_TITLES="$dir/titles" POCKET_NATIVE_CONFIG="$dir/config"
# a ROM kept on the card instead of beside the game, as ROM=/sdcard/... gives it
rom="${ROM:-$game/game.3ds}"

cd "$game" || exit 1
rm -rf "probe-$1"
"$dir/pocket-native" --probe "$rom" "${2:-3600}" > "$data/probe-log.txt" 2>&1
status=$?
echo "exit status $status" >> "$data/probe-log.txt"
cp "$data/probe-log.txt" "probe-$1/" 2>/dev/null
echo "probe exited with $status, report in $game/probe-$1"
exit $status
