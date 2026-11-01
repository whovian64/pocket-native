#!/bin/bash
# device.sh [--test] <what> ...: looks at the games in the app on the
# handheld over adb, by any part of a game's name.
#
#   games                    the games added, with their title ids
#   env <game> NAME=value... sets the game's switches in data/env.txt
#                            (none clears them), read when it next starts
#   fresh <game> [compilers] switches for a first run's shader building:
#                            a new, empty program cache, compiler threads
#                            (1 by default), and the system's shader cache
#                            emptied
#   log <game> [pattern]     the game's log, or its lines matching pattern
#   shaders <game>           what building shaders cost in its last run
#
# --test looks at Pocket Native (test), io.github.pocketnative.test, rather
# than the app itself.
set -euo pipefail

package=io.github.pocketnative
if [ "${1:-}" = --test ]; then
    package=io.github.pocketnative.test
    shift
fi
what="${1:-}"
[ -n "$what" ] || { sed -n '2,17p' "$0"; exit 2; }
shift
command -v adb >/dev/null || { echo "adb is not installed (Android platform-tools)"; exit 1; }
adb get-state >/dev/null 2>&1 || { echo "no handheld over adb: plug it in with USB debugging on"; exit 1; }

# every game folder the app has, on each storage volume
folders() {
    adb shell "for d in /storage/*/Android/data/$package/files/games/* /sdcard/Android/data/$package/files/games/*; do [ -f \"\$d/game.3ds\" ] && echo \"\$d\"; done" 2>/dev/null | tr -d '\r' | awk '!seen[$0]++'
}

name_of() {
    adb shell "cat '$1/name.txt' 2>/dev/null" | tr -d '\r' | head -1
}

# the folder of the one game whose name or title id holds $1, any case
find_game() {
    local wanted="${1,,}" matches=() folder name
    while IFS= read -r folder; do
        [ -n "$folder" ] || continue
        name=$(name_of "$folder")
        if [[ "${name,,}" == *"$wanted"* || "${folder,,}" == *"$wanted"* ]]; then
            matches+=("$folder")
        fi
    done < <(folders)
    if [ ${#matches[@]} -eq 0 ]; then
        echo "no game in $package is called \"$1\", these are:" >&2
        list >&2
        exit 2
    fi
    if [ ${#matches[@]} -gt 1 ]; then
        echo "\"$1\" could be any of these, name one more closely:" >&2
        for folder in "${matches[@]}"; do echo "   $(name_of "$folder")" >&2; done
        exit 2
    fi
    echo "${matches[0]}"
}

list() {
    local folder
    while IFS= read -r folder; do
        [ -n "$folder" ] && echo "   $(basename "$folder")  $(name_of "$folder")"
    done < <(folders)
}

set_env() {
    local folder="$1"
    shift
    local file
    file=$(mktemp)
    for setting in "$@"; do echo "$setting"; done > "$file"
    if [ $# -eq 0 ]; then
        adb shell "rm -f '$folder/data/env.txt'"
        echo "== no switches for $(name_of "$folder")"
    else
        adb push "$file" "$folder/data/env.txt" >/dev/null
        adb shell "chmod 666 '$folder/data/env.txt'" 2>/dev/null || true
        echo "== $(name_of "$folder") starts with: $*"
    fi
    rm -f "$file"
}

case "$what" in
    games) list ;;
    env)
        folder=$(find_game "${1:?which game}")
        shift
        set_env "$folder" "$@"
        ;;
    fresh)
        folder=$(find_game "${1:?which game}")
        compilers="${2:-1}"
        cache="/data/user/0/$package/files/fresh-$(date +%s)"
        set_env "$folder" "ZAKURO_COMPILERS=$compilers" "XDG_CACHE_HOME=$cache"
        if adb shell "rm -rf /data/user/0/$package/cache/*" 2>/dev/null; then
            echo "== the system's shader cache emptied"
        else
            echo "== could not empty the system's shader cache, the run may be quicker than a real first one"
        fi
        echo "== start the game in the app, play, quit with L3 + Start, then: $0 $([ $package = io.github.pocketnative ] || echo '--test ')shaders \"$1\""
        ;;
    log)
        folder=$(find_game "${1:?which game}")
        if [ -n "${2:-}" ]; then
            adb shell "grep -E '$2' '$folder/data/log.txt'" | tr -d '\r'
        else
            adb shell "cat '$folder/data/log.txt'" | tr -d '\r'
        fi
        ;;
    shaders)
        folder=$(find_game "${1:?which game}")
        adb shell "grep -E 'prepared|slow program|threads compiling|programs compiled|shader compiling' '$folder/data/log.txt'" | tr -d '\r' | tail -25
        ;;
    *) sed -n '2,17p' "$0"; exit 2 ;;
esac
