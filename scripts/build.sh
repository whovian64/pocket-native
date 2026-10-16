#!/bin/bash
# build.sh <rom> [--keys aes_keys.txt] [--out dir]
#
# Builds Pocket Native and a game for the handheld from your own ROM, a
# decrypted .3ds/.cci or a .cia with your console's aes_keys.txt, inside a
# container (podman or docker). Leaves "Pocket Native" in the output
# directory, build/ by default, to copy into roms/ports on the card.
set -euo pipefail

repo="$(cd "$(dirname "$0")/.." && pwd)"
rom="" keys="" out="$repo/build"
while [ $# -gt 0 ]; do
    case "$1" in
        --keys) keys="$(realpath "$2")"; shift 2 ;;
        --out) out="$(realpath -m "$2")"; shift 2 ;;
        -h|--help) sed -n '2,7p' "$0"; exit 0 ;;
        *) rom="$(realpath "$1")"; shift ;;
    esac
done
if [ ! -f "$rom" ]; then
    echo "usage: scripts/build.sh <rom> [--keys aes_keys.txt] [--out dir]"
    exit 2
fi
engine=$(command -v podman || command -v docker) || { echo "the build needs podman or docker"; exit 1; }
image=pocket-native-build:$(sha256sum "$repo/scripts/container/Containerfile" | cut -c1-12)
if ! "$engine" image inspect "$image" >/dev/null 2>&1; then
    echo "== making the build container, once"
    "$engine" build -f "$repo/scripts/container/Containerfile" -t "$image" "$repo/scripts/container"
fi

mkdir -p "$out/.work"
user=(--user "$(id -u):$(id -g)")
[ "$(basename "$engine")" = podman ] && user=(--userns=keep-id)
mounts=(-v "$repo:/src:ro" -v "$out/.work:/work" -v "$out:/out" -v "$(dirname "$rom"):/rom:ro")
args=("/rom/$(basename "$rom")")
if [ -n "$keys" ]; then
    mounts+=(-v "$(dirname "$keys"):/keys:ro")
    args+=("/keys/$(basename "$keys")")
fi
"$engine" run --rm "${user[@]}" "${mounts[@]}" -e CARGO_HOME=/work/cargo -e CARGO_TARGET_DIR=/work/target -e HOME=/work \
    "$image" bash /src/scripts/container/inside.sh "${args[@]}"
