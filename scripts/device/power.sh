#!/bin/sh
# power.sh saver|balanced|performance|normal: the CPU, GPU and memory speeds
# for Pocket Native's Power setting, normal as the system keeps them, for after
# a game
cpu=/sys/devices/system/cpu/cpufreq/policy0
# Rockchip's ff400000.gpu and the H700's 1800000.gpu alike, as muOS finds it
gpu=$(ls -d /sys/class/devfreq/*gpu* 2>/dev/null | head -1)
dmc=/sys/devices/platform/dmc/devfreq/dmc
# Knulli runs ports as root, with no sudo
set_to() { if [ "$(id -u)" = 0 ]; then echo "$2" > "$1"; else echo "$2" | sudo -n tee "$1" >/dev/null; fi 2>/dev/null; }

# the governors the system had before the game, which normal puts back:
# ROCKNIX keeps others than ArkOS, - for one a handheld lacks
saved=/tmp/pocket-native-governors
governor() { cat "$1/$2" 2>/dev/null || echo -; }
[ "$1" = normal ] || [ -f "$saved" ] || echo "$(governor "$cpu" scaling_governor) $(governor "$gpu" governor) $(governor "$dmc" governor)" > "$saved"

case "$1" in
    performance) governors="performance performance performance" ;;
    # the GPU left on demand stayed slow enough to hold Pokémon X at 16 fps,
    # at its top speed 30, the CPU slowing down where it can all the same
    balanced) governors="ondemand performance performance" ;;
    # nothing changed them, nothing to put back
    *) [ -f "$saved" ] || exit 0; governors=$(cat "$saved"); rm -f "$saved" ;;
esac
set -- "$1" $governors
set_to "$cpu/scaling_max_freq" "$(cat "$cpu/cpuinfo_max_freq")"
[ "$2" = - ] || set_to "$cpu/scaling_governor" "$2"
[ -n "$gpu" ] && [ "$3" != - ] && set_to "$gpu/governor" "$3"
[ "$4" = - ] || set_to "$dmc/governor" "$4"
[ "$1" = saver ] && set_to "$cpu/scaling_max_freq" 1200000
exit 0
