#!/bin/bash
# run.sh <title id> [rom]: plays a game Pocket Native was built with, from the
# launcher scripts/build.sh puts beside this folder, or from 3ds.sh with the
# ROM it was given. EmulationStation waits, stopped, while the game runs, and
# the game starts again when its menu's Reset asks for it.
dir="$(cd "$(dirname "$0")" && pwd)"
game="$dir/games/$1"
rom="${2:-$game/game.3ds}"
data="$game/data"
mkdir -p "$data" "$dir/config"

export ZAKURO_FAST=1 ZAKURO_KMS=1 ZAKURO_DATA="$data" ZAKURO_RECOMPILED="$game/title.so" ZAKURO_SHADER_MANIFEST="$data/shaders.txt"
export POCKET_NATIVE_TITLES="$dir/titles" POCKET_NATIVE_POWER="$dir/power.sh" POCKET_NATIVE_CONFIG="$dir/config"
# draws to leave out besides the game's options, as ZAKURO_DROP_DRAWS takes them
[ -f "$data/drops.txt" ] && export ZAKURO_DROP_DRAWS="$(cat "$data/drops.txt")"

es=$(pgrep -f "^/usr/bin/emulationstation/emulationstation( |$)" || true)
[ -n "$es" ] && kill -STOP $es
# spruceOS (Miyoo Flip) keeps two of the four cores offline to save battery;
# wake them for the game and put them back as they were afterwards
offline=""
for c in /sys/devices/system/cpu/cpu[1-9]/online; do
    [ -f "$c" ] && [ "$(cat "$c")" = 0 ] && { offline="$offline $c"; echo 1 > "$c" 2>/dev/null; }
done
if [ "$(id -u)" = 0 ]; then echo 1 > /proc/sys/debug/exception-trace 2>/dev/null; else sudo -n sh -c "echo 1 > /proc/sys/debug/exception-trace" 2>/dev/null; fi
cd "$game"
while true; do
    "$dir/pocket-native" "$rom" > "$data/log.txt" 2>&1
    status=$?
    [ $status -eq 3 ] || break
done
if [ $status -ne 0 ]; then
    echo "exit status $status" >> "$data/log.txt"
    { dmesg | grep -iE "pocket|segv|sigsegv|fault|Oops|killed" | tail -20; dmesg | tail -10; } >> "$data/log.txt"
fi
"$dir/power.sh" balanced
for c in $offline; do echo 0 > "$c" 2>/dev/null; done
[ -n "$es" ] && kill -CONT $es
sudo -n systemctl restart oga_events >/dev/null 2>&1 &
exit 0
