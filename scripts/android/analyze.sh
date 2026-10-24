#!/bin/bash
# analyze.sh [game] [--out dir]
#
# Reports how much of a built game's code the recompiler finds on its own,
# and sorts the places the handheld still had to interpret (the hints
# install.sh --log pulls) by what the recompiler made of them, for telling
# what it misses in games generally. The game is named as install.sh takes
# it. Prints addresses and counts only, nothing of the game itself.
set -euo pipefail

repo="$(cd "$(dirname "$0")/../.." && pwd)"
out="$repo/build" what=""
while [ $# -gt 0 ]; do
    case "$1" in
        --out) out="$(realpath -m "$2")"; shift 2 ;;
        -h|--help) sed -n '2,9p' "$0"; exit 0 ;;
        *) what="$1"; shift ;;
    esac
done
pn="$out/Pocket Native Android/pocket-native"
id=$(bash "$repo/scripts/android/pick_game.sh" "$pn/games" "$what")
engine=$(command -v podman || command -v docker) || { echo "this needs podman or docker"; exit 1; }
image=pocket-native-android:$(sha256sum "$repo/scripts/android/Containerfile" | cut -c1-12)
"$engine" image inspect "$image" >/dev/null 2>&1 || { echo "build a game with scripts/android/build.sh first"; exit 1; }
user=(--user "$(id -u):$(id -g)")
[ "$(basename "$engine")" = podman ] && user=(--userns=keep-id)
# HOME is the work folder, where the build keeps the hints, as in build.sh
"$engine" run --rm "${user[@]}" -v "$repo:/src:ro" -v "$out/.work-android:/work" -v "$pn/games/$id:/game:ro" \
    -e CARGO_HOME=/work/cargo -e CARGO_TARGET_DIR=/work/target -e HOME=/work -w /src "$image" \
    bash -c 'cargo build -q --release --locked -p recomp3ds --bin 3dsrecomp && "$CARGO_TARGET_DIR/release/3dsrecomp" analyze /game/game.3ds'
