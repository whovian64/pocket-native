#!/bin/bash
# inside.sh <rom> [aes_keys.txt]: the build, in the container. /src is the
# repository, read only, /work the scratch space kept between builds, and
# /out where "Pocket Native" is put together.
set -euo pipefail

# the open-source system font from Azahar's archives, generated from them
# at build time rather than shipped
FONT_URL=https://raw.githubusercontent.com/azahar-emu/azahar/c9e1e946a0b5d30a58ae14dee83c1f203a49622d/externals/open_source_archives/include/shared_font.app.romfs.h
# and their open-source Mii parts, heads with no faces, for Mii heads where
# the console's own aren't given
MII_URL=https://raw.githubusercontent.com/azahar-emu/azahar/c9e1e946a0b5d30a58ae14dee83c1f203a49622d/externals/open_source_archives/include/mii.app.romfs.h

rom="$1"
keys="${2:-}"
cd /src

# a Z3DS-compressed .zcci/.zcia is its zstd stream after the header and
# the metadata, whose sizes the header gives
case "${rom,,}" in
    *.zcci|*.zcia)
        echo "== decompressing the ${rom##*.} (Z3DS)"
        kind=.${rom##*.}
        skip=$(( $(od -An -tu2 -j10 -N2 "$rom") + $(od -An -tu4 -j12 -N4 "$rom") ))
        tail -c +$((skip + 1)) "$rom" | zstd -d -c > "/work/game.${kind#.z}"
        rom="/work/game.${kind#.z}"
        ;;
esac
case "${rom,,}" in
    *.cia)
        [ -n "$keys" ] || { echo "a .cia needs your console's aes_keys.txt, give it with --keys"; exit 1; }
        echo "== decrypting the .cia"
        python3 tools/cia2cci.py "$rom" /work/game.3ds "$keys"
        game=/work/game.3ds
        ;;
    *)
        game="$rom"
        # without the no-crypto flag, decrypted with the keys, or only the
        # flag fixed where an older tool decrypted it and left the flag set
        if [ "$(dd if="$rom" bs=4 skip=64 count=1 2>/dev/null)" = NCSD ]; then
            first=$(od -An -tu4 -j$((0x120)) -N4 "$rom")
            if (( ! ($(od -An -tu1 -j$((first * 0x200 + 0x18F)) -N1 "$rom") & 4) )); then
                echo "== decrypting the game"
                python3 tools/cia2cci.py "$rom" /work/game.decrypted.3ds ${keys:+"$keys"}
                game=/work/game.decrypted.3ds
            fi
        fi
        ;;
esac
id=$(python3 tools/title_id.py "$game")
name=$(sed -n '/^name = "/{s/^name = "\(.*\)"$/\1/p;q}' "titles/$id/profile.toml" 2>/dev/null || true)
echo "== $id ${name:-(no profile yet)}"

echo "== building the recompiler and Pocket Native"
cargo build --release --locked -p recomp3ds --bin 3dsrecomp
cargo build --release --locked --target aarch64-unknown-linux-gnu -p pocket-native

echo "== recompiling the game for the handheld"
# where the handheld had to interpret, which it writes beside the game on
# the card, comes back with the card's folder and is recompiled too
hints="$HOME/.local/share/3dsrecomp/$id.hints"
rm -f "$hints"
if [ -s "/out/Pocket Native/pocket-native/games/$id/title.hints" ]; then
    mkdir -p "$(dirname "$hints")"
    cp "/out/Pocket Native/pocket-native/games/$id/title.hints" "$hints"
    echo "== with $(wc -l < "$hints") places the handheld interpreted"
fi
overrides=()
[ -d "titles/$id/overrides" ] && overrides=(--overrides "titles/$id/overrides")
RECOMP_CFLAGS=-DRECOMP_FASTMEM CC=aarch64-linux-gnu-gcc-8 "$CARGO_TARGET_DIR/release/3dsrecomp" build "$game" "/work/recomp/$id" "${overrides[@]}"

if [ ! -f /work/shared_font.bin ]; then
    echo "== making the system font"
    curl -sSfL "$FONT_URL" -o /work/shared_font.app.romfs.h
    python3 tools/make_shared_font.py /work/shared_font.app.romfs.h /work/shared_font.bin
fi

echo "== putting Pocket Native together"
top="/out/Pocket Native"
pn="$top/pocket-native"
mkdir -p "$pn/games/$id/sysdata"
aarch64-linux-gnu-strip -o "$pn/pocket-native" "$CARGO_TARGET_DIR/aarch64-unknown-linux-gnu/release/pocket-native"
rm -rf "$pn/titles" && cp -r titles "$pn/titles"
cp scripts/device/run.sh scripts/device/power.sh scripts/device/3ds.sh "$pn/"
cp LICENSE CREDITS.md assets/fonts/OFL.txt "$pn/"
python3 tools/third_party.py "$pn/THIRD_PARTY_LICENSES.txt" >/dev/null
aarch64-linux-gnu-strip -o "$pn/games/$id/title.so" "/work/recomp/$id/$id.so"
cp /work/shared_font.bin "$pn/games/$id/sysdata/shared_font.bin"
# Mii heads, experimental: the console's Mii parts from the kit's folder, a
# game that carries them (Animal Crossing: New Leaf does) or Nintendo's
# servers with the keys' common key, into every game built, else Azahar's
# placeholder
for own in CFL_Res.dat 00000000.app.romfs; do
    [ -f "$own" ] && python3 tools/mii_parts.py "$own" /work/mii_parts.dat && break
done
[ -f "/work/recomp/$id/CFL_Res.dat" ] && [ ! -f /work/mii_parts.dat ] && cp "/work/recomp/$id/CFL_Res.dat" /work/mii_parts.dat
if [ ! -f /work/mii_parts.dat ] && [ -n "$keys" ]; then
    python3 tools/mii_download.py "$keys" /work/mii_parts.dat || true
fi
if [ ! -f /work/mii_parts.dat ] && [ ! -f /work/mii_placeholder.dat ]; then
    curl -sSfL "$MII_URL" -o /work/mii.app.romfs.h && python3 tools/mii_parts.py /work/mii.app.romfs.h /work/mii_placeholder.dat || echo "== no Mii placeholder, Mii heads stay blank"
fi
for sysdata in "$pn"/games/*/sysdata; do
    if [ -f /work/mii_parts.dat ]; then
        cp /work/mii_parts.dat "$sysdata/CFL_Res.dat"
        rm -f "$sysdata/CFL_Res.placeholder.dat"
    elif [ -f /work/mii_placeholder.dat ]; then
        cp /work/mii_placeholder.dat "$sysdata/CFL_Res.placeholder.dat"
    fi
done
cp "$game" "$pn/games/$id/game.3ds"
rm -f /work/game.*
# FAT cards and Windows take no colons and a few other characters in a name
launcher="$top/$(printf %s "${name:-$id}" | sed 's/: / - /g; s/[\\/:*?"<>|]//g').sh"
printf '#!/bin/bash\nexec "$(dirname "$0")/pocket-native/run.sh" %s\n' "$id" > "$launcher"
chmod +x "$launcher" "$pn/run.sh" "$pn/power.sh" "$pn/3ds.sh" "$pn/pocket-native"
echo "== done: copy \"Pocket Native\" into roms/ports on the handheld's card"
