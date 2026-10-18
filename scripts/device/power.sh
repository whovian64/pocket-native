#!/bin/sh
# power.sh saver|balanced|performance: the CPU, GPU and memory speeds for
# Pocket Native's Power setting
cpu=/sys/devices/system/cpu/cpufreq/policy0
gpu=$(ls -d /sys/devices/platform/*.gpu/devfreq/*.gpu 2>/dev/null | head -1)
dmc=/sys/devices/platform/dmc/devfreq/dmc
# Knulli runs ports as root, with no sudo
set_to() { if [ "$(id -u)" = 0 ]; then echo "$2" > "$1"; else echo "$2" | sudo -n tee "$1" >/dev/null; fi 2>/dev/null; }

case "$1" in
    performance) governors="performance performance performance" ;;
    *) governors="ondemand simple_ondemand dmc_ondemand" ;;
esac
set -- "$1" $governors
set_to "$cpu/scaling_max_freq" "$(cat "$cpu/cpuinfo_max_freq")"
set_to "$cpu/scaling_governor" "$2"
[ -n "$gpu" ] && set_to "$gpu/governor" "$3"
set_to "$dmc/governor" "$4"
[ "$1" = saver ] && set_to "$cpu/scaling_max_freq" 1200000
exit 0
