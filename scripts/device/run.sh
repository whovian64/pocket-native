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
# ROCKNIX: sway has the panels. Switching the console away from its VT has
# seatd take them from it for the game, and switching back gives them back.
# tty12 stays a text console, cleared, as the kernel ignores switches from
# one in graphics mode
vt=""
if systemctl is-active -q sway.service 2>/dev/null && command -v chvt >/dev/null; then
    vt=$(fgconsole 2>/dev/null)
    [ -z "$vt" ] || [ "$vt" = 12 ] && vt=1
    printf '\033[2J\033[H\033[?25l' > /dev/tty12 2>/dev/null
    chvt 12
    # a blanked console turns the panels off under the game
    echo 0 > /sys/class/graphics/fb0/blank 2>/dev/null
fi
sudo -n sh -c "echo 1 > /proc/sys/debug/exception-trace" 2>/dev/null
cd "$game"
while true; do
    "$dir/pocket-native" "$rom" > "$data/log.txt" 2>&1
    status=$?
    [ $status -eq 3 ] || break
done
if [ $status -ne 0 ]; then
    echo "exit status $status" >> "$data/log.txt"
    dmesg | tail -40 >> "$data/log.txt"
fi
"$dir/power.sh" balanced
if [ -n "$vt" ]; then
    chvt "$vt"
    # sway's libinput finds its devices again only once one changes
    udevadm trigger --action=change /sys/class/input/event0 2>/dev/null
    # a sway that came back with no panels is started again, outside the
    # menu's unit, which goes with it
    sock=${SWAYSOCK:-$(ls "${XDG_RUNTIME_DIR:-/var/run/0-runtime-dir}"/sway-ipc.*.sock 2>/dev/null | head -n1)}
    if [ -n "$sock" ] && command -v swaymsg >/dev/null; then
        i=0
        until swaymsg -s "$sock" -t get_outputs 2>/dev/null | grep -q '"active": true'; do
            i=$((i + 1))
            if [ $i -ge 60 ]; then
                echo "sway has no panels after the game, starting it again" >> "$data/log.txt"
                systemd-run --no-block --collect sh -c 'systemctl restart sway.service; systemctl is-active -q essway.service || systemctl start essway.service' >/dev/null 2>&1
                break
            fi
            sleep 0.05
        done
    fi
fi
[ -n "$es" ] && kill -CONT $es
sudo -n systemctl restart oga_events >/dev/null 2>&1 &
exit 0
