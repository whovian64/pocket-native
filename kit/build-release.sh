#!/bin/bash
# kit/build-release.sh <out dir>: the kit's window for a release, Linux built
# in the handhelds' build image (its old C library runs on most systems) and
# Windows cross built with MinGW
set -euo pipefail
out="$(realpath -m "${1:?usage: kit/build-release.sh <out dir>}")"
repo="$(cd "$(dirname "$0")/.." && pwd)"
engine=$(command -v podman || command -v docker)
work="$out/.work"
mkdir -p "$work"
hash=$(sha256sum "$repo/scripts/container/Containerfile" | cut -c1-12)
linux="pocket-native-build:$hash"
"$engine" image exists "$linux" 2>/dev/null || "$engine" build -t "$linux" -f "$repo/scripts/container/Containerfile" "$repo/scripts/container"
"$engine" build -q -t pocket-native-kit-windows -f "$repo/kit/Containerfile.windows" "$repo/kit" >/dev/null
user=(--user "$(id -u):$(id -g)")
[ "$(basename "$engine")" = podman ] && user=(--userns=keep-id)
run() { "$engine" run --rm "${user[@]}" -v "$repo:/src:ro" -v "$work:/work" -e CARGO_HOME=/work/cargo -e CARGO_TARGET_DIR=/work/target -e HOME=/work "$@"; }
run "$linux" sh -c 'cd /src && cargo build -q --release --locked -p pocket-native-kit && strip -o /work/pocket-native-kit /work/target/release/pocket-native-kit'
run localhost/pocket-native-kit-windows sh -c 'cd /src && cargo build -q --release --locked --target x86_64-pc-windows-gnu -p pocket-native-kit && x86_64-w64-mingw32-strip -o /work/pocket-native-kit.exe /work/target/x86_64-pc-windows-gnu/release/pocket-native-kit.exe'
mv "$work/pocket-native-kit" "$work/pocket-native-kit.exe" "$out/"
ls -l "$out/pocket-native-kit" "$out/pocket-native-kit.exe"
